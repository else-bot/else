//! An email's HTML part, drawn on a white page like a mail client would.
//! Rendering is offline: remote images, fonts and stylesheets stay unloaded
//! and are reported in [`crate::Rendered::blocked`]. Images the message
//! carries itself, named by `cid:`, are drawn. Links come back as boxes on
//! the page for the host to make clickable; nothing follows them here.

use std::collections::HashMap;

use html5ever::tendril::TendrilSink as _;
use html5ever::{Attribute, QualName, local_name, serialize::SerializeOpts};
use markup5ever_rcdom::Handle;
use markup5ever_rcdom::{NodeData, RcDom, SerializableHandle};
use serde::{Deserialize, Serialize};
use takumi_core::paint_tree::{self, ElementInfo, PaintDocument};

use crate::{Error, Rendered};

/// Largest HTML part an email render takes.
pub const MAX_HTML_BYTES: usize = 2 * 1024 * 1024;
/// Largest total of base64 inline-image data an email render takes.
pub const MAX_INLINE_BASE64: usize = 6 * 1024 * 1024;
/// Content below this many CSS pixels is cut off.
pub const MAX_HEIGHT: u32 = 20_000;
/// Links a render locates, in document order.
const MAX_LINKS: usize = 100;
const MAX_LINK_BYTES: usize = 2048;
/// Boxes one link keeps, one per line it covers once merged.
const MAX_LINK_RECTS: usize = 64;
const MAX_LINK_TEXT: usize = 200;
/// Image types the renderer decodes.
const INLINE_TYPES: [&str; 3] = ["image/png", "image/jpeg", "image/webp"];

/// An image part the message carries and its HTML names as `cid:`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inline {
    /// The part's Content-ID, without angle brackets.
    pub content_id: String,
    pub media_type: String,
    pub data_base64: String,
}

/// A clickable `http`, `https` or `mailto` link on a rendered email.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Link {
    pub href: String,
    /// The text the link shows, empty for an image link.
    pub text: String,
    /// Where it sits, in device pixels of [`crate::Rendered`]. A link that
    /// wraps has one box per line.
    pub rects: Vec<Rect>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    fn right(&self) -> f32 {
        self.x + self.width
    }

    fn bottom(&self) -> f32 {
        self.y + self.height
    }

    fn union(self, other: Rect) -> Rect {
        let (x, y) = (self.x.min(other.x), self.y.min(other.y));
        Rect {
            x,
            y,
            width: self.right().max(other.right()) - x,
            height: self.bottom().max(other.bottom()) - y,
        }
    }

    /// Whether `other` sits on the same line and touches this box, as the
    /// runs of one link do.
    fn joins(&self, other: &Rect) -> bool {
        let overlap = self.bottom().min(other.bottom()) - self.y.max(other.y);
        overlap > 0.5 * self.height.min(other.height)
            && other.x <= self.right() + 1.0
            && self.x <= other.right() + 1.0
    }
}

/// An email's HTML part and the inline images it refers to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    pub html: String,
    pub inline: Vec<Inline>,
}

impl Source {
    pub(crate) fn check(&self) -> Result<(), Error> {
        let inline: usize = self.inline.iter().map(|i| i.data_base64.len()).sum();
        if self.html.len() > MAX_HTML_BYTES || inline > MAX_INLINE_BASE64 {
            return Err(Error::Limit(format!(
                "the email is {} KiB; the limit is {} KiB",
                (self.html.len() + inline) / 1024,
                (MAX_HTML_BYTES + MAX_INLINE_BASE64) / 1024
            )));
        }
        Ok(())
    }
}

/// The page under every email: white paper and black text, as mail clients
/// assume, whatever the app theme.
const BASE_CSS: &str = ":root { color-scheme: light; }\n\
    :root, html, body { margin: 0; padding: 0; background: #ffffff; color: #000000; \
    font-family: \"Inter\", sans-serif; font-size: 14px; line-height: 1.4; }\n\
    code, pre, kbd, samp { font-family: \"JetBrains Mono\", monospace; }\n\
    img { max-width: 100%; }\n";

