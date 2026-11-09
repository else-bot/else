use std::time::{Duration, Instant};

const MAX_STEP: f32 = 1. / 30.;
const AFTER_GAP: f32 = 1. / 60.;
const SUBSTEP: f32 = 1. / 240.;
const BLINK: f32 = 0.18;
const FIRST_BLINK: Duration = Duration::from_millis(2600);

#[derive(Debug, Clone, Copy, PartialEq)]
struct Spring {
    value: f32,
    velocity: f32,
}

impl Spring {
    const REST: Self = Self {
        value: 0.,
        velocity: 0.,
    };

    fn step(&mut self, target: f32, stiffness: f32, damping: f32, dt: f32) {
        let mut left = dt;
        while left > 0. {
            let h = left.min(SUBSTEP);
            let accel = stiffness * (target - self.value) - damping * self.velocity;
            self.velocity += accel * h;
            self.value += self.velocity * h;
            left -= h;
        }
        if (target - self.value).abs() < 1e-3 && self.velocity.abs() < 1e-2 {
            *self = Self {
                value: target,
                velocity: 0.,
            };
        }
    }

    fn resting_at(&self, target: f32) -> bool {
        self.value == target && self.velocity == 0.
    }
}

/// Where the pointer pulls the avatar this frame.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Input {
    /// Gaze direction, each axis in -1..=1; zero looks straight ahead.
    pub gaze: (f32, f32),
    pub hovered: bool,
    pub working: bool,
}

/// What to draw this frame.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Pose {
    pub gaze: (f32, f32),
    /// 0 at rest, about 1 while hovered (springs overshoot slightly).
    pub hover: f32,
    /// 0 with eyes open, 1 fully closed.
    pub blink: f32,
    /// 0 when idle, about 1 while working; eases in and out.
    pub work: f32,
}

/// When the avatar needs to be painted again.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Next {
    /// Still moving: the next display frame.
    Frame,
    /// At rest until an idle blink is due.
    At(Instant),
    /// Fully at rest; only input wakes it.
    Rest,
}

#[derive(Debug, Clone)]
pub struct Motion {
    gaze: (Spring, Spring),
    hover: Spring,
    work: Spring,
    blink_started: Option<Instant>,
    blink_due: Option<Instant>,
    blinks: u32,
    last: Option<(Instant, Input)>,
}

impl Default for Motion {
    fn default() -> Self {
        Self {
            gaze: (Spring::REST, Spring::REST),
            hover: Spring::REST,
            work: Spring::REST,
            blink_started: None,
            blink_due: None,
            blinks: 0,
            last: None,
        }
    }
}

impl Motion {
    pub fn advance(&mut self, now: Instant, input: Input) -> (Pose, Next) {
        let dt = match self.last {
            Some((last, _)) => {
                let gap = now.saturating_duration_since(last).as_secs_f32();
                if gap > MAX_STEP { AFTER_GAP } else { gap }
            }
            None => 0.,
        };
        let entering = input.hovered && self.last.is_none_or(|(_, previous)| !previous.hovered);
        self.last = Some((now, input));

        let (gx, gy) = clamp_unit(input.gaze);
        self.gaze.0.step(gx, 170., 21., dt);
        self.gaze.1.step(gy, 170., 21., dt);
        let hover_target = if input.hovered { 1. } else { 0. };
        self.hover.step(hover_target, 320., 16., dt);
        let work_target = if input.working { 1. } else { 0. };
        self.work.step(work_target, 170., 24., dt);

        if !input.hovered {
            self.blink_due = None;
        } else if entering {
            self.blink_started = Some(now);
            self.blink_due = Some(now + FIRST_BLINK);
        } else if self.blink_due.is_some_and(|due| now >= due) {
            self.blink_started = Some(now);
            self.blinks = self.blinks.wrapping_add(1);
            let gap = 2200 + (self.blinks.wrapping_mul(1777) % 2400) as u64;
            self.blink_due = Some(now + Duration::from_millis(gap));
        }
        let blink = match self.blink_started {
            Some(start) => {
                let t = now.saturating_duration_since(start).as_secs_f32() / BLINK;
                if t >= 1. {
                    self.blink_started = None;
                    0.
                } else {
                    (t * std::f32::consts::PI).sin()
                }
            }
            None => 0.,
        };

        let pose = Pose {
            gaze: (self.gaze.0.value, self.gaze.1.value),
            hover: self.hover.value,
            blink,
            work: self.work.value,
        };
        let moving = !self.gaze.0.resting_at(gx)
            || !self.gaze.1.resting_at(gy)
            || !self.hover.resting_at(hover_target)
            || input.working
            || !self.work.resting_at(work_target)
            || self.blink_started.is_some();
        let next = match (moving, self.blink_due) {
            (true, _) => Next::Frame,
            (false, Some(due)) => Next::At(due),
            (false, None) => Next::Rest,
        };
        (pose, next)
    }
}

