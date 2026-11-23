//! How each avatar preset looks: its name, its outline, where its eyes sit
//! and its fill, so the desktop and the iPhone draw the same bot.

use std::f32::consts::{PI, TAU};

use crate::bots::{AvatarColor, AvatarShape};

/// Points on an outline, evenly spaced by angle.
pub const SAMPLES: usize = 72;

/// The eyes' color on every fill.
pub const INK: u32 = 0x0d1424;

#[derive(Clone, Copy)]
enum Outline {
    Wave {
        base: f32,
        waves: &'static [(f32, f32, f32)],
    },
    Points {
        count: f32,
        base: f32,
    },
    Squircle {
        power: f32,
    },
    Polygon {
        sides: u8,
    },
    Ghost,
}

struct Art {
    label: &'static str,
    outline: Outline,
    soften: usize,
    stretch: (f32, f32),
    turn: f32,
    eyes: (f32, f32),
}

fn art(shape: AvatarShape) -> Art {
    let wave = |base, waves| Outline::Wave { base, waves };
    let art = |label, outline, soften, stretch, turn, eyes| Art {
        label,
        outline,
        soften,
        stretch,
        turn,
        eyes,
    };
    match shape {
        AvatarShape::Star => art(
            "Star",
            wave(0.37, &[(7., 0.08, 0.3), (2., 0.012, 1.)]),
            1,
            (1., 1.),
            0.,
            (0.03, -0.03),
        ),
        AvatarShape::Round => art(
            "Round",
            wave(0.43, &[(3., 0.012, 0.5)]),
            0,
            (1., 0.97),
            0.,
            (0.04, -0.04),
        ),
        AvatarShape::Cloud => art(
            "Cloud",
            wave(0.38, &[(4., 0.045, 0.6), (7., 0.022, 0.)]),
            1,
            (1.1, 0.94),
            0.,
            (0.02, -0.01),
        ),
        AvatarShape::Tile => art(
            "Tile",
            Outline::Squircle { power: 4. },
            0,
            (1., 1.),
            0.3,
            (0.02, -0.02),
        ),
        AvatarShape::Sparkle => art(
            "Sparkle",
            Outline::Points {
                count: 4.,
                base: 0.27,
            },
            0,
            (1., 1.),
            0.2,
            (0., -0.01),
        ),
        AvatarShape::Flower => art(
            "Flower",
            wave(0.4, &[(8., 0.045, 0.)]),
            1,
            (1., 1.),
            0.,
            (0., -0.02),
        ),
        AvatarShape::Egg => art(
            "Egg",
            wave(0.4, &[(1., 0.07, PI / 2.), (2., 0.015, 0.)]),
            0,
            (0.9, 1.04),
            -0.25,
            (0.03, 0.),
        ),
        AvatarShape::Bean => art(
            "Bean",
            Outline::Squircle { power: 2.6 },
            0,
            (1.18, 0.78),
            -0.12,
            (0.06, -0.02),
        ),
        AvatarShape::Cone => art(
            "Cone",
            Outline::Polygon { sides: 3 },
            3,
            (1., 1.),
            0.,
            (-0.06, 0.),
        ),
        AvatarShape::House => art(
            "House",
            Outline::Polygon { sides: 5 },
            2,
            (1., 0.96),
            -PI / 2. + 0.12,
            (0., 0.03),
        ),
        AvatarShape::Gem => art(
            "Gem",
            Outline::Polygon { sides: 6 },
            2,
            (1., 0.94),
            0.25,
            (0.03, -0.02),
        ),
        AvatarShape::Ghost => art("Ghost", Outline::Ghost, 1, (0.96, 1.04), 0., (0.02, -0.06)),
    }
}

