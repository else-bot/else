use std::io::Read;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub fn candidate(text: Option<&str>) -> bool {
    text.is_none_or(|text| {
        text.lines().all(|line| {
            let line = line.trim();
            line.starts_with("file://")
                || line.starts_with('/')
                || line.starts_with('#')
                || line.is_empty()
        })
    })
}

pub fn file_paths() -> Option<Result<Vec<PathBuf>, String>> {
    use wl_clipboard_rs::paste::{ClipboardType, MimeType, Seat, get_contents};
    let result = get_contents(
        ClipboardType::Regular,
        Seat::Unspecified,
        MimeType::Specific("text/uri-list"),
    )
    .or_else(|_| {
        get_contents(
            ClipboardType::Regular,
            Seat::Unspecified,
            MimeType::Specific("x-special/gnome-copied-files"),
        )
    });
    let Ok((mut pipe, mime)) = result else {
        return None;
    };
    Some(
        read_list(&mut pipe)
            .and_then(|text| parse_paths(&text, mime == "x-special/gnome-copied-files")),
    )
}

fn read_list(pipe: &mut (impl Read + AsRawFd)) -> Result<String, String> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut bytes = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let mut descriptor = libc::pollfd {
            fd: pipe.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut descriptor, 1, remaining.as_millis() as i32) };
        if ready == 0 {
            return Err("Reading the copied file list timed out. Paste again.".into());
        }
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error.to_string());
        }
        let mut buffer = [0; 4096];
        let count = pipe.read(&mut buffer).map_err(|e| e.to_string())?;
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..count]);
        if bytes.len() > 64 * 1024 {
            return Err("The copied file list is too large.".into());
        }
    }
    String::from_utf8(bytes).map_err(|e| e.to_string())
}

fn parse_paths(text: &str, gnome: bool) -> Result<Vec<PathBuf>, String> {
    let mut lines = text.lines();
    if gnome && !matches!(lines.next(), Some("copy" | "cut")) {
        return Err("The copied file list is invalid.".into());
    }
    let paths = lines
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            url::Url::parse(line)
                .ok()
                .and_then(|url| url.to_file_path().ok())
                .ok_or_else(|| "Only local files can be pasted.".to_owned())
        })
        .collect::<Result<Vec<_>, _>>()?;
    if paths.is_empty() {
        return Err("No files were copied.".into());
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::{candidate, parse_paths};
    use std::path::PathBuf;

    #[test]
    fn native_file_lists_preserve_order_and_decode_paths() {
        assert_eq!(
            parse_paths(
                "# copied files\r\nfile:///tmp/%C3%A9cran%20one.png\r\nfile:///tmp/logs.txt\r\n",
                false
            )
            .unwrap(),
            vec![
                PathBuf::from("/tmp/écran one.png"),
                PathBuf::from("/tmp/logs.txt")
            ]
        );
        assert_eq!(
            parse_paths("copy\nfile:///tmp/logs.txt\n", true).unwrap(),
            vec![PathBuf::from("/tmp/logs.txt")]
        );
        assert!(parse_paths("https://example.com/file.txt", false).is_err());
        assert!(parse_paths("file://other-machine/tmp/file.txt", false).is_err());
        assert!(parse_paths("file:///tmp/one\ninvalid", false).is_err());
    }

    #[test]
    fn ordinary_prose_bypasses_native_file_list_reads() {
        assert!(!candidate(Some("please open /tmp/logs.txt")));
        assert!(candidate(Some("file:///tmp/logs.txt")));
        assert!(candidate(None));
    }
}
