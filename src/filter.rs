//! Turns the raw pen stream (position + "touching?") into clean *finger-like* touch events.
//!
//! A real finger is big and steady; a pen is precise and twitchy. Mapped onto a whole screen,
//! 1 mm of pen is ~12 px, so hand tremor alone would cancel taps and long-presses in every
//! toolkit. This filter makes the pen behave like a finger:
//!
//!  * **blips** (contact < `down_delay`) are ignored, so a bounce above the surface never taps;
//!  * **tap tolerance**: the touch is held exactly where it began until the pen travels more than
//!    `slop_mm`, so taps and long-presses are rock steady;
//!  * **smooth hand-over**: once the pen leaves that circle the touch glides to the pen instead of
//!    jumping (a jump would look like a fling and make scrolling start with a lurch);
//!  * **flicker**: a lift shorter than `lift_debounce` is pressure flicker and is bridged;
//!  * **double-tap snapping**: a quick second tap near the first lands on exactly the same spot,
//!    so toolkits recognise it as a double-tap (word select, with native selection handles).
//!
//! Everything is pure logic driven by explicit timestamps, so it is fully unit-tested.

/// Interval at which the catch-up glide is advanced while the pen is stationary.
pub const STEP: f64 = 1.0 / 120.0;

#[derive(Clone, Copy, Debug)]
pub struct Params {
    pub slop_mm: f64,
    pub down_delay: f64,
    pub lift_debounce: f64,
    pub double_tap_time: f64,
    pub double_tap_mm: f64,
    pub catchup_tau: f64,
}

impl Params {
    pub fn with_slop(slop_mm: f64) -> Params {
        Params { slop_mm, ..Params::default() }
    }
}

impl Default for Params {
    fn default() -> Self {
        Params {
            slop_mm: 2.0,
            down_delay: 0.020,
            lift_debounce: 0.030,
            double_tap_time: 0.45,
            double_tap_mm: 3.0,
            catchup_tau: 0.040,
        }
    }
}

/// Tablet units per millimetre on each axis.
#[derive(Clone, Copy, Debug)]
pub struct Geom {
    pub upm_x: f64,
    pub upm_y: f64,
}

/// What to do with the virtual touch screen. Coordinates are in tablet units.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Out {
    Down(f64, f64),
    Move(f64, f64),
    Up,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum St {
    Idle,
    /// Contact seen, waiting `down_delay` to be sure it is not a blip. Nothing sent yet.
    Arming,
    /// Finger is down.
    Down,
    /// Contact lost; waiting `lift_debounce` to see whether it comes back (flicker).
    Lifting,
}

pub struct TouchFilter {
    p: Params,
    g: Geom,
    st: St,
    t_contact: f64,
    anchor: (f64, f64),
    pen: (f64, f64),
    /// reported position minus pen position while gliding toward the pen
    off: (f64, f64),
    released: bool,
    last_t: f64,
    rep: (f64, f64),
    lift_t: f64,
    /// (time of lift, x, y) of the last touch that never left its tap circle
    last_tap: Option<(f64, f64, f64)>,
}

impl TouchFilter {
    pub fn new(p: Params, g: Geom) -> TouchFilter {
        TouchFilter {
            p,
            g,
            st: St::Idle,
            t_contact: 0.0,
            anchor: (0.0, 0.0),
            pen: (0.0, 0.0),
            off: (0.0, 0.0),
            released: false,
            last_t: 0.0,
            rep: (0.0, 0.0),
            lift_t: 0.0,
            last_tap: None,
        }
    }

    fn mm(&self, dx: f64, dy: f64) -> f64 {
        (dx / self.g.upm_x).hypot(dy / self.g.upm_y)
    }

    /// Feed one pen report. `contact` = tip is touching (and the pen is in range).
    pub fn update(&mut self, now: f64, x: f64, y: f64, contact: bool, out: &mut Vec<Out>) {
        self.expire(now, out);
        self.pen = (x, y);
        match self.st {
            St::Idle => {
                if contact {
                    self.begin(now);
                }
            }
            St::Arming => {
                if !contact {
                    self.st = St::Idle; // a blip: it never happened
                }
            }
            St::Down => {
                if contact {
                    self.follow(now, out);
                } else {
                    self.st = St::Lifting;
                    self.lift_t = now;
                }
            }
            St::Lifting => {
                if contact {
                    self.st = St::Down; // pressure flicker: carry on as if nothing happened
                    self.follow(now, out);
                }
            }
        }
    }

