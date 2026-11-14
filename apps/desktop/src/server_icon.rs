//! A saved server's picture in the server rail. Every server gets one
//! generated from its name: letters on a gradient whose colour and faint
//! pattern come from a hash of the name, or a laptop for this computer's
//! server. The user may pick letters, a colour, a pattern, an emoji or a
//! picture instead. Choices live beside the registry in `desktop.icons.json`,
//! pictures in `desktop.<id>.icon.png`.

use std::collections::{BTreeMap, HashMap};
use std::io::Cursor;
use std::path::Path;
use std::sync::Arc;

use gpui_kit::assets::IconName;
use gpui_kit::component::{ActiveTheme as _, Icon, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::config::{ProfileStore, ServerId, read_private_limited, replace_private};

/// The side of a stored picture: twice the largest size the rail draws.
const PICTURE_SIDE: u32 = 128;
const MAX_ICONS_BYTES: u64 = 64 * 1024;
const MAX_PICTURE_BYTES: u64 = 1024 * 1024;
/// The largest image file the editor accepts before scaling it down.
pub(crate) const MAX_SOURCE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_LETTERS: usize = 2;
const MAX_EMOJI_BYTES: usize = 32;

/// The colours the editor offers besides the one from the name.
pub(crate) const HUES: [u16; 12] = [0, 22, 40, 95, 145, 170, 195, 215, 235, 265, 295, 330];

pub(crate) const EMOJI: [&str; 32] = [
    "🚀", "🛰️", "🏠", "🧪", "🔬", "🍓", "🐙", "🦊", "🐝", "🌊", "🔥", "⚡", "🌙", "⭐", "🍀", "🌵",
    "🎯", "🎛️", "🧠", "🤖", "💼", "📈", "🛠️", "🧰", "🗄️", "🖥️", "💻", "🎮", "🎨", "🏔️", "🌐", "🔒",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LetterStyle {
    Gradient,
    Monogram,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) enum ServerIcon {
    /// A laptop for this computer's server, gradient initials for the rest.
    /// It follows renames.
    #[default]
    Default,
    /// `None` fields follow the name.
    Letters {
        style: LetterStyle,
        letters: Option<String>,
        hue: Option<u16>,
    },
    Pattern {
        hue: Option<u16>,
    },
    Emoji {
        emoji: String,
        hue: Option<u16>,
    },
    Picture(Arc<Image>),
}

/// Up to two letters without control characters, or `None` for blank.
pub(crate) fn letters(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let valid = !trimmed.is_empty()
        && trimmed.chars().count() <= MAX_LETTERS
        && !trimmed.chars().any(char::is_control);
    valid.then(|| trimmed.to_owned())
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Stored {
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
    Picture,
}

impl Stored {
    fn of(icon: &ServerIcon) -> Option<Self> {
        Some(match icon.clone() {
            ServerIcon::Default => return None,
            ServerIcon::Letters {
                style,
                letters,
                hue,
            } => Self::Letters {
                style,
                letters,
                hue,
            },
            ServerIcon::Pattern { hue } => Self::Pattern { hue },
            ServerIcon::Emoji { emoji, hue } => Self::Emoji { emoji, hue },
            ServerIcon::Picture(_) => Self::Picture,
        })
    }

    /// The icon this entry describes, or `None` when it is invalid or its
    /// picture is gone.
    fn icon(self, picture: &Path) -> Option<ServerIcon> {
        let hue_ok = |hue: Option<u16>| hue.is_none_or(|hue| hue < 360);
        match self {
            Self::Letters {
                style,
                letters: chosen,
                hue,
            } if hue_ok(hue) && chosen.as_deref().is_none_or(|l| letters(l).is_some()) => {
                Some(ServerIcon::Letters {
                    style,
                    letters: chosen,
                    hue,
                })
            }
            Self::Pattern { hue } if hue_ok(hue) => Some(ServerIcon::Pattern { hue }),
            Self::Emoji { emoji, hue }
                if hue_ok(hue)
                    && !emoji.is_empty()
                    && emoji.len() <= MAX_EMOJI_BYTES
                    && !emoji.chars().any(char::is_control) =>
            {
                Some(ServerIcon::Emoji { emoji, hue })
            }
            Self::Picture => read_picture(picture).map(ServerIcon::Picture),
            _ => None,
        }
    }
}

fn read_picture(path: &Path) -> Option<Arc<Image>> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_PICTURE_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    if image::guess_format(&bytes).ok()? != image::ImageFormat::Png {
        return None;
    }
    let decoded = crate::app::decode_image(&bytes).ok()?;
    ((decoded.width(), decoded.height()) == (PICTURE_SIDE, PICTURE_SIDE))
        .then(|| Arc::new(Image::from_bytes(ImageFormat::Png, bytes)))
}

/// The icons the user chose, by server. A server without one shows
/// [`ServerIcon::Default`].
#[derive(Default)]
pub(crate) struct Icons {
    chosen: HashMap<ServerId, ServerIcon>,
}

static DEFAULT: ServerIcon = ServerIcon::Default;

impl Icons {
    /// Entries that are invalid or whose picture is missing fall back to
    /// the default; an unreadable file is an error.
    pub(crate) fn load(store: &ProfileStore) -> Result<Self, String> {
        let path = store.server_icons_path();
        let Some(text) = read_private_limited(&path, MAX_ICONS_BYTES)? else {
            return Ok(Self::default());
        };
        let stored: BTreeMap<ServerId, serde_json::Value> =
            serde_json::from_str(&text).map_err(|err| {
                format!(
                    "The server icons in {} are not valid: {err}",
                    path.display()
                )
            })?;
        let chosen = stored
            .into_iter()
            .filter_map(|(id, value)| {
                let icon = serde_json::from_value::<Stored>(value)
                    .ok()?
                    .icon(&store.server_picture_path(id))?;
                Some((id, icon))
            })
            .collect();
        Ok(Self { chosen })
    }

    pub(crate) fn get(&self, id: ServerId) -> &ServerIcon {
        self.chosen.get(&id).unwrap_or(&DEFAULT)
    }

    /// Save `icon` for `id`: its picture first, then the list, so the list
    /// never names a picture that wasn't written.
    pub(crate) fn set(
        &mut self,
        store: &ProfileStore,
        id: ServerId,
        icon: ServerIcon,
    ) -> Result<(), String> {
        let picture = store.server_picture_path(id);
        let fail = |err: std::io::Error| format!("Cannot save the server icon: {err}");
        if let ServerIcon::Picture(image) = &icon {
            replace_private(&picture, &image.bytes).map_err(fail)?;
        }
        let mut chosen = self.chosen.clone();
        match icon {
            ServerIcon::Default => chosen.remove(&id),
            icon => chosen.insert(id, icon),
        };
        Self::write(store, &chosen)?;
        let keeps_picture = matches!(chosen.get(&id), Some(ServerIcon::Picture(_)));
        self.chosen = chosen;
        if keeps_picture {
            return Ok(());
        }
        // The list no longer names the picture, so a leftover file is only
        // clutter; report it without undoing the saved choice.
        remove_if_present(&picture).map_err(fail)
    }

    /// Drop a removed server's icon and picture.
    pub(crate) fn forget(&mut self, store: &ProfileStore, id: ServerId) -> Result<(), String> {
        if self.chosen.contains_key(&id) {
            self.set(store, id, ServerIcon::Default)?;
        }
        Ok(())
    }

    fn write(store: &ProfileStore, chosen: &HashMap<ServerId, ServerIcon>) -> Result<(), String> {
        let stored: BTreeMap<ServerId, Stored> = chosen
            .iter()
            .filter_map(|(id, icon)| Some((*id, Stored::of(icon)?)))
            .collect();
        let json = serde_json::to_vec_pretty(&stored).expect("icons serialize");
        replace_private(&store.server_icons_path(), &[&json[..], b"\n"].concat())
            .map_err(|err| format!("Cannot save the server icons: {err}"))
    }
}

fn remove_if_present(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(err) if err.kind() != std::io::ErrorKind::NotFound => Err(err),
        _ => Ok(()),
    }
}

/// Scale an image file to a square picture, cropping the longer side
/// around its centre.
pub(crate) fn square_picture(bytes: &[u8]) -> Result<Arc<Image>, String> {
    let decoded = crate::app::decode_image(bytes)?;
    let mut png = Cursor::new(Vec::new());
    decoded
        .resize_to_fill(
            PICTURE_SIDE,
            PICTURE_SIDE,
            image::imageops::FilterType::Lanczos3,
        )
        .write_to(&mut png, image::ImageFormat::Png)
        .map_err(|err| err.to_string())?;
    Ok(Arc::new(Image::from_bytes(
        ImageFormat::Png,
        png.into_inner(),
    )))
}

fn fnv(text: &str, salt: u32) -> u32 {
    text.to_lowercase()
        .bytes()
        .fold(0x811c_9dc5 ^ salt, |hash, byte| {
            (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
        })
}

/// The hue a name's generated icon uses.
pub(crate) fn name_hue(name: &str) -> u16 {
    (fnv(name, 0) % 360) as u16
}

/// The first letters of the first two words, or the first letter of one.
pub(crate) fn initials(name: &str) -> String {
    let mut words = name
        .split(|c: char| c.is_whitespace() || matches!(c, '-' | '_' | '.'))
        .filter_map(|word| word.chars().next());
    let first = words.next().unwrap_or('?');
    if !first.is_alphanumeric() {
        return first.to_string();
    }
    first
        .to_uppercase()
        .chain(words.next().into_iter().flat_map(char::to_uppercase))
        .collect()
}

/// A five by five grid, mirrored left to right, drawn from the name.
pub(crate) fn pattern(name: &str) -> [[bool; 5]; 5] {
    let bits = fnv(name, 0x9e37_79b9);
    std::array::from_fn(|row| {
        std::array::from_fn(|col| {
            let col = col.min(4 - col);
            bits >> (row * 3 + col) & 1 == 1
        })
    })
}

fn color(hue: u16, saturation: f32, lightness: f32, alpha: f32) -> Hsla {
    hsla(f32::from(hue % 360) / 360., saturation, lightness, alpha)
}

fn grid(name: &str, cell: Hsla, inset: Pixels) -> Div {
    v_flex()
        .absolute()
        .inset_0()
        .p(inset)
        .children(pattern(name).map(|row| {
            h_flex()
                .flex_1()
                .children(row.map(|on| div().flex_1().h_full().when(on, |c| c.bg(cell))))
        }))
}

/// A server's icon at `size`, with the corner radius the rail uses.
pub(crate) fn render(
    icon: &ServerIcon,
    name: &str,
    device: bool,
    size: Pixels,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let tile = div()
        .relative()
        .flex_none()
        .size(size)
        .rounded(size * 0.275)
        .overflow_hidden()
        .flex()
        .items_center()
        .justify_center()
        .font_weight(FontWeight::SEMIBOLD)
        .line_height(size)
        .text_color(gpui_kit::white());
    let text_size = |text: &str| {
        let ratio = if text.chars().count() > 1 { 0.36 } else { 0.42 };
        size * ratio
    };
    let auto_hue = |hue: Option<u16>| hue.unwrap_or_else(|| name_hue(name));
    match icon {
        ServerIcon::Default if device => tile
            .bg(theme.muted)
            .child(
                Icon::new(IconName::Laptop)
                    .size(size * 0.45)
                    .text_color(theme.muted_foreground),
            )
            .into_any_element(),
        ServerIcon::Default
        | ServerIcon::Letters {
            style: LetterStyle::Gradient,
            ..
        } => {
            let (text, hue) = match icon {
                ServerIcon::Letters { letters, hue, .. } => (
                    letters.clone().unwrap_or_else(|| initials(name)),
                    auto_hue(*hue),
                ),
                _ => (initials(name), name_hue(name)),
            };
            tile.bg(linear_gradient(
                140.,
                linear_color_stop(color(hue, 0.68, 0.58, 1.), 0.),
                linear_color_stop(color(hue + 32, 0.62, 0.40, 1.), 1.),
            ))
            .child(grid(name, gpui_kit::white().opacity(0.1), size * 0.1))
            .child(div().relative().text_size(text_size(&text)).child(text))
            .into_any_element()
        }
        ServerIcon::Letters {
            style: LetterStyle::Monogram,
            letters,
            hue,
        } => {
            let hue = auto_hue(*hue);
            let text = letters.clone().unwrap_or_else(|| initials(name));
            tile.bg(color(hue, 0.25, 0.16, 1.))
                .border(size / 24.)
                .border_color(color(hue, 0.70, 0.60, 0.55))
                .text_color(color(hue, 0.85, 0.75, 1.))
                .text_size(text_size(&text))
                .child(text)
                .into_any_element()
        }
        ServerIcon::Pattern { hue } => {
            let hue = auto_hue(*hue);
            tile.bg(color(hue, 0.45, 0.30, 1.))
                .child(grid(name, color(hue, 0.80, 0.82, 0.9), size * 0.12))
                .into_any_element()
        }
        ServerIcon::Emoji { emoji, hue } => tile
            .bg(color(auto_hue(*hue), 0.30, 0.24, 1.))
            .text_size(size * 0.52)
            .child(emoji.clone())
            .into_any_element(),
        ServerIcon::Picture(image) => tile
            .bg(theme.muted)
            .child(img(image.clone()).size_full().object_fit(ObjectFit::Cover))
            .into_any_element(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    #[::core::prelude::v1::test]
    fn generated_icons_come_from_the_name() {
        assert_eq!(initials("Proliquid"), "P");
        assert_eq!(initials("Home Lab"), "HL");
        assert_eq!(initials("raspberry-pi"), "RP");
        assert_eq!(initials("else.proliquid.xyz"), "EP");
        assert_eq!(initials("  acme  research  cluster"), "AR");
        assert_eq!(initials("🚀 Launch"), "🚀");
        assert_eq!(initials(""), "?");
        assert_eq!(name_hue("Staging"), name_hue("staging"), "case-blind");
        assert_ne!(name_hue("Staging"), name_hue("Production"));
        for name in ["Staging", "Home Lab", "x"] {
            let grid = pattern(name);
            assert!(grid.iter().all(|row| row[0] == row[4] && row[1] == row[3]));
            assert_eq!(grid, pattern(name), "stable");
        }
    }

    #[::core::prelude::v1::test]
    fn letters_are_one_or_two_visible_characters() {
        assert_eq!(letters(" ab "), Some("ab".into()));
        assert_eq!(letters("é"), Some("é".into()));
        assert_eq!(letters("   "), None);
        assert_eq!(letters("abc"), None);
        assert_eq!(letters("a\u{7}"), None);
    }

    fn store() -> (tempfile::TempDir, ProfileStore) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let store = ProfileStore::at(dir.path().join("desktop.json"));
        (dir, store)
    }

    #[::core::prelude::v1::test]
    fn chosen_icons_and_pictures_survive_a_reload_and_are_cleaned_up() {
        let (_dir, store) = store();
        let (a, b, c) = (ServerId::new_v4(), ServerId::new_v4(), ServerId::new_v4());
        let mut icons = Icons::load(&store).unwrap();
        assert_eq!(icons.get(a), &ServerIcon::Default);
        let source = {
            let mut png = Cursor::new(Vec::new());
            image::DynamicImage::new_rgb8(300, 200)
                .write_to(&mut png, image::ImageFormat::Png)
                .unwrap();
            png.into_inner()
        };
        let picture = square_picture(&source).unwrap();
        let decoded = image::load_from_memory(&picture.bytes).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (128, 128));
        let emoji = ServerIcon::Emoji {
            emoji: "🐙".into(),
            hue: Some(195),
        };
        icons.set(&store, a, emoji.clone()).unwrap();
        icons
            .set(&store, b, ServerIcon::Picture(picture.clone()))
            .unwrap();
        icons
            .set(
                &store,
                c,
                ServerIcon::Letters {
                    style: LetterStyle::Monogram,
                    letters: Some("Q".into()),
                    hue: None,
                },
            )
            .unwrap();
        let reloaded = Icons::load(&store).unwrap();
        assert_eq!(reloaded.get(a), &emoji);
        assert_eq!(reloaded.get(b), &ServerIcon::Picture(picture.clone()));
        assert!(matches!(
            reloaded.get(c),
            ServerIcon::Letters { letters: Some(l), .. } if l == "Q"
        ));
        icons
            .set(&store, b, ServerIcon::Pattern { hue: None })
            .unwrap();
        assert!(
            !store.server_picture_path(b).exists(),
            "old picture removed"
        );
        icons.forget(&store, a).unwrap();
        let reloaded = Icons::load(&store).unwrap();
        assert_eq!(reloaded.get(a), &ServerIcon::Default);
        assert_eq!(reloaded.get(b), &ServerIcon::Pattern { hue: None });
    }

    #[::core::prelude::v1::test]
    fn invalid_entries_and_missing_pictures_fall_back_to_the_default() {
        let (_dir, store) = store();
        let [good, bad_hue, too_long, gone, truncated, oversized] =
            std::array::from_fn(|_| ServerId::new_v4());
        let png = |side: u32| {
            let mut png = Cursor::new(Vec::new());
            image::DynamicImage::new_rgb8(side, side)
                .write_to(&mut png, image::ImageFormat::Png)
                .unwrap();
            png.into_inner()
        };
        let whole = png(PICTURE_SIDE);
        replace_private(
            &store.server_picture_path(truncated),
            &whole[..whole.len() / 2],
        )
        .unwrap();
        replace_private(&store.server_picture_path(oversized), &png(4096)).unwrap();
        let json = format!(
            r#"{{"{good}": {{"kind": "pattern", "hue": 145}},
                "{bad_hue}": {{"kind": "pattern", "hue": 400}},
                "{too_long}": {{"kind": "letters", "style": "gradient", "letters": "abc"}},
                "{gone}": {{"kind": "picture"}},
                "{truncated}": {{"kind": "picture"}},
                "{oversized}": {{"kind": "picture"}}}}"#
        );
        replace_private(&store.server_icons_path(), json.as_bytes()).unwrap();
        let icons = Icons::load(&store).unwrap();
        assert_eq!(icons.get(good), &ServerIcon::Pattern { hue: Some(145) });
        for id in [bad_hue, too_long, gone, truncated, oversized] {
            assert_eq!(icons.get(id), &ServerIcon::Default);
        }
        replace_private(&store.server_icons_path(), b"not json").unwrap();
        assert!(Icons::load(&store).is_err());
    }
}
