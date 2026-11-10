//! The background service. It starts at login, reads the saved on/off state, and keeps the
//! engine running (or stopped) to match - so the switch survives logouts and reboots.
//! The window is just a remote control for this process.

use crate::config::{self, Config};
use crate::engine::{run_engine, SharedStatus};
use crate::paths::Paths;
use crate::pipeline::Settings;
use crate::status::{self, State, Status};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

fn install_signal_handlers() {
    let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
    unsafe {
        libc::signal(libc::SIGTERM, handler);
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGHUP, handler);
    }
}

struct Running {
    handle: JoinHandle<()>,
    stop: Arc<AtomicBool>,
    settings: Settings,
}

/// Makes the engine thread match what the config asks for.
pub struct Supervisor {
    running: Option<Running>,
    status: SharedStatus,
    retry_at: Option<Instant>,
}

impl Supervisor {
    pub fn new(status: SharedStatus) -> Supervisor {
        Supervisor { running: None, status, retry_at: None }
    }

    fn set_status(&self, state: State, message: &str) {
        let mut s = self.status.lock().unwrap_or_else(|e| e.into_inner());
        s.state = state;
        s.device.clear();
        s.message = message.to_string();
    }

    fn start(&mut self, settings: Settings) {
        self.set_status(State::Waiting, "");
        let stop = Arc::new(AtomicBool::new(false));
        let (s2, st2, cfg) = (stop.clone(), self.status.clone(), settings.clone());
        let handle = thread::spawn(move || run_engine(cfg, s2, st2));
        self.running = Some(Running { handle, stop, settings });
    }

    /// Stop the engine and wait until it has released the tablet and removed the virtual devices.
    pub fn stop(&mut self) {
        if let Some(r) = self.running.take() {
            r.stop.store(true, Ordering::Relaxed);
            let _ = r.handle.join();
        }
        self.set_status(State::Off, "");
    }

    /// `Some(settings)` = should be running with these settings; `None` = should be off.
    pub fn apply(&mut self, want: Option<Settings>) {
        // An engine thread that died on its own (a bug): report it and restart after a pause.
        if self.running.as_ref().is_some_and(|r| r.handle.is_finished()) {
            if let Some(r) = self.running.take() {
                let _ = r.handle.join();
            }
            self.set_status(State::Error, "The engine stopped unexpectedly; restarting");
            self.retry_at = Some(Instant::now() + Duration::from_secs(2));
        }
        match (self.running.as_ref().map(|r| &r.settings), want) {
            (Some(cur), Some(w)) if *cur == w => {}
            (Some(_), Some(w)) => {
                self.stop(); // settings changed: restart with the new ones
                self.start(w);
            }
            (Some(_), None) => self.stop(),
            (None, Some(w)) => {
                if self.retry_at.is_none_or(|t| Instant::now() >= t) {
                    self.retry_at = None;
                    self.start(w);
                }
            }
            (None, None) => {
                self.retry_at = None;
                let cur = self.status.lock().unwrap_or_else(|e| e.into_inner()).state;
                if cur != State::Off {
                    self.set_status(State::Off, "");
                }
            }
        }
    }
}

/// Run the service until SIGTERM/SIGINT. Returns the process exit code.
pub fn run(paths: &Paths) -> i32 {
    let Some(_lock) = status::lock_daemon(paths) else {
        eprintln!("pen-touch: the background service is already running");
        return 0;
    };
    install_signal_handlers();

    let shared: SharedStatus = Arc::new(Mutex::new(Status::new(State::Off)));
    let mut sup = Supervisor::new(shared.clone());
    let mut cfg = Config::load(paths);
    let mut cfg_stamp = config::stamp(paths);
    let mut last_written: Option<Status> = None;
    let mut last_write = Instant::now();
    let mut first = true;

    while !STOP.load(Ordering::Relaxed) {
        let now_stamp = config::stamp(paths);
        if now_stamp != cfg_stamp {
            cfg_stamp = now_stamp;
            cfg = Config::load(paths);
        }
        sup.apply(cfg.enabled.then(|| Settings::from_config(&cfg)));

        let snapshot = shared.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if first || last_written.as_ref() != Some(&snapshot) || last_write.elapsed() > Duration::from_secs(2) {
            let _ = snapshot.write(paths);
            last_written = Some(snapshot);
            last_write = Instant::now();
            first = false;
        }
        thread::sleep(Duration::from_millis(150));
    }

    sup.stop();
    Status::remove(paths);
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::tests::settings;

    fn shared() -> SharedStatus {
        Arc::new(Mutex::new(Status::new(State::Off)))
    }
    fn state(s: &SharedStatus) -> State {
        s.lock().unwrap().state
    }

    // No tablet in the test environment, so a started engine simply reports "waiting".
    #[test]
    fn switching_on_and_off_starts_and_stops_the_engine() {
        let st = shared();
        let mut sup = Supervisor::new(st.clone());
        sup.apply(None);
        assert_eq!(state(&st), State::Off);

        sup.apply(Some(settings()));
        assert!(sup.running.is_some());
        let t0 = Instant::now();
        while state(&st) == State::Off && t0.elapsed() < Duration::from_secs(3) {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(matches!(state(&st), State::Waiting | State::NoPermission | State::Active | State::Error));

        sup.apply(Some(settings())); // same settings: no restart
        sup.apply(None);
        assert!(sup.running.is_none());
        assert_eq!(state(&st), State::Off);
    }

    #[test]
    fn changed_settings_restart_the_engine() {
        let st = shared();
        let mut sup = Supervisor::new(st);
        sup.apply(Some(settings()));
        let mut s2 = settings();
        s2.rotation = 90;
        sup.apply(Some(s2.clone()));
        assert_eq!(sup.running.as_ref().unwrap().settings, s2);
        sup.stop();
    }

    #[test]
    fn stopping_is_prompt() {
        let st = shared();
        let mut sup = Supervisor::new(st);
        sup.apply(Some(settings()));
        thread::sleep(Duration::from_millis(100));
        let t0 = Instant::now();
        sup.stop();
        assert!(t0.elapsed() < Duration::from_millis(600), "took {:?}", t0.elapsed());
    }
}
