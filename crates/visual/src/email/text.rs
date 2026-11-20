//! An email's HTML part as plain text with its links written out, for a
//! reader that cannot click. A single forward scan that keeps no tree and
//! only the attributes it reads, so the cost stays linear in the HTML
//! however a sender nests tags or piles attributes on one.

use std::borrow::Cow;

use super::{MAX_LINK_TEXT, MAX_LINKS, is_link};

/// Longest URL written out; past it a link reads as its label alone.
/// Servers commonly refuse longer request lines, nginx's default among them.
const MAX_TEXT_LINK_BYTES: usize = 8192;

/// A link an email names, for a reader that sees its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextLink {
    /// What the link shows, or its images' `alt` when it shows no text.
    pub text: String,
    pub url: String,
}

/// What an email's HTML part shows, as plain text.
#[derive(Debug, Default)]
pub struct Text {
    /// Each `http`, `https` or `mailto` link's URL follows what it shows as
    /// ` <url>`, so a label naming somewhere else cannot hide where it goes.
    pub text: String,
    /// Those links in document order, one per URL, at most [`MAX_LINKS`].
    pub links: Vec<TextLink>,
    /// Words the email shows, not counting the URLs written out.
    pub words: usize,
}

/// The HTML parts of one message, in order, each read on its own so one
/// left unfinished cannot swallow the next.
pub fn as_text<'a>(parts: impl IntoIterator<Item = &'a str>) -> Text {
    let mut sink = Sink::default();
    for html in parts {
        scan(html, &mut sink);
        sink.close_link();
        sink.block();
    }
    while sink.text.text.ends_with('\n') {
        sink.text.text.pop();
    }
    sink.text
}

fn scan(html: &str, sink: &mut Sink) {
    let mut rest = html;
    while let Some(open) = rest.find('<') {
        sink.characters(&decode(&rest[..open], Strip::Invisible));
        rest = &rest[open..];
        if let Some(comment) = rest.strip_prefix("<!--") {
            rest = comment.find("-->").map_or("", |end| &comment[end + 3..]);
        } else if rest[1..].starts_with(['!', '?']) {
            rest = rest.find('>').map_or("", |end| &rest[end + 1..]);
        } else if let Some((tag, after)) = Tag::parse(rest) {
            rest = after;
            let Some(tag) = tag else { continue };
            if !tag.end && !tag.self_closing && matches!(&*tag.name, "script" | "style" | "title") {
                rest = past_end_tag(rest, &tag.name);
            } else {
                sink.tag(tag);
            }
        } else {
            sink.characters("<");
            rest = &rest[1..];
        }
    }
    sink.characters(&decode(rest, Strip::Invisible));
}

/// A tag with only what the text needs from it.
struct Tag {
    end: bool,
    self_closing: bool,
    /// Lowercased.
    name: String,
    href: Option<String>,
    alt: Option<String>,
}

impl Tag {
    /// The tag `html` starts with and what follows it, or `None` when the
    /// `<` opens no tag and is text. A tag cut off by the end of the HTML
    /// takes the rest and counts for nothing, as in a browser.
    fn parse(html: &str) -> Option<(Option<Tag>, &str)> {
        let bytes = html.as_bytes();
        let stops = |b: u8| matches!(b, b'>' | b'/' | b'=') || b.is_ascii_whitespace();
        let end = bytes.get(1) == Some(&b'/');
        let mut at = 1 + usize::from(end);
        if !bytes.get(at).is_some_and(u8::is_ascii_alphabetic) {
            return None;
        }
        let start = at;
        while at < bytes.len() && !stops(bytes[at]) {
            at += 1;
        }
        let mut tag = Tag {
            end,
            self_closing: false,
            name: html[start..at].to_ascii_lowercase(),
            href: None,
            alt: None,
        };
        loop {
            while at < bytes.len() && (bytes[at].is_ascii_whitespace() || bytes[at] == b'/') {
                tag.self_closing = bytes[at] == b'/';
                at += 1;
            }
            match bytes.get(at) {
                None => return Some((None, "")),
                Some(b'>') => return Some((Some(tag), &html[at + 1..])),
                Some(_) => {}
            }
            let name_start = at;
            while at < bytes.len() && !stops(bytes[at]) {
                at += 1;
            }
            let name = &html[name_start..at];
            while at < bytes.len() && bytes[at].is_ascii_whitespace() {
                at += 1;
            }
            let mut value = "";
            if bytes.get(at) == Some(&b'=') {
                at += 1;
                while at < bytes.len() && bytes[at].is_ascii_whitespace() {
                    at += 1;
                }
                if let Some(&quote @ (b'"' | b'\'')) = bytes.get(at) {
                    let close = html[at + 1..]
                        .find(char::from(quote))
                        .map_or(bytes.len(), |i| at + 1 + i);
                    value = &html[at + 1..close];
                    at = (close + 1).min(bytes.len());
                } else {
                    let start = at;
                    while at < bytes.len() && bytes[at] != b'>' && !bytes[at].is_ascii_whitespace()
                    {
                        at += 1;
                    }
                    value = &html[start..at];
                }
            }
            let slot = if name.eq_ignore_ascii_case("href") {
                &mut tag.href
            } else if name.eq_ignore_ascii_case("alt") {
                &mut tag.alt
            } else {
                continue;
            };
            if slot.is_none() {
                *slot = Some(decode(value, Strip::Nothing).into_owned());
            }
        }
    }
}