/// Render `source` at `width` CSS pixels and `scale` device pixels per CSS
/// pixel, with the height its content needs up to [`MAX_HEIGHT`].
pub fn render(source: &Source, width: u32, scale: f32) -> Result<Rendered, Error> {
    source.check()?;
    let prepared = prepare(source);
    crate::draw(
        &prepared.html,
        BASE_CSS.to_owned(),
        MAX_HEIGHT,
        width,
        scale,
        Some(&prepared.anchors),
    )
}

/// Marks an element inside the `<a>` at this index of [`Prepared::anchors`].
const ANCHOR_CLASS: &str = "else-link-";

/// A distinct link destination in an email and the text it first shows.
pub(crate) struct Anchor {
    href: String,
    text: String,
}

/// The `anchors` that `document` paints, with their boxes cut at `height`
/// CSS pixels and scaled to device pixels by `scale`. Each text run and
/// box carries its element's [`ANCHOR_CLASS`] mark from [`prepare`].
pub(crate) fn links(
    anchors: &[Anchor],
    document: &PaintDocument,
    height: f32,
    scale: f32,
) -> Vec<Link> {
    let mut rects: Vec<Vec<Rect>> = vec![Vec::new(); anchors.len()];
    let mut add = |element: Option<&ElementInfo>, rect: Rect| {
        let Some(kept) = element
            .and_then(anchor)
            .and_then(|index| rects.get_mut(index))
        else {
            return;
        };
        if rect.width <= 0.0 || rect.height <= 0.0 || rect.y >= height {
            return;
        }
        let rect = Rect {
            height: rect.height.min(height - rect.y),
            ..rect
        };
        if let Some(line) = kept.iter_mut().find(|line| line.joins(&rect)) {
            *line = line.union(rect);
        } else if kept.len() < MAX_LINK_RECTS {
            kept.push(rect);
        }
    };
    for node in &document.nodes {
        match &node.kind {
            paint_tree::NodeKind::Text { runs, .. } => {
                for run in runs {
                    let rect = Rect {
                        x: run.x,
                        y: run.y - run.ascent,
                        width: run.width,
                        height: run.ascent + run.descent,
                    };
                    add(
                        run.element.as_ref().or(node.element.as_ref()),
                        transformed(node.transform, rect),
                    );
                }
            }
            paint_tree::NodeKind::Box { .. } | paint_tree::NodeKind::Image { .. } => {
                let b = node.bounds;
                let rect = Rect {
                    x: b.x,
                    y: b.y,
                    width: b.width,
                    height: b.height,
                };
                add(node.element.as_ref(), rect);
            }
        }
    }
    anchors
        .iter()
        .zip(rects)
        .filter(|(_, rects)| !rects.is_empty())
        .map(|(anchor, rects)| Link {
            href: anchor.href.clone(),
            text: anchor.text.clone(),
            rects: rects
                .into_iter()
                .map(|rect| Rect {
                    x: rect.x * scale,
                    y: rect.y * scale,
                    width: rect.width * scale,
                    height: rect.height * scale,
                })
                .collect(),
        })
        .collect()
}

/// The index of the anchor `element` sits in, from its class.
fn anchor(element: &ElementInfo) -> Option<usize> {
    element
        .class_name
        .as_deref()?
        .split_ascii_whitespace()
        .find_map(|class| class.strip_prefix(ANCHOR_CLASS)?.parse().ok())
}

/// The bounding box of `rect` under an affine `transform`, as Takumi's
/// column array `[a, b, c, d, e, f]`.
fn transformed([a, b, c, d, e, f]: [f32; 6], rect: Rect) -> Rect {
    let corners = [
        (rect.x, rect.y),
        (rect.right(), rect.y),
        (rect.x, rect.bottom()),
        (rect.right(), rect.bottom()),
    ]
    .map(|(x, y)| (a * x + c * y + e, b * x + d * y + f));
    let xs = corners.map(|(x, _)| x);
    let ys = corners.map(|(_, y)| y);
    let (left, top) = (
        xs.iter().copied().fold(f32::INFINITY, f32::min),
        ys.iter().copied().fold(f32::INFINITY, f32::min),
    );
    Rect {
        x: left,
        y: top,
        width: xs.iter().copied().fold(f32::NEG_INFINITY, f32::max) - left,
        height: ys.iter().copied().fold(f32::NEG_INFINITY, f32::max) - top,
    }
}

