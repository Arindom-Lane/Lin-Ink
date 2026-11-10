//! The window. It is only a remote control: the switch edits the saved config, and the
//! background service (which keeps running after the window closes) does the work.

use crate::config::{self, Config};
use std::time::SystemTime;
use crate::paths::Paths;
use crate::setup::{self, Perms, TabletAccess, UinputAccess};
use crate::status::{self, State, Status};
use eframe::egui::{self, Color32, RichText};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// What the window tells the user, derived from "what they asked for" + "what the service reports".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    ServiceDown,
    Off,
    /// The switch was just flipped and the service has not caught up yet.
    Applying,
    Waiting,
    Active,
    NeedsPermission,
    Problem,
}

pub fn view_of(alive: bool, enabled: bool, st: Option<&Status>) -> View {
    if !alive {
        return View::ServiceDown;
    }
    let Some(st) = st else { return View::Applying };
    match (enabled, st.state) {
        (false, State::Off) => View::Off,
        (false, _) | (true, State::Off) => View::Applying,
        (true, State::Waiting) => View::Waiting,
        (true, State::Active) => View::Active,
        (true, State::NoPermission) => View::NeedsPermission,
        (true, State::Error) => View::Problem,
    }
}

const GREEN: Color32 = Color32::from_rgb(46, 170, 90);
const GREY: Color32 = Color32::from_rgb(140, 140, 150);
const AMBER: Color32 = Color32::from_rgb(220, 160, 30);
const RED: Color32 = Color32::from_rgb(210, 70, 60);

pub struct App {
    paths: Paths,
    cfg: Config,
    cfg_stamp: Option<(SystemTime, u64)>,
    status: Option<Status>,
    alive: bool,
    perms: Perms,
    last_poll: Instant,
    last_perms: Instant,
    last_respawn: Instant,
    notice: Option<String>,
    fixing: Option<Receiver<Result<(), String>>>,
    spawn_service: bool,
}

impl App {
    pub fn new(paths: Paths) -> App {
        Self::build(paths, true)
    }

    fn build(paths: Paths, spawn_service: bool) -> App {
        let cfg = Config::load(&paths);
        let mut notice = None;
        if spawn_service {
            if let Err(e) = setup::sync_integration(&paths, &setup::exe_path(), cfg.start_at_login) {
                notice = Some(format!("Could not update the menu / autostart entries: {e}"));
            }
            if let Err(e) = setup::ensure_daemon(&paths) {
                notice = Some(format!("Could not start the background service: {e}"));
            }
        }
        let mut app = App {
            cfg_stamp: config::stamp(&paths),
            paths,
            cfg,
            status: None,
            alive: false,
            perms: setup::check_permissions(),
            last_poll: Instant::now() - Duration::from_secs(10),
            last_perms: Instant::now(),
            last_respawn: Instant::now(),
            notice,
            fixing: None,
            spawn_service,
        };
        app.poll(true);
        app
    }

    pub fn view(&self) -> View {
        view_of(self.alive, self.cfg.enabled, self.status.as_ref())
    }

    fn poll(&mut self, force: bool) {
        if !force && self.last_poll.elapsed() < Duration::from_millis(400) {
            return;
        }
        self.last_poll = Instant::now();
        // Someone else (the terminal command, a text editor) changed the settings: follow them,
        // otherwise the switch would show a stale state and the next click would overwrite their change.
        let stamp_now = config::stamp(&self.paths);
        if stamp_now != self.cfg_stamp {
            self.cfg = Config::load(&self.paths);
            self.cfg_stamp = stamp_now;
        }
        self.alive = status::daemon_alive(&self.paths);
        self.status = if self.alive { Status::read(&self.paths) } else { None };

        // If the service vanished (crash, killed), bring it back.
        if !self.alive && self.spawn_service && self.last_respawn.elapsed() > Duration::from_secs(3) {
            self.last_respawn = Instant::now();
            let _ = setup::ensure_daemon(&self.paths);
        }
        if self.last_perms.elapsed() > Duration::from_secs(2) {
            self.last_perms = Instant::now();
            self.perms = setup::check_permissions();
        }
        if let Some(rx) = &self.fixing {
            match rx.try_recv() {
                Ok(res) => {
                    self.fixing = None;
                    self.notice = Some(match res {
                        Ok(()) => "Permissions installed. You can use the switch now.".to_string(),
                        Err(e) => e,
                    });
                    self.perms = setup::check_permissions();
                }
                Err(TryRecvError::Disconnected) => self.fixing = None,
                Err(TryRecvError::Empty) => {}
            }
        }
    }

