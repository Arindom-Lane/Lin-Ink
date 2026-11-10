//! Raw pen events in, touch + cursor commands out. No hardware in here: the output side is
//! the `Sink` trait, so the whole chain is testable with a fake.

use crate::filter::{Geom, Out, Params, TouchFilter};
use std::io;

// Linux input ABI numbers (linux/input-event-codes.h).
pub const ABS_X: u16 = 0x00;
pub const ABS_Y: u16 = 0x01;
pub const BTN_TOUCH: u16 = 0x14a;
/// BTN_TOOL_PEN, RUBBER, BRUSH, PENCIL, AIRBRUSH: "some pen tool is in range".
pub const TOOL_CODES: [u16; 5] = [0x140, 0x141, 0x142, 0x143, 0x144];
/// Range of the virtual touch screen and cursor axes.
pub const OUT_MAX: i32 = 32767;

/// One raw event from the physical pen, reduced to what we use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Raw {
    Abs(u16, i32),
    Key(u16, i32),
    /// SYN_REPORT: the events so far form one complete report.
    Syn,
}

/// The virtual devices.
pub trait Sink {
    fn touch_down(&mut self, x: i32, y: i32) -> io::Result<()>;
    fn touch_move(&mut self, x: i32, y: i32) -> io::Result<()>;
    fn touch_up(&mut self) -> io::Result<()>;
    fn cursor_to(&mut self, x: i32, y: i32) -> io::Result<()>;
}

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub tolerance_mm: f64,
    pub rotation: u16,
    pub show_cursor: bool,
    pub cursor_area: [f64; 4],
}

impl Settings {
    pub fn from_config(c: &crate::config::Config) -> Settings {
        Settings {
            tolerance_mm: c.tap_tolerance_mm,
            rotation: c.rotation,
            show_cursor: c.show_cursor,
            cursor_area: c.cursor_area,
        }
    }
}

/// Size and resolution of the physical tablet.
#[derive(Clone, Copy, Debug)]
pub struct Geometry {
    pub x_min: i32,
    pub x_max: i32,
    pub y_min: i32,
    pub y_max: i32,
    pub upm_x: f64,
    pub upm_y: f64,
}

impl Geometry {
    /// `res_*` is the kernel's units-per-mm (0 = unknown; then assume a 150 x 100 mm active area).
    pub fn new(x_min: i32, x_max: i32, y_min: i32, y_max: i32, res_x: i32, res_y: i32) -> Geometry {
        let xr = (x_max - x_min).max(1) as f64;
        let yr = (y_max - y_min).max(1) as f64;
        Geometry {
            x_min,
            x_max,
            y_min,
            y_max,
            upm_x: if res_x > 0 { res_x as f64 } else { xr / 150.0 },
            upm_y: if res_y > 0 { res_y as f64 } else { yr / 100.0 },
        }
    }
}

/// Rotate a point in the unit square: "the tablet is turned `rot` degrees clockwise".
pub fn rotate(fx: f64, fy: f64, rot: u16) -> (f64, f64) {
    match rot {
        90 => (1.0 - fy, fx),
        180 => (1.0 - fx, 1.0 - fy),
        270 => (fy, 1.0 - fx),
        _ => (fx, fy),
    }
}

pub struct Pipeline<S: Sink> {
    sink: S,
    filter: TouchFilter,
    geom: Geometry,
    settings: Settings,
    x: i32,
    y: i32,
    touch: bool,
    tool: Option<u16>,
    outs: Vec<Out>,
}

impl<S: Sink> Pipeline<S> {
    pub fn new(sink: S, geom: Geometry, settings: Settings) -> Pipeline<S> {
        let filter = TouchFilter::new(
            Params::with_slop(settings.tolerance_mm),
            Geom { upm_x: geom.upm_x, upm_y: geom.upm_y },
        );
        Pipeline { sink, filter, geom, settings, x: geom.x_min, y: geom.y_min, touch: false, tool: None, outs: Vec::new() }
    }

    #[cfg(test)]
    pub fn sink(&self) -> &S {
        &self.sink
    }

