mod motion;
mod picker;

pub use picker::AvatarPicker;

use std::cell::Cell;
use std::f32::consts::{PI, TAU};
use std::rc::Rc;
use std::time::Instant;

use else_wire::avatar::{INK, SAMPLES};
use else_wire::bots::{Avatar, AvatarColor};
use gpui_kit::*;
use motion::{Input, Motion, Next, Pose, Work};

pub fn color_fill(color: AvatarColor) -> Hsla {
    rgb(color.rgb()).into()
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

struct Live {
    motion: Motion,
    started: Instant,
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
            let pose = Pose {
                work: if self.working { 1. } else { 0. },
                ..Pose::default()
            };
            paint_avatar(bounds, self.avatar, pose, 0., window);
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
            work: match (self.working, crate::ambient::ambient_motion(window, cx)) {
                (false, _) => Work::Idle,
                (true, false) => Work::Still,
                (true, true) => Work::Looping,
            },
        };
        let view = window.current_view();
        let now = Instant::now();
        let (pose, elapsed, awake) = window.with_element_state(id, |live: Option<Live>, window| {
            let mut live = live.unwrap_or_else(|| Live {
                motion: Motion::default(),
                started: now,
                awake: Rc::default(),
                wake: None,
            });
            let (pose, next) = live.motion.advance(now, input);
            // A paused loop holds the reduced-motion pose, so unrelated
            // repaints do not jump it to a new phase.
            let elapsed = match input.work {
                Work::Still => 0.,
                Work::Idle | Work::Looping => now.duration_since(live.started).as_secs_f32(),
            };
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
            ((pose, elapsed, live.awake.clone()), live)
        });
        paint_avatar(bounds, self.avatar, pose, elapsed, window);

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

/// `elapsed` is seconds since the avatar first painted; it phases the working loop.
fn paint_avatar(
    bounds: Bounds<Pixels>,
    avatar: Avatar,
    pose: Pose,
    elapsed: f32,
    window: &mut Window,
) {
    let unit = f32::from(bounds.size.width.min(bounds.size.height));
    let work = pose.work;
    let orbit = TAU * elapsed / 3.6;
    let gx = pose.gaze.0 + (0.42 * orbit.cos() - pose.gaze.0) * work;
    let gy = pose.gaze.1 + (0.3 * orbit.sin() - pose.gaze.1) * work;
    let pulse = (TAU * elapsed / 1.8).sin();
    let blink = pose.blink.max(focus_blink(elapsed) * work);
    let scale = unit * (1. + 0.06 * pose.hover) * (1. + 0.015 * pulse * work);
    let lean = (gx * 0.025 * unit, gy * 0.02 * unit);
    let tilt = gx * 0.05 + 0.03 * orbit.cos() * work;
    let (sin, cos) = tilt.sin_cos();
    let center = bounds.center();
    let place_at = |scale: f32| {
        move |(x, y): (f32, f32)| {
            let (x, y) = (x * cos - y * sin, x * sin + y * cos);
            point(
                center.x + px(x * scale + lean.0),
                center.y + px(y * scale + lean.1),
            )
        }
    };
    let place = place_at(scale);

    let shape = avatar.shape.outline();
    if work > 0.01 {
        paint_halo(
            shape.map(place_at(scale * 1.26)),
            center,
            unit,
            color_fill(avatar.color),
            work,
            elapsed,
            window,
        );
    }
    let points = shape.map(place);
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
        window.paint_path(path, color_fill(avatar.color));
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
    let height = (0.165 * unit * (1. + 0.06 * pose.hover)).max(3.6) * (1. - 0.35 * work);
    let height = (height * (1. - 0.95 * blink)).max(0.75);
    let spread = (0.08 * unit).max(width * 1.15);
    let look = (gx * 0.075, gy * 0.06);
    let (x, y) = avatar.shape.eyes();
    let eyes = (x + look.0, y + look.1);
    let middle = place(eyes);
    let ink: Hsla = rgb(INK).into();
    for side in [-1., 1.] {
        let eye = point(middle.x + px(side * spread), middle.y);
        let rect = Bounds::centered_at(eye, size(px(width), px(height)));
        window.paint_quad(fill(rect, ink).corner_radii(px(width / 2.)));
    }
}

/// A periodic blink while working, so a focused bot still looks alive.
fn focus_blink(elapsed: f32) -> f32 {
    const PERIOD: f32 = 3.8;
    const LENGTH: f32 = 0.17;
    let t = elapsed.rem_euclid(PERIOD);
    if t < LENGTH {
        (t / LENGTH * PI).sin()
    } else {
        0.
    }
}

/// A soft glow behind the avatar and a light tracing its outline, one lap per 1.7 s.
fn paint_halo(
    points: [Point<Pixels>; SAMPLES],
    center: Point<Pixels>,
    unit: f32,
    color: Hsla,
    work: f32,
    elapsed: f32,
    window: &mut Window,
) {
    let glow = 0.3 * unit;
    let breathe = 0.35 + 0.15 * (TAU * elapsed / 1.8).sin();
    window.paint_drop_shadows(
        Bounds::centered_at(center, size(px(glow * 2.), px(glow * 2.))),
        Corners::all(px(glow)),
        &[BoxShadow {
            color: color.opacity(breathe * work),
            offset: point(px(0.), px(0.)),
            blur_radius: px(0.4 * unit),
            spread_radius: px(0.),
            inset: false,
        }],
    );

    let mut track = PathBuilder::stroke(px((0.035 * unit).max(1.)));
    track.move_to(points[0]);
    for p in &points[1..] {
        track.line_to(*p);
    }
    track.close();
    if let Ok(path) = track.build() {
        window.paint_path(path, color.opacity(0.16 * work));
    }

    const LIT: usize = SAMPLES * 38 / 100;
    let at = |i: isize| points[i.rem_euclid(SAMPLES as isize) as usize];
    let head = (elapsed / 1.7).fract() * SAMPLES as f32;
    for i in 0..LIT {
        let k = 1. - i as f32 / LIT as f32;
        let position = head - i as f32;
        let index = position.floor() as isize;
        let (from, to, next) = (at(index - 1), at(index), at(index + 1));
        let mut segment = PathBuilder::stroke(px((unit * (0.03 + 0.045 * k)).max(1.1)));
        segment.move_to(from);
        segment.line_to(to);
        segment.line_to(to + (next - to) * position.fract());
        if let Ok(path) = segment.build() {
            let light = Hsla {
                l: color.l + (1. - color.l) * 0.35 * k,
                ..color
            };
            window.paint_path(path, light.opacity(k * k * work));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::gaze_toward;
    use else_wire::bots::Avatar;
    use else_wire::bots::AvatarColor;
    use else_wire::bots::AvatarShape;
    use gpui_kit::{point, px};

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
