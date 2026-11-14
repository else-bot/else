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
//!
//! Code, paths and keys use a modern monospace family the same way: the
//! first installed one from a list, else the theme's default. GPUI panics
//! on a family it cannot load, so only an installed name is ever chosen.
//! macOS keeps the theme's Menlo.

use std::sync::OnceLock;

use gpui_kit::SharedString;

static FAMILY: OnceLock<Option<SharedString>> = OnceLock::new();
static MONO: OnceLock<Option<SharedString>> = OnceLock::new();

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

/// Monospace families to try, in order: designed-for-screen faces first,
/// then the common distribution defaults.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const MONO_CANDIDATES: &[&str] = &[
    "JetBrains Mono",
    "JetBrainsMono Nerd Font",
    "Geist Mono",
    "Cascadia Mono",
    "CaskaydiaMono Nerd Font",
    "Fira Code",
    "Source Code Pro",
    "Noto Sans Mono",
    "DejaVu Sans Mono",
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
    let family = first_match(CANDIDATES, has_weights);
    FAMILY.get_or_init(|| family);
    let mono = first_match(MONO_CANDIDATES, |family| {
        installed.iter().any(|name| name == family)
    });
    MONO.get_or_init(|| mono);
}

#[cfg(not(target_os = "linux"))]
pub fn init(_cx: &gpui_kit::App) {
    FAMILY.get_or_init(|| None);
    MONO.get_or_init(|| None);
}

/// The family the theme should name, or `None` to keep the system font.
pub fn family() -> Option<SharedString> {
    FAMILY.get().cloned().flatten()
}

/// The monospace family the theme should name, or `None` to keep its
/// default.
pub fn mono_family() -> Option<SharedString> {
    MONO.get().cloned().flatten()
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn first_match(candidates: &[&str], mut usable: impl FnMut(&str) -> bool) -> Option<SharedString> {
    candidates
        .iter()
        .find(|family| usable(family))
        .map(|family| SharedString::from(family.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_first_family_with_weight_faces() {
        let weighted = ["Noto Sans", "DejaVu Sans"];
        assert_eq!(
            first_match(CANDIDATES, |family| weighted.contains(&family)),
            Some(SharedString::from("Noto Sans"))
        );
    }

    #[test]
    fn prefers_inter_when_it_has_weights() {
        assert_eq!(
            first_match(CANDIDATES, |_| true),
            Some(SharedString::from("Inter"))
        );
    }

    #[test]
    fn prefers_a_modern_mono_and_falls_back_to_the_distribution_default() {
        assert_eq!(
            first_match(MONO_CANDIDATES, |_| true),
            Some(SharedString::from("JetBrains Mono"))
        );
        assert_eq!(
            first_match(MONO_CANDIDATES, |family| family == "DejaVu Sans Mono"),
            Some(SharedString::from("DejaVu Sans Mono"))
        );
        assert_eq!(first_match(MONO_CANDIDATES, |_| false), None);
    }

    #[test]
    fn keeps_the_system_font_without_a_weighted_family() {
        assert_eq!(first_match(CANDIDATES, |_| false), None);
    }
}