/// What an element takes from its ancestors' presentational attributes.
#[derive(Clone, Default)]
struct Inherited {
    /// The enclosing table's `cellpadding`, for its cells.
    cellpadding: Option<String>,
    /// Inside `<center>` or an `align="center"` block, which centers
    /// tables as well as text.
    centered: bool,
    /// Inside the `<a>` at this index of [`Prepared::anchors`].
    anchor: Option<usize>,
}

pub(crate) struct Prepared {
    pub html: String,
    /// The `http`, `https` and `mailto` links, in order, each once.
    pub anchors: Vec<Anchor>,
}

/// The HTML with its presentational attributes (`bgcolor`, `width`,
/// `align`, `cellpadding` and the like) moved into inline styles, which is
/// all the renderer reads, and `cid:` images replaced by their data. Each
/// link's elements get its [`ANCHOR_CLASS`] mark, so the paint tree tells
/// which link a box belongs to.
pub(crate) fn prepare(source: &Source) -> Prepared {
    let inline: HashMap<String, String> = source
        .inline
        .iter()
        .filter(|i| INLINE_TYPES.contains(&i.media_type.to_ascii_lowercase().as_str()))
        .map(|i| {
            (
                i.content_id.trim_matches(['<', '>']).to_ascii_lowercase(),
                format!("data:{};base64,{}", i.media_type, i.data_base64),
            )
        })
        .collect();
    let dom = html5ever::parse_document(RcDom::default(), Default::default()).one(&*source.html);
    let mut anchors: Vec<Anchor> = Vec::new();
    let mut stack = vec![(dom.document.clone(), Inherited::default())];
    while let Some((node, inherited)) = stack.pop() {
        let mut passed = inherited.clone();
        if let NodeData::Element { name, attrs, .. } = &node.data {
            let tag = &*name.local;
            let mut attrs = attrs.borrow_mut();
            let mut hints = hints(tag, &attrs);
            match tag {
                "table" => {
                    passed.cellpadding = attribute(&attrs, "cellpadding").and_then(length);
                    if inherited.centered && attribute(&attrs, "align").is_none() {
                        hints.push("margin-left:auto;margin-right:auto".into());
                    }
                }
                "td" | "th" => {
                    if let Some(padding) = &inherited.cellpadding {
                        hints.insert(0, format!("padding:{padding}"));
                    }
                }
                _ => {}
            }
            if tag == "center" {
                passed.centered = true;
            } else if matches!(tag, "td" | "th" | "tr" | "div" | "p")
                && let Some(align) = attribute(&attrs, "align")
            {
                passed.centered = align.trim().eq_ignore_ascii_case("center");
            }
            if tag == "a"
                && let Some(href) = attribute(&attrs, "href").map(str::trim)
                && href.len() <= MAX_LINK_BYTES
                && is_link(href)
            {
                let text = text_of(&node);
                passed.anchor = match anchors.iter().position(|a| a.href == href) {
                    Some(index) => {
                        if anchors[index].text.is_empty() {
                            anchors[index].text = text;
                        }
                        Some(index)
                    }
                    None if anchors.len() < MAX_LINKS => {
                        anchors.push(Anchor {
                            href: href.to_owned(),
                            text,
                        });
                        Some(anchors.len() - 1)
                    }
                    None => None,
                };
            }
            if let Some(index) = passed.anchor {
                let mark = format!("{ANCHOR_CLASS}{index}");
                match attrs.iter_mut().find(|a| &*a.name.local == "class") {
                    Some(class) => class.value = format!("{} {mark}", class.value).into(),
                    None => attrs.push(new_attribute(local_name!("class"), mark)),
                }
            }
            if tag == "img" {
                for attr in attrs.iter_mut() {
                    if &*attr.name.local == "src"
                        && let Some(id) = cid(&attr.value)
                        && let Some(data) = inline.get(&id)
                    {
                        attr.value = data.as_str().into();
                    }
                }
            }
            if !hints.is_empty() {
                let mut style = hints.join(";");
                if let Some(own) = attrs.iter_mut().find(|a| &*a.name.local == "style") {
                    style.push(';');
                    style.push_str(&own.value);
                    own.value = style.into();
                } else {
                    attrs.push(new_attribute(local_name!("style"), style));
                }
            }
        }
        let children = node.children.borrow();
        stack.extend(
            children
                .iter()
                .rev()
                .map(|child| (child.clone(), passed.clone())),
        );
    }
    let mut html = Vec::new();
    let document: SerializableHandle = dom.document.clone().into();
    let options = SerializeOpts {
        traversal_scope: html5ever::serialize::TraversalScope::ChildrenOnly(None),
        ..Default::default()
    };
    let html = match html5ever::serialize(&mut html, &document, options) {
        Ok(()) => String::from_utf8(html).unwrap_or_default(),
        Err(_) => String::new(),
    };
    Prepared { html, anchors }
}

