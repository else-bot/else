//! Untrusted HTML renders in a child process of the host binary, so a crash
//! or runaway allocation inside the engine ends only that child. The host
//! kills a worker past its time or memory limit. [`MAX_WORKERS`] dedicated
//! threads run workers from a bounded queue; a full queue refuses at once,
//! and a request whose caller stopped waiting is skipped.

use std::future::Future;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use futures_channel::oneshot;

use serde::{Deserialize, Serialize};

use crate::{Error, Rendered, Theme};

/// The first argument that turns the host binary into a render worker.
pub const WORKER_ARG: &str = "--else-visual-worker";
const MAX_WORKERS: usize = 2;
const QUEUE: usize = 16;
/// Bumped whenever [`Request`] or the reply changes, so a worker binary an
/// update replaced refuses a request it would misread.
const PROTOCOL: u32 = 1;
const TIME_LIMIT: Duration = Duration::from_secs(10);
const MEMORY_LIMIT: u64 = 384 * 1024 * 1024;
/// A worker's own address-space cap where the kernel enforces one.
#[cfg(target_os = "linux")]
const ADDRESS_LIMIT: u64 = 2 * 1024 * 1024 * 1024;
const WORKER_STACK: usize = 64 * 1024 * 1024;

static WORKER: OnceLock<PathBuf> = OnceLock::new();

/// Name the executable that starts with [`WORKER_ARG`] and calls
/// [`run_worker`].
pub fn use_worker(executable: PathBuf) {
    let _ = WORKER.set(executable);
}

/// Use the running host binary as the worker. On Linux this is the running
/// image itself, which stays executable after an update prunes its file.
pub fn use_this_executable() {
    #[cfg(target_os = "linux")]
    use_worker(PathBuf::from("/proc/self/exe"));
    #[cfg(not(target_os = "linux"))]
    if let Ok(executable) = std::env::current_exe() {
        use_worker(executable);
    }
}

#[derive(Serialize, Deserialize)]
struct Request {
    protocol: u32,
    html: String,
    width: u32,
    scale: f32,
    theme: Theme,
}

#[derive(Serialize, Deserialize)]
struct Header {
    width: u32,
    height: u32,
    content_height: u32,
    clipped: bool,
    blocked: Vec<String>,
}

/// Render in a worker process, waiting for the result. See
/// [`crate::render`] for the arguments.
pub fn render(html: &str, width: u32, scale: f32, theme: &Theme) -> Result<Rendered, Error> {
    futures_executor::block_on(submit(html, width, scale, theme))
}

/// Queue a render in a worker process. Dropping the future before a worker
/// starts it skips the render.
pub fn submit(
    html: &str,
    width: u32,
    scale: f32,
    theme: &Theme,
) -> impl Future<Output = Result<Rendered, Error>> + use<> {
    let queued = queue(html, width, scale, theme);
    async move {
        queued?
            .await
            .unwrap_or_else(|_| Err(Error::Render("the render worker stopped".into())))
    }
}

type Reply = Result<Rendered, Error>;

struct Job {
    request: Vec<u8>,
    reply: oneshot::Sender<Reply>,
}

fn queue(
    html: &str,
    width: u32,
    scale: f32,
    theme: &Theme,
) -> Result<oneshot::Receiver<Reply>, Error> {
    if html.len() > crate::MAX_BYTES {
        return Err(Error::TooLarge(html.len()));
    }
    let request = serde_json::to_vec(&Request {
        protocol: PROTOCOL,
        html: html.to_owned(),
        width,
        scale,
        theme: theme.clone(),
    })
    .map_err(|e| Error::Render(e.to_string()))?;
    let (reply, receiver) = oneshot::channel();
    match jobs().try_send(Job { request, reply }) {
        Ok(()) => Ok(receiver),
        Err(TrySendError::Full(_)) => Err(Error::Limit("too many visuals are rendering".into())),
        Err(TrySendError::Disconnected(_)) => {
            Err(Error::Render("the render worker stopped".into()))
        }
    }
}

fn jobs() -> &'static SyncSender<Job> {
    static JOBS: OnceLock<SyncSender<Job>> = OnceLock::new();
    JOBS.get_or_init(|| {
        let (sender, receiver) = sync_channel::<Job>(QUEUE);
        let receiver = Arc::new(Mutex::new(receiver));
        for index in 0..MAX_WORKERS {
            let receiver = receiver.clone();
            let _ = std::thread::Builder::new()
                .name(format!("visual-render-{index}"))
                .spawn(move || serve(&receiver));
        }
        sender
    })
}

fn serve(receiver: &Mutex<Receiver<Job>>) {
    loop {
        let job = match receiver.lock().unwrap_or_else(|e| e.into_inner()).recv() {
            Ok(job) => job,
            Err(_) => return,
        };
        if job.reply.is_canceled() {
            continue;
        }
        let result = match WORKER.get() {
            Some(executable) => run_child(executable, job.request),
            None => Err(Error::Render("no render worker is configured".into())),
        };
        let _ = job.reply.send(result);
    }
}

