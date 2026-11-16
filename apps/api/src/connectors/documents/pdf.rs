//! PDF text extraction in a child process of the host binary. A PDF comes
//! from an email sender, and a crafted one can recurse past the stack or
//! inflate streams without bound; only the child dies then. The host kills
//! a child past its time or memory limit, and runs at most [`MAX_WORKERS`].

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::Semaphore;

use super::{MAX_TEXT, Unreadable};

/// The first argument that turns the host binary into a PDF worker.
pub const WORKER_ARG: &str = "--else-pdf-worker";
const MAX_WORKERS: usize = 2;
const TIME_LIMIT: Duration = Duration::from_secs(30);
const MEMORY_LIMIT: u64 = 512 * 1024 * 1024;
#[cfg(target_os = "linux")]
const ADDRESS_LIMIT: u64 = 2 * 1024 * 1024 * 1024;
const WORKER_STACK: usize = 64 * 1024 * 1024;
/// Exit status of a worker that parsed the file but found it damaged.
const DAMAGED: i32 = 3;
/// Exit status of a worker whose text is over [`MAX_TEXT`].
const TOO_LARGE: i32 = 4;
/// Separates pages on the worker's stdout; page text never contains it.
const PAGE_BREAK: char = '\u{c}';

static WORKER: OnceLock<PathBuf> = OnceLock::new();

/// Name the executable that starts with [`WORKER_ARG`] and calls
/// [`run_worker`].
pub fn use_worker(executable: PathBuf) {
    let _ = WORKER.set(executable);
}

/// Use the running host binary as the worker; on Linux its running image,
/// which stays executable after an update replaces the file.
pub fn use_this_executable() {
    #[cfg(target_os = "linux")]
    use_worker(PathBuf::from("/proc/self/exe"));
    #[cfg(not(target_os = "linux"))]
    if let Ok(executable) = std::env::current_exe() {
        use_worker(executable);
    }
}

/// The text of each page, extracted in a worker.
pub async fn pages(bytes: Vec<u8>) -> Result<Vec<String>, Unreadable> {
    static SLOTS: Semaphore = Semaphore::const_new(MAX_WORKERS);
    let executable = WORKER.get().ok_or(Unreadable::ReaderFailed)?;
    let _slot = SLOTS
        .acquire()
        .await
        .map_err(|_| Unreadable::ReaderFailed)?;
    let mut child = Command::new(executable)
        .arg(WORKER_ARG)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| {
            tracing::warn!(%error, "the PDF worker did not start");
            Unreadable::ReaderFailed
        })?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let mut stdout = child.stdout.take().expect("piped stdout");
    let pid = child.id();
    let work = async {
        let write = async {
            let _ = stdin.write_all(&bytes).await;
            drop(stdin);
        };
        let read = async {
            let mut output = Vec::new();
            let read = (&mut stdout)
                .take(MAX_TEXT as u64 + 1)
                .read_to_end(&mut output)
                .await;
            read.map(|_| output)
        };
        let watch = async {
            loop {
                tokio::time::sleep(Duration::from_millis(20)).await;
                if pid
                    .and_then(else_visual::isolate::resident_bytes)
                    .is_some_and(|bytes| bytes > MEMORY_LIMIT)
                {
                    return;
                }
            }
        };
        let finish = async {
            let ((), output) = tokio::join!(write, read);
            if output.as_ref().is_ok_and(|output| output.len() > MAX_TEXT) {
                return (output, None);
            }
            let status = child.wait().await.ok();
            (output, status)
        };
        tokio::select! {
            done = finish => Some(done),
            () = watch => None,
        }
    };
    match tokio::time::timeout(TIME_LIMIT, work).await {
        Ok(Some((Ok(output), None))) if output.len() > MAX_TEXT => Err(Unreadable::TooLarge),
        Ok(Some((Ok(output), Some(status)))) if status.success() => {
            Ok(String::from_utf8_lossy(&output)
                .split(PAGE_BREAK)
                .map(str::to_owned)
                .collect())
        }
        Ok(Some((_, Some(status)))) if status.code() == Some(TOO_LARGE) => {
            Err(Unreadable::TooLarge)
        }
        Ok(Some(_)) => Err(Unreadable::Damaged),
        Ok(None) | Err(_) => Err(Unreadable::TooComplex),
    }
}

/// Extract in this process. Only a worker, or a test with a trusted file,
/// calls it.
pub fn extract(bytes: &[u8]) -> Option<Vec<String>> {
    let pages = pdf_extract::extract_text_from_mem_by_pages(bytes).ok()?;
    Some(
        pages
            .into_iter()
            .map(|page| page.replace(PAGE_BREAK, "\n"))
            .collect(),
    )
}

/// The worker's whole life: read a PDF from stdin, write its pages'
/// text separated by [`PAGE_BREAK`], and exit. The host binary calls this when started with [`WORKER_ARG`].
pub fn run_worker() -> ! {
    use std::io::{Read as _, Write as _};
    #[cfg(target_os = "linux")]
    let _ = rustix::process::setrlimit(
        rustix::process::Resource::As,
        rustix::process::Rlimit {
            current: Some(ADDRESS_LIMIT),
            maximum: Some(ADDRESS_LIMIT),
        },
    );
    let mut input = Vec::new();
    if std::io::stdin().read_to_end(&mut input).is_err() {
        std::process::exit(1);
    }
    let pages = std::thread::Builder::new()
        .stack_size(WORKER_STACK)
        .spawn(move || extract(&input))
        .ok()
        .and_then(|thread| thread.join().ok())
        .flatten();
    let Some(pages) = pages else {
        std::process::exit(DAMAGED);
    };
    let text = pages.join(&PAGE_BREAK.to_string());
    if text.len() > MAX_TEXT {
        std::process::exit(TOO_LARGE);
    }
    let mut stdout = std::io::stdout().lock();
    let written = stdout
        .write_all(text.as_bytes())
        .ok()
        .and_then(|()| stdout.flush().ok());
    std::process::exit(if written.is_some() { 0 } else { 1 })
}