fn new_attribute(name: html5ever::LocalName, value: String) -> Attribute {
    Attribute {
        name: QualName::new(None, html5ever::Namespace::from(""), name),
        value: value.into(),
    }
}

/// The text `node` shows, with images standing in by their `alt`, its
/// white space collapsed and cut at [`MAX_LINK_TEXT`] characters.
fn text_of(node: &Handle) -> String {
    let mut words = Vec::new();
    let mut stack = vec![node.clone()];
    while let Some(node) = stack.pop() {
        match &node.data {
            NodeData::Text { contents } => {
                words.extend(contents.borrow().split_whitespace().map(str::to_owned));
            }
            NodeData::Element { name, attrs, .. } if &*name.local == "img" => {
                if let Some(alt) = attribute(&attrs.borrow(), "alt") {
                    words.extend(alt.split_whitespace().map(str::to_owned));
                }
            }
            _ => {}
        }
        stack.extend(node.children.borrow().iter().rev().cloned());
    }
    words.join(" ").chars().take(MAX_LINK_TEXT).collect()
}

fn attribute<'a>(attrs: &'a [Attribute], name: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|a| &*a.name.local == name)
        .map(|a| &*a.value)
}

/// The CSS a browser derives from `tag`'s presentational attributes.
fn hints(tag: &str, attrs: &[Attribute]) -> Vec<String> {
    let get = |name| attribute(attrs, name);
    let mut hints = Vec::new();
    if let Some(color) = get("bgcolor").and_then(color) {
        hints.push(format!("background-color:{color}"));
    }
    if tag == "body"
        && let Some(color) = get("text").and_then(color)
    {
        hints.push(format!("color:{color}"));
    }
    if matches!(tag, "table" | "td" | "th" | "col" | "hr" | "div") {
        if let Some(width) = get("width").and_then(length) {
            hints.push(format!("width:{width}"));
        }
        if tag != "hr"
            && let Some(height) = get("height").and_then(length)
        {
            hints.push(format!("height:{height}"));
        }
    }
    match (tag, get("align").map(str::to_ascii_lowercase).as_deref()) {
        ("table", Some("center")) => hints.push("margin-left:auto;margin-right:auto".into()),
        ("table" | "img", Some("left")) => hints.push("float:left".into()),
        ("table" | "img", Some("right")) => hints.push("float:right".into()),
        (
            "td" | "th" | "tr" | "p" | "div" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6",
            Some(align @ ("left" | "center" | "right" | "justify")),
        ) => hints.push(format!("text-align:{align}")),
        _ => {}
    }
    if let Some(align @ ("top" | "middle" | "bottom" | "baseline")) =
        get("valign").map(str::to_ascii_lowercase).as_deref()
    {
        hints.push(format!("vertical-align:{align}"));
    }
    if tag == "table" {
        if let Some(spacing) = get("cellspacing").and_then(length) {
            hints.push(format!("border-spacing:{spacing}"));
        }
        if let Some(border) = get("border").and_then(length)
            && border != "0px"
        {
            hints.push(format!("border:{border} solid #808080"));
        }
    }
    if tag == "font" {
        if let Some(color) = get("color").and_then(color) {
            hints.push(format!("color:{color}"));
        }
        let size = match get("size").map(str::trim) {
            Some("1") => Some("10px"),
            Some("2") => Some("13px"),
            Some("3") => Some("16px"),
            Some("4") => Some("18px"),
            Some("5") => Some("24px"),
            Some("6") => Some("32px"),
            Some("7") => Some("48px"),
            _ => None,
        };
        if let Some(size) = size {
            hints.push(format!("font-size:{size}"));
        }
    }
    hints
}

