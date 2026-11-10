//! What the background service is doing, shared with the window through a small JSON file,
//! plus the lock that guarantees only one service runs at a time.

use crate::paths::Paths;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Switched off (tablet behaves normally).
    Off,
    /// Switched on, but no pen tablet found yet.
    Waiting,
    /// Switched on and translating pen input into touch.
    Active,
    /// Switched on, but this user may not read the tablet or create virtual devices.
    NoPermission,
    /// Something else went wrong; see `message`.
    Error,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub pid: u32,
    pub state: State,
    pub device: String,
    pub message: String,
    pub version: String,
}

impl Status {
    pub fn new(state: State) -> Status {
        Status {
            pid: std::process::id(),
            state,
            device: String::new(),
            message: String::new(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    pub fn write(&self, paths: &Paths) -> io::Result<()> {
        if let Some(dir) = paths.status_file.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = paths.status_file.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec(self).map_err(io::Error::other)?)?;
        fs::rename(&tmp, &paths.status_file)
    }

    pub fn read(paths: &Paths) -> Option<Status> {
        let bytes = fs::read(&paths.status_file).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    pub fn remove(paths: &Paths) {
        let _ = fs::remove_file(&paths.status_file);
    }

    /// Human-readable one-liner for the window and `pen-touch status`.
    pub fn headline(&self) -> String {
        match self.state {
            State::Off => "Off".to_string(),
            State::Waiting => "On - waiting for a pen tablet...".to_string(),
            State::Active => format!("On - {}", self.device),
            State::NoPermission => "On - needs permission".to_string(),
            State::Error => format!("Problem: {}", self.message),
        }
    }
}

fn open_lock(paths: &Paths) -> io::Result<File> {
    if let Some(dir) = paths.lock_file.parent() {
        fs::create_dir_all(dir)?;
    }
    OpenOptions::new().create(true).truncate(false).read(true).write(true).open(&paths.lock_file)
}

fn try_flock(f: &File) -> bool {
    unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

/// Take the single-instance lock. `None` means another service already holds it.
/// Keep the returned file alive for as long as the service runs.
pub fn lock_daemon(paths: &Paths) -> Option<File> {
    let f = open_lock(paths).ok()?;
    if try_flock(&f) {
        Some(f)
    } else {
        None
    }
}

/// Is a service running right now? (Probes the lock; no stale-pid guessing.)
pub fn daemon_alive(paths: &Paths) -> bool {
    match open_lock(paths) {
        Ok(f) => {
            if try_flock(&f) {
                unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_UN) };
                false
            } else {
                true
            }
        }
        Err(_) => false,
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
    fn status_roundtrip() {
        let p = tmp("status");
        assert!(Status::read(&p).is_none());
        let mut s = Status::new(State::Active);
        s.device = "Wacom Intuos S Pen".into();
        s.write(&p).unwrap();
        assert_eq!(Status::read(&p).unwrap(), s);
        assert_eq!(s.headline(), "On - Wacom Intuos S Pen");
        Status::remove(&p);
        assert!(Status::read(&p).is_none());
    }

    #[test]
    fn only_one_service_can_hold_the_lock() {
        let p = tmp("lock");
        assert!(!daemon_alive(&p));
        let first = lock_daemon(&p).expect("first service gets the lock");
        assert!(daemon_alive(&p), "window can see the service is running");
        assert!(lock_daemon(&p).is_none(), "a second service is refused");
        drop(first);
        assert!(!daemon_alive(&p), "lock is released when the service exits");
        assert!(lock_daemon(&p).is_some());
    }
}
