//! Animation math for the reader. Pure functions and small state machines with no gpui types, so
//! they can be tested with simulated frame rates.
//!
//! Everything here is driven by the *time elapsed since the previous frame* (`dt`, in seconds)
//! rather than by a frame count, so an animation takes the same wall-clock time at 60 Hz, 144 Hz
//! or 240 Hz. No timers and no animation queue: callers advance the state once per rendered frame
//! and stop asking for frames when nothing is moving.

/// How long a page turn slides for.
pub const PAGE_TURN_SECS: f32 = 0.16;
/// Page turns that interrupt a running one (holding a key, mashing the wheel) are quicker so the
/// reader keeps up instead of lagging behind the input.
pub const PAGE_TURN_INTERRUPTED_SECS: f32 = 0.09;
/// Time constant of the smooth-scroll glide: the remaining distance shrinks to 1/e (about 37%)
/// every this many seconds.
pub const SCROLL_TAU_SECS: f32 = 0.06;
/// Frames that take longer than this (window dragged, system hiccup) are treated as this long so
/// an animation never jumps.
pub const MAX_FRAME_SECS: f32 = 0.05;
/// A scroll closer than this (in logical pixels) to its target snaps to it.
const SNAP_PX: f32 = 0.05;

pub fn clamp_frame_dt(dt: f32) -> f32 {
    if dt.is_finite() {
        dt.clamp(0.0, MAX_FRAME_SECS)
    } else {
        0.0
    }
}

pub fn ease_out_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

/// Move `current` toward `target` exponentially. Because the decay is exponential, applying it in
/// many small steps gives the same result as one big step of the same total duration: the glide
/// is identical at any refresh rate.
pub fn smooth_toward(current: f32, target: f32, dt: f32, tau: f32) -> f32 {
    let delta = target - current;
    if delta.abs() <= SNAP_PX {
        return target;
    }
    let k = 1.0 - (-dt.max(0.0) / tau.max(1e-4)).exp();
    current + delta * k
}

/// One frame of smooth scrolling for both axes. Returns the new offset and whether it is still
/// moving (i.e. whether another frame is needed).
pub fn step_scroll(current: (f32, f32), target: (f32, f32), dt: f32) -> ((f32, f32), bool) {
    let next = (
        smooth_toward(current.0, target.0, dt, SCROLL_TAU_SECS),
        smooth_toward(current.1, target.1, dt, SCROLL_TAU_SECS),
    );
    (next, next != target)
}

/// Which way the page content travels during a turn: `-1.0` means it moves left (the new page
/// slides in from the right), `+1.0` means it moves right.
///
/// Left-to-right: going forward moves content left, going back moves it right.
/// Right-to-left (manga) is the mirror image.
pub fn travel(forward: bool, rtl: bool) -> f32 {
    if forward != rtl { -1.0 } else { 1.0 }
}

/// A page-turn slide. Shifts are expressed in viewport widths so a window resize cannot leave the
/// animation in the wrong place.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PageTransition {
    travel: f32,
    /// Where the outgoing page starts, in viewport widths. Non-zero only when this transition
    /// interrupts another one and takes over the page that was still sliding in.
    out_start: f32,
    duration: f32,
    elapsed: f32,
}

impl PageTransition {
    pub fn new(travel: f32, out_start: f32, duration: f32) -> Self {
        Self {
            travel,
            out_start,
            duration: duration.max(0.001),
            elapsed: 0.0,
        }
    }

    pub fn starting(travel: f32) -> Self {
        Self::new(travel, 0.0, PAGE_TURN_SECS)
    }

    /// The transition that replaces this one when another page turn arrives mid-slide. The page
    /// that was sliding in becomes the outgoing page, continuing from exactly where it is on
    /// screen right now. Nothing is queued: at most one transition ever exists.
    pub fn interrupted_by(&self, travel: f32) -> Self {
        Self::new(travel, self.incoming_shift(), PAGE_TURN_INTERRUPTED_SECS)
    }

    pub fn travel(&self) -> f32 {
        self.travel
    }

    pub fn advance(&mut self, dt: f32) {
        self.elapsed = (self.elapsed + dt.max(0.0)).min(self.duration);
    }

    pub fn is_done(&self) -> bool {
        self.elapsed >= self.duration
    }

    pub fn progress(&self) -> f32 {
        self.elapsed / self.duration
    }

    fn eased(&self) -> f32 {
        ease_out_cubic(self.progress())
    }

    /// Horizontal shift of the page that is leaving, in viewport widths.
    pub fn outgoing_shift(&self) -> f32 {
        self.out_start + (self.travel - self.out_start) * self.eased()
    }

