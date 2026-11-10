//! The thin layer that talks to the kernel: finds the pen tablet, grabs it, and creates the two
//! virtual devices (a multitouch screen and an absolute cursor). Everything above this is hardware-free.

use crate::engine::Pen;
use crate::pipeline::{Geometry, Raw, Sink, ABS_X, ABS_Y, BTN_TOUCH, OUT_MAX, TOOL_CODES};
use evdev::uinput::VirtualDevice;
use evdev::{
    AbsInfo, AbsoluteAxisCode, AttributeSet, BusType, Device, EventType, InputEvent, InputId, KeyCode, PropType,
    UinputAbsSetup,
};
use std::fs;
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const TOUCH_NAME: &str = "Pen Touchscreen (virtual)";
pub const CURSOR_NAME: &str = "Pen Cursor (virtual)";

// ---------------------------------------------------------------- finding the tablet

#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    pub path: PathBuf,
    pub name: String,
    pub vendor: u16,
}

/// Test one bit of a sysfs capability string such as "3 0 1c03" (most significant word first).
pub fn bit_set(caps: &str, bit: usize) -> bool {
    let words: Vec<&str> = caps.split_whitespace().collect();
    let w = usize::BITS as usize;
    let idx = bit / w;
    if idx >= words.len() {
        return false;
    }
    let tok = words[words.len() - 1 - idx];
    u64::from_str_radix(tok, 16).map(|v| (v >> (bit % w)) & 1 == 1).unwrap_or(false)
}

/// Is this input device a pen tablet (as opposed to a touch screen, pad, mouse, or one of our own virtual devices)?
pub fn looks_like_pen(name: &str, abs: &str, key: &str) -> bool {
    const ABS_MT_POSITION_X: usize = 0x35;
    const BTN_TOOL_PEN: usize = 0x140;
    const BTN_STYLUS: usize = 0x14b;
    !name.contains("(virtual)")
        && bit_set(abs, ABS_X as usize)
        && bit_set(abs, ABS_Y as usize)
        && !bit_set(abs, ABS_MT_POSITION_X)
        && (bit_set(key, BTN_TOOL_PEN) || bit_set(key, BTN_STYLUS))
}

/// Pen tablets present on the system, identified from world-readable sysfs (so we can tell
/// "no tablet" apart from "tablet present but we may not open it"). Wacom first.
pub fn candidates() -> Vec<Candidate> {
    candidates_in(Path::new("/dev/input"), Path::new("/sys/class/input"))
}

pub fn candidates_in(dev_dir: &Path, sys_dir: &Path) -> Vec<Candidate> {
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir(dev_dir) else { return out };
    for entry in rd.flatten() {
        let fname = entry.file_name().to_string_lossy().to_string();
        if !fname.starts_with("event") {
            continue;
        }
        let base = sys_dir.join(&fname).join("device");
        let read = |rel: &str| fs::read_to_string(base.join(rel)).unwrap_or_default().trim().to_string();
        let name = read("name");
        if looks_like_pen(&name, &read("capabilities/abs"), &read("capabilities/key")) {
            let vendor = u16::from_str_radix(&read("id/vendor"), 16).unwrap_or(0);
            out.push(Candidate { path: entry.path(), name, vendor });
        }
    }
    out.sort_by(|a, b| (a.vendor != 0x056a, &a.path).cmp(&(b.vendor != 0x056a, &b.path)));
    out
}

// ---------------------------------------------------------------- the physical pen

pub struct RealPen {
    dev: Device,
    pub name: String,
    pub geom: Geometry,
}