    /// Advance time without a pen report (timers).
    pub fn tick(&mut self, now: f64, out: &mut Vec<Out>) {
        self.expire(now, out);
        if self.st == St::Down {
            self.follow(now, out);
        }
    }

    /// When `tick` next needs to run (absolute time), if at all.
    pub fn next_deadline(&self) -> Option<f64> {
        match self.st {
            St::Arming => Some(self.t_contact + self.p.down_delay),
            St::Lifting => Some(self.lift_t + self.p.lift_debounce),
            St::Down if self.catching_up() => Some(self.last_t + STEP),
            _ => None,
        }
    }

    /// Release everything (switch turned off, tablet unplugged, shutting down).
    pub fn reset(&mut self, out: &mut Vec<Out>) {
        if matches!(self.st, St::Down | St::Lifting) {
            out.push(Out::Up);
        }
        self.st = St::Idle;
        self.released = false;
        self.last_tap = None;
    }

    #[cfg(test)]
    pub fn is_down(&self) -> bool {
        matches!(self.st, St::Down | St::Lifting)
    }

    fn catching_up(&self) -> bool {
        self.released && (self.off.0.abs() + self.off.1.abs()) > 0.5
    }

    fn begin(&mut self, now: f64) {
        self.st = St::Arming;
        self.t_contact = now;
        self.released = false;
        self.off = (0.0, 0.0);
        let mut a = self.pen;
        if let Some((t, x, y)) = self.last_tap {
            if now - t <= self.p.double_tap_time && self.mm(a.0 - x, a.1 - y) <= self.p.double_tap_mm {
                a = (x, y); // second tap of a double tap: land exactly on the first
            }
        }
        self.anchor = a;
        self.rep = a;
        self.last_t = now;
    }

    /// Fire due timers. The comparisons use exactly the same expressions as `next_deadline`, so a
    /// timer that is reported due always fires (no floating-point off-by-one-ulp spin).
    fn expire(&mut self, now: f64, out: &mut Vec<Out>) {
        match self.st {
            St::Arming if now >= self.t_contact + self.p.down_delay => {
                self.st = St::Down;
                self.last_t = now;
                self.rep = self.anchor;
                out.push(Out::Down(self.anchor.0, self.anchor.1));
            }
            St::Lifting if now >= self.lift_t + self.p.lift_debounce => {
                out.push(Out::Up);
                self.last_tap = if self.released { None } else { Some((now, self.anchor.0, self.anchor.1)) };
                self.st = St::Idle;
            }
            _ => {}
        }
    }