    fn save(&mut self) {
        if let Err(e) = self.cfg.save(&self.paths) {
            self.notice = Some(format!("Could not save settings: {e}"));
        }
        self.cfg_stamp = config::stamp(&self.paths); // our own write is not an "external change"
    }

    pub fn set_enabled(&mut self, on: bool) {
        self.cfg.enabled = on;
        self.save();
        if self.spawn_service {
            let _ = setup::ensure_daemon(&self.paths);
        }
    }

    fn start_fix(&mut self) {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(setup::run_setup_with_pkexec());
        });
        self.fixing = Some(rx);
        self.notice = None;
    }

    fn perms_problem(&self) -> Option<&'static str> {
        match (self.perms.uinput, self.perms.tablet) {
            (UinputAccess::Missing, _) => Some("The \"uinput\" kernel module is not loaded."),
            (UinputAccess::NoPermission, _) => Some("This user may not create virtual input devices."),
            (_, TabletAccess::Denied) => Some("This user may not read the pen tablet."),
            _ => None,
        }
    }

    /// Draw one frame of the window.
    pub fn ui(&mut self, ctx: &egui::Context) {
        let view = self.view();
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(10.0);
                    ui.label(RichText::new("Pen Touch").size(26.0).strong());
                    ui.label(RichText::new("Use your pen tablet like a touch screen").color(GREY));
                    ui.add_space(18.0);

                    let mut on = self.cfg.enabled;
                    if switch(ui, &mut on).clicked() {
                        self.set_enabled(on);
                    }
                    ui.add_space(10.0);

                    let (colour, text) = match view {
                        View::ServiceDown => (AMBER, "Starting the background service...".to_string()),
                        View::Off => (GREY, "Off - the tablet works normally".to_string()),
                        View::Applying => (AMBER, "Applying...".to_string()),
                        View::Waiting => (AMBER, "On - plug in or wake the tablet".to_string()),
                        View::Active => (
                            GREEN,
                            format!("On - {}", self.status.as_ref().map(|s| s.device.as_str()).unwrap_or("")),
                        ),
                        View::NeedsPermission => (RED, "On - needs a one-time permission setup".to_string()),
                        View::Problem => (
                            RED,
                            format!("Problem: {}", self.status.as_ref().map(|s| s.message.as_str()).unwrap_or("")),
                        ),
                    };
                    ui.horizontal(|ui| {
                        let w = ui.fonts(|f| f.layout_no_wrap(text.clone(), egui::FontId::proportional(14.0), colour).size().x);
                        ui.add_space(((ui.available_width() - w - 18.0) / 2.0).max(0.0));
                        let (dot, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
                        ui.painter().circle_filled(dot.center(), 5.0, colour);
                        ui.label(RichText::new(text).color(colour));
                    });
                });

                ui.add_space(14.0);
                self.permission_box(ui);
                if let Some(n) = self.notice.clone() {
                    ui.add_space(6.0);
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new(n).color(AMBER));
                        if ui.small_button("x").clicked() {
                            self.notice = None;
                        }
                    });
                }

                ui.add_space(10.0);
                self.options(ui);
                ui.add_space(12.0);
                ui.separator();
                ui.add_space(4.0);
                ui.label(RichText::new("Tap = click   Drag = scroll   Hold = right-click").small().color(GREY));
                ui.label(RichText::new("Double-tap text = select a word (drag the handles to extend)").small().color(GREY));
                ui.label(
                    RichText::new(
                        "Closing this window leaves it running. Turn the switch off to use the tablet \
                         normally, e.g. for drawing with pressure.",
                    )
                    .small()
                    .color(GREY),
                );
            });
        });
    }

    fn permission_box(&mut self, ui: &mut egui::Ui) {
        let Some(problem) = self.perms_problem() else { return };
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new("One-time setup needed").strong().color(AMBER));
            ui.label(problem);
            ui.label(
                RichText::new("This lets your user create the virtual touch screen. You will be asked for your password once.")
                    .small()
                    .color(GREY),
            );
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let busy = self.fixing.is_some();
                if ui.add_enabled(!busy, egui::Button::new("Fix permissions")).clicked() {
                    self.start_fix();
                }
                if busy {
                    ui.spinner();
                    ui.label("Waiting for authorisation...");
                }
            });
            let cmd = setup::manual_command(&setup::exe_path());
            ui.collapsing("Or run this in a terminal", |ui| {
                ui.add(egui::Label::new(RichText::new(&cmd).monospace().small()).wrap());
                if ui.small_button("Copy command").clicked() {
                    ui.ctx().copy_text(cmd.clone());
                }
            });
        });
    }

    fn options(&mut self, ui: &mut egui::Ui) {
        let before = self.cfg.clone();
        egui::CollapsingHeader::new(RichText::new("Options").strong()).default_open(true).show(ui, |ui| {
            ui.checkbox(&mut self.cfg.start_at_login, "Start automatically when I log in");
            ui.checkbox(&mut self.cfg.show_cursor, "Show a cursor that follows the pen");
            ui.horizontal(|ui| {
                ui.label("Tap tolerance");
                ui.add(egui::Slider::new(&mut self.cfg.tap_tolerance_mm, 0.5..=6.0).suffix(" mm"));
            });
            ui.horizontal(|ui| {
                ui.label("Tablet rotation");
                egui::ComboBox::from_id_salt("rotation")
                    .selected_text(format!("{}\u{00B0}", self.cfg.rotation))
                    .show_ui(ui, |ui| {
                        for r in [0u16, 90, 180, 270] {
                            ui.selectable_value(&mut self.cfg.rotation, r, format!("{r}\u{00B0}"));
                        }
                    });
            });
            ui.collapsing("Cursor area (several monitors)", |ui| {
                ui.label(
                    RichText::new("Which part of the whole desktop the cursor covers (0 to 1). For touches, choose the output in your system's touch screen settings.")
                        .small()
                        .color(GREY),
                );
                let a = &mut self.cfg.cursor_area;
                ui.horizontal(|ui| {
                    ui.label("left");
                    ui.add(egui::DragValue::new(&mut a[0]).range(0.0..=0.95).speed(0.01));
                    ui.label("top");
                    ui.add(egui::DragValue::new(&mut a[1]).range(0.0..=0.95).speed(0.01));
                });
                ui.horizontal(|ui| {
                    ui.label("right");
                    ui.add(egui::DragValue::new(&mut a[2]).range(0.05..=1.0).speed(0.01));
                    ui.label("bottom");
                    ui.add(egui::DragValue::new(&mut a[3]).range(0.05..=1.0).speed(0.01));
                });
                if ui.small_button("Whole desktop").clicked() {
                    self.cfg.cursor_area = [0.0, 0.0, 1.0, 1.0];
                }
            });
        });

        if self.cfg != before {
            self.cfg.sanitize();
            self.save();
            if self.cfg.start_at_login != before.start_at_login && self.spawn_service {
                if let Err(e) = setup::sync_integration(&self.paths, &setup::exe_path(), self.cfg.start_at_login) {
                    self.notice = Some(format!("Could not update autostart: {e}"));
                }
            }
        }
    }
}