    /// Normalised (0..1, after rotation) position for a tablet point.
    fn unit(&self, x: f64, y: f64) -> (f64, f64) {
        let xr = (self.geom.x_max - self.geom.x_min).max(1) as f64;
        let yr = (self.geom.y_max - self.geom.y_min).max(1) as f64;
        let fx = ((x - self.geom.x_min as f64) / xr).clamp(0.0, 1.0);
        let fy = ((y - self.geom.y_min as f64) / yr).clamp(0.0, 1.0);
        rotate(fx, fy, self.settings.rotation)
    }

    fn touch_xy(&self, x: f64, y: f64) -> (i32, i32) {
        let (fx, fy) = self.unit(x, y);
        ((fx * OUT_MAX as f64).round() as i32, (fy * OUT_MAX as f64).round() as i32)
    }

    fn cursor_xy(&self, x: f64, y: f64) -> (i32, i32) {
        let (fx, fy) = self.unit(x, y);
        let [x0, y0, x1, y1] = self.settings.cursor_area;
        let cx = x0 + fx * (x1 - x0);
        let cy = y0 + fy * (y1 - y0);
        ((cx * OUT_MAX as f64).round() as i32, (cy * OUT_MAX as f64).round() as i32)
    }

    pub fn feed(&mut self, ev: Raw, now: f64) -> io::Result<()> {
        match ev {
            Raw::Abs(ABS_X, v) => self.x = v,
            Raw::Abs(ABS_Y, v) => self.y = v,
            Raw::Key(BTN_TOUCH, v) => self.touch = v != 0,
            Raw::Key(c, v) if TOOL_CODES.contains(&c) => {
                if v != 0 {
                    self.tool = Some(c);
                } else if self.tool == Some(c) {
                    self.tool = None;
                }
            }
            Raw::Syn => self.frame(now)?,
            _ => {}
        }
        Ok(())
    }

    fn frame(&mut self, now: f64) -> io::Result<()> {
        let contact = self.touch && self.tool.is_some();
        let (x, y) = (self.x as f64, self.y as f64);
        self.filter.update(now, x, y, contact, &mut self.outs);
        self.apply()?;
        if self.settings.show_cursor && self.tool.is_some() {
            let (cx, cy) = self.cursor_xy(x, y);
            self.sink.cursor_to(cx, cy)?;
        }
        Ok(())
    }

    pub fn tick(&mut self, now: f64) -> io::Result<()> {
        self.filter.tick(now, &mut self.outs);
        self.apply()
    }

    pub fn next_deadline(&self) -> Option<f64> {
        self.filter.next_deadline()
    }

    /// Lift any finger that is still down.
    pub fn shutdown(&mut self) -> io::Result<()> {
        self.filter.reset(&mut self.outs);
        self.apply()
    }