/// A legacy color attribute as CSS: `#rgb`/`#rrggbb` (with or without the
/// `#`) or a color name. Anything else is dropped, so no attribute text
/// reaches the CSS unchecked.
fn color(value: &str) -> Option<String> {
    let value = value.trim();
    let hex = value.strip_prefix('#').unwrap_or(value);
    if matches!(hex.len(), 3 | 6) && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Some(format!("#{hex}"));
    }
    (!value.is_empty() && value.len() <= 20 && value.bytes().all(|b| b.is_ascii_alphabetic()))
        .then(|| value.to_ascii_lowercase())
}

/// A legacy length attribute (`600`, `600px` or `100%`) as CSS.
fn length(value: &str) -> Option<String> {
    let value = value.trim();
    let (number, unit) = match value.strip_suffix('%') {
        Some(number) => (number, "%"),
        None => (value.strip_suffix("px").unwrap_or(value), "px"),
    };
    let valid = !number.is_empty()
        && number.len() <= 6
        && number.bytes().all(|b| b.is_ascii_digit() || b == b'.')
        && number.bytes().filter(|b| *b == b'.').count() <= 1;
    valid.then(|| format!("{number}{unit}"))
}

/// The lowercased Content-ID a `cid:` URL names.
fn cid(src: &str) -> Option<String> {
    let src = src.trim();
    let scheme = src.get(..4)?;
    scheme
        .eq_ignore_ascii_case("cid:")
        .then(|| src[4..].trim_matches(['<', '>']).to_ascii_lowercase())
}