    /// Horizontal shift of the page that is arriving, in viewport widths (0 once it has settled).
    pub fn incoming_shift(&self) -> f32 {
        -self.travel * (1.0 - self.eased())
    }
}

/// Parse `gsettings get org.gnome.desktop.interface enable-animations` output.
/// Returns `true` when the desktop asks for reduced motion (animations disabled).
pub fn gsettings_disables_animations(output: &str) -> bool {
    output.trim() == "false"
}

/// Whether the user (or their desktop) asks for reduced motion.
///
/// * `YOMU_REDUCE_MOTION=1` forces it on, `=0` forces it off.
/// * Otherwise GNOME's "Animations" switch is honoured via `gsettings`, when available.
///   (Desktops without that setting simply report no preference.)
pub fn detect_reduced_motion() -> bool {
    if let Ok(value) = std::env::var("YOMU_REDUCE_MOTION") {
        return matches!(value.trim(), "1" | "true" | "yes" | "on");
    }
    std::process::Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", "enable-animations"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| gsettings_disables_animations(&String::from_utf8_lossy(&out.stdout)))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `steps` frames of `dt` and return the state after them.
    fn run_transition(mut t: PageTransition, hz: f32) -> (f32, usize) {
        let dt = 1.0 / hz;
        let mut frames = 0;
        while !t.is_done() {
            t.advance(dt);
            frames += 1;
            assert!(frames < 10_000);
        }
        (frames as f32 * dt, frames)
    }

    #[test]
    fn page_turn_takes_the_same_time_at_any_refresh_rate() {
        for hz in [30.0, 60.0, 90.0, 120.0, 144.0, 240.0, 360.0] {
            let (seconds, frames) = run_transition(PageTransition::starting(-1.0), hz);
            assert!(
                (seconds - PAGE_TURN_SECS).abs() <= 1.0 / hz + 1e-4,
                "{hz} Hz took {seconds}s ({frames} frames)"
            );
        }
    }

    #[test]
    fn slide_positions_agree_across_refresh_rates_at_the_same_moment() {
        // 50 ms in, sampled with 60 Hz frames and with 240 Hz frames.
        let mut slow = PageTransition::starting(-1.0);
        let mut fast = PageTransition::starting(-1.0);
        for _ in 0..3 {
            slow.advance(1.0 / 60.0);
        }
        for _ in 0..12 {
            fast.advance(1.0 / 240.0);
        }
        assert!((slow.outgoing_shift() - fast.outgoing_shift()).abs() < 1e-4);
        assert!((slow.incoming_shift() - fast.incoming_shift()).abs() < 1e-4);
    }

    #[test]
    fn pages_start_and_end_in_the_right_places() {
        let mut t = PageTransition::starting(-1.0);
        assert_eq!(t.outgoing_shift(), 0.0, "the old page starts where it is");
        assert_eq!(
            t.incoming_shift(),
            1.0,
            "the new page starts one viewport to the right"
        );
        t.advance(10.0);
        assert!(t.is_done());
        assert_eq!(
            t.outgoing_shift(),
            -1.0,
            "the old page ends fully off screen to the left"
        );
        assert_eq!(t.incoming_shift(), 0.0, "the new page ends at rest");
    }

    #[test]
    fn pages_stay_exactly_one_viewport_apart() {
        // Moving together: the gap between the two pages is constant, so they read as one strip.
        let mut t = PageTransition::starting(-1.0);
        for _ in 0..30 {
            t.advance(0.006);
            let gap = t.incoming_shift() - t.outgoing_shift();
            assert!((gap - 1.0).abs() < 1e-5, "gap {gap}");
        }
    }

    #[test]
    fn motion_is_monotonic_and_eased_out() {
        let mut t = PageTransition::starting(-1.0);
        let mut last = t.incoming_shift();
        let mut last_step = f32::MAX;
        for _ in 0..16 {
            t.advance(0.01);
            let now = t.incoming_shift();
            assert!(now <= last + 1e-6, "incoming page only moves toward rest");
            let step = (last - now).abs();
            assert!(step <= last_step + 1e-6, "ease-out: steps never grow");
            last_step = step;
            last = now;
        }
        assert_eq!(t.incoming_shift(), 0.0);
    }

    #[test]
    fn travel_follows_reading_direction_and_navigation_action() {
        // left to right
        assert_eq!(travel(true, false), -1.0, "forward: content moves left");
        assert_eq!(travel(false, false), 1.0, "back: content moves right");
        // right to left (manga) is the mirror image
        assert_eq!(travel(true, true), 1.0);
        assert_eq!(travel(false, true), -1.0);
    }

    #[test]
    fn interruption_continues_from_where_the_page_is_and_never_queues() {
        let mut first = PageTransition::starting(-1.0);
        first.advance(0.04);
        let on_screen = first.incoming_shift();
        assert!(on_screen > 0.0 && on_screen < 1.0);

        let second = first.interrupted_by(-1.0);
        assert_eq!(
            second.outgoing_shift(),
            on_screen,
            "no jump at the moment of interruption"
        );
        assert_eq!(second.incoming_shift(), 1.0);
        assert!(!second.is_done());

        // pressing back while sliding forward reverses smoothly
        let reversed = first.interrupted_by(1.0);
        assert_eq!(reversed.outgoing_shift(), on_screen);
        assert_eq!(reversed.incoming_shift(), -1.0);

        // ... and it is quicker than a normal turn
        let (seconds, _) = run_transition(second, 120.0);
        assert!(seconds < PAGE_TURN_SECS);
    }

    #[test]
    fn mashing_the_key_converges_instead_of_lagging() {
        // 40 presses, one every 25 ms, at 144 Hz: each interrupts the last.
        let dt = 1.0 / 144.0;
        let mut transition: Option<PageTransition> = None;
        let mut presses = 0;
        let mut time = 0.0f32;
        let mut next_press = 0.0f32;
        while presses < 40 || transition.is_some() {
            if presses < 40 && time >= next_press {
                transition = Some(match transition {
                    Some(t) => t.interrupted_by(-1.0),
                    None => PageTransition::starting(-1.0),
                });
                presses += 1;
                next_press += 0.025;
            }
            if let Some(mut t) = transition {
                t.advance(dt);
                transition = if t.is_done() { None } else { Some(t) };
            }
            time += dt;
            assert!(time < 3.0, "must settle");
        }
        assert!(
            time < 40.0 * 0.025 + PAGE_TURN_INTERRUPTED_SECS + 0.05,
            "settled {time}s"
        );
    }

    #[test]
    fn smooth_scroll_is_identical_at_every_refresh_rate() {
        for (hz, expected_frames) in [(60.0, 12), (144.0, 29), (240.0, 48)] {
            let dt = 1.0 / hz;
            let mut off = 0.0;
            for _ in 0..expected_frames {
                off = smooth_toward(off, 300.0, dt, SCROLL_TAU_SECS);
            }
            // 0.2 s of gliding, however it is sliced
            let exact = 300.0 * (1.0 - (-0.2f32 / SCROLL_TAU_SECS).exp());
            assert!((off - exact).abs() < 0.5, "{hz} Hz: {off} vs {exact}");
        }
    }

    #[test]
    fn smooth_scroll_converges_and_snaps_without_overshoot() {
        let mut off = (0.0, 0.0);
        let target = (0.0, 480.0);
        let mut moving = true;
        let mut frames = 0;
        let mut last = 0.0;
        while moving {
            let (next, still) = step_scroll(off, target, 1.0 / 120.0);
            assert!(
                next.1 >= last && next.1 <= target.1,
                "no overshoot, no reversal"
            );
            last = next.1;
            off = next;
            moving = still;
            frames += 1;
            assert!(frames < 2_000, "must stop requesting frames");
        }
        assert_eq!(off, target, "ends exactly on the target");
        let seconds = frames as f32 / 120.0;
        assert!(seconds < 0.6, "glides, then is done: {seconds}s");
    }

    #[test]
    fn an_idle_scroll_asks_for_no_frames() {
        let (off, moving) = step_scroll((12.0, 34.0), (12.0, 34.0), 0.016);
        assert_eq!(off, (12.0, 34.0));
        assert!(!moving);
    }

    #[test]
    fn long_frames_are_clamped() {
        assert_eq!(clamp_frame_dt(0.2), MAX_FRAME_SECS);
        assert_eq!(clamp_frame_dt(-1.0), 0.0);
        assert_eq!(clamp_frame_dt(f32::NAN), 0.0);
        assert_eq!(clamp_frame_dt(0.008), 0.008);
    }

    #[test]
    fn easing_has_the_right_shape() {
        assert_eq!(ease_out_cubic(0.0), 0.0);
        assert_eq!(ease_out_cubic(1.0), 1.0);
        assert_eq!(ease_out_cubic(2.0), 1.0);
        assert!(ease_out_cubic(0.5) > 0.5, "fast start");
    }

    #[test]
    fn reads_the_gnome_animation_switch() {
        assert!(gsettings_disables_animations("false\n"));
        assert!(!gsettings_disables_animations("true\n"));
        assert!(!gsettings_disables_animations(""));
    }
}
