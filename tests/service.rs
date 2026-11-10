//! End-to-end tests: run the real `pen-touch` binary as a separate background process,
//! the way the system does, and check the on/off switch behaves across restarts.
//! (There is no tablet in the test environment, so an "on" service reports `waiting`.)

use std::fs;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

struct Env {
    dir: PathBuf,
}

impl Env {
    fn new(name: &str) -> Env {
        let dir = std::env::temp_dir().join(format!("pen-touch-e2e-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        for d in ["home", "config", "data", "run"] {
            fs::create_dir_all(dir.join(d)).unwrap();
        }
        Env { dir }
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_pen-touch"));
        c.args(args)
            .env("HOME", self.dir.join("home"))
            .env("XDG_CONFIG_HOME", self.dir.join("config"))
            .env("XDG_DATA_HOME", self.dir.join("data"))
            .env("XDG_RUNTIME_DIR", self.dir.join("run"))
            .env_remove("APPIMAGE")
            .stdin(Stdio::null());
        c
    }

    fn run(&self, args: &[&str]) -> (i32, String) {
        let out = self.cmd(args).output().unwrap();
        (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).to_string())
    }

    fn status_file(&self) -> PathBuf {
        self.dir.join("run/pen-touch/status.json")
    }
    fn config_file(&self) -> PathBuf {
        self.dir.join("config/pen-touch/config.toml")
    }

    fn state(&self) -> Option<String> {
        let v: serde_json::Value = serde_json::from_slice(&fs::read(self.status_file()).ok()?).ok()?;
        v["state"].as_str().map(str::to_string)
    }
    fn pid(&self) -> Option<i32> {
        let v: serde_json::Value = serde_json::from_slice(&fs::read(self.status_file()).ok()?).ok()?;
        v["pid"].as_i64().map(|p| p as i32)
    }

    fn start_daemon(&self) -> Daemon {
        Daemon { child: Some(self.cmd(&["--daemon"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap()) }
    }

    fn wait_state(&self, accept: &[&str]) -> String {
        let t0 = Instant::now();
        loop {
            if let Some(s) = self.state() {
                if accept.contains(&s.as_str()) {
                    return s;
                }
            }
            assert!(t0.elapsed() < Duration::from_secs(8), "timed out waiting for {accept:?}, state is {:?}", self.state());
            thread::sleep(Duration::from_millis(30));
        }
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        // a service the CLI started on its own is not owned by any guard: stop it
        if let Some(pid) = self.pid() {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}

struct Daemon {
    child: Option<Child>,
}

impl Daemon {
    /// Ask it to stop the way logout / `kill` does, and return its exit code.
    fn terminate(&mut self) -> i32 {
        let mut c = self.child.take().unwrap();
        unsafe { libc::kill(c.id() as i32, libc::SIGTERM) };
        let t0 = Instant::now();
        loop {
            if let Some(st) = c.try_wait().unwrap() {
                return st.code().unwrap_or(-1);
            }
            if t0.elapsed() > Duration::from_secs(5) {
                let _ = c.kill();
                panic!("service did not exit within 5 s of SIGTERM");
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

#[test]
fn switch_state_survives_service_restarts() {
    let env = Env::new("persist");

    // fresh install: off
    let mut d = env.start_daemon();
    assert_eq!(env.wait_state(&["off"]), "off");

    // switch on from the command line (same thing the window's switch does: edit the saved config)
    let (code, out) = env.run(&["on"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("ON"));
    let on_state = env.wait_state(&["waiting", "no_permission", "error", "active"]);
    println!("service state while on (no tablet here): {on_state}");
    assert!(fs::read_to_string(env.config_file()).unwrap().contains("enabled = true"));

    // "reboot": the service is stopped and started again, nobody touches the switch
    assert_eq!(d.terminate(), 0, "clean exit on SIGTERM");
    assert!(!env.status_file().exists(), "status file removed on exit");
    let mut d = env.start_daemon();
    let after_reboot = env.wait_state(&["waiting", "no_permission", "error", "active", "off"]);
    assert_ne!(after_reboot, "off", "the switch stayed ON across the restart");

    // switch off, "reboot" again: it stays off
    assert_eq!(env.run(&["off"]).0, 0);
    env.wait_state(&["off"]);
    assert_eq!(d.terminate(), 0);
    let mut d = env.start_daemon();
    assert_eq!(env.wait_state(&["off"]), "off");
    thread::sleep(Duration::from_millis(500));
    assert_eq!(env.state().as_deref(), Some("off"), "and it stays off");
    d.terminate();
}

#[test]
fn a_second_service_is_refused_without_fuss() {
    let env = Env::new("single");
    let mut first = env.start_daemon();
    env.wait_state(&["off"]);
    let (code, _) = env.run(&["--daemon"]);
    assert_eq!(code, 0, "second copy exits quietly");
    assert_eq!(env.state().as_deref(), Some("off"), "the first service is undisturbed");
    first.terminate();
}

#[test]
fn turning_it_on_starts_the_service_by_itself() {
    // like opening the window after a fresh login: no service is running yet
    let env = Env::new("autostart-service");
    assert!(env.state().is_none());
    let (code, out) = env.run(&["on"]);
    assert_eq!(code, 0, "{out}");
    env.wait_state(&["waiting", "no_permission", "error", "active"]);
    let (_, status) = env.run(&["status"]);
    assert!(status.contains("switch:  on"), "{status}");
    assert!(status.contains("service: running"), "{status}");
    // the service outlives the command that started it
    thread::sleep(Duration::from_millis(400));
    assert!(env.state().is_some());
}

#[test]
fn install_creates_menu_entry_and_login_autostart() {
    let env = Env::new("install");
    let (code, out) = env.run(&["--install"]);
    assert_eq!(code, 0, "{out}");
    let autostart = fs::read_to_string(env.dir.join("config/autostart/pen-touch.desktop")).unwrap();
    assert!(autostart.contains("--daemon") && autostart.contains("pen-touch"), "{autostart}");
    let menu = fs::read_to_string(env.dir.join("data/applications/pen-touch.desktop")).unwrap();
    assert!(menu.contains("Name=Pen Touch") && menu.contains("StartupWMClass=pen-touch"));
    assert!(env.dir.join("data/icons/hicolor/256x256/apps/pen-touch.png").exists());
}

#[test]
fn small_commands_behave() {
    let env = Env::new("cli");
    let (c, out) = env.run(&["--version"]);
    assert!(c == 0 && out.starts_with("pen-touch "));
    let (c, out) = env.run(&["--print-udev-rules"]);
    assert!(c == 0 && out.contains("uaccess") && out.contains("uinput"));
    let (c, _) = env.run(&["--help"]);
    assert_eq!(c, 0);
    let (c, _) = env.run(&["--bogus"]);
    assert_eq!(c, 2, "unknown option is an error");
    let (c, out) = env.run(&["status"]);
    assert_eq!(c, 0);
    assert!(out.contains("switch:  off") && out.contains("service: not running"), "{out}");
}
