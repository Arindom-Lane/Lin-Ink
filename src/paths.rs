//! Every file the app reads or writes, gathered in one place so tests can point them at a temp dir.

use std::env;
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Paths {
    /// ~/.config/pen-touch/config.toml  - the persistent on/off state and settings
    pub config_file: PathBuf,
    /// $XDG_RUNTIME_DIR/pen-touch/status.json - what the background service is doing right now
    pub status_file: PathBuf,
    /// $XDG_RUNTIME_DIR/pen-touch/daemon.lock - held by the running service (single instance)
    pub lock_file: PathBuf,
    /// ~/.config/autostart/pen-touch.desktop - starts the service at login
    pub autostart_file: PathBuf,
    /// ~/.local/share/applications/pen-touch.desktop - the application-menu entry
    pub menu_file: PathBuf,
    /// ~/.local/share/icons/hicolor/256x256/apps/pen-touch.png
    pub icon_file: PathBuf,
}

fn non_empty(var: &str) -> Option<PathBuf> {
    env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from)
}

impl Paths {
    pub fn from_env() -> Paths {
        let home = non_empty("HOME").unwrap_or_else(|| PathBuf::from("/tmp"));
        let config_home = non_empty("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"));
        let data_home = non_empty("XDG_DATA_HOME").unwrap_or_else(|| home.join(".local/share"));
        let runtime = non_empty("XDG_RUNTIME_DIR")
            .map(|p| p.join("pen-touch"))
            .unwrap_or_else(|| PathBuf::from(format!("/tmp/pen-touch-{}", unsafe { libc::getuid() })));
        Paths {
            config_file: config_home.join("pen-touch/config.toml"),
            status_file: runtime.join("status.json"),
            lock_file: runtime.join("daemon.lock"),
            autostart_file: config_home.join("autostart/pen-touch.desktop"),
            menu_file: data_home.join("applications/pen-touch.desktop"),
            icon_file: data_home.join("icons/hicolor/256x256/apps/pen-touch.png"),
        }
    }

    /// Everything inside one directory (used by tests).
    #[cfg(test)]
    pub fn in_dir(dir: &std::path::Path) -> Paths {
        Paths {
            config_file: dir.join("config/pen-touch/config.toml"),
            status_file: dir.join("run/status.json"),
            lock_file: dir.join("run/daemon.lock"),
            autostart_file: dir.join("config/autostart/pen-touch.desktop"),
            menu_file: dir.join("data/applications/pen-touch.desktop"),
            icon_file: dir.join("data/icons/hicolor/256x256/apps/pen-touch.png"),
        }
    }
}