/// What follows the end tag of the `name` element whose content `html`
/// starts in, or nothing when it never ends.
fn past_end_tag<'a>(html: &'a str, name: &str) -> &'a str {
    let mut from = 0;
    while let Some(at) = html[from..].find("</").map(|i| from + i) {
        let after = &html[at + 2..];
        if after
            .get(..name.len())
            .is_some_and(|candidate| candidate.eq_ignore_ascii_case(name))
            && after[name.len()..]
                .starts_with(|c: char| c.is_ascii_whitespace() || c == '/' || c == '>')
        {
            return after.find('>').map_or("", |end| &after[end + 1..]);
        }
        from = at + 2;
    }
    ""
}

/// What [`decode`] leaves out besides the references it decodes.
#[derive(Clone, Copy, PartialEq)]
enum Strip {
    /// The invisible characters senders pad previews with, from text.
    Invisible,
    /// Nothing, for a URL, where every character is part of the address.
    Nothing,
}

/// `text` with its character references decoded as an HTML parser would
/// when they end in `;`. An unknown reference stays as written.
fn decode(text: &str, strip: Strip) -> Cow<'_, str> {
    let keep = |c: &char| strip == Strip::Nothing || !is_invisible(*c);
    if !text.contains('&') && text.chars().all(|c| keep(&c)) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.extend(rest[..amp].chars().filter(keep));
        rest = &rest[amp..];
        let decoded = rest
            .bytes()
            .skip(1)
            .take(MAX_REFERENCE_BYTES)
            .position(|b| b == b';')
            .and_then(|end| reference(&rest[1..2 + end]).map(|chars| (chars, end + 2)));
        match decoded {
            Some((chars, len)) => {
                out.extend(chars.into_iter().flatten().filter(keep));
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.extend(rest.chars().filter(keep));
    Cow::Owned(out)
}

/// Longest reference name with its `;`, `&CounterClockwiseContourIntegral;`.
const MAX_REFERENCE_BYTES: usize = 33;

/// The characters a reference such as `amp;` or `#x41;` stands for.
fn reference(name: &str) -> Option<[Option<char>; 2]> {
    if let Some(number) = name.strip_prefix('#') {
        let number = number.strip_suffix(';')?;
        let code = match number.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => number.parse().ok()?,
        };
        return Some([char::from_u32(code), None]);
    }
    match html5ever::data::NAMED_ENTITIES.get(name)? {
        (0, _) => None,
        (first, second) => Some([
            char::from_u32(*first),
            char::from_u32(*second).filter(|c| *c != '\0'),
        ]),
    }
}

fn is_invisible(c: char) -> bool {
    matches!(
        c,
        '\u{ad}' | '\u{34f}' | '\u{200b}'..='\u{200d}' | '\u{2060}' | '\u{feff}'
    )
}

#[derive(Default)]
struct Sink {
    text: Text,
    /// A space separates the next word from the last.
    space: bool,
    /// A URL or a link's start comes between the last word and the next,
    /// so a next word starting with a letter or digit is set apart.
    boundary: bool,
    link: Option<OpenLink>,
}

struct OpenLink {
    url: String,
    shown: String,
    alt: Vec<String>,
}

impl Sink {
    fn word(&mut self, word: &str, counted: bool) {
        let out = &mut self.text.text;
        let space = self.space
            || (self.boundary
                && word.starts_with(char::is_alphanumeric)
                && out.ends_with(|c: char| c.is_alphanumeric() || c == '>'));
        let starts = out.is_empty() || out.ends_with('\n') || space;
        if space && !out.is_empty() && !out.ends_with('\n') {
            out.push(' ');
        }
        out.push_str(word);
        self.space = false;
        self.boundary = false;
        if counted && starts {
            self.text.words += 1;
        }
    }

