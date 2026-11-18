//! An email's HTML part, drawn on a white page like a mail client would.
//! Rendering is offline: remote images, fonts and stylesheets stay unloaded
//! and are reported in [`crate::Rendered::blocked`]. Images the message
//! carries itself, named by `cid:`, are drawn. Links are listed, never live.

use std::collections::HashMap;

use html5ever::tendril::TendrilSink as _;
use html5ever::{Attribute, QualName, local_name, serialize::SerializeOpts};
use markup5ever_rcdom::{NodeData, RcDom, SerializableHandle};
use serde::{Deserialize, Serialize};

use crate::{Error, Rendered};

/// Largest HTML part an email render takes.
pub const MAX_HTML_BYTES: usize = 2 * 1024 * 1024;
/// Largest total of base64 inline-image data an email render takes.
pub const MAX_INLINE_BASE64: usize = 6 * 1024 * 1024;
/// Content below this many CSS pixels is cut off.
pub const MAX_HEIGHT: u32 = 20_000;
/// Links listed beside the render.
const MAX_LINKS: usize = 100;
const MAX_LINK_BYTES: usize = 2048;
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
    let mut rendered = crate::draw(
        &prepared.html,
        BASE_CSS.to_owned(),
        MAX_HEIGHT,
        width,
        scale,
    )?;
    rendered.links = prepared.links;
    Ok(rendered)
}

/// What an element takes from its ancestors' presentational attributes.
#[derive(Clone, Default)]
struct Inherited {
    /// The enclosing table's `cellpadding`, for its cells.
    cellpadding: Option<String>,
    /// Inside `<center>` or an `align="center"` block, which centers
    /// tables as well as text.
    centered: bool,
}

pub(crate) struct Prepared {
    pub html: String,
    pub links: Vec<String>,
}

/// The HTML with its presentational attributes (`bgcolor`, `width`,
/// `align`, `cellpadding` and the like) moved into inline styles, which is
/// all the renderer reads, and `cid:` images replaced by their data. Also
/// returns the `http`, `https` and `mailto` links it holds, in order.
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
    let mut links = Vec::new();
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
            if tag == "a"
                && let Some(href) = attribute(&attrs, "href").map(str::trim)
                && links.len() < MAX_LINKS
                && href.len() <= MAX_LINK_BYTES
                && is_link(href)
                && !links.iter().any(|link| link == href)
            {
                links.push(href.to_owned());
            }
            if !hints.is_empty() {
                let mut style = hints.join(";");
                if let Some(own) = attrs.iter_mut().find(|a| &*a.name.local == "style") {
                    style.push(';');
                    style.push_str(&own.value);
                    own.value = style.into();
                } else {
                    attrs.push(Attribute {
                        name: QualName::new(
                            None,
                            html5ever::Namespace::from(""),
                            local_name!("style"),
                        ),
                        value: style.into(),
                    });
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
    Prepared { html, links }
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

    fn prepared(html: &str) -> Prepared {
        prepare(&Source {
            html: html.into(),
            inline: vec![Inline {
                content_id: "<Logo@x>".into(),
                media_type: "image/png".into(),
                data_base64: "AAAA".into(),
            }],
        })
    }

    #[test]
    fn presentational_attributes_become_inline_styles_before_the_own_style() {
        let html = prepared(
            r#"<table width="600" bgcolor="ffffff" align="center" cellpadding="8"><tr><td style="color:red" valign="top">x</td></tr></table>"#,
        )
        .html;
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
        )
        .html;
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
        )
        .html;
        assert!(!html.contains("style="), "{html}");
    }

    #[test]
    fn cid_images_get_their_data_and_remote_images_stay_remote() {
        let html = prepared(
            r#"<img src="cid:logo@X"><img src="https://x.test/p.gif"><img src="cid:other">"#,
        )
        .html;
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

    #[test]
    fn links_are_listed_once_in_order_and_scripts_are_not_links() {
        let links = prepared(
            r#"<a href="https://a.test/1">a</a><a href="javascript:alert(1)">b</a><a href=" mailto:me@x.test ">c</a><a href="https://a.test/1">d</a>"#,
        )
        .links;
        assert_eq!(links, ["https://a.test/1", "mailto:me@x.test"]);
    }
}
