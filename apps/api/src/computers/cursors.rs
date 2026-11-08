//! Names for the guest's cursor shapes, so the owner's app can show its own
//! native cursor while it controls the desktop instead of a cursor painted
//! into the frames.
//!
//! RFB sends a cursor as pixels, never as a name. The guest image pins its
//! cursor theme (Adwaita), so each shape is recognized by a fingerprint of its
//! size, hotspot, and mask. `tests/desktop_live.rs` regenerates the table from
//! the guest image. An unknown shape is shown as an arrow.

/// CSS cursor names the app maps to native cursors.
pub const STYLES: &[&str] = &[
    "default",
    "pointer",
    "text",
    "vertical-text",
    "crosshair",
    "grab",
    "grabbing",
    "not-allowed",
    "copy",
    "alias",
    "context-menu",
    "ew-resize",
    "ns-resize",
    "nesw-resize",
    "nwse-resize",
    "col-resize",
    "row-resize",
    "n-resize",
    "s-resize",
    "e-resize",
    "w-resize",
];

const SHAPES: &[(u64, &str)] = &[
    (0xe8489bcc53681790, "default"),
    (0x7a04d818fd045110, "pointer"),
    (0x764291e08db0666e, "text"),
    (0x75463ee33414129a, "vertical-text"),
    (0xfb87f8d9d5bc7d7d, "crosshair"),
    (0x9b2956ef90113143, "grab"),
    (0x5c514cbda85b5abf, "grabbing"),
    (0x0d52f76e6ffb37d7, "not-allowed"),
    (0x8c24b1d1746b02cf, "copy"),
    (0xd03a8b871fe60946, "alias"),
    (0xedd802f918c91e56, "context-menu"),
    (0xeb1163e3864eae9f, "ew-resize"),
    (0x9934311e9290e5ee, "ns-resize"),
    (0x6876ee947aa7bbf4, "nesw-resize"),
    (0x2ca4c02dd43a77e5, "nwse-resize"),
    (0xfd1589777024ba5b, "col-resize"),
    (0xeeffefd8cade0304, "row-resize"),
    (0xaabb08bb8152dc09, "n-resize"),
    (0xfe390b9428cca349, "s-resize"),
    (0x41fdff16ddf4d4cf, "e-resize"),
    (0x6289274cc24d7646, "w-resize"),
];

/// FNV-1a over the shape's size, hotspot, and mask.
pub fn fingerprint(hotspot: (u16, u16), size: (u16, u16), mask: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let head = [hotspot.0, hotspot.1, size.0, size.1].map(u16::to_be_bytes);
    for byte in head.as_flattened().iter().chain(mask) {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// The CSS name of a guest cursor shape, `default` when unknown.
pub fn style(hotspot: (u16, u16), size: (u16, u16), mask: &[u8]) -> &'static str {
    let print = fingerprint(hotspot, size, mask);
    SHAPES
        .iter()
        .find(|(known, _)| *known == print)
        .map_or("default", |(_, style)| style)
}
