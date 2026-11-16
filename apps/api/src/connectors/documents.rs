//! Text from email attachments the owner approved, kept by page so a bot
//! can search a long report and quote where a passage is.

pub mod pdf;

use serde_json::{Value, json};

/// How an attachment's bytes become text. Other types cannot be opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Pdf,
    Html,
    Text,
}

impl Format {
    /// The format of a part with this media type and file name. A generic
    /// binary type falls back to the file extension.
    pub fn of(media_type: &str, name: Option<&str>) -> Option<Self> {
        match media_type {
            "application/pdf" => return Some(Self::Pdf),
            "text/html" | "application/xhtml+xml" => return Some(Self::Html),
            "application/json" | "application/xml" | "application/csv" => return Some(Self::Text),
            text if text.starts_with("text/") => return Some(Self::Text),
            "application/octet-stream" | "binary/octet-stream" => {}
            _ => return None,
        }
        let extension = name?.rsplit_once('.')?.1.to_ascii_lowercase();
        match extension.as_str() {
            "pdf" => Some(Self::Pdf),
            "html" | "htm" => Some(Self::Html),
            "txt" | "csv" | "md" | "json" | "xml" | "log" => Some(Self::Text),
            _ => None,
        }
    }
}

/// Bytes of extracted text kept per file.
pub const MAX_TEXT: usize = 16 * 1024 * 1024;
/// Characters of one page `page` returns.
const PAGE_CHARS: usize = 20_000;
/// Characters of one search match, around the matched words.
const MATCH_CHARS: usize = 600;
pub const MAX_MATCHES: usize = 50;
pub const DEFAULT_MATCHES: usize = 20;

/// Why an approved file has no text to search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unreadable {
    /// The bytes are not a readable document of their format: damaged,
    /// encrypted, or mislabeled.
    Damaged,
    /// It parsed but holds no text, like a scanned PDF of images.
    NoText,
    /// Its text is over [`MAX_TEXT`].
    TooLarge,
    /// The PDF reader ran past its time or memory limit.
    TooComplex,
    /// The PDF reader could not start.
    ReaderFailed,
}

impl Unreadable {
    pub fn code(self) -> &'static str {
        match self {
            Self::Damaged => "attachment_unreadable",
            Self::NoText => "attachment_has_no_text",
            Self::TooLarge => "attachment_text_too_large",
            Self::TooComplex => "attachment_too_complex",
            Self::ReaderFailed => "attachment_reader_failed",
        }
    }
}

/// A file's text split by page. Text and HTML files are one page. Stored
/// with a form feed between pages, the convention `pdftotext` uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    pages: Vec<String>,
}

const PAGE_BREAK: char = '\u{c}';

impl Document {
    /// Extract the text; a PDF in a [`pdf`] worker process.
    pub async fn extract(format: Format, bytes: Vec<u8>) -> Result<Self, Unreadable> {
        let pages = match format {
            Format::Text => vec![String::from_utf8_lossy(&bytes).into_owned()],
            Format::Html => vec![mail_parser::decoders::html::html_to_text(
                &String::from_utf8_lossy(&bytes),
            )],
            Format::Pdf => pdf::pages(bytes).await?,
        };
        Self::from_pages(pages)
    }

    fn from_pages(pages: Vec<String>) -> Result<Self, Unreadable> {
        let pages: Vec<String> = pages
            .into_iter()
            .map(|page| page.replace(PAGE_BREAK, "\n"))
            .collect();
        if pages.iter().all(|page| page.trim().is_empty()) {
            return Err(Unreadable::NoText);
        }
        if pages.iter().map(String::len).sum::<usize>() + pages.len() > MAX_TEXT {
            return Err(Unreadable::TooLarge);
        }
        Ok(Self { pages })
    }

    pub fn encode(&self) -> String {
        self.pages.join(&PAGE_BREAK.to_string())
    }

    pub fn decode(text: &str) -> Self {
        Self {
            pages: text.split(PAGE_BREAK).map(str::to_owned).collect(),
        }
    }

    pub fn pages(&self) -> usize {
        self.pages.len()
    }

    pub fn chars(&self) -> usize {
        self.pages.iter().map(|page| page.chars().count()).sum()
    }

    /// One page by its 1-based number, cut at [`PAGE_CHARS`].
    pub fn page(&self, number: usize) -> Option<Value> {
        let text = self.pages.get(number.checked_sub(1)?)?;
        let shown: String = text.chars().take(PAGE_CHARS).collect();
        Some(json!({"page":number,"pages":self.pages(),"text":shown,
            "truncated":shown.len() < text.len()}))
    }

    /// Lines matching any of the `|`-separated terms, ignoring case, in page
    /// order with their 1-based page and line numbers.
    pub fn search(&self, query: &str, limit: usize) -> Value {
        let terms: Vec<String> = query
            .split('|')
            .map(|term| term.trim().to_lowercase())
            .filter(|term| !term.is_empty())
            .collect();
        let mut matches = Vec::new();
        let mut total = 0;
        for (page_ix, page) in self.pages.iter().enumerate() {
            for (line_ix, line) in page.lines().enumerate() {
                let line = line.trim();
                let lower = line.to_lowercase();
                let Some(at) = terms
                    .iter()
                    .filter_map(|term| lower.find(term.as_str()))
                    .min()
                else {
                    continue;
                };
                total += 1;
                if matches.len() < limit {
                    matches.push(json!({"page":page_ix + 1,"line":line_ix + 1,
                        "text":excerpt(line, at)}));
                }
            }
        }
        json!({"matches":matches,"total":total,"truncated":total > matches.len(),
            "pages":self.pages()})
    }
}

