//! Picks a Linux UI family whose bold and semibold text draws heavier.
//!
//! GPUI's Linux text system loads each face of a family at its default
//! weight and picks a face for a run by the face's own weight, so a variable
//! font, which ships every weight in one face, always draws regular.
//! `.SystemUIFont` resolves through GPUI's fallback stack to the first
//! installed family, and on GNOME and Omarchy that is Adwaita Sans, a
//! variable font. This names an installed family with separate weight faces
//! instead. macOS draws `.SystemUIFont` as SF, whose weights work, so the
//! theme keeps it there.

use std::sync::OnceLock;

use gpui_kit::SharedString;

static FAMILY: OnceLock<Option<SharedString>> = OnceLock::new();

/// Families to try, in order. Inter comes first because Adwaita Sans, which
/// the fallback stack lands on otherwise, is derived from it, so only the
/// weights change.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const CANDIDATES: &[&str] = &[
    "Inter",
    "Noto Sans",
    "Cantarell",
    "Ubuntu",
    "DejaVu Sans",
    "Liberation Sans",
];

/// Probes the installed fonts once. Call before the first theme is built.
#[cfg(target_os = "linux")]
pub fn init(cx: &gpui_kit::App) {
    use gpui_kit::{Font, FontWeight, font};

    let installed = cx.text_system().all_font_names();
    let text_system = cx.text_system();
    let has_weights = |family: &str| {
        installed.iter().any(|name| name == family) && {
            let regular = text_system.resolve_font(&font(family.to_owned()));
            let heavy = text_system.resolve_font(&Font {
                weight: FontWeight::SEMIBOLD,
                ..font(family.to_owned())
            });
            regular != heavy
        }
    };
    let family = first_with_weights(CANDIDATES, has_weights);
    FAMILY.get_or_init(|| family);
}

#[cfg(not(target_os = "linux"))]
pub fn init(_cx: &gpui_kit::App) {
    FAMILY.get_or_init(|| None);
}

/// The family the theme should name, or `None` to keep the system font.
pub fn family() -> Option<SharedString> {
    FAMILY.get().cloned().flatten()
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn first_with_weights(
    candidates: &[&str],
    mut has_weights: impl FnMut(&str) -> bool,
) -> Option<SharedString> {
    candidates
        .iter()
        .find(|family| has_weights(family))
        .map(|family| SharedString::from(family.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_first_family_with_weight_faces() {
        let weighted = ["Noto Sans", "DejaVu Sans"];
        assert_eq!(
            first_with_weights(CANDIDATES, |family| weighted.contains(&family)),
            Some(SharedString::from("Noto Sans"))
        );
    }

    #[test]
    fn prefers_inter_when_it_has_weights() {
        assert_eq!(
            first_with_weights(CANDIDATES, |_| true),
            Some(SharedString::from("Inter"))
        );
    }

    #[test]
    fn keeps_the_system_font_without_a_weighted_family() {
        assert_eq!(first_with_weights(CANDIDATES, |_| false), None);
    }
}
