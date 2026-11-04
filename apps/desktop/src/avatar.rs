mod motion;

use std::cell::Cell;
use std::f32::consts::{PI, TAU};
use std::rc::Rc;
use std::time::Instant;

use gpui_kit::*;
use motion::{Input, Motion, Next, Pose};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "LenientAvatar")]
pub struct Avatar {
    pub shape: AvatarShape,
    pub color: AvatarColor,
}

impl Default for Avatar {
    /// The blue star the API gives the main bot; Reset restores it.
    fn default() -> Self {
        Self {
            shape: AvatarShape::Star,
            color: AvatarColor::Blue,
        }
    }
}

/// A newer API may offer presets this build does not draw; those fall back
/// to the default rather than failing the whole bot.
#[derive(Deserialize)]
struct LenientAvatar {
    #[serde(default, deserialize_with = "lenient")]
    shape: Option<AvatarShape>,
    #[serde(default, deserialize_with = "lenient")]
    color: Option<AvatarColor>,
}

fn lenient<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).ok())
}

impl From<LenientAvatar> for Avatar {
    fn from(raw: LenientAvatar) -> Self {
        let fallback = Self::default();
        Self {
            shape: raw.shape.unwrap_or(fallback.shape),
            color: raw.color.unwrap_or(fallback.color),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AvatarShape {
    Star,
    Round,
    Cloud,
    Tile,
    Sparkle,
    Flower,
    Egg,
    Bean,
    Cone,
    House,
    Gem,
    Ghost,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AvatarColor {
    White,
    Brown,
    Red,
    Orange,
    Amber,
    Yellow,
    Green,
    Teal,
    Blue,
    Purple,
    Pink,
    Gray,
}

/// How a body's outline is generated: a radius around the center for each
/// angle (y grows downward), then softened, stretched, and turned.
#[derive(Clone, Copy)]
enum Outline {
    Wave {
        base: f32,
        waves: &'static [(f32, f32, f32)],
    },
    /// Rounded points between flat valleys.
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
    /// Samples averaged on each side; rounds polygon corners.
    soften: usize,
    stretch: (f32, f32),
    turn: f32,
    /// Center between the eyes, as a fraction of the size from the middle.
    eyes: (f32, f32),
}

impl AvatarShape {
    pub const ALL: [Self; 12] = [
        Self::Star,
        Self::Round,
        Self::Cloud,
        Self::Tile,
        Self::Sparkle,
        Self::Flower,
        Self::Egg,
        Self::Bean,
        Self::Cone,
        Self::House,
        Self::Gem,
        Self::Ghost,
    ];

    pub fn label(self) -> &'static str {
        self.art().label
    }

    fn art(self) -> Art {
        let wave = |base, waves| Outline::Wave { base, waves };
        let art = |label, outline, soften, stretch, turn, eyes| Art {
            label,
            outline,
            soften,
            stretch,
            turn,
            eyes,
        };
        match self {
            Self::Star => art(
                "Star",
                wave(0.37, &[(7., 0.08, 0.3), (2., 0.012, 1.)]),
                1,
                (1., 1.),
                0.,
                (0.03, -0.03),
            ),
            Self::Round => art(
                "Round",
                wave(0.43, &[(3., 0.012, 0.5)]),
                0,
                (1., 0.97),
                0.,
                (0.04, -0.04),
            ),
            Self::Cloud => art(
                "Cloud",
                wave(0.38, &[(4., 0.045, 0.6), (7., 0.022, 0.)]),
                1,
                (1.1, 0.94),
                0.,
                (0.02, -0.01),
            ),
            Self::Tile => art(
                "Tile",
                Outline::Squircle { power: 4. },
                0,
                (1., 1.),
                0.3,
                (0.02, -0.02),
            ),
            Self::Sparkle => art(
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
            Self::Flower => art(
                "Flower",
                wave(0.4, &[(8., 0.045, 0.)]),
                1,
                (1., 1.),
                0.,
                (0., -0.02),
            ),
            Self::Egg => art(
                "Egg",
                wave(0.4, &[(1., 0.07, PI / 2.), (2., 0.015, 0.)]),
                0,
                (0.9, 1.04),
                -0.25,
                (0.03, 0.),
            ),
            Self::Bean => art(
                "Bean",
                Outline::Squircle { power: 2.6 },
                0,
                (1.18, 0.78),
                -0.12,
                (0.06, -0.02),
            ),
            Self::Cone => art(
                "Cone",
                Outline::Polygon { sides: 3 },
                3,
                (1., 1.),
                0.,
                (-0.06, 0.),
            ),
            Self::House => art(
                "House",
                Outline::Polygon { sides: 5 },
                2,
                (1., 0.96),
                -PI / 2. + 0.12,
                (0., 0.03),
            ),
            Self::Gem => art(
                "Gem",
                Outline::Polygon { sides: 6 },
                2,
                (1., 0.94),
                0.25,
                (0.03, -0.02),
            ),
            Self::Ghost => art("Ghost", Outline::Ghost, 1, (0.96, 1.04), 0., (0.02, -0.06)),
        }
    }
}

impl AvatarColor {
    pub const ALL: [Self; 12] = [
        Self::White,
        Self::Brown,
        Self::Red,
        Self::Orange,
        Self::Amber,
        Self::Yellow,
        Self::Green,
        Self::Teal,
        Self::Blue,
        Self::Purple,
        Self::Pink,
        Self::Gray,
    ];

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

    pub fn fill(self) -> Hsla {
        rgb(match self {
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
        })
        .into()
    }
}

const SAMPLES: usize = 72;

/// The body outline in unit coordinates around the center, fitted so its
/// widest point reaches `REACH` of the size.
fn outline(art: &Art) -> [(f32, f32); SAMPLES] {
    const REACH: f32 = 0.455;
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

/// A bot's avatar at a fixed size.
pub struct BotAvatar {
    id: ElementId,
    avatar: Avatar,
    size: Pixels,
    live: bool,
    working: bool,
}

pub fn bot_avatar(id: impl Into<ElementId>, avatar: Avatar, size: Pixels) -> BotAvatar {
    BotAvatar {
        id: id.into(),
        avatar,
        size,
        live: true,
        working: false,
    }
}

impl BotAvatar {
    pub fn working(mut self) -> Self {
        self.working = true;
        self
    }

    /// Drawn at rest, without tracking the pointer (picker thumbnails).
    pub fn still(mut self) -> Self {
        self.live = false;
        self
    }
}

impl IntoElement for BotAvatar {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

/// Motion kept across frames for one live avatar.
struct Live {
    motion: Motion,
    started: Instant,
    /// Whether pointer moves outside the attention zone still matter: the
    /// eyes are away from center or still moving back.
    awake: Rc<Cell<bool>>,
    wake: Option<(Instant, Task<()>)>,
}

impl Element for BotAvatar {
    type RequestLayoutState = ();
    type PrepaintState = Option<Hitbox>;

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let style = Style {
            size: size(self.size.into(), self.size.into()),
            flex_shrink: 0.,
            ..Style::default()
        };
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        _: &mut App,
    ) -> Option<Hitbox> {
        self.live
            .then(|| window.insert_hitbox(bounds, HitboxBehavior::Normal))
    }

    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        hitbox: &mut Option<Hitbox>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let (Some(id), Some(hitbox), false) = (id, hitbox.as_ref(), cx.reduce_motion()) else {
            paint_avatar(bounds, self.avatar, Pose::default(), window);
            return;
        };
        let center = bounds.center();
        let reach = (self.size * 3.).max(px(72.));
        let zone = Bounds::centered_at(center, size(reach * 2., reach * 2.));
        let attending = window.is_window_active() && window.is_window_hovered();
        let input = Input {
            gaze: if attending {
                gaze_toward(center, window.mouse_position(), self.size, reach)
            } else {
                (0., 0.)
            },
            hovered: attending && hitbox.is_hovered(window),
        };
        let view = window.current_view();
        let now = Instant::now();
        let (pose, awake) = window.with_element_state(id, |live: Option<Live>, window| {
            let mut live = live.unwrap_or_else(|| Live {
                motion: Motion::default(),
                started: now,
                awake: Rc::default(),
                wake: None,
            });
            let (mut pose, next) = live.motion.advance(now, input);
            if self.working {
                let phase = now.duration_since(live.started).as_secs_f32() * 4.;
                pose.gaze = (phase.sin() * 0.6, phase.cos() * 0.35);
                pose.hover = (phase.sin() + 1.) * 0.5;
                window.request_animation_frame();
            }
            live.awake.set(next != Next::Rest || input.gaze != (0., 0.));
            match next {
                Next::Frame => {
                    live.wake = None;
                    window.request_animation_frame();
                }
                Next::At(due) if live.wake.as_ref().is_some_and(|(at, _)| *at == due) => {}
                Next::At(due) => {
                    let delay = due.saturating_duration_since(now);
                    let task = window.spawn(cx, async move |cx| {
                        cx.background_executor().timer(delay).await;
                        let _ = cx.update(|_, cx| cx.notify(view));
                    });
                    live.wake = Some((due, task));
                }
                Next::Rest => live.wake = None,
            }
            ((pose, live.awake.clone()), live)
        });
        paint_avatar(bounds, self.avatar, pose, window);

        let moved = awake.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
            if phase == DispatchPhase::Bubble && (moved.get() || zone.contains(&event.position)) {
                cx.notify(view);
            }
        });
        window.on_mouse_event(move |_: &MouseExitEvent, phase, _, cx| {
            if phase == DispatchPhase::Bubble && awake.get() {
                cx.notify(view);
            }
        });
    }
}

/// Gaze toward the pointer: eases out as it moves away, and lets go past
/// `reach` so the eyes return to center.
fn gaze_toward(
    center: Point<Pixels>,
    pointer: Point<Pixels>,
    size: Pixels,
    reach: Pixels,
) -> (f32, f32) {
    let dx = f32::from(pointer.x - center.x);
    let dy = f32::from(pointer.y - center.y);
    let distance = (dx * dx + dy * dy).sqrt();
    if distance == 0. || distance > f32::from(reach) {
        return (0., 0.);
    }
    let pull = distance / (distance + f32::from(size) * 0.5);
    (dx / distance * pull, dy / distance * pull)
}

fn paint_avatar(bounds: Bounds<Pixels>, avatar: Avatar, pose: Pose, window: &mut Window) {
    let art = avatar.shape.art();
    let unit = f32::from(bounds.size.width.min(bounds.size.height));
    let (gx, gy) = pose.gaze;
    let scale = unit * (1. + 0.06 * pose.hover);
    let lean = (gx * 0.025 * unit, gy * 0.02 * unit);
    let tilt = gx * 0.05;
    let (sin, cos) = tilt.sin_cos();
    let center = bounds.center();
    let place = |(x, y): (f32, f32)| {
        let (x, y) = (x * cos - y * sin, x * sin + y * cos);
        point(
            center.x + px(x * scale + lean.0),
            center.y + px(y * scale + lean.1),
        )
    };

    let points = outline(&art).map(place);
    let mut body = PathBuilder::fill();
    body.move_to(points[0]);
    for i in 0..SAMPLES {
        let p0 = points[(i + SAMPLES - 1) % SAMPLES];
        let p1 = points[i];
        let p2 = points[(i + 1) % SAMPLES];
        let p3 = points[(i + 2) % SAMPLES];
        body.cubic_bezier_to(p2, p1 + (p2 - p0) / 6., p2 - (p3 - p1) / 6.);
    }
    body.close();
    if let Ok(path) = body.build() {
        window.paint_path(path, avatar.color.fill());
    }
    if avatar.color == AvatarColor::White {
        let mut edge = PathBuilder::stroke(px(1.));
        edge.move_to(points[0]);
        for p in &points[1..] {
            edge.line_to(*p);
        }
        edge.close();
        if let Ok(path) = edge.build() {
            window.paint_path(path, hsla(0., 0., 0., 0.18));
        }
    }

    let width = (0.075 * unit).max(1.6);
    let height = (0.165 * unit * (1. + 0.06 * pose.hover)).max(3.6);
    let height = (height * (1. - 0.95 * pose.blink)).max(0.75);
    let spread = (0.08 * unit).max(width * 1.15);
    let look = (gx * 0.075, gy * 0.06);
    let eyes = (art.eyes.0 + look.0, art.eyes.1 + look.1);
    let middle = place(eyes);
    let ink: Hsla = rgb(0x0d1424).into();
    for side in [-1., 1.] {
        let eye = point(middle.x + px(side * spread), middle.y);
        let rect = Bounds::centered_at(eye, size(px(width), px(height)));
        window.paint_quad(fill(rect, ink).corner_radii(px(width / 2.)));
    }
}

#[cfg(test)]
mod tests {
    use super::{Avatar, AvatarColor, AvatarShape, gaze_toward, outline};
    use gpui_kit::{point, px};

    #[test]
    fn every_outline_is_finite_and_fits_its_box() {
        for shape in AvatarShape::ALL {
            let points = outline(&shape.art());
            for (x, y) in points {
                assert!(x.is_finite() && y.is_finite(), "{shape:?}");
                assert!(x.abs() <= 0.456 && y.abs() <= 0.456, "{shape:?} {x} {y}");
            }
        }
    }

    #[test]
    fn avatars_parse_by_name_and_tolerate_newer_presets() {
        let parsed: Avatar =
            serde_json::from_value(serde_json::json!({ "shape": "ghost", "color": "teal" }))
                .unwrap();
        assert_eq!(
            parsed,
            Avatar {
                shape: AvatarShape::Ghost,
                color: AvatarColor::Teal
            }
        );
        let newer: Avatar =
            serde_json::from_value(serde_json::json!({ "shape": "comet", "color": "teal" }))
                .unwrap();
        assert_eq!(newer.shape, Avatar::default().shape);
        assert_eq!(newer.color, AvatarColor::Teal);
        assert_eq!(
            serde_json::to_value(Avatar::default()).unwrap(),
            serde_json::json!({ "shape": "star", "color": "blue" })
        );
    }

    #[test]
    fn gaze_eases_toward_the_pointer_and_lets_go_far_away() {
        let center = point(px(100.), px(100.));
        let (x, y) = gaze_toward(center, point(px(140.), px(100.)), px(40.), px(160.));
        assert!(x > 0.6 && x < 1. && y == 0.);
        assert_eq!(
            gaze_toward(center, point(px(400.), px(100.)), px(40.), px(160.)),
            (0., 0.)
        );
        assert_eq!(gaze_toward(center, center, px(40.), px(160.)), (0., 0.));
    }
}