    fn url(&mut self, url: &str) {
        self.word(&format!("<{url}>"), false);
        self.boundary = true;
    }

    fn characters(&mut self, chars: &str) {
        if chars.is_empty() {
            return;
        }
        if let Some(link) = &mut self.link {
            link.shown.push_str(chars);
        }
        for (i, word) in chars.split(char::is_whitespace).enumerate() {
            if i > 0 {
                self.space = true;
            }
            if !word.is_empty() {
                self.word(word, !word.contains("://"));
            }
        }
    }

    /// Separates what follows, in the text and in an open link's label.
    fn gap(&mut self) {
        self.space = true;
        if let Some(link) = &mut self.link {
            link.shown.push(' ');
        }
    }

    fn line_break(&mut self) {
        self.gap();
        let out = &mut self.text.text;
        if !out.is_empty() && !out.ends_with("\n\n") {
            out.push('\n');
        }
        self.space = false;
        self.boundary = false;
    }

    fn block(&mut self) {
        if self.text.text.ends_with('\n') {
            return;
        }
        self.line_break();
    }

    fn close_link(&mut self) {
        let Some(link) = self.link.take() else {
            return;
        };
        let url = link.url;
        let shown = link.shown.split_whitespace().collect::<Vec<_>>().join(" ");
        let alt = link.alt.join(" ");
        if !shown.is_empty() {
            if shown == url {
                self.boundary = true;
            } else {
                self.space = true;
                self.url(&url);
            }
        } else if !alt.is_empty() && alt != url {
            self.word(&alt, false);
            self.space = true;
            self.url(&url);
        } else {
            self.url(&url);
        }
        let label = if shown.is_empty() { alt } else { shown };
        let links = &mut self.text.links;
        if links.len() < MAX_LINKS && links.iter().all(|link| link.url != url) {
            links.push(TextLink {
                text: label.chars().take(MAX_LINK_TEXT).collect(),
                url,
            });
        }
    }

    fn tag(&mut self, tag: Tag) {
        match (tag.end, &*tag.name) {
            (false, "a") => {
                self.close_link();
                self.link = tag
                    .href
                    .as_deref()
                    .map(str::trim)
                    .filter(|href| href.len() <= MAX_TEXT_LINK_BYTES && is_link(href))
                    .map(|href| OpenLink {
                        url: spelled_url(href),
                        shown: String::new(),
                        alt: Vec::new(),
                    });
                self.boundary |= self.link.is_some();
            }
            (true, "a") => self.close_link(),
            (false, "img") => {
                if let Some(link) = &mut self.link
                    && let Some(alt) = tag.alt.filter(|alt| !alt.trim().is_empty())
                {
                    link.alt
                        .push(alt.split_whitespace().collect::<Vec<_>>().join(" "));
                }
            }
            (false, "br") => self.line_break(),
            (_, "td" | "th") => self.gap(),
            (
                _,
                "p" | "div" | "tr" | "li" | "ul" | "ol" | "table" | "blockquote" | "hr" | "h1"
                | "h2" | "h3" | "h4" | "h5" | "h6" | "pre" | "section" | "article" | "header"
                | "footer" | "center" | "dt" | "dd" | "address",
            ) => self.block(),
            _ => {}
        }
    }
}

