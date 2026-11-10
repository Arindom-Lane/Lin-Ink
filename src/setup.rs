//! Everything that makes the app "stay set up": starting the service, starting it at login,
//! the application-menu entry, and the one-time permission setup.

use crate::input;
use crate::paths::Paths;
use crate::status;
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// The icon, embedded so the AppImage is one self-contained file.
pub const ICON_PNG: &[u8] = include_bytes!("../packaging/pen-touch-256.png");

/// Path to run to start ourselves again. Inside an AppImage that is the .AppImage file itself
/// (the mount point inside disappears when the window closes).
pub fn exe_path() -> PathBuf {
    if let Some(a) = env::var_os("APPIMAGE").filter(|v| !v.is_empty()) {
        let p = PathBuf::from(a);
        if p.exists() {
            return p;
        }
    }
    env::current_exe().unwrap_or_else(|_| PathBuf::from("pen-touch"))
}

// ---------------------------------------------------------------- the service

/// Start the background service in its own session so it outlives the window.
pub fn spawn_daemon(exe: &Path) -> io::Result<()> {
    use std::os::unix::process::CommandExt;
    let mut child = Command::new(exe)
        .arg("--daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?;
    thread::spawn(move || {
        let _ = child.wait(); // reap it if it exits while we are still running
    });
    Ok(())
}

/// Make sure the service is running, starting it if needed (waits briefly for it to come up).
pub fn ensure_daemon(paths: &Paths) -> io::Result<()> {
    if status::daemon_alive(paths) {
        return Ok(());
    }
    spawn_daemon(&exe_path())?;
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(4) {
        if status::daemon_alive(paths) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }
    Err(io::Error::new(io::ErrorKind::TimedOut, "the background service did not start"))
}

// ---------------------------------------------------------------- desktop entries

/// Quote a path for the `Exec=` key of a .desktop file.
pub fn exec_quote(p: &Path) -> String {
    let mut out = String::from("\"");
    for c in p.to_string_lossy().chars() {
        match c {
            '"' | '`' | '$' => {
                out.push('\\');
                out.push(c);
            }
            '\\' => out.push_str("\\\\\\\\"),
            '%' => out.push_str("%%"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

pub fn autostart_entry(exe: &Path) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Pen Touch (background service)\n\
         Comment=Makes the pen tablet work like a touch screen\n\
         Exec={} --daemon\n\
         Icon=pen-touch\n\
         Terminal=false\n\
         NoDisplay=true\n\
         X-GNOME-Autostart-enabled=true\n",
        exec_quote(exe)
    )
}

pub fn menu_entry(exe: &Path) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Pen Touch\n\
         GenericName=Pen tablet as touch screen\n\
         Comment=Turn the pen tablet into a native touch screen\n\
         Exec={}\n\
         Icon=pen-touch\n\
         Terminal=false\n\
         Categories=Utility;Settings;HardwareSettings;\n\
         Keywords=pen;tablet;touch;wacom;stylus;\n\
         StartupWMClass=pen-touch\n",
        exec_quote(exe)
    )
}

fn write_if_changed(path: &Path, content: &[u8]) -> io::Result<()> {
    if fs::read(path).ok().as_deref() == Some(content) {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(path, content)
}

/// Keep the menu entry, icon and login autostart pointing at the file we are running from
/// (so moving or updating the AppImage never breaks them).
pub fn sync_integration(paths: &Paths, exe: &Path, start_at_login: bool) -> io::Result<()> {
    write_if_changed(&paths.icon_file, ICON_PNG)?;
    write_if_changed(&paths.menu_file, menu_entry(exe).as_bytes())?;
    if start_at_login {
        write_if_changed(&paths.autostart_file, autostart_entry(exe).as_bytes())
    } else {
        match fs::remove_file(&paths.autostart_file) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

// ---------------------------------------------------------------- permissions

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UinputAccess {
    Ok,
    /// /dev/uinput exists but this user may not use it.
    NoPermission,
    /// /dev/uinput does not exist (kernel module not loaded).
    Missing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabletAccess {
    Ok,
    NoTablet,
    Denied,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Perms {
    pub uinput: UinputAccess,
    pub tablet: TabletAccess,
}

pub fn check_permissions() -> Perms {
    let uinput = match fs::OpenOptions::new().write(true).open("/dev/uinput") {
        Ok(_) => UinputAccess::Ok,
        Err(e) if input::is_permission_error(&e) => UinputAccess::NoPermission,
        Err(_) => UinputAccess::Missing,
    };
    let tablet = match input::candidates().first() {
        None => TabletAccess::NoTablet,
        Some(c) => {
            if fs::File::open(&c.path).is_ok() {
                TabletAccess::Ok
            } else {
                TabletAccess::Denied
            }
        }
    };
    Perms { uinput, tablet }
}

pub const UDEV_RULES: &str = "\
# Installed by Pen Touch.
# Let the logged-in user create virtual input devices (needed to act as a touch screen)...
KERNEL==\"uinput\", SUBSYSTEM==\"misc\", OPTIONS+=\"static_node=uinput\", TAG+=\"uaccess\"
# ...and read pen tablets.
SUBSYSTEM==\"input\", KERNEL==\"event*\", ENV{ID_INPUT_TABLET}==\"1\", TAG+=\"uaccess\"
";

/// Shell script (run as root) that grants the permissions. Idempotent.
pub fn setup_script() -> String {
    format!(
        "set -e\n\
         cat > /etc/udev/rules.d/70-pen-touch.rules <<'PEN_TOUCH_EOF'\n\
         {UDEV_RULES}PEN_TOUCH_EOF\n\
         mkdir -p /etc/modules-load.d\n\
         echo uinput > /etc/modules-load.d/pen-touch.conf\n\
         modprobe uinput || true\n\
         udevadm control --reload-rules\n\
         udevadm trigger --subsystem-match=input --subsystem-match=misc\n"
    )
}

/// `--setup-permissions` when already root: just run the script.
pub fn run_setup_as_root() -> Result<(), String> {
    let out = Command::new("sh").arg("-c").arg(setup_script()).output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Ask the desktop for administrator rights (polkit) and run the script. One password prompt, once.
pub fn run_setup_with_pkexec() -> Result<(), String> {
    let out = Command::new("pkexec").arg("sh").arg("-c").arg(setup_script()).output().map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            "pkexec is not installed. Use the terminal command shown below instead.".to_string()
        } else {
            e.to_string()
        }
    })?;
    match out.status.code() {
        Some(0) => Ok(()),
        Some(126) | Some(127) => Err("Authorisation was cancelled or refused.".to_string()),
        _ => Err(format!("Setup failed: {}", String::from_utf8_lossy(&out.stderr).trim())),
    }
}

/// The command to copy into a terminal if the one-click fix is unavailable.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub fn manual_command(exe: &Path) -> String {
    format!("sudo {} --setup-permissions", exec_quote(exe))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> Paths {
        let d = env::temp_dir().join(format!("pen-touch-test-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&d);
        Paths::in_dir(&d)
    }

    #[test]
    fn exec_paths_are_quoted_safely() {
        assert_eq!(exec_quote(Path::new("/home/me/Pen Touch.AppImage")), "\"/home/me/Pen Touch.AppImage\"");
        assert_eq!(exec_quote(Path::new("/a/$b`c\"d")), "\"/a/\\$b\\`c\\\"d\"");
        assert_eq!(exec_quote(Path::new("/100%/x")), "\"/100%%/x\"");
    }

    #[test]
    fn autostart_runs_the_service_and_the_menu_entry_opens_the_window() {
        let exe = Path::new("/opt/Pen Touch/pen-touch.AppImage");
        let a = autostart_entry(exe);
        assert!(a.contains("Exec=\"/opt/Pen Touch/pen-touch.AppImage\" --daemon\n"));
        assert!(a.contains("NoDisplay=true") && a.contains("X-GNOME-Autostart-enabled=true"));
        let m = menu_entry(exe);
        assert!(m.contains("Exec=\"/opt/Pen Touch/pen-touch.AppImage\"\n"));
        assert!(m.contains("Icon=pen-touch") && m.contains("StartupWMClass=pen-touch"));
    }

    #[test]
    fn integration_files_follow_the_setting_and_heal_a_moved_appimage() {
        let p = tmp("integration");
        let exe1 = Path::new("/old/place/PenTouch.AppImage");
        sync_integration(&p, exe1, true).unwrap();
        assert!(p.menu_file.exists() && p.icon_file.exists() && p.autostart_file.exists());
        assert!(fs::read_to_string(&p.autostart_file).unwrap().contains("/old/place/"));
        assert_eq!(fs::read(&p.icon_file).unwrap(), ICON_PNG);

        // the user moves the AppImage and opens it again: entries are rewritten to the new path
        let exe2 = Path::new("/new/place/PenTouch.AppImage");
        sync_integration(&p, exe2, true).unwrap();
        assert!(fs::read_to_string(&p.autostart_file).unwrap().contains("/new/place/"));
        assert!(fs::read_to_string(&p.menu_file).unwrap().contains("/new/place/"));

        // "start at login" off removes only the autostart entry; repeating it is fine
        sync_integration(&p, exe2, false).unwrap();
        assert!(!p.autostart_file.exists() && p.menu_file.exists());
        sync_integration(&p, exe2, false).unwrap();
    }

    #[test]
    fn setup_script_installs_rules_and_loads_uinput() {
        let s = setup_script();
        assert!(s.starts_with("set -e\n"));
        assert!(s.contains("/etc/udev/rules.d/70-pen-touch.rules"));
        assert!(s.contains("KERNEL==\"uinput\"") && s.contains("TAG+=\"uaccess\""));
        assert!(s.contains("ID_INPUT_TABLET"));
        assert!(s.contains("/etc/modules-load.d/pen-touch.conf") && s.contains("modprobe uinput"));
        assert!(s.contains("udevadm trigger"));
        // the heredoc is properly terminated on its own line
        assert!(s.contains("\nPEN_TOUCH_EOF\n"));
        // and the script itself is valid shell
        let ok = Command::new("sh").arg("-n").arg("-c").arg(&s).status().unwrap();
        assert!(ok.success());
    }

    #[test]
    fn manual_command_is_copy_pasteable() {
        let c = manual_command(Path::new("/home/me/My Apps/PenTouch.AppImage"));
        assert_eq!(c, "sudo \"/home/me/My Apps/PenTouch.AppImage\" --setup-permissions");
    }
}
