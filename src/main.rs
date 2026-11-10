//! Pen Touch - use a pen tablet as a native touch screen.
//!
//!   pen-touch                     open the window
//!   pen-touch --daemon            run the background service (started at login)
//!   pen-touch on | off | toggle   flip the saved switch from a terminal
//!   pen-touch status              show what the service is doing
//!   pen-touch --install           (re)create the menu entry and login autostart
//!   pen-touch --setup-permissions one-time udev rule + uinput setup (asks for admin rights)

mod config;
mod daemon;
mod engine;
mod filter;
mod input;
mod paths;
mod pipeline;
mod setup;
mod status;

#[cfg(feature = "gui")]
mod gui;

use config::Config;
use paths::Paths;
use std::process::exit;

const HELP: &str = "\
Pen Touch - use a pen tablet as a native touch screen

USAGE:
    pen-touch                      open the window
    pen-touch on | off | toggle    flip the saved switch (survives reboots)
    pen-touch status               show what the background service is doing
    pen-touch --daemon             run the background service (normally started at login)
    pen-touch --install            (re)create the menu entry and login autostart
    pen-touch --setup-permissions  one-time setup so your user may create virtual input devices
    pen-touch --print-udev-rules   print the udev rules that setup installs
    pen-touch --version
";

fn set_enabled(paths: &Paths, on: bool) -> i32 {
    let mut cfg = Config::load(paths);
    cfg.enabled = on;
    if let Err(e) = cfg.save(paths) {
        eprintln!("pen-touch: cannot save settings: {e}");
        return 1;
    }
    if let Err(e) = setup::ensure_daemon(paths) {
        eprintln!("pen-touch: saved, but the background service did not start: {e}");
        return 1;
    }
    println!("Pen Touch is now {}.", if on { "ON" } else { "OFF" });
    0
}

fn print_status(paths: &Paths) -> i32 {
    let cfg = Config::load(paths);
    println!("switch:  {}", if cfg.enabled { "on" } else { "off" });
    if status::daemon_alive(paths) {
        match status::Status::read(paths) {
            Some(s) => println!("service: running (pid {}) - {}", s.pid, s.headline()),
            None => println!("service: running"),
        }
    } else {
        println!("service: not running (it starts when you open the app or log in)");
    }
    let p = setup::check_permissions();
    println!("uinput:  {:?}   tablet: {:?}", p.uinput, p.tablet);
    0
}

fn main() {
    let paths = Paths::from_env();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        None | Some("gui") => run_gui(&paths),
        Some("--daemon") | Some("daemon") => daemon::run(&paths),
        Some("on") | Some("enable") => set_enabled(&paths, true),
        Some("off") | Some("disable") => set_enabled(&paths, false),
        Some("toggle") => {
            let now = Config::load(&paths).enabled;
            set_enabled(&paths, !now)
        }
        Some("status") => print_status(&paths),
        Some("--install") => {
            let cfg = Config::load(&paths);
            match setup::sync_integration(&paths, &setup::exe_path(), cfg.start_at_login) {
                Ok(()) => {
                    println!("Menu entry and autostart are set up for {}", setup::exe_path().display());
                    0
                }
                Err(e) => {
                    eprintln!("pen-touch: {e}");
                    1
                }
            }
        }
        Some("--setup-permissions") => {
            let result = if unsafe { libc::geteuid() } == 0 {
                setup::run_setup_as_root()
            } else {
                setup::run_setup_with_pkexec()
            };
            match result {
                Ok(()) => {
                    println!("Permissions installed. No restart needed.");
                    0
                }
                Err(e) => {
                    eprintln!("pen-touch: {e}");
                    1
                }
            }
        }
        Some("--print-udev-rules") => {
            print!("{}", setup::UDEV_RULES);
            0
        }
        Some("--version") | Some("-V") => {
            println!("pen-touch {}", env!("CARGO_PKG_VERSION"));
            0
        }
        Some("--help") | Some("-h") | Some("help") => {
            print!("{HELP}");
            0
        }
        Some(other) => {
            eprintln!("pen-touch: unknown option '{other}'\n\n{HELP}");
            2
        }
    };
    exit(code);
}

#[cfg(feature = "gui")]
fn run_gui(paths: &Paths) -> i32 {
    match gui::run(paths.clone()) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("pen-touch: cannot open the window: {e}");
            eprintln!("(the background service and `pen-touch on|off|status` still work without a window)");
            1
        }
    }
}

#[cfg(not(feature = "gui"))]
fn run_gui(_paths: &Paths) -> i32 {
    eprintln!("pen-touch: this build has no window; use `pen-touch on|off|status`");
    1
}