/// `href` as one unambiguous token between `<` and `>`: the white space and
/// angle brackets a browser would percent-encode are encoded, and the tabs
/// and line breaks it would drop are dropped.
fn spelled_url(href: &str) -> String {
    let mut url = String::with_capacity(href.len());
    for c in href.chars() {
        match c {
            '\t' | '\n' | '\r' => {}
            ' ' => url.push_str("%20"),
            '<' => url.push_str("%3C"),
            '>' => url.push_str("%3E"),
            c => url.push(c),
        }
    }
    url
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(text: &str, url: &str) -> TextLink {
        TextLink {
            text: text.into(),
            url: url.into(),
        }
    }

    #[test]
    fn blocks_break_lines_and_hidden_content_stays_out() {
        let text = as_text([
            "<!DOCTYPE html><html><head><title>T</title><style>p{color:red}</style></head><body>\
             <!-- note --><script>if (a<b) x(\"</p>\")</script><p>Hello  <b>there</b>,</p>\
             <div>second\nline</div><table><tr><td>a</td><td>b</td></tr></table>\
             one<br>two &amp; AT&amp;T &#8217;&#x41;&nbsp;&zwnj;&bogus; 1 < 2</body></html>",
        ]);
        assert_eq!(
            text.text,
            "Hello there,\nsecond line\na b\none\ntwo & AT&T \u{2019}A &bogus; 1 < 2"
        );
        assert_eq!(text.words, 15);
    }

    #[test]
    fn urls_are_written_as_one_token_whatever_the_href_holds() {
        let text = as_text([
            "<a class=x HREF=\" https://x.test/a  b?q=1&gt;2&amp;r\n=3 \" href=\"https://x.test/2\">Go</a> \
             <a href=https://x.test/i><img src=\"i.png\" alt=\"https://x.test/i\"></a>Confirmed \
             <a href='https://x.test/j'><img src=\"j.png\"></a>.",
        ]);
        assert_eq!(
            text.text,
            "Go <https://x.test/a%20%20b?q=1%3E2&r=3> <https://x.test/i> Confirmed <https://x.test/j>."
        );
        assert_eq!(
            text.links,
            vec![
                link("Go", "https://x.test/a%20%20b?q=1%3E2&r=3"),
                link("https://x.test/i", "https://x.test/i"),
                link("", "https://x.test/j"),
            ]
        );
        assert_eq!(text.words, 2);
    }

    #[test]
    fn a_link_label_reads_like_its_text_and_an_image_link_reads_as_its_alt() {
        let text = as_text([
            "<a href=\"https://x.test/b\"><div>View</div><div>booking</div></a>\
             <a href=\"https://x.test/h\"><img alt=\" Hotel  logo \"></a>",
        ]);
        assert_eq!(
            text.text,
            "View\nbooking\n<https://x.test/b> Hotel logo <https://x.test/h>"
        );
        assert_eq!(
            text.links,
            vec![
                link("View booking", "https://x.test/b"),
                link("Hotel logo", "https://x.test/h"),
            ]
        );
    }

    #[test]
    fn an_svg_title_without_content_hides_nothing() {
        let text = as_text(["<svg><title/></svg><a href=\"https://x.test/r\">Reply</a>\
             <template><a href=\"https://x.test/t\">Later</a></template>"]);
        assert_eq!(
            text.text,
            "Reply <https://x.test/r> Later <https://x.test/t>"
        );
    }

    #[test]
    fn a_long_signed_url_is_still_written_out() {
        let url = format!("https://x.test/t?sig={}", "a".repeat(3000));
        let text = as_text([format!("<a href=\"{url}\">Details</a>").as_str()]);
        assert_eq!(text.text, format!("Details <{url}>"));
        assert_eq!(text.links, vec![link("Details", &url)]);
    }

    #[test]
    fn deep_nesting_and_piled_attributes_still_read_their_links() {
        let attrs: String = (0..200_000).map(|i| format!(" a{i}=1")).collect();
        let html = format!(
            "{}<p{attrs}>x</p>{}<a href=\"https://x.test/deep\">deep</a>",
            "<div>".repeat(200_000),
            "&".repeat(200_000)
        );
        assert_eq!(
            as_text([html.as_str()]).links,
            vec![link("deep", "https://x.test/deep")]
        );
    }

    #[test]
    fn references_decode_as_an_html_parser_reads_them() {
        let text = as_text([
            "Cr&egrave;me &NotEqualTilde; <a href=\"https://x.test/s?q=a&plus;b&zwj;c&copy\">Go</a>",
        ]);
        assert_eq!(
            text.text,
            "Cr\u{e8}me \u{2242}\u{338} Go <https://x.test/s?q=a+b\u{200d}c&copy>"
        );
    }

    #[test]
    fn script_content_ends_only_at_its_own_end_tag() {
        let text = as_text([
            "<script>x = \"</script-data>\"; y = '<a href=\"https://x.test/in\">In</a>';</SCRIPT >\
             <a href=\"https://x.test/out\">Out</a>",
        ]);
        assert_eq!(text.links, vec![link("Out", "https://x.test/out")]);
    }

    #[test]
    fn a_link_showing_its_own_url_is_set_apart_and_not_counted() {
        let text = as_text(["Go<a href=\"https://x.test/r\">https://x.test/r</a>now"]);
        assert_eq!(text.text, "Go https://x.test/r now");
        assert_eq!(text.words, 2);
    }

    #[test]
    fn an_unfinished_part_does_not_swallow_the_next() {
        let text = as_text([
            "Intro<!--",
            "<a href=\"https://x.test/reply\">Reply</a> <a href=\"https://x.test/cut",
        ]);
        assert_eq!(text.text, "Intro\nReply <https://x.test/reply>");
        assert_eq!(text.links, vec![link("Reply", "https://x.test/reply")]);
    }
}