impl AvatarShape {
    /// The name people see for it.
    pub fn label(self) -> &'static str {
        art(self).label
    }

    /// Where the middle of its two eyes sits, from the center of a unit box.
    pub fn eyes(self) -> (f32, f32) {
        art(self).eyes
    }

    /// Its outline in a unit box centered on the origin, reaching 0.455 at
    /// most on each axis. Joined by a smooth curve through every point.
    pub fn outline(self) -> [(f32, f32); SAMPLES] {
        const REACH: f32 = 0.455;
        let art = art(self);
        let mut radius = [0f32; SAMPLES];
        for (i, r) in radius.iter_mut().enumerate() {
            let a = i as f32 / SAMPLES as f32 * TAU;
            *r = match art.outline {
                Outline::Points { count, base } => {
                    base + 0.18 * ((1. + (count * a).cos()) / 2.).powf(1.6)
                }
                Outline::Wave { base, waves } => {
                    base + waves
                        .iter()
                        .map(|(k, amp, phase)| amp * (k * (a + phase)).cos())
                        .sum::<f32>()
                }
                Outline::Squircle { power } => {
                    (a.cos().abs().powf(power) + a.sin().abs().powf(power)).powf(-1. / power)
                }
                Outline::Polygon { sides } => {
                    let sector = TAU / f32::from(sides);
                    let local = a.rem_euclid(sector) - sector / 2.;
                    (sector / 2.).cos() / local.cos()
                }
                Outline::Ghost => {
                    let below = a.sin().max(0.).powi(2);
                    0.42 + below * (0.03 + 0.035 * (6. * a).cos())
                }
            };
        }
        if art.soften > 0 {
            let k = art.soften;
            let raw = radius;
            for (i, r) in radius.iter_mut().enumerate() {
                let sum: f32 = (0..=2 * k)
                    .map(|j| raw[(i + SAMPLES + j - k) % SAMPLES])
                    .sum();
                *r = sum / (2 * k + 1) as f32;
            }
        }
        let (sin, cos) = art.turn.sin_cos();
        let mut points = [(0f32, 0f32); SAMPLES];
        let mut extent = 0f32;
        for (i, point) in points.iter_mut().enumerate() {
            let a = i as f32 / SAMPLES as f32 * TAU;
            let x = radius[i] * a.cos() * art.stretch.0;
            let y = radius[i] * a.sin() * art.stretch.1;
            *point = (x * cos - y * sin, x * sin + y * cos);
            extent = extent.max(point.0.abs()).max(point.1.abs());
        }
        let fit = REACH / extent;
        points.map(|(x, y)| (x * fit, y * fit))
    }
}

impl AvatarColor {
    /// The name people see for it.
    pub fn label(self) -> &'static str {
        match self {
            Self::White => "White",
            Self::Brown => "Brown",
            Self::Red => "Red",
            Self::Orange => "Orange",
            Self::Amber => "Amber",
            Self::Yellow => "Yellow",
            Self::Green => "Green",
            Self::Teal => "Teal",
            Self::Blue => "Blue",
            Self::Purple => "Purple",
            Self::Pink => "Pink",
            Self::Gray => "Gray",
        }
    }

    /// Its fill as `0xRRGGBB`. White is drawn with a faint edge, since it
    /// would vanish on a light background.
    pub fn rgb(self) -> u32 {
        match self {
            Self::White => 0xf2f2f4,
            Self::Brown => 0xa4693f,
            Self::Red => 0xee3b47,
            Self::Orange => 0xf6631d,
            Self::Amber => 0xf59e0b,
            Self::Yellow => 0xf8cb2e,
            Self::Green => 0x22c162,
            Self::Teal => 0x17b3a3,
            Self::Blue => 0x2b88fb,
            Self::Purple => 0x8f5cf6,
            Self::Pink => 0xec4b9a,
            Self::Gray => 0x7d7d84,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::bots::AvatarShape;

    #[test]
    fn every_outline_is_finite_and_fits_its_box() {
        for shape in AvatarShape::ALL {
            for (x, y) in shape.outline() {
                assert!(x.is_finite() && y.is_finite(), "{shape:?}");
                assert!(x.abs() <= 0.456 && y.abs() <= 0.456, "{shape:?} {x} {y}");
            }
        }
    }
}