/// The trimmed matched line, or [`MATCH_CHARS`] of it around the match.
/// `at` is a byte offset into `line`'s lowercase form, which can be longer
/// than `line`, so it is mapped back to a character of `line`.
fn excerpt(line: &str, at: usize) -> String {
    let count = line.chars().count();
    if count <= MATCH_CHARS {
        return line.to_owned();
    }
    let mut lowered = 0;
    let center = line
        .chars()
        .take_while(|c| {
            lowered += c.to_lowercase().map(char::len_utf8).sum::<usize>();
            lowered <= at
        })
        .count();
    let start = center
        .saturating_sub(MATCH_CHARS / 3)
        .min(count - MATCH_CHARS);
    let text: String = line.chars().skip(start).take(MATCH_CHARS).collect();
    let before = if start > 0 { "…" } else { "" };
    let after = if start + MATCH_CHARS < count {
        "…"
    } else {
        ""
    };
    format!("{before}{}{after}", text.trim())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A PDF with one line of Helvetica text per page, with a correct
    /// cross-reference table.
    pub(crate) fn pdf(pages: &[&str]) -> Vec<u8> {
        let n = pages.len();
        let mut objects = vec![
            "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
            format!(
                "<< /Type /Pages /Kids [{}] /Count {n} >>",
                (0..n)
                    .map(|i| format!("{} 0 R", 4 + 2 * i))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_owned(),
        ];
        for (i, text) in pages.iter().enumerate() {
            let stream = format!("BT /F1 12 Tf 72 720 Td ({text}) Tj ET");
            objects.push(format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
                 /Resources << /Font << /F1 3 0 R >> >> /Contents {} 0 R >>",
                5 + 2 * i
            ));
            objects.push(format!(
                "<< /Length {} >>\nstream\n{stream}\nendstream",
                stream.len()
            ));
        }
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, object) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.extend(format!("{} 0 obj\n{object}\nendobj\n", i + 1).bytes());
        }
        let xref = out.len();
        out.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).bytes());
        for offset in offsets {
            out.extend(format!("{offset:010} 00000 n \n").bytes());
        }
        out.extend(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .bytes(),
        );
        out
    }

    #[test]
    fn formats_follow_the_media_type_then_the_extension() {
        assert_eq!(Format::of("application/pdf", None), Some(Format::Pdf));
        assert_eq!(Format::of("text/html", Some("a.pdf")), Some(Format::Html));
        assert_eq!(Format::of("text/csv", None), Some(Format::Text));
        assert_eq!(
            Format::of("application/octet-stream", Some("Report.PDF")),
            Some(Format::Pdf)
        );
        assert_eq!(Format::of("application/octet-stream", Some("a.zip")), None);
        assert_eq!(Format::of("image/png", Some("a.pdf")), None);
    }

    #[test]
    fn a_pdf_is_searched_by_page_and_line() {
        let pages = pdf::extract(&pdf(&[
            "Annual report 2026",
            "Net income rose to 12 million",
        ]));
        let document = Document::from_pages(pages.unwrap()).unwrap();
        let document = Document::decode(&document.encode());
        assert_eq!(document.pages(), 2);
        let found = document.search("NET INCOME | dividend", 20);
        assert_eq!(found["total"], 1);
        assert_eq!(found["matches"][0]["page"], 2);
        assert!(
            found["matches"][0]["text"]
                .as_str()
                .unwrap()
                .contains("Net income rose")
        );
        assert!(
            document.page(1).unwrap()["text"]
                .as_str()
                .unwrap()
                .contains("Annual report")
        );
        assert_eq!(document.page(3), None);
        assert_eq!(document.page(0), None);
    }

    #[tokio::test]
    async fn damaged_and_empty_files_say_why_they_have_no_text() {
        assert_eq!(pdf::extract(b"%PDF-1.4 not really"), None);
        assert_eq!(
            Document::extract(Format::Text, b"  \n ".to_vec()).await,
            Err(Unreadable::NoText)
        );
    }

    #[tokio::test]
    async fn html_becomes_text_and_matches_are_capped() {
        let html = "<html><body><p>Board meeting</p><p>board vote</p></body></html>";
        let document = Document::extract(Format::Html, html.as_bytes().to_vec())
            .await
            .unwrap();
        let found = document.search("board", 1);
        assert_eq!(found["total"], 2);
        assert_eq!(found["matches"].as_array().unwrap().len(), 1);
        assert_eq!(found["truncated"], true);
    }

    #[test]
    fn a_long_line_is_cut_around_the_match() {
        let line = format!("{}needle{}", "İ".repeat(2000), "b".repeat(2000));
        let document = Document::from_pages(vec![format!("{}{line}", " ".repeat(2000))]).unwrap();
        let shown = document.search("needle", 1)["matches"][0]["text"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(shown.contains("needle"), "{shown}");
        assert!(shown.starts_with('…') && shown.ends_with('…'));
        assert!(shown.chars().count() <= MATCH_CHARS + 2);
    }
}