    fn follow(&mut self, now: f64, out: &mut Vec<Out>) {
        let dt = (now - self.last_t).clamp(0.0, 0.05);
        self.last_t = now;
        let (px, py) = self.pen;
        let mut newly = false;
        if !self.released && self.mm(px - self.anchor.0, py - self.anchor.1) > self.p.slop_mm {
            self.released = true;
            newly = true;
            self.off = (self.anchor.0 - px, self.anchor.1 - py);
        }
        let pos = if !self.released {
            self.anchor
        } else {
            if !newly {
                let k = (-dt / self.p.catchup_tau).exp();
                self.off.0 *= k;
                self.off.1 *= k;
            }
            (px + self.off.0, py + self.off.1)
        };
        if pos != self.rep {
            self.rep = pos;
            out.push(Out::Move(pos.0, pos.1));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Intuos-S-like: 100 units per mm.
    fn filter() -> TouchFilter {
        TouchFilter::new(Params::default(), Geom { upm_x: 100.0, upm_y: 100.0 })
    }

    /// Helper that drives the filter on a timeline like the real event loop does:
    /// run due timers first, then deliver the pen report.
    struct Rig {
        f: TouchFilter,
        out: Vec<Out>,
        t: f64,
    }
    impl Rig {
        fn new() -> Rig {
            Rig { f: filter(), out: vec![], t: 100.0 }
        }
        fn run_to(&mut self, t: f64) {
            let mut guard = 0;
            while let Some(d) = self.f.next_deadline() {
                if d > t {
                    break;
                }
                guard += 1;
                assert!(guard < 10_000, "timer loop does not converge: d={d} t={t} self.t={} state={:?} off={:?} released={}", self.t, self.f.st, self.f.off, self.f.released);
                self.t = self.t.max(d);
                let now = self.t;
                self.f.tick(now, &mut self.out);
            }
            self.t = t;
            let now = self.t;
            self.f.tick(now, &mut self.out);
        }
        fn pen(&mut self, dt: f64, x: f64, y: f64, contact: bool) {
            self.run_to(self.t + dt);
            let now = self.t;
            self.f.update(now, x, y, contact, &mut self.out);
        }
        fn take(&mut self) -> Vec<Out> {
            std::mem::take(&mut self.out)
        }
    }

    #[test]
    fn tap_is_down_then_up_at_the_touch_point() {
        let mut r = Rig::new();
        r.pen(0.0, 5000.0, 3000.0, true);
        r.pen(0.06, 5000.0, 3000.0, true);
        r.pen(0.02, 5000.0, 3000.0, false);
        r.run_to(r.t + 0.2);
        assert_eq!(r.take(), vec![Out::Down(5000.0, 3000.0), Out::Up]);
        assert!(!r.f.is_down());
    }

    #[test]
    fn a_blip_never_reaches_the_apps() {
        let mut r = Rig::new();
        r.pen(0.0, 5000.0, 3000.0, true);
        r.pen(0.008, 5000.0, 3000.0, false); // 8 ms of contact
        r.run_to(r.t + 0.5);
        assert_eq!(r.take(), vec![]);
    }

    #[test]
    fn hand_tremor_inside_the_tolerance_is_invisible() {
        let mut r = Rig::new();
        r.pen(0.0, 5000.0, 3000.0, true);
        for i in 0..80 {
            // +/- 1.2 mm of jitter around the start point for 0.5 s: a long press
            let j = if i % 2 == 0 { 120.0 } else { -120.0 };
            r.pen(0.006, 5000.0 + j, 3000.0 - j, true);
        }
        r.pen(0.01, 5000.0, 3000.0, false);
        r.run_to(r.t + 0.2);
        assert_eq!(r.take(), vec![Out::Down(5000.0, 3000.0), Out::Up], "no Move at all");
    }

    #[test]
    fn leaving_the_circle_glides_instead_of_jumping_and_ends_on_the_pen() {
        let mut r = Rig::new();
        r.pen(0.0, 5000.0, 8000.0, true);
        r.pen(0.03, 5000.0, 8000.0, true);
        r.take();
        // drag up at 100 mm/s (10000 units/s) for 0.3 s
        let mut y = 8000.0;
        let mut last = 8000.0;
        let mut biggest_step: f64 = 0.0;
        let mut ys = vec![];
        for _ in 0..50 {
            y -= 60.0;
            r.pen(0.006, 5000.0, y, true);
            for o in r.take() {
                if let Out::Move(_, my) = o {
                    biggest_step = biggest_step.max((my - last).abs());
                    last = my;
                    ys.push(my);
                }
            }
        }
        assert!(!ys.is_empty());
        // pen moves 60 units/frame; the glide may add a little extra but never a lurch like the
        // 200-unit (2 mm) circle all at once
        assert!(biggest_step < 120.0, "biggest step {biggest_step}");
        // monotonic: always moving up the screen, never backwards
        assert!(ys.windows(2).all(|w| w[1] <= w[0] + 1e-9));
        // keep the pen still: the touch converges onto it
        r.run_to(r.t + 0.5);
        for o in r.take() {
            if let Out::Move(_, my) = o {
                last = my;
            }
        }
        assert!((last - y).abs() < 1.0, "ended at {last}, pen at {y}");
    }

    #[test]
    fn pressure_flicker_mid_drag_is_bridged() {
        let mut r = Rig::new();
        r.pen(0.0, 5000.0, 8000.0, true);
        r.pen(0.03, 5000.0, 8000.0, true);
        let mut y = 8000.0;
        for _ in 0..10 {
            y -= 80.0;
            r.pen(0.006, 5000.0, y, true);
        }
        r.take();
        r.pen(0.006, 5000.0, y, false); // 12 ms gap
        r.pen(0.006, 5000.0, y, false);
        r.pen(0.006, 5000.0, y - 80.0, true);
        r.pen(0.006, 5000.0, y - 160.0, true);
        let outs = r.take();
        assert!(!outs.contains(&Out::Up) && !outs.iter().any(|o| matches!(o, Out::Down(..))), "{outs:?}");
        assert!(r.f.is_down());
    }

    #[test]
    fn a_real_lift_sends_up_after_the_debounce() {
        let mut r = Rig::new();
        r.pen(0.0, 5000.0, 3000.0, true);
        r.pen(0.05, 5000.0, 3000.0, true);
        r.take();
        r.pen(0.006, 5000.0, 3000.0, false);
        assert!(r.take().is_empty(), "not yet: waiting to see if it is flicker");
        r.run_to(r.t + 0.05);
        assert_eq!(r.take(), vec![Out::Up]);
    }

    #[test]
    fn quick_second_tap_snaps_onto_the_first_for_double_tap() {
        let mut r = Rig::new();
        let tap = |r: &mut Rig, x: f64, y: f64, gap: f64| {
            r.pen(gap, x, y, true);
            r.pen(0.06, x, y, true);
            r.pen(0.01, x, y, false);
        };
        tap(&mut r, 5000.0, 3000.0, 0.0);
        tap(&mut r, 5060.0, 3040.0, 0.10); // 0.7 mm away, 100 ms later
        r.run_to(r.t + 0.2);
        let downs: Vec<Out> = r.take().into_iter().filter(|o| matches!(o, Out::Down(..))).collect();
        assert_eq!(downs, vec![Out::Down(5000.0, 3000.0), Out::Down(5000.0, 3000.0)]);
    }

    #[test]
    fn far_or_slow_second_tap_is_not_snapped() {
        for (dx, gap) in [(600.0, 0.10), (60.0, 0.9)] {
            let mut r = Rig::new();
            let tap = |r: &mut Rig, x: f64, g: f64| {
                r.pen(g, x, 3000.0, true);
                r.pen(0.06, x, 3000.0, true);
                r.pen(0.01, x, 3000.0, false);
            };
            tap(&mut r, 5000.0, 0.0);
            tap(&mut r, 5000.0 + dx, gap);
            r.run_to(r.t + 0.2);
            let downs: Vec<Out> = r.take().into_iter().filter(|o| matches!(o, Out::Down(..))).collect();
            assert_eq!(downs[1], Out::Down(5000.0 + dx, 3000.0), "dx={dx} gap={gap}");
        }
    }

    #[test]
    fn a_drag_is_never_a_double_tap_candidate() {
        let mut r = Rig::new();
        r.pen(0.0, 5000.0, 8000.0, true);
        r.pen(0.03, 5000.0, 8000.0, true);
        for i in 1..=20 {
            r.pen(0.006, 5000.0, 8000.0 - 60.0 * i as f64, true);
        }
        r.pen(0.01, 5000.0, 6800.0, false);
        r.run_to(r.t + 0.1);
        r.take();
        r.pen(0.05, 5000.0, 6800.0, true);
        r.run_to(r.t + 0.05);
        assert_eq!(r.take(), vec![Out::Down(5000.0, 6800.0)]);
    }

    #[test]
    fn reset_releases_a_held_finger_and_is_harmless_when_idle() {
        let mut r = Rig::new();
        let mut out = vec![];
        r.f.reset(&mut out);
        assert!(out.is_empty());
        r.pen(0.0, 5000.0, 3000.0, true);
        r.pen(0.05, 5000.0, 3000.0, true);
        r.take();
        r.f.reset(&mut out);
        assert_eq!(out, vec![Out::Up]);
        assert!(!r.f.is_down());
        assert_eq!(r.f.next_deadline(), None);
    }

    #[test]
    fn deadlines_are_reported_only_when_needed() {
        let mut r = Rig::new();
        assert_eq!(r.f.next_deadline(), None);
        r.pen(0.0, 5000.0, 3000.0, true);
        assert!(r.f.next_deadline().is_some(), "arming timer");
        r.pen(0.05, 5000.0, 3000.0, true);
        assert_eq!(r.f.next_deadline(), None, "finger down and steady: nothing to wait for");
    }

    #[test]
    fn respects_a_larger_tolerance() {
        let mut f = TouchFilter::new(Params::with_slop(4.0), Geom { upm_x: 100.0, upm_y: 100.0 });
        let mut out = vec![];
        f.update(0.0, 1000.0, 1000.0, true, &mut out);
        f.tick(0.03, &mut out);
        f.update(0.04, 1300.0, 1000.0, true, &mut out); // 3 mm: inside 4 mm
        assert_eq!(out, vec![Out::Down(1000.0, 1000.0)]);
    }
}
