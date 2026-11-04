//! Where a Markdown link in a reply leads, and how the desktop opens it.

use std::path::{Path, PathBuf};
use url::Url;

/// A link the desktop can open: a web page in the browser, or a file on
/// this machine in its default application.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum LinkTarget {
    Web(Url),
    File(PathBuf),
}

impl LinkTarget {
    /// Reads a link destination. Relative paths are refused: a reply has no
    /// directory they could be relative to on this machine.
    pub(super) fn parse(destination: &str) -> Option<Self> {
        if Path::new(destination).is_absolute() {
            return Some(Self::File(PathBuf::from(strip_line_anchor(destination))));
        }
        let url = Url::parse(destination).ok()?;
        match url.scheme() {
            "http" | "https" => Some(Self::Web(url)),
            "file" => url.to_file_path().ok().map(Self::File),
            _ => None,
        }
    }

    /// The full destination, as the link's tooltip shows it.
    pub(super) fn destination(&self) -> String {
        match self {
            Self::Web(url) => url.to_string(),
            Self::File(path) => path.display().to_string(),
        }
    }

    pub(super) fn open(&self, cx: &gpui_kit::App) {
        match self {
            Self::Web(url) => cx.open_url(url.as_str()),
            Self::File(path) => cx.open_with_system(path),
        }
    }
}

fn strip_line_anchor(path: &str) -> &str {
    match path.rsplit_once('#') {
        Some((file, anchor)) if is_line_anchor(anchor) => file,
        _ => path,
    }
}

fn is_line_anchor(anchor: &str) -> bool {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let position = |s: &str| match s.split_once('C') {
        Some((line, column)) => digits(line) && digits(column),
        None => digits(s),
    };
    let Some(range) = anchor.strip_prefix('L') else {
        return false;
    };
    match range.split_once('-') {
        Some((start, end)) => position(start) && position(end.strip_prefix('L').unwrap_or(end)),
        None => position(range),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str) -> Option<LinkTarget> {
        Some(LinkTarget::File(PathBuf::from(path)))
    }

    #[test]
    fn link_destinations_resolve_to_what_the_desktop_opens() {
        let web = |url: &str| Some(LinkTarget::Web(Url::parse(url).unwrap()));
        for (destination, expected) in [
            (
                "https://example.com/a?b=1#c",
                web("https://example.com/a?b=1#c"),
            ),
            ("http://example.com", web("http://example.com")),
            ("/home/me/report.json", file("/home/me/report.json")),
            ("/home/me/data.tar.gz", file("/home/me/data.tar.gz")),
            ("/home/me/src/main.rs#L12", file("/home/me/src/main.rs")),
            ("/home/me/src/main.rs#L12-L20", file("/home/me/src/main.rs")),
            ("/home/me/src/main.rs#L12-20", file("/home/me/src/main.rs")),
            ("/home/me/src/main.rs#L12C3", file("/home/me/src/main.rs")),
            ("/home/me/notes#1.md", file("/home/me/notes#1.md")),
            ("/home/me/issue#L", file("/home/me/issue#L")),
            ("/home/me/c#", file("/home/me/c#")),
            (
                "file:///home/me/my%20report.json",
                file("/home/me/my report.json"),
            ),
            ("file:///home/me/main.rs#L4", file("/home/me/main.rs")),
            ("file://other-host/home/me/report.json", None),
            ("report.json", None),
            ("./report.json", None),
            ("../report.json", None),
            ("javascript:alert(1)", None),
            ("data:text/html,hi", None),
            ("mailto:me@example.com", None),
            ("ftp://example.com/file", None),
            ("", None),
        ] {
            assert_eq!(LinkTarget::parse(destination), expected, "{destination:?}");
        }
    }

    #[test]
    fn the_tooltip_shows_the_full_destination() {
        assert_eq!(
            LinkTarget::parse("file:///home/me/my%20report.json")
                .unwrap()
                .destination(),
            "/home/me/my report.json"
        );
        assert_eq!(
            LinkTarget::parse("https://example.com/a b")
                .unwrap()
                .destination(),
            "https://example.com/a%20b"
        );
    }
}