/// A big iOS-style on/off switch.
fn switch(ui: &mut egui::Ui, on: &mut bool) -> egui::Response {
    let size = egui::vec2(112.0, 56.0);
    let (rect, mut response) = ui.allocate_exact_size(size, egui::Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    response.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Checkbox, ui.is_enabled(), *on, "Pen Touch"));
    if ui.is_rect_visible(rect) {
        let t = ui.ctx().animate_bool_responsive(response.id, *on);
        let lerp = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t) as u8;
        let bg = Color32::from_rgb(lerp(150, GREEN.r()), lerp(150, GREEN.g()), lerp(158, GREEN.b()));
        let radius = rect.height() / 2.0;
        ui.painter().rect_filled(rect, radius, bg);
        let cx = egui::lerp((rect.left() + radius)..=(rect.right() - radius), t);
        ui.painter().circle_filled(egui::pos2(cx, rect.center().y), radius - 5.0, Color32::WHITE);
    }
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

pub fn run(paths: Paths) -> Result<(), eframe::Error> {
    let icon = egui::IconData { rgba: include_bytes!("../packaging/icon64.rgba").to_vec(), width: 64, height: 64 };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Pen Touch")
            .with_app_id("pen-touch")
            .with_inner_size([380.0, 640.0])
            .with_min_inner_size([340.0, 420.0])
            .with_icon(Arc::new(icon)),
        ..Default::default()
    };
    eframe::run_native(
        "Pen Touch",
        options,
        Box::new(move |_cc| Ok(Box::new(AppShell { app: App::new(paths) }))),
    )
}

struct AppShell {
    app: App,
}