fn run_child(executable: &Path, request: Vec<u8>) -> Result<Rendered, Error> {
    let mut child = Command::new(executable)
        .arg(WORKER_ARG)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::Render(format!("the render worker did not start: {e}")))?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&request);
    });
    let mut stdout = child.stdout.take().expect("piped stdout");
    let reader = std::thread::spawn(move || {
        let mut output = Vec::new();
        stdout.read_to_end(&mut output).map(|_| output)
    });
    let stderr = child.stderr.take().expect("piped stderr");
    let diagnostics = std::thread::spawn(move || read_bounded(stderr));
    let started = Instant::now();
    let limit = loop {
        match child.try_wait() {
            Ok(Some(_)) => break None,
            Ok(None) => {}
            Err(e) => break Some(Error::Render(e.to_string())),
        }
        if started.elapsed() > TIME_LIMIT {
            break Some(Error::Limit(format!(
                "rendering took longer than {} seconds",
                TIME_LIMIT.as_secs()
            )));
        }
        if resident_bytes(child.id()).is_some_and(|bytes| bytes > MEMORY_LIMIT) {
            break Some(memory_limit());
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    if let Some(error) = limit {
        let _ = child.kill();
        let _ = child.wait();
        let _ = writer.join();
        let _ = reader.join();
        let _ = diagnostics.join();
        return Err(error);
    }
    let _ = writer.join();
    let output = reader
        .join()
        .map_err(|_| Error::Render("reading the render failed".into()))?
        .map_err(|e| Error::Render(e.to_string()))?;
    let diagnostics = diagnostics.join().unwrap_or_default();
    decode(&output).map_err(|error| match error {
        Error::Render(_) if ran_out_of_memory(&diagnostics) => memory_limit(),
        error => error,
    })
}

fn memory_limit() -> Error {
    Error::Limit(format!(
        "rendering used more than {} MiB",
        MEMORY_LIMIT / 1024 / 1024
    ))
}

/// The worker's address-space cap makes a large allocation fail before its
/// resident size crosses [`MEMORY_LIMIT`]; std then aborts with this message.
fn ran_out_of_memory(diagnostics: &[u8]) -> bool {
    diagnostics
        .windows(OUT_OF_MEMORY.len())
        .any(|window| window == OUT_OF_MEMORY)
}

const OUT_OF_MEMORY: &[u8] = b"memory allocation of ";
const DIAGNOSTICS: u64 = 16 * 1024;

/// Keep the start of the worker's stderr and drain the rest, so a chatty
/// worker never blocks on a full pipe.
fn read_bounded(stderr: impl std::io::Read) -> Vec<u8> {
    let mut kept = Vec::new();
    let mut stderr = stderr;
    let _ = (&mut stderr).take(DIAGNOSTICS).read_to_end(&mut kept);
    let _ = std::io::copy(&mut stderr, &mut std::io::sink());
    kept
}

fn decode(output: &[u8]) -> Result<Rendered, Error> {
    let crashed = || Error::Render("the renderer crashed".into());
    let newline = output
        .iter()
        .position(|b| *b == b'\n')
        .ok_or_else(crashed)?;
    let header: Result<Header, Error> =
        serde_json::from_slice(&output[..newline]).map_err(|_| crashed())?;
    let header = header?;
    let rgba = output[newline + 1..].to_vec();
    if rgba.len() != header.width as usize * header.height as usize * 4 {
        return Err(crashed());
    }
    Ok(Rendered {
        width: header.width,
        height: header.height,
        rgba,
        content_height: header.content_height,
        clipped: header.clipped,
        blocked: header.blocked,
    })
}

/// The worker's whole life: read one request, render it, write the result,
/// and exit. The host binary calls this when started with [`WORKER_ARG`].
pub fn run_worker() -> ! {
    #[cfg(target_os = "linux")]
    unsafe {
        let limit = libc::rlimit {
            rlim_cur: ADDRESS_LIMIT,
            rlim_max: ADDRESS_LIMIT,
        };
        libc::setrlimit(libc::RLIMIT_AS, &limit);
    }
    let mut input = Vec::new();
    if std::io::stdin().read_to_end(&mut input).is_err() {
        std::process::exit(1);
    }
    let request = match serde_json::from_slice::<Request>(&input) {
        Ok(request) if request.protocol == PROTOCOL => Ok(request),
        _ => Err(Error::Render(
            "else.bot was updated; restart it to show visuals".into(),
        )),
    };
    let result = request.and_then(|request| {
        std::thread::Builder::new()
            .stack_size(WORKER_STACK)
            .spawn(move || {
                crate::render(&request.html, request.width, request.scale, &request.theme)
            })
            .ok()
            .and_then(|thread| thread.join().ok())
            .unwrap_or_else(|| Err(Error::Render("the renderer crashed".into())))
    });
    let mut stdout = std::io::stdout().lock();
    let written = match result {
        Ok(rendered) => serde_json::to_writer(
            &mut stdout,
            &Ok::<Header, Error>(Header {
                width: rendered.width,
                height: rendered.height,
                content_height: rendered.content_height,
                clipped: rendered.clipped,
                blocked: rendered.blocked,
            }),
        )
        .ok()
        .and_then(|()| stdout.write_all(b"\n").ok())
        .and_then(|()| stdout.write_all(&rendered.rgba).ok()),
        Err(error) => serde_json::to_writer(&mut stdout, &Err::<Header, Error>(error))
            .ok()
            .and_then(|()| stdout.write_all(b"\n").ok()),
    };
    let flushed = written.and_then(|()| stdout.flush().ok());
    std::process::exit(if flushed.is_some() { 0 } else { 1 })
}

#[cfg(target_os = "macos")]
fn resident_bytes(pid: u32) -> Option<u64> {
    let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
    let read = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTASKINFO,
            0,
            (&mut info as *mut libc::proc_taskinfo).cast(),
            size,
        )
    };
    (read == size).then_some(info.pti_resident_size)
}

#[cfg(target_os = "linux")]
fn resident_bytes(pid: u32) -> Option<u64> {
    let statm = std::fs::read_to_string(format!("/proc/{pid}/statm")).ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    Some(pages * u64::try_from(page).ok()?)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn resident_bytes(_: u32) -> Option<u64> {
    None
}