fn is_link(href: &str) -> bool {
    ["https://", "http://", "mailto:"].iter().any(|scheme| {
        href.get(..scheme.len())
            .is_some_and(|start| start.eq_ignore_ascii_case(scheme))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prepared(html: &str) -> String {
        prepare(&Source {
            html: html.into(),
            inline: vec![Inline {
                content_id: "<Logo@x>".into(),
                media_type: "image/png".into(),
                data_base64: "AAAA".into(),
            }],
        })
        .html
    }

    #[test]
    fn presentational_attributes_become_inline_styles_before_the_own_style() {
        let html = prepared(
            r#"<table width="600" bgcolor="ffffff" align="center" cellpadding="8"><tr><td style="color:red" valign="top">x</td></tr></table>"#,
        );
        assert!(
            html.contains(r#"<table width="600" bgcolor="ffffff" align="center" cellpadding="8" style="background-color:#ffffff;width:600px;margin-left:auto;margin-right:auto">"#),
            "{html}"
        );
        assert!(
            html.contains(r#"<td style="padding:8px;vertical-align:top;color:red" valign="top">"#),
            "{html}"
        );
    }

    #[test]
    fn tables_inside_a_centered_cell_are_centered() {
        let html = prepared(
            r#"<table><tr><td align="center"><table><tr><td>x</td></tr></table></td></tr></table><table><tr><td>y</td></tr></table>"#,
        );
        assert_eq!(html.matches("margin-left:auto").count(), 1, "{html}");
        assert!(
            html.contains(r#"<td align="center" style="text-align:center"><table style="margin-left:auto;margin-right:auto">"#),
            "{html}"
        );
    }

    #[test]
    fn attribute_text_that_is_not_a_color_or_length_never_reaches_the_css() {
        let html = prepared(
            r#"<table bgcolor="red;background:url(https://x.test/a)" width="1;x:y"><tr><td>x</td></tr></table>"#,
        );
        assert!(!html.contains("style="), "{html}");
    }

    #[test]
    fn cid_images_get_their_data_and_remote_images_stay_remote() {
        let html = prepared(
            r#"<img src="cid:logo@X"><img src="https://x.test/p.gif"><img src="cid:other">"#,
        );
        assert!(
            html.contains(r#"<img src="data:image/png;base64,AAAA">"#),
            "{html}"
        );
        assert!(
            html.contains(r#"<img src="https://x.test/p.gif">"#),
            "{html}"
        );
        assert!(html.contains(r#"<img src="cid:other">"#), "{html}");
    }

    fn links(html: &str, width: u32, scale: f32) -> Vec<Link> {
        render(
            &Source {
                html: html.into(),
                inline: Vec::new(),
            },
            width,
            scale,
        )
        .unwrap()
        .links
    }

    const PIXEL: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

    #[test]
    fn an_image_link_covers_the_image_in_device_pixels() {
        let html = format!(
            r#"<a href="https://a.test/x"><img src="{PIXEL}" width="40" height="30" style="display:block"></a>"#
        );
        let at = |scale| links(&html, 300, scale);
        assert_eq!(
            at(1.0),
            [Link {
                href: "https://a.test/x".into(),
                text: String::new(),
                rects: vec![Rect {
                    x: 0.0,
                    y: 0.0,
                    width: 40.0,
                    height: 30.0
                }],
            }]
        );
        assert_eq!(
            at(2.0)[0].rects,
            [Rect {
                x: 0.0,
                y: 0.0,
                width: 80.0,
                height: 60.0
            }]
        );
    }

    #[test]
    fn a_wrapped_link_has_a_box_per_line_and_its_whole_text() {
        let found = links(
            r#"<p style="margin:0">Go <a href="https://a.test/long">to the <b>very</b> long page that wraps over lines</a> now</p>"#,
            240,
            1.0,
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].href, "https://a.test/long");
        assert_eq!(found[0].text, "to the very long page that wraps over lines");
        let rects = &found[0].rects;
        assert!(rects.len() >= 2, "{rects:?}");
        assert!(
            rects
                .windows(2)
                .all(|pair| pair[1].y >= pair[0].y + pair[0].height - 1.0),
            "{rects:?}"
        );
        assert!(
            rects[0].x > 0.0,
            "the first line starts after \"Go\": {rects:?}"
        );
        assert_eq!(rects[1].x, 0.0, "{rects:?}");
    }

    #[test]
    fn a_link_in_a_table_cell_sits_inside_the_cell() {
        let found = links(
            r#"<table cellpadding="10" style="margin-left:100px"><tr><td><a href="mailto:me@x.test">write</a></td></tr></table>"#,
            400,
            1.0,
        );
        assert_eq!(found.len(), 1);
        assert_eq!(
            (found[0].href.as_str(), found[0].text.as_str()),
            ("mailto:me@x.test", "write")
        );
        let [rect] = found[0].rects[..] else {
            panic!("{found:?}");
        };
        assert!(rect.x >= 110.0 && rect.y >= 10.0, "{rect:?}");
    }

    #[test]
    fn only_web_and_mail_links_get_boxes() {
        let found = links(
            r#"<a href="javascript:alert(1)">a</a> <a href="file:///etc/passwd">b</a> <a href="data:text/html,x">c</a> <a href=" https://a.test/1 ">d</a> <a href="https://a.test/1">e</a>"#,
            300,
            1.0,
        );
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(
            (found[0].href.as_str(), found[0].text.as_str()),
            ("https://a.test/1", "d")
        );
        assert_eq!(found[0].rects.len(), 2, "{found:?}");
    }

    #[test]
    fn link_boxes_past_the_height_limit_are_dropped() {
        let html = format!(
            r#"<a href="https://a.test/top">top</a><div style="height:{MAX_HEIGHT}px"></div><a href="https://a.test/bottom">bottom</a>"#
        );
        let found = links(&html, 300, 1.0);
        assert_eq!(
            found.iter().map(|l| l.href.as_str()).collect::<Vec<_>>(),
            ["https://a.test/top"]
        );
    }
}
