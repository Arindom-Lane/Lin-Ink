//! Persistent settings. The `enabled` flag is what makes the switch survive restarts:
//! it lives in a file, and the background service obeys it every time it starts.

use crate::paths::Paths;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Write};
use std::time::SystemTime;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// The big switch. Stays as you left it across logouts and reboots.
    pub enabled: bool,
    /// Start the background service automatically when you log in.
    pub start_at_login: bool,
    /// Show a mouse cursor that follows the pen (also while scrolling).
    pub show_cursor: bool,
    /// How far (mm) the pen may wobble during a tap / long-press before it counts as a drag.
    pub tap_tolerance_mm: f64,
    /// 0, 90, 180 or 270: how the tablet is turned relative to the screen.
    pub rotation: u16,
    /// Part of the whole desktop the cursor covers: [x0, y0, x1, y1] as 0..1 fractions.
    pub cursor_area: [f64; 4],
}

impl Default for Config {
    fn default() -> Self {
        Config {
            enabled: false,
            start_at_login: true,
            show_cursor: true,
            tap_tolerance_mm: 2.0,
            rotation: 0,
            cursor_area: [0.0, 0.0, 1.0, 1.0],
        }
    }
}

/// Cheap fingerprint of the config file, to notice edits made by someone else (CLI, text editor, other window).
pub fn stamp(paths: &Paths) -> Option<(SystemTime, u64)> {
    let m = fs::metadata(&paths.config_file).ok()?;
    Some((m.modified().ok()?, m.len()))
}

impl Config {
    /// Read the config; a missing or unreadable file gives the defaults.
    /// A corrupt file is moved aside (config.toml.bad) rather than silently overwritten.
    pub fn load(paths: &Paths) -> Config {
        let text = match fs::read_to_string(&paths.config_file) {
            Ok(t) => t,
            Err(_) => return Config::default(),
        };
        match toml::from_str::<Config>(&text) {
            Ok(mut c) => {
                c.sanitize();
                c
            }
            Err(_) => {
                let _ = fs::rename(&paths.config_file, paths.config_file.with_extension("toml.bad"));
                Config::default()
            }
        }
    }

    /// Atomic save: write a temp file, then rename over the real one, so the service never sees half a file.
    pub fn save(&self, paths: &Paths) -> io::Result<()> {
        let mut c = self.clone();
        c.sanitize();
        let text = toml::to_string_pretty(&c).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        if let Some(dir) = paths.config_file.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = paths.config_file.with_extension("toml.tmp");
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &paths.config_file)
    }

    /// Clamp everything to sane values (also protects against hand-edited files).
    pub fn sanitize(&mut self) {
        if !matches!(self.rotation, 0 | 90 | 180 | 270) {
            self.rotation = 0;
        }
        if !self.tap_tolerance_mm.is_finite() {
            self.tap_tolerance_mm = 2.0;
        }
        self.tap_tolerance_mm = self.tap_tolerance_mm.clamp(0.5, 6.0);
        let [x0, y0, x1, y1] = self.cursor_area;
        let ok = [x0, y0, x1, y1].iter().all(|v| v.is_finite() && (0.0..=1.0).contains(v)) && x0 < x1 && y0 < y1;
        if !ok {
            self.cursor_area = [0.0, 0.0, 1.0, 1.0];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> Paths {
        let d = std::env::temp_dir().join(format!("pen-touch-test-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&d);
        Paths::in_dir(&d)
    }

    #[test]
    fn default_is_off_with_sane_values() {
        let c = Config::default();
        assert!(!c.enabled && c.start_at_login && c.show_cursor);
        assert_eq!(c.rotation, 0);
    }

    #[test]
    fn save_and_load_roundtrip_keeps_the_switch() {
        let p = tmp("roundtrip");
        let mut c = Config::default();
        c.enabled = true;
        c.rotation = 180;
        c.tap_tolerance_mm = 3.5;
        c.show_cursor = false;
        c.save(&p).unwrap();
        // "restart": a brand-new load sees exactly what was saved
        assert_eq!(Config::load(&p), c);
        c.enabled = false;
        c.save(&p).unwrap();
        assert!(!Config::load(&p).enabled);
    }

    #[test]
    fn missing_file_gives_defaults_and_partial_file_fills_in_the_rest() {
        let p = tmp("partial");
        assert_eq!(Config::load(&p), Config::default());
        fs::create_dir_all(p.config_file.parent().unwrap()).unwrap();
        fs::write(&p.config_file, "enabled = true\n").unwrap();
        let c = Config::load(&p);
        assert!(c.enabled && c.show_cursor && c.tap_tolerance_mm == 2.0);
    }

    #[test]
    fn hand_edited_nonsense_is_clamped() {
        let p = tmp("nonsense");
        fs::create_dir_all(p.config_file.parent().unwrap()).unwrap();
        fs::write(
            &p.config_file,
            "rotation = 45\ntap_tolerance_mm = 99.0\ncursor_area = [0.9, 0.0, 0.1, 1.0]\n",
        )
        .unwrap();
        let c = Config::load(&p);
        assert_eq!(c.rotation, 0);
        assert_eq!(c.tap_tolerance_mm, 6.0);
        assert_eq!(c.cursor_area, [0.0, 0.0, 1.0, 1.0]);
    }

    #[test]
    fn corrupt_file_is_moved_aside_not_destroyed() {
        let p = tmp("corrupt");
        fs::create_dir_all(p.config_file.parent().unwrap()).unwrap();
        fs::write(&p.config_file, "this is [not toml").unwrap();
        assert_eq!(Config::load(&p), Config::default());
        assert!(p.config_file.with_extension("toml.bad").exists());
    }
}
