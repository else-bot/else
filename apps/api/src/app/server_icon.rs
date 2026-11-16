//! The icon every desktop shows for this server: letters, a pattern, an
//! emoji, a 128-pixel PNG picture, or each desktop's default. Desktops
//! draw it; the server only checks and keeps the owner's choice.

use std::io::Cursor;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// The side of a picture, as desktops store it.
pub const PICTURE_SIDE: u32 = 128;
/// The largest picture; the database refuses more.
pub const MAX_PICTURE_BYTES: usize = 128 * 1024;
const MAX_LETTERS: usize = 2;
const MAX_EMOJI_BYTES: usize = 32;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LetterStyle {
    Gradient,
    Monogram,
}

/// An icon as the owner chose it. A picture's PNG travels beside it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Look {
    /// Each desktop draws its own default for the server.
    Default {},
    /// `None` fields follow the server's name.
    Letters {
        style: LetterStyle,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        letters: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hue: Option<u16>,
    },
    Pattern {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hue: Option<u16>,
    },
    Emoji {
        emoji: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hue: Option<u16>,
    },
    Picture {},
}

/// A checked icon, ready to store.
#[derive(Debug)]
pub struct ServerIcon {
    /// What capabilities advertise: the look, and a picture's SHA-256 as
    /// `digest` so desktops fetch the PNG only when it changes.
    pub advertised: Value,
    pub picture: Option<Vec<u8>>,
}

impl ServerIcon {
    pub fn new(look: Look, picture: Option<Vec<u8>>) -> Result<Self, &'static str> {
        let hue = match &look {
            Look::Letters { hue, .. } | Look::Pattern { hue } | Look::Emoji { hue, .. } => *hue,
            Look::Default {} | Look::Picture {} => None,
        };
        if hue.is_some_and(|hue| hue >= 360) {
            return Err("A hue is a whole number of degrees below 360.");
        }
        let visible = |text: &str| !text.is_empty() && !text.chars().any(char::is_control);
        match (&look, &picture) {
            (Look::Picture {}, Some(png)) => check_picture(png)?,
            (Look::Picture {}, None) => return Err("A picture icon needs its PNG."),
            (_, Some(_)) => return Err("Only a picture icon carries a PNG."),
            (
                Look::Letters {
                    letters: Some(letters),
                    ..
                },
                None,
            ) if !visible(letters)
                || letters.trim() != letters
                || letters.chars().count() > MAX_LETTERS =>
            {
                return Err("Letters are one or two visible characters.");
            }
            (Look::Emoji { emoji, .. }, None)
                if !visible(emoji) || emoji.len() > MAX_EMOJI_BYTES =>
            {
                return Err("An emoji is one visible symbol.");
            }
            _ => {}
        }
        let mut advertised = serde_json::to_value(&look).expect("icon serializes");
        if let Some(png) = &picture {
            advertised["digest"] = Value::String(digest(png));
        }
        Ok(Self {
            advertised,
            picture,
        })
    }
}

/// The lowercase hex SHA-256 desktops compare pictures by.
pub fn digest(png: &[u8]) -> String {
    Sha256::digest(png)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn check_picture(png: &[u8]) -> Result<(), &'static str> {
    const INVALID: &str = "A picture is a 128 by 128 pixel PNG of at most 128 KiB.";
    if png.len() > MAX_PICTURE_BYTES {
        return Err(INVALID);
    }
    let mut reader = image::ImageReader::new(Cursor::new(png))
        .with_guessed_format()
        .map_err(|_| INVALID)?;
    if reader.format() != Some(image::ImageFormat::Png) {
        return Err(INVALID);
    }
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(PICTURE_SIDE);
    limits.max_image_height = Some(PICTURE_SIDE);
    reader.limits(limits);
    let decoded = reader.decode().map_err(|_| INVALID)?;
    if (decoded.width(), decoded.height()) != (PICTURE_SIDE, PICTURE_SIDE) {
        return Err(INVALID);
    }
    Ok(())
}