    fn apply(&mut self) -> io::Result<()> {
        let outs = std::mem::take(&mut self.outs);
        let mut result = Ok(());
        for o in &outs {
            let r = match *o {
                Out::Down(x, y) => {
                    let (ix, iy) = self.touch_xy(x, y);
                    self.sink.touch_down(ix, iy)
                }
                Out::Move(x, y) => {
                    let (ix, iy) = self.touch_xy(x, y);
                    self.sink.touch_move(ix, iy)
                }
                Out::Up => self.sink.touch_up(),
            };
            if r.is_err() && result.is_ok() {
                result = r;
            }
        }
        self.outs = outs;
        self.outs.clear();
        result
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Clone, Copy)]
    pub enum Cmd {
        Down(i32, i32),
        Move(i32, i32),
        Up,
        Cursor(i32, i32),
    }

    #[derive(Default)]
    pub struct FakeSink {
        pub cmds: Vec<Cmd>,
    }
    impl Sink for FakeSink {
        fn touch_down(&mut self, x: i32, y: i32) -> io::Result<()> {
            self.cmds.push(Cmd::Down(x, y));
            Ok(())
        }
        fn touch_move(&mut self, x: i32, y: i32) -> io::Result<()> {
            self.cmds.push(Cmd::Move(x, y));
            Ok(())
        }
        fn touch_up(&mut self) -> io::Result<()> {
            self.cmds.push(Cmd::Up);
            Ok(())
        }
        fn cursor_to(&mut self, x: i32, y: i32) -> io::Result<()> {
            self.cmds.push(Cmd::Cursor(x, y));
            Ok(())
        }
    }

    pub fn geom() -> Geometry {
        Geometry::new(0, 15200, 0, 9500, 100, 100)
    }
    pub fn settings() -> Settings {
        Settings { tolerance_mm: 2.0, rotation: 0, show_cursor: true, cursor_area: [0.0, 0.0, 1.0, 1.0] }
    }
    pub const PEN: u16 = 0x140;
    pub const RUBBER: u16 = 0x141;

    pub fn report(p: &mut Pipeline<FakeSink>, now: f64, evs: &[Raw]) {
        for e in evs {
            p.feed(*e, now).unwrap();
        }
        p.feed(Raw::Syn, now).unwrap();
    }

    #[test]
    fn rotation_maps_corners_as_documented() {
        // tablet top-left (0,0) -> screen corner for each rotation
        assert_eq!(rotate(0.0, 0.0, 0), (0.0, 0.0));
        assert_eq!(rotate(0.0, 0.0, 90), (1.0, 0.0));
        assert_eq!(rotate(0.0, 0.0, 180), (1.0, 1.0));
        assert_eq!(rotate(0.0, 0.0, 270), (0.0, 1.0));
        // a 90-degree turn then a 270 turn is the identity
        let (a, b) = rotate(0.3, 0.8, 90);
        let (c, d) = rotate(a, b, 270);
        assert!((c - 0.3).abs() < 1e-9 && (d - 0.8).abs() < 1e-9);
    }

    #[test]
    fn tap_becomes_touch_down_and_up_at_the_scaled_position() {
        let mut p = Pipeline::new(FakeSink::default(), geom(), settings());
        report(&mut p, 0.0, &[Raw::Key(PEN, 1), Raw::Abs(ABS_X, 7600), Raw::Abs(ABS_Y, 4750)]);
        report(&mut p, 0.01, &[Raw::Key(BTN_TOUCH, 1)]);
        p.tick(0.05).unwrap();
        report(&mut p, 0.08, &[Raw::Key(BTN_TOUCH, 0)]);
        p.tick(0.2).unwrap();
        let touches: Vec<Cmd> = p.sink().cmds.iter().copied().filter(|c| !matches!(c, Cmd::Cursor(..))).collect();
        assert_eq!(touches, vec![Cmd::Down(16384, 16384), Cmd::Up]);
    }

    #[test]
    fn cursor_follows_the_pen_while_hovering_and_while_touching() {
        let mut p = Pipeline::new(FakeSink::default(), geom(), settings());
        report(&mut p, 0.0, &[Raw::Key(PEN, 1), Raw::Abs(ABS_X, 0), Raw::Abs(ABS_Y, 0)]);
        report(&mut p, 0.01, &[Raw::Abs(ABS_X, 15200), Raw::Abs(ABS_Y, 9500)]);
        report(&mut p, 0.02, &[Raw::Key(BTN_TOUCH, 1)]);
        for i in 0..10 {
            report(&mut p, 0.03 + i as f64 * 0.006, &[Raw::Abs(ABS_Y, 9500 - i * 200)]);
        }
        let cur: Vec<(i32, i32)> = p.sink().cmds.iter().filter_map(|c| if let Cmd::Cursor(x, y) = c { Some((*x, *y)) } else { None }).collect();
        assert_eq!(cur[0], (0, 0));
        assert_eq!(cur[1], (OUT_MAX, OUT_MAX));
        assert!(cur.len() >= 12, "one cursor update per report, including during contact");
        assert!(cur.last().unwrap().1 < OUT_MAX, "cursor kept moving while the pen was down");
    }

    #[test]
    fn cursor_can_be_switched_off_and_confined_to_an_area() {
        let mut s = settings();
        s.show_cursor = false;
        let mut p = Pipeline::new(FakeSink::default(), geom(), s);
        report(&mut p, 0.0, &[Raw::Key(PEN, 1), Raw::Abs(ABS_X, 100), Raw::Abs(ABS_Y, 100)]);
        assert!(p.sink().cmds.iter().all(|c| !matches!(c, Cmd::Cursor(..))));

        let mut s = settings();
        s.cursor_area = [0.5, 0.0, 1.0, 1.0]; // right half of the desktop
        let mut p = Pipeline::new(FakeSink::default(), geom(), s);
        report(&mut p, 0.0, &[Raw::Key(PEN, 1), Raw::Abs(ABS_X, 0), Raw::Abs(ABS_Y, 0)]);
        report(&mut p, 0.01, &[Raw::Abs(ABS_X, 15200)]);
        let cur: Vec<(i32, i32)> = p.sink().cmds.iter().filter_map(|c| if let Cmd::Cursor(x, y) = c { Some((*x, *y)) } else { None }).collect();
        assert_eq!(cur[0].0, (0.5 * OUT_MAX as f64).round() as i32);
        assert_eq!(cur[1].0, OUT_MAX);
    }

    #[test]
    fn no_cursor_and_no_touch_while_the_pen_is_out_of_range() {
        let mut p = Pipeline::new(FakeSink::default(), geom(), settings());
        // contact flag without a tool in range must not create a touch (stale state after unplug etc.)
        report(&mut p, 0.0, &[Raw::Key(BTN_TOUCH, 1), Raw::Abs(ABS_X, 5), Raw::Abs(ABS_Y, 5)]);
        p.tick(0.1).unwrap();
        assert!(p.sink().cmds.is_empty());
    }

    #[test]
    fn eraser_end_acts_as_a_finger_too_and_pen_leaving_range_lifts_it() {
        let mut p = Pipeline::new(FakeSink::default(), geom(), settings());
        report(&mut p, 0.0, &[Raw::Key(RUBBER, 1), Raw::Abs(ABS_X, 1000), Raw::Abs(ABS_Y, 1000), Raw::Key(BTN_TOUCH, 1)]);
        p.tick(0.05).unwrap();
        assert!(p.sink().cmds.iter().any(|c| matches!(c, Cmd::Down(..))));
        report(&mut p, 0.1, &[Raw::Key(BTN_TOUCH, 0), Raw::Key(RUBBER, 0)]);
        p.tick(0.3).unwrap();
        assert!(p.sink().cmds.iter().any(|c| matches!(c, Cmd::Up)));
    }

    #[test]
    fn rotation_applies_to_touch_and_cursor() {
        let mut s = settings();
        s.rotation = 180;
        let mut p = Pipeline::new(FakeSink::default(), geom(), s);
        report(&mut p, 0.0, &[Raw::Key(PEN, 1), Raw::Abs(ABS_X, 0), Raw::Abs(ABS_Y, 0), Raw::Key(BTN_TOUCH, 1)]);
        p.tick(0.05).unwrap();
        assert!(p.sink().cmds.contains(&Cmd::Cursor(OUT_MAX, OUT_MAX)));
        assert!(p.sink().cmds.contains(&Cmd::Down(OUT_MAX, OUT_MAX)));
    }

    #[test]
    fn shutdown_lifts_a_finger_that_is_down() {
        let mut p = Pipeline::new(FakeSink::default(), geom(), settings());
        report(&mut p, 0.0, &[Raw::Key(PEN, 1), Raw::Abs(ABS_X, 500), Raw::Abs(ABS_Y, 500), Raw::Key(BTN_TOUCH, 1)]);
        p.tick(0.05).unwrap();
        p.shutdown().unwrap();
        assert_eq!(p.sink().cmds.last(), Some(&Cmd::Up));
        p.shutdown().unwrap();
        assert_eq!(p.sink().cmds.iter().filter(|c| matches!(c, Cmd::Up)).count(), 1, "idempotent");
    }

    #[test]
    fn unknown_resolution_falls_back_to_a_sensible_size() {
        let g = Geometry::new(0, 15000, 0, 10000, 0, 0);
        assert_eq!((g.upm_x, g.upm_y), (100.0, 100.0));
    }
}