impl eframe::App for AppShell {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.app.poll(false);
        self.app.ui(ctx);
        ctx.request_repaint_after(Duration::from_millis(500));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> Paths {
        let d = std::env::temp_dir().join(format!("pen-touch-test-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        Paths::in_dir(&d)
    }

    fn st(state: State) -> Status {
        let mut s = Status::new(state);
        s.device = "Wacom Intuos S Pen".into();
        s.message = "boom".into();
        s
    }

    #[test]
    fn view_reflects_what_was_asked_and_what_is_true() {
        assert_eq!(view_of(false, true, None), View::ServiceDown);
        assert_eq!(view_of(true, false, Some(&st(State::Off))), View::Off);
        assert_eq!(view_of(true, true, Some(&st(State::Off))), View::Applying, "just switched on");
        assert_eq!(view_of(true, false, Some(&st(State::Active))), View::Applying, "just switched off");
        assert_eq!(view_of(true, true, Some(&st(State::Active))), View::Active);
        assert_eq!(view_of(true, true, Some(&st(State::Waiting))), View::Waiting);
        assert_eq!(view_of(true, true, Some(&st(State::NoPermission))), View::NeedsPermission);
        assert_eq!(view_of(true, true, Some(&st(State::Error))), View::Problem);
        assert_eq!(view_of(true, true, None), View::Applying);
    }

    #[test]
    fn flipping_the_switch_persists_across_a_restart() {
        let p = tmp("gui-persist");
        let mut app = App::build(p.clone(), false);
        assert!(!app.cfg.enabled, "first run starts off");
        app.set_enabled(true);
        // a brand-new window (as after rebooting) sees the saved state
        let again = App::build(p.clone(), false);
        assert!(again.cfg.enabled);
        let mut again = again;
        again.set_enabled(false);
        assert!(!App::build(p, false).cfg.enabled);
    }

    #[test]
    fn window_follows_changes_made_from_outside() {
        let p = tmp("gui-external");
        let mut app = App::build(p.clone(), false);
        assert!(!app.cfg.enabled);
        // `pen-touch on` in a terminal (a different process) edits the file
        std::thread::sleep(Duration::from_millis(20)); // distinct mtime
        let mut c = Config::load(&p);
        c.enabled = true;
        c.rotation = 90;
        c.save(&p).unwrap();
        app.poll(true);
        assert!(app.cfg.enabled && app.cfg.rotation == 90, "the switch shows the real state");
        // and the window's own saves are not mistaken for outside changes
        app.cfg.tap_tolerance_mm = 4.0;
        app.save();
        app.poll(true);
        assert_eq!(app.cfg.tap_tolerance_mm, 4.0);
        assert!(app.cfg.enabled);
    }

    #[test]
    fn every_state_renders_without_panicking() {
        let ctx = egui::Context::default();
        let p = tmp("gui-render");
        let mut app = App::build(p, false);
        let cases = [
            (false, false, None),
            (true, false, Some(st(State::Off))),
            (true, true, Some(st(State::Off))),
            (true, true, Some(st(State::Waiting))),
            (true, true, Some(st(State::Active))),
            (true, true, Some(st(State::NoPermission))),
            (true, true, Some(st(State::Error))),
        ];
        for (alive, enabled, status) in cases {
            app.alive = alive;
            app.cfg.enabled = enabled;
            app.status = status;
            for perms in [
                Perms { uinput: UinputAccess::Ok, tablet: TabletAccess::Ok },
                Perms { uinput: UinputAccess::Missing, tablet: TabletAccess::NoTablet },
                Perms { uinput: UinputAccess::NoPermission, tablet: TabletAccess::Denied },
            ] {
                app.perms = perms;
                app.notice = Some("a notice".into());
                for _ in 0..2 {
                    let out = ctx.run(egui::RawInput::default(), |ctx| app.ui(ctx));
                    assert!(!out.shapes.is_empty(), "something was drawn");
                }
            }
        }
    }

    #[test]
    fn permission_problems_are_described() {
        let mut app = App::build(tmp("gui-perm"), false);
        app.perms = Perms { uinput: UinputAccess::Ok, tablet: TabletAccess::NoTablet };
        assert!(app.perms_problem().is_none(), "no tablet plugged in is not a permission problem");
        app.perms = Perms { uinput: UinputAccess::Missing, tablet: TabletAccess::Ok };
        assert!(app.perms_problem().unwrap().contains("uinput"));
        app.perms = Perms { uinput: UinputAccess::Ok, tablet: TabletAccess::Denied };
        assert!(app.perms_problem().unwrap().contains("tablet"));
    }
}