impl RealPen {
    /// Open the tablet and take exclusive ownership of it (so the desktop only sees our virtual devices).
    pub fn open(path: &Path) -> io::Result<RealPen> {
        let mut dev = Device::open(path)?;
        dev.set_nonblocking(true)?;
        let name = dev.name().unwrap_or("pen tablet").to_string();
        let (mut ax, mut ay) = (None, None);
        for (code, info) in dev.get_absinfo()? {
            if code == AbsoluteAxisCode::ABS_X {
                ax = Some(info);
            } else if code == AbsoluteAxisCode::ABS_Y {
                ay = Some(info);
            }
        }
        let (Some(ax), Some(ay)) = (ax, ay) else {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "device has no X/Y axes"));
        };
        let geom = Geometry::new(ax.minimum(), ax.maximum(), ay.minimum(), ay.maximum(), ax.resolution(), ay.resolution());
        dev.grab()?;
        Ok(RealPen { dev, name, geom })
    }

    /// The pen's current state, so a pen that is already hovering or touching at start-up is handled.
    pub fn initial_events(&self) -> Vec<Raw> {
        let mut v = Vec::new();
        if let Ok(abs) = self.dev.get_abs_state() {
            v.push(Raw::Abs(ABS_X, abs[ABS_X as usize].value));
            v.push(Raw::Abs(ABS_Y, abs[ABS_Y as usize].value));
        }
        if let Ok(keys) = self.dev.get_key_state() {
            for &c in TOOL_CODES.iter().chain(std::iter::once(&BTN_TOUCH)) {
                if keys.contains(KeyCode(c)) {
                    v.push(Raw::Key(c, 1));
                }
            }
        }
        v
    }

    /// Give the tablet back to the desktop.
    pub fn release(&mut self) {
        let _ = self.dev.ungrab();
    }
}