fn clamp_unit((x, y): (f32, f32)) -> (f32, f32) {
    let length = (x * x + y * y).sqrt();
    if length > 1. {
        (x / length, y / length)
    } else {
        (x, y)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(motion: &mut Motion, start: Instant, frames: u32, input: Input) -> (Pose, Next, u32) {
        let mut last = (Pose::default(), Next::Rest);
        for frame in 1..=frames {
            let now = start + Duration::from_micros(16_667 * u64::from(frame));
            last = motion.advance(now, input);
            if last.1 != Next::Frame {
                return (last.0, last.1, frame);
            }
        }
        (last.0, last.1, frames)
    }

    #[test]
    fn gaze_follows_the_pointer_and_comes_to_rest() {
        let mut motion = Motion::default();
        let start = Instant::now();
        motion.advance(start, Input::default());
        let look = Input {
            gaze: (3., 0.),
            hovered: false,
            working: false,
        };
        let (pose, next, frames) = run(&mut motion, start, 120, look);
        assert_eq!(pose.gaze, (1., 0.), "clamped to the unit circle");
        assert_eq!(next, Next::Rest, "settles and stops asking for frames");
        assert!(frames < 60, "settles within a second, took {frames} frames");

        let later = start + Duration::from_secs(10);
        let (pose, next, _) = run(&mut motion, later, 120, Input::default());
        assert_eq!(pose.gaze, (0., 0.), "returns to center");
        assert_eq!(next, Next::Rest);
    }

    #[test]
    fn a_long_gap_moves_one_frame_not_the_whole_gap() {
        let mut motion = Motion::default();
        let start = Instant::now();
        motion.advance(start, Input::default());
        let look = Input {
            gaze: (1., 0.),
            hovered: false,
            working: false,
        };
        let (pose, next) = motion.advance(start + Duration::from_secs(5), look);
        assert!(pose.gaze.0 > 0. && pose.gaze.0 < 0.2, "{:?}", pose.gaze);
        assert_eq!(next, Next::Frame);
    }

    #[test]
    fn hovering_blinks_then_waits_for_the_next_blink() {
        let mut motion = Motion::default();
        let start = Instant::now();
        motion.advance(start, Input::default());
        let hover = Input {
            gaze: (0., 0.),
            hovered: true,
            working: false,
        };
        let mut closed = 0f32;
        let mut next = Next::Frame;
        for frame in 1..=90 {
            let now = start + Duration::from_micros(16_667 * frame);
            let (pose, n) = motion.advance(now, hover);
            closed = closed.max(pose.blink);
            next = n;
            if n != Next::Frame {
                break;
            }
        }
        assert!(closed > 0.9, "blinked on entering, peak {closed}");
        assert!(
            matches!(next, Next::At(_)),
            "waits for an idle blink: {next:?}"
        );

        let (pose, next, _) = run(
            &mut motion,
            start + Duration::from_secs(20),
            60,
            Input::default(),
        );
        assert_eq!(pose.hover, 0.);
        assert_eq!(next, Next::Rest, "leaving cancels idle blinks");
    }

    #[test]
    fn returning_before_the_hover_spring_settles_keeps_blinking() {
        let mut motion = Motion::default();
        let start = Instant::now();
        let hover = Input {
            hovered: true,
            ..Input::default()
        };
        let (_, next, _) = run(&mut motion, start, 120, hover);
        assert!(matches!(next, Next::At(_)));

        let leave = start + Duration::from_secs(1);
        motion.advance(leave, Input::default());
        let (_, next, _) = run(&mut motion, leave, 120, hover);
        assert!(
            matches!(next, Next::At(_)),
            "returning schedules another blink"
        );
    }

    #[test]
    fn working_eases_in_animates_until_it_stops_then_eases_out() {
        let mut motion = Motion::default();
        let start = Instant::now();
        motion.advance(start, Input::default());
        let working = Input {
            working: true,
            ..Input::default()
        };
        let (pose, next, frames) = run(&mut motion, start, 240, working);
        assert_eq!(frames, 240, "keeps asking for frames while working");
        assert_eq!(next, Next::Frame);
        assert_eq!(pose.work, 1., "fully in after a few seconds");

        let early = motion.advance(
            start + Duration::from_micros(16_667 * 241),
            Input::default(),
        );
        assert!(early.0.work > 0.9, "eases out instead of snapping");
        let later = start + Duration::from_secs(10);
        let (pose, next, _) = run(&mut motion, later, 120, Input::default());
        assert_eq!(pose.work, 0.);
        assert_eq!(next, Next::Rest, "stops asking for frames once idle");
    }
}