impl Pen for RealPen {
    fn wait(&mut self, timeout: Option<Duration>) -> io::Result<Vec<Raw>> {
        let ms = match timeout {
            None => -1,
            Some(d) => {
                let whole = d.as_millis().min(i32::MAX as u128 - 1) as i32;
                whole + (d.subsec_nanos() % 1_000_000 != 0) as i32
            }
        };
        let mut p = libc::pollfd { fd: self.dev.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        let r = unsafe { libc::poll(&mut p, 1, ms) };
        if r < 0 {
            let e = io::Error::last_os_error();
            return if e.kind() == io::ErrorKind::Interrupted { Ok(Vec::new()) } else { Err(e) };
        }
        if p.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return Err(io::Error::from_raw_os_error(libc::ENODEV));
        }
        if r == 0 {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        match self.dev.fetch_events() {
            Ok(events) => {
                for ev in events {
                    match ev.event_type() {
                        EventType::KEY => out.push(Raw::Key(ev.code(), ev.value())),
                        EventType::ABSOLUTE => out.push(Raw::Abs(ev.code(), ev.value())),
                        EventType::SYNCHRONIZATION if ev.code() == 0 => out.push(Raw::Syn),
                        _ => {}
                    }
                }
                Ok(out)
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }
}

// ---------------------------------------------------------------- the virtual devices

fn abs_ev(code: AbsoluteAxisCode, v: i32) -> InputEvent {
    InputEvent::new(EventType::ABSOLUTE.0, code.0, v)
}
fn key_ev(code: KeyCode, v: i32) -> InputEvent {
    InputEvent::new(EventType::KEY.0, code.0, v)
}
fn axis(code: AbsoluteAxisCode, min: i32, max: i32) -> UinputAbsSetup {
    UinputAbsSetup::new(code, AbsInfo::new(0, min, max, 0, 0, 0))
}

/// A multitouch screen with one finger, the pen cursor, or both.
pub struct UinputSink {
    touch: VirtualDevice,
    cursor: Option<VirtualDevice>,
    tracking_id: i32,
    touch_pos: (i32, i32),
    cursor_pos: Option<(i32, i32)>,
}

impl UinputSink {
    pub fn new(with_cursor: bool) -> io::Result<UinputSink> {
        let mut keys = AttributeSet::<KeyCode>::new();
        keys.insert(KeyCode::BTN_TOUCH);
        let mut props = AttributeSet::<PropType>::new();
        props.insert(PropType::DIRECT); // "this is a touch screen", not a touchpad
        let touch = VirtualDevice::builder()?
            .name(TOUCH_NAME)
            .input_id(InputId::new(BusType::BUS_VIRTUAL, 0x1209, 0x5054, 1))
            .with_keys(&keys)?
            .with_properties(&props)?
            .with_absolute_axis(&axis(AbsoluteAxisCode::ABS_X, 0, OUT_MAX))?
            .with_absolute_axis(&axis(AbsoluteAxisCode::ABS_Y, 0, OUT_MAX))?
            .with_absolute_axis(&axis(AbsoluteAxisCode::ABS_MT_SLOT, 0, 4))?
            .with_absolute_axis(&axis(AbsoluteAxisCode::ABS_MT_TRACKING_ID, 0, 65535))?
            .with_absolute_axis(&axis(AbsoluteAxisCode::ABS_MT_POSITION_X, 0, OUT_MAX))?
            .with_absolute_axis(&axis(AbsoluteAxisCode::ABS_MT_POSITION_Y, 0, OUT_MAX))?
            .build()?;

        let cursor = if with_cursor {
            let mut buttons = AttributeSet::<KeyCode>::new();
            for b in [KeyCode::BTN_LEFT, KeyCode::BTN_RIGHT, KeyCode::BTN_MIDDLE] {
                buttons.insert(b);
            }
            Some(
                VirtualDevice::builder()?
                    .name(CURSOR_NAME)
                    .input_id(InputId::new(BusType::BUS_VIRTUAL, 0x1209, 0x5055, 1))
                    .with_keys(&buttons)?
                    .with_absolute_axis(&axis(AbsoluteAxisCode::ABS_X, 0, OUT_MAX))?
                    .with_absolute_axis(&axis(AbsoluteAxisCode::ABS_Y, 0, OUT_MAX))?
                    .build()?,
            )
        } else {
            None
        };
        Ok(UinputSink { touch, cursor, tracking_id: 0, touch_pos: (0, 0), cursor_pos: None })
    }
}

impl Sink for UinputSink {
    fn touch_down(&mut self, x: i32, y: i32) -> io::Result<()> {
        self.tracking_id = (self.tracking_id + 1) & 0xffff;
        self.touch_pos = (x, y);
        self.touch.emit(&[
            abs_ev(AbsoluteAxisCode::ABS_MT_SLOT, 0),
            abs_ev(AbsoluteAxisCode::ABS_MT_TRACKING_ID, self.tracking_id),
            abs_ev(AbsoluteAxisCode::ABS_MT_POSITION_X, x),
            abs_ev(AbsoluteAxisCode::ABS_MT_POSITION_Y, y),
            abs_ev(AbsoluteAxisCode::ABS_X, x),
            abs_ev(AbsoluteAxisCode::ABS_Y, y),
            key_ev(KeyCode::BTN_TOUCH, 1),
        ])
    }

    fn touch_move(&mut self, x: i32, y: i32) -> io::Result<()> {
        if (x, y) == self.touch_pos {
            return Ok(());
        }
        self.touch_pos = (x, y);
        self.touch.emit(&[
            abs_ev(AbsoluteAxisCode::ABS_MT_POSITION_X, x),
            abs_ev(AbsoluteAxisCode::ABS_MT_POSITION_Y, y),
            abs_ev(AbsoluteAxisCode::ABS_X, x),
            abs_ev(AbsoluteAxisCode::ABS_Y, y),
        ])
    }

    fn touch_up(&mut self) -> io::Result<()> {
        self.touch.emit(&[abs_ev(AbsoluteAxisCode::ABS_MT_TRACKING_ID, -1), key_ev(KeyCode::BTN_TOUCH, 0)])
    }

    fn cursor_to(&mut self, x: i32, y: i32) -> io::Result<()> {
        let Some(c) = self.cursor.as_mut() else { return Ok(()) };
        if self.cursor_pos == Some((x, y)) {
            return Ok(());
        }
        self.cursor_pos = Some((x, y));
        c.emit(&[abs_ev(AbsoluteAxisCode::ABS_X, x), abs_ev(AbsoluteAxisCode::ABS_Y, y)])
    }
}

pub fn is_permission_error(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::EACCES) | Some(libc::EPERM)) || e.kind() == io::ErrorKind::PermissionDenied
}

pub fn is_missing_error(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::NotFound
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Format like the kernel's sysfs capability files (most significant word first, no padding).
    fn mask(bits: &[usize]) -> String {
        let w = 64;
        let top = bits.iter().max().unwrap() / w;
        let mut words = vec![0u64; top + 1];
        for b in bits {
            words[b / w] |= 1u64 << (b % w);
        }
        words.iter().rev().map(|x| format!("{x:x}")).collect::<Vec<_>>().join(" ")
    }

    fn intuos_abs() -> String {
        mask(&[0, 1, 24, 25, 26, 27, 40]) // X Y PRESSURE DISTANCE TILT_X TILT_Y MISC
    }
    fn intuos_key() -> String {
        mask(&[0x140, 0x141, 0x14a, 0x14b, 0x14c]) // PEN RUBBER TOUCH STYLUS STYLUS2
    }

    #[test]
    fn bit_set_reads_multiword_capability_strings() {
        let m = mask(&[0, 1, 24, 0x140, 0x14b]);
        assert!(m.contains(' '), "key bits above 63 need several words: {m}");
        for b in [0, 1, 24, 0x140, 0x14b] {
            assert!(bit_set(&m, b), "bit {b} in {m}");
        }
        for b in [2, 25, 0x141, 0x14c, 500] {
            assert!(!bit_set(&m, b), "bit {b} must be clear in {m}");
        }
        assert!(!bit_set("", 0));
        assert!(!bit_set("zz", 0));
    }

    #[test]
    fn recognises_a_wacom_pen_and_rejects_everything_else() {
        assert!(looks_like_pen("Wacom Intuos S Pen", &intuos_abs(), &intuos_key()));
        // the pad has buttons but no X/Y pen axes
        assert!(!looks_like_pen("Wacom Intuos S Pad", &mask(&[40, 0x28]), &mask(&[0x100, 0x101])));
        // a touch screen / touch strip has multitouch axes
        assert!(!looks_like_pen("Wacom Finger", &mask(&[0, 1, 0x35, 0x36]), &mask(&[0x145, 0x14a])));
        // a mouse
        assert!(!looks_like_pen("Logitech Mouse", "", &mask(&[0x110, 0x111])));
        // our own devices are never picked up again
        assert!(!looks_like_pen(TOUCH_NAME, &intuos_abs(), &intuos_key()));
        assert!(!looks_like_pen(CURSOR_NAME, &intuos_abs(), &intuos_key()));
    }

    #[test]
    fn scans_a_fake_sysfs_and_puts_wacom_first() {
        let root = std::env::temp_dir().join(format!("pen-touch-sys-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let dev = root.join("dev");
        let sys = root.join("sys");
        fs::create_dir_all(&dev).unwrap();
        let mk = |ev: &str, name: &str, abs: String, key: String, vendor: &str| {
            fs::write(dev.join(ev), "").unwrap();
            let d = sys.join(ev).join("device");
            fs::create_dir_all(d.join("capabilities")).unwrap();
            fs::create_dir_all(d.join("id")).unwrap();
            fs::write(d.join("name"), format!("{name}\n")).unwrap();
            fs::write(d.join("capabilities/abs"), format!("{abs}\n")).unwrap();
            fs::write(d.join("capabilities/key"), format!("{key}\n")).unwrap();
            fs::write(d.join("id/vendor"), format!("{vendor}\n")).unwrap();
        };
        mk("event3", "Huion Tablet Pen", intuos_abs(), intuos_key(), "256c");
        mk("event7", "Wacom Intuos S Pen", intuos_abs(), intuos_key(), "056a");
        mk("event9", "Wacom Intuos S Pad", mask(&[40]), mask(&[0x100]), "056a");
        mk("event4", "AT Keyboard", String::new(), mask(&[30, 31]), "0001");
        fs::write(dev.join("mouse0"), "").unwrap(); // not an event node
        let found = candidates_in(&dev, &sys);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].name, "Wacom Intuos S Pen");
        assert_eq!(found[0].vendor, 0x056a);
        assert_eq!(found[1].name, "Huion Tablet Pen");
        assert!(candidates_in(&root.join("nope"), &sys).is_empty());
    }

    #[test]
    fn permission_and_missing_errors_are_classified() {
        assert!(is_permission_error(&io::Error::from_raw_os_error(libc::EACCES)));
        assert!(is_permission_error(&io::Error::from(io::ErrorKind::PermissionDenied)));
        assert!(is_missing_error(&io::Error::from_raw_os_error(libc::ENOENT)));
        assert!(!is_permission_error(&io::Error::from_raw_os_error(libc::ENOENT)));
    }
}
