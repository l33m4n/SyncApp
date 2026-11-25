//! SyncApp manager window (egui). Opened on demand; closing it leaves only the
//! lightweight tray service running.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use eframe::egui::{self, Color32, RichText, Sense};
use egui_extras::{Column, TableBuilder};

use syncapp::config::{config_path, Config, DriveReg};
use syncapp::engine::{self, human_bytes, Event, Manifest, VerifyReport, VerifyStatus};
use syncapp::schedule::{self, Clock, Schedule};
use syncapp::service::{self, HistoryRecord, Identity};
use syncapp::win::{self, VolumeInfo};

mod overview;
mod sources;
mod drives;
mod restore;
mod cleanup;
mod verify;
mod history;
mod settings;
mod widgets;

use widgets::*;
use restore::*;
use cleanup::*;
use verify::*;
use drives::*;

const OK: Color32 = Color32::from_rgb(34, 160, 90);

const WARN: Color32 = Color32::from_rgb(220, 140, 20);

const BAD: Color32 = Color32::from_rgb(210, 60, 60);

const ROW_H: f32 = 24.0;

pub fn run(show_update: bool) -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(crate::MANAGER_TITLE)
            .with_inner_size([1060.0, 700.0])
            .with_min_inner_size([820.0, 520.0])
            .with_icon(crate::icon_data()),
        ..Default::default()
    };
    eframe::run_native(
        crate::MANAGER_TITLE,
        options,
        Box::new(move |_cc| {
            let mut app = App::new();
            if show_update {
                // From an "Update now" notification: go to Updates and refresh the check.
                app.tab = Tab::Settings;
                app.dev_check_update = true;
            }
            Ok(Box::new(app))
        }),
    )
}

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Overview,
    Sources,
    Drives,
    Restore,
    Cleanup,
    History,
    Settings,
}

// ------------------------------------------------------------------ background jobs

#[derive(PartialEq, Clone, Copy)]
enum JobKind {
    Backup,
    Verify,
    Restore,
    Cleanup,
    Update,
}

struct JobResult {
    ok: bool,
    msg: String,
    errors: Vec<(String, String)>,
    verify: Option<(String, VerifyReport)>,
}

#[derive(Default)]
struct JobProgress {
    files_total: usize,
    file_index: usize,
    current: String,
    bytes_done: u64,
    bytes_total: u64,
    passed: u64,
    failed: u64,
    result: Option<JobResult>,
}

struct Job {
    label: String,
    kind: JobKind,
    cancel: Arc<AtomicBool>,
    shared: Arc<Mutex<JobProgress>>,
    started: Instant,
    handled: bool,
}

// ------------------------------------------------------------------ app

#[derive(Clone)]
struct DriveStat {
    files: u64,
    bytes: u64,
    /// PCs that back up to this drive: (name, last backup).
    pcs: Vec<(String, u64)>,
}

enum Confirm {
    InstallUpdate(PathBuf, String),
    Cleanup(Vec<String>, u64),
    Uninstall,
    Unregister { id: String, name: String },
    RemoveSource(String),
    RestoreOverwrite(Vec<(String, PathBuf)>, String),
}

struct UnmarkedDrive {
    root: String,
    info: Option<VolumeInfo>,
    space: Option<(u64, u64)>,
    previously_marked: bool,
}

struct ConnectedDrive {
    root: String,
    info: Option<VolumeInfo>,
    space: Option<(u64, u64)>,
}

struct App {
    cfg: Config,
    cfg_error: Option<String>,
    cfg_mtime: Option<SystemTime>,
    tab: Tab,
    connected: Vec<(ConnectedDrive, DriveReg)>,
    mismatched: Vec<(String, DriveReg)>,
    unmarked: Vec<UnmarkedDrive>,
    last_scan: Instant,
    last_full_scan: Instant,
    last_mask: u32,
    last_probe: Instant,
    history_mtime: Option<SystemTime>,
    tray_running: bool,
    history: Vec<HistoryRecord>,
    history_selected: Option<usize>,
    job: Option<Job>,
    message: Option<(String, bool)>,
    confirm: Option<Confirm>,
    exclude_input: HashMap<String, String>,
    autostart: bool,
    // drives tab
    selected_drive: Option<String>,
    renaming: Option<(String, String)>,
    mark: Option<MarkDialog>,
    /// (files, bytes) per connected drive's manifest, loaded off the UI thread.
    drive_stats: Arc<Mutex<HashMap<String, DriveStat>>>,
    drive_stats_pending: HashSet<String>,
    /// Drives currently locked by a backup/verify/restore (refreshed every couple of seconds).
    busy_drives: HashSet<String>,
    /// Settings are written shortly after the last change, not on every frame of a drag.
    save_due: Option<Instant>,
    uninstall_remove_data: bool,
    // sizes of sources, computed in the background
    sizes: Arc<Mutex<HashMap<String, (u64, u64)>>>,
    sizes_sig: String,
    // restore tab
    browser: Option<Browser>,
    // clean-up tab
    cleanup: Option<Cleanup>,
    // updates
    update_state: syncapp::update::UpdateState,
    update_check: Option<Arc<Mutex<Option<Result<Option<syncapp::update::Release>, String>>>>>,
    update_ready: Arc<Mutex<Option<PathBuf>>>,
    update_banner_hidden: bool,
    update_status: Option<(String, bool)>,
    // verification report page
    verify_view: Option<VerifyView>,
    dev_verify: bool,
    dev_check_update: bool,
    // schedule editor
    interval_hours: HashMap<String, bool>,
    new_time: (u32, u32),
}

impl App {
    fn new() -> Self {
        let (cfg, cfg_error) = match Config::load() {
            Ok(c) => (c, None),
            Err(e) => (Config::default(), Some(e)),
        };
        let mut app = App {
            selected_drive: cfg.drives.first().map(|d| d.drive_id.clone()),
            cfg,
            cfg_error,
            cfg_mtime: file_mtime(&config_path()),
            tab: Tab::Overview,
            connected: vec![],
            mismatched: vec![],
            unmarked: vec![],
            last_scan: Instant::now() - Duration::from_secs(60),
            last_full_scan: Instant::now() - Duration::from_secs(60),
            last_mask: 0,
            last_probe: Instant::now() - Duration::from_secs(60),
            history_mtime: None,
            tray_running: false,
            history: service::history_load(),
            history_selected: None,
            job: None,
            message: None,
            confirm: None,
            exclude_input: Default::default(),
            autostart: win::get_autostart().is_some(),
            renaming: None,
            mark: None,
            drive_stats: Default::default(),
            drive_stats_pending: HashSet::new(),
            busy_drives: HashSet::new(),
            save_due: None,
            uninstall_remove_data: false,
            sizes: Default::default(),
            sizes_sig: String::new(),
            browser: None,
            cleanup: None,
            update_state: syncapp::update::load_state(),
            update_check: None,
            update_ready: Default::default(),
            update_banner_hidden: false,
            update_status: None,
            verify_view: None,
            dev_verify: false,
            dev_check_update: false,
            interval_hours: HashMap::new(),
            new_time: (12, 0),
        };
        app.refresh();
        // Developer aid (screenshots/docs): open on a given page.
        match std::env::var("SYNCAPP_DEV_TAB").as_deref() {
            Ok("sources") => app.tab = Tab::Sources,
            Ok("drives") => app.tab = Tab::Drives,
            Ok("restore") => app.tab = Tab::Restore,
            Ok("cleanup") => app.tab = Tab::Cleanup,
            Ok("history") => app.tab = Tab::History,
            Ok("settings") => app.tab = Tab::Settings,
            Ok("verify") => app.dev_verify = true,
            Ok("updates") => {
                app.tab = Tab::Settings;
                app.dev_check_update = true;
            }
            _ => {}
        }
        app
    }

    fn refresh(&mut self) {
        self.last_scan = Instant::now();
        // Pick up changes made by the tray (e.g. pause toggled) unless we couldn't parse the file.
        let mt = file_mtime(&config_path());
        if mt != self.cfg_mtime && self.cfg_error.is_none() {
            if let Ok(c) = Config::load() {
                self.cfg = c;
            }
            self.cfg_mtime = mt;
        }
        let was_connected: HashSet<String> = self.connected.iter().map(|(_, r)| r.drive_id.clone()).collect();
        self.connected.clear();
        self.mismatched.clear();
        self.unmarked.clear();
        for root in win::candidate_drives() {
            let p = Path::new(&root);
            let info = win::volume_info(p);
            if info.is_none() {
                continue; // empty card reader etc.
            }
            let space = win::disk_space(p).ok();
            match service::identify(&self.cfg, p) {
                Identity::Registered(reg) => self.connected.push((ConnectedDrive { root, info, space }, reg)),
                Identity::SerialMismatch(reg) => self.mismatched.push((root, reg)),
                Identity::Unknown => {
                    let previously_marked = service::read_marker(p).is_some();
                    self.unmarked.push(UnmarkedDrive { root, info, space, previously_marked });
                }
            }
        }
        // Drop cached stats for drives that were unplugged.
        let now: HashSet<String> = self.connected.iter().map(|(_, r)| r.drive_id.clone()).collect();
        if now != was_connected {
            self.drive_stats.lock().unwrap().retain(|id, _| now.contains(id));
        }
        self.last_full_scan = Instant::now();
        self.last_mask = win::drive_mask();
        self.probe_tray();
        self.reload_history_if_changed();
        if self.selected_drive.as_ref().is_none_or(|id| self.cfg.drive(id).is_none()) {
            self.selected_drive = self.cfg.drives.first().map(|d| d.drive_id.clone());
        }
    }

    /// Cheap periodic check: only does real work when something actually changed.
    fn tick(&mut self) {
        self.last_scan = Instant::now();
        if win::drive_mask() != self.last_mask || self.last_full_scan.elapsed() > Duration::from_secs(15) {
            self.refresh(); // drives changed, or refresh free space now and then
            return;
        }
        let mt = file_mtime(&config_path());
        if mt != self.cfg_mtime && self.cfg_error.is_none() && self.save_due.is_none() {
            if let Ok(c) = Config::load() {
                self.cfg = c;
            }
            self.cfg_mtime = mt;
        }
        self.reload_history_if_changed();
        self.update_busy();
        if self.last_probe.elapsed() > Duration::from_secs(10) {
            self.probe_tray();
        }
    }

    fn update_busy(&mut self) {
        self.busy_drives = self
            .connected
            .iter()
            .map(|(_, r)| r.drive_id.clone())
            .filter(|id| service::is_busy(id))
            .collect();
    }

    fn probe_tray(&mut self) {
        self.last_probe = Instant::now();
        self.tray_running = crate::tray_running();
    }

    fn reload_history_if_changed(&mut self) {
        let mt = file_mtime(&service::history_path_pub());
        if mt != self.history_mtime {
            self.history_mtime = mt;
            self.history = service::history_load();
        }
    }

    /// Mark settings as changed; they're written ~0.7 s after the last change
    /// (and once any drag has ended), so dragging a value doesn't write every frame.
    fn save(&mut self) {
        if let Some(e) = &self.cfg_error {
            self.message = Some((format!("Not saved: {e}"), true));
            return;
        }
        self.save_due = Some(Instant::now() + Duration::from_millis(700));
    }

    /// Write pending settings now.
    fn flush_save(&mut self) {
        if self.save_due.take().is_none() {
            return;
        }
        match self.cfg.save() {
            Ok(()) => {
                self.cfg_mtime = file_mtime(&config_path());
                crate::poke_tray(0);
            }
            Err(e) => self.message = Some((e, true)),
        }
    }

    fn connected_root(&self, id: &str) -> Option<String> {
        self.connected.iter().find(|(_, r)| r.drive_id == id).map(|(c, _)| c.root.clone())
    }

    fn job_running(&self) -> bool {
        self.job.as_ref().is_some_and(|j| j.shared.lock().unwrap().result.is_none())
    }

    /// What a connected drive's manifest says (files, size, PCs). Loaded in the
    /// background (a big manifest takes a moment to parse); None until ready.
    fn drive_stat(&mut self, ctx: &egui::Context, id: &str) -> Option<DriveStat> {
        if let Some(s) = self.drive_stats.lock().unwrap().get(id) {
            return Some(s.clone());
        }
        let root = self.connected_root(id)?;
        if self.drive_stats_pending.insert(id.to_string()) {
            let (stats, id, ctx) = (self.drive_stats.clone(), id.to_string(), ctx.clone());
            std::thread::spawn(move || {
                let m = Manifest::load(Path::new(&root));
                let pcs = m.pcs.values().map(|p| (p.name.clone(), p.last_backup)).collect();
                stats.lock().unwrap().insert(id, DriveStat { files: m.files.len() as u64, bytes: m.total_bytes(), pcs });
                ctx.request_repaint();
            });
        }
        None
    }

    /// Recompute source sizes in the background whenever the source list changes.
    fn ensure_sizes(&mut self, ctx: &egui::Context) {
        let sig: String = self
            .cfg
            .sources
            .iter()
            .map(|s| format!("{}|{}|{}", s.name, s.path.display(), s.exclude.join(",")))
            .collect::<Vec<_>>()
            .join(";");
        if sig == self.sizes_sig {
            return;
        }
        self.sizes_sig = sig;
        let sources = self.cfg.sources.clone();
        let sizes = self.sizes.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            for s in sources {
                let v = engine::source_size(&s);
                sizes.lock().unwrap().insert(s.name.clone(), v);
                ctx.request_repaint();
            }
        });
    }

    // ------------------------------------------------------------------ jobs
    fn start_job<F>(&mut self, ctx: &egui::Context, kind: JobKind, label: String, work: F)
    where
        F: FnOnce(&AtomicBool, &mut dyn FnMut(Event)) -> JobResult + Send + 'static,
    {
        let cancel = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(Mutex::new(JobProgress::default()));
        let (c2, s2, ctx2) = (cancel.clone(), shared.clone(), ctx.clone());
        std::thread::spawn(move || {
            let mut cb = |ev: Event| {
                let mut p = s2.lock().unwrap();
                match ev {
                    Event::Planned { files, bytes } => {
                        p.files_total = files;
                        p.bytes_total = bytes;
                    }
                    Event::File { index, key, .. } => {
                        p.file_index = index;
                        p.current = key.to_string();
                    }
                    Event::Bytes { done, total } => {
                        p.bytes_done = done;
                        p.bytes_total = total;
                    }
                    Event::Checked { ok } => {
                        if ok { p.passed += 1 } else { p.failed += 1 }
                    }
                }
                ctx2.request_repaint_after(Duration::from_millis(100));
            };
            let result = work(&c2, &mut cb);
            s2.lock().unwrap().result = Some(result);
            ctx2.request_repaint();
        });
        self.job = Some(Job { label, kind, cancel, shared, started: Instant::now(), handled: false });
    }

    /// React once to a job that just finished.
    fn handle_finished_job(&mut self, ctx: &egui::Context) {
        let Some(job) = &mut self.job else { return };
        if job.handled {
            return;
        }
        let mut p = job.shared.lock().unwrap();
        let Some(res) = &mut p.result else { return };
        job.handled = true;
        let kind = job.kind;
        let verify = res.verify.take();
        drop(p);
        if kind == JobKind::Update {
            if let Some(path) = self.update_ready.lock().unwrap().take() {
                let v = self.update_state.newer().map(|r| r.version.clone()).unwrap_or_default();
                self.confirm = Some(Confirm::InstallUpdate(path, v));
            }
            return;
        }
        self.drive_stats.lock().unwrap().clear();
        self.drive_stats_pending.clear();
        self.reload_history_if_changed();
        self.update_busy();
        crate::poke_tray(crate::tray::RELOAD_HISTORY);
        if let Some((name, report)) = verify {
            self.verify_view = Some(VerifyView::new(name, win::now_unix(), report));
        }
        if kind != JobKind::Verify {
            if let Some(b) = &mut self.browser {
                b.reload(ctx, &self.cfg);
            }
            if let Some(c) = &mut self.cleanup {
                c.scan(ctx, &self.cfg);
            }
        }
    }

    fn backup(&mut self, ctx: &egui::Context, root: String, reg: DriveReg) {
        let cfg = self.cfg.clone();
        self.start_job(ctx, JobKind::Backup, format!("Backing up to {}", reg.name), move |cancel, cb| {
            match service::backup_drive(&cfg, &root, &reg, cancel, Some(cb)) {
                Ok(r) => {
                    let (title, body, problem) = service::describe(&reg, &root, &r);
                    let mut errors = r.failed.clone();
                    errors.extend(r.missing_sources.iter().map(|s| (s.clone(), "source not found".into())));
                    JobResult { ok: !problem, msg: format!("{title}. {body}"), errors, verify: None }
                }
                Err(e) => JobResult { ok: false, msg: e, errors: vec![], verify: None },
            }
        });
    }

    fn verify(&mut self, ctx: &egui::Context, root: String, reg: DriveReg) {
        self.start_job(ctx, JobKind::Verify, format!("Verifying {}", reg.name), move |cancel, cb| {
            match service::verify(&root, &reg, cancel, Some(cb)) {
                Ok(v) => {
                    let msg = match &v.aborted {
                        Some(a) => format!("Verification stopped: {a}"),
                        None if v.problems() == 0 => format!("All {} on {} are intact.", files_word(v.ok), reg.name),
                        None => format!("{} file(s) intact, {} problem(s) found.", v.ok, v.problems()),
                    };
                    JobResult { ok: v.problems() == 0 && v.aborted.is_none(), msg, errors: vec![], verify: Some((reg.name.clone(), v)) }
                }
                Err(e) => JobResult { ok: false, msg: e, errors: vec![], verify: None },
            }
        });
    }

    // ------------------------------------------------------------------ frame
    fn ui_root(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let minimized = ctx.input(|i| i.viewport().minimized).unwrap_or(false);
        if minimized {
            // Nothing to show: check back rarely and do no scanning.
            ctx.request_repaint_after(Duration::from_secs(10));
        } else {
            if self.last_scan.elapsed() > Duration::from_secs(2) {
                self.tick();
            }
            ctx.request_repaint_after(Duration::from_secs(2));
        }
        self.handle_finished_job(&ctx);
        if std::mem::take(&mut self.dev_check_update) {
            self.check_for_update(&ctx);
        }
        if std::mem::take(&mut self.dev_verify) {
            if let Some((c, r)) = self.connected.first() {
                let (root, reg) = (c.root.clone(), r.clone());
                self.verify(&ctx, root, reg);
            }
        }
        self.ensure_sizes(&ctx);

        egui::Panel::top("tabs").show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading(RichText::new("SyncApp").strong());
                ui.add_space(16.0);
                for (t, label) in [
                    (Tab::Overview, "Overview"),
                    (Tab::Sources, "What to back up"),
                    (Tab::Drives, "Backup drives"),
                    (Tab::Restore, "Restore"),
                    (Tab::Cleanup, "Clean up"),
                    (Tab::History, "History"),
                    (Tab::Settings, "Settings"),
                ] {
                    if ui.selectable_label(self.tab == t && self.verify_view.is_none(), RichText::new(label).size(15.0)).clicked() {
                        self.tab = t;
                        self.verify_view = None;
                    }
                }
            });
            ui.add_space(6.0);
        });

        egui::Panel::bottom("status").show(ui, |ui| {
            ui.horizontal(|ui| {
                if self.tray_running {
                    dot(ui, if self.cfg.paused { WARN } else { OK });
                    ui.label(if self.cfg.paused {
                        "Background service running: automatic backups PAUSED"
                    } else {
                        "Background service running: marked drives are backed up when plugged in"
                    });
                } else {
                    dot(ui, BAD);
                    ui.label("Background service not running: automatic backups won't happen");
                    if ui.button("Start it").clicked() {
                        crate::spawn_tray();
                        self.last_scan = Instant::now() - Duration::from_secs(10);
                    }
                }
            });
        });

        egui::CentralPanel::default_margins().show(ui, |ui| {
            if let Some(e) = self.cfg_error.clone() {
                ui.colored_label(BAD, format!("{e}\nFix or delete that file, then restart SyncApp. Changes won't be saved until then."));
                ui.separator();
            }
            if let Some((m, bad)) = self.message.clone() {
                ui.horizontal(|ui| {
                    dot(ui, if bad { BAD } else { OK });
                    ui.label(m);
                    if ui.small_button("Dismiss").clicked() {
                        self.message = None;
                    }
                });
                ui.separator();
            }
            self.poll_update_check();
            self.ui_update_banner(ui);
            self.ui_job(ui);
            if self.verify_view.is_some() {
                self.ui_verify_report(ui);
                return;
            }
            match self.tab {
                Tab::Drives => self.ui_drives(ui),
                Tab::Restore => self.ui_restore(ui),
                Tab::Cleanup => self.ui_cleanup(ui),
                _ => {
                    egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| match self.tab {
                        Tab::Overview => self.ui_overview(ui),
                        Tab::Sources => self.ui_sources(ui),
                        Tab::History => self.ui_history(ui),
                        _ => self.ui_settings(ui),
                    });
                }
            }
        });

        self.ui_mark_dialog(&ctx);
        self.ui_confirm(&ctx);

        if let Some(due) = self.save_due {
            let dragging = ctx.input(|i| i.pointer.any_down());
            let now = Instant::now();
            if now >= due && !dragging {
                self.flush_save();
            } else {
                ctx.request_repaint_after(due.saturating_duration_since(now).max(Duration::from_millis(100)));
            }
        }
    }

    fn ui_job(&mut self, ui: &mut egui::Ui) {
        let Some(job) = &self.job else { return };
        let p = job.shared.lock().unwrap();
        let mut dismiss = false;
        let mut open_report = false;
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_width(ui.available_width());
            match &p.result {
                None => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.strong(&job.label);
                        let secs = job.started.elapsed().as_secs_f64().max(0.1);
                        ui.label(format!("{}s", secs as u64));
                        if p.bytes_done > 0 {
                            ui.label(RichText::new(format!("{:.1} MB/s", p.bytes_done as f64 / 1e6 / secs)).weak());
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("Cancel").clicked() {
                                job.cancel.store(true, Ordering::Relaxed);
                            }
                        });
                    });
                    let frac = if p.bytes_total > 0 { p.bytes_done as f32 / p.bytes_total as f32 } else { 0.0 };
                    ui.horizontal(|ui| {
                        ui.label(format!("{} of {} files", p.file_index, p.files_total));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(RichText::new(format!(
                                "{} of {}  ({:.0}%)",
                                human_bytes(p.bytes_done),
                                human_bytes(p.bytes_total),
                                frac * 100.0
                            )).weak());
                        });
                    });
                    thin_bar(ui, frac, ui.visuals().selection.bg_fill);
                    if job.kind == JobKind::Verify {
                        ui.horizontal(|ui| {
                            dot(ui, OK);
                            ui.label(format!("{} passed", p.passed));
                            ui.add_space(10.0);
                            dot(ui, if p.failed > 0 { BAD } else { Color32::GRAY });
                            ui.label(format!("{} problem(s)", p.failed));
                            ui.add_space(10.0);
                            ui.label(RichText::new("Reading each file from the drive and comparing its BLAKE3 checksum with the one recorded at backup time").weak());
                        });
                    }
                    ui.label(RichText::new(&p.current).weak().small());
                }
                Some(res) => {
                    ui.horizontal(|ui| {
                        dot(ui, if res.ok { OK } else { WARN });
                        ui.label(&res.msg);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("Dismiss").clicked() {
                                dismiss = true;
                            }
                            if job.kind == JobKind::Verify && self.verify_view.is_none() && ui.small_button("View report").clicked() {
                                open_report = true;
                            }
                        });
                    });
                    if !res.errors.is_empty() {
                        egui::CollapsingHeader::new(format!("{} problem(s)", res.errors.len())).show(ui, |ui| {
                            egui::ScrollArea::vertical().max_height(160.0).show(ui, |ui| {
                                for (k, e) in res.errors.iter().take(500) {
                                    ui.label(format!("{k}: {e}"));
                                }
                            });
                        });
                    }
                }
            }
        });
        drop(p);
        if dismiss {
            self.job = None;
        }
        if open_report {
            self.open_last_verify_any();
        }
        ui.add_space(8.0);
    }

    // ------------------------------------------------------------------ confirmations
    fn ui_confirm(&mut self, ctx: &egui::Context) {
        let Some(c) = &self.confirm else { return };
        let (title, body) = match c {
            Confirm::InstallUpdate(_, v) => (
                format!("Restart to finish updating to SyncApp {v}?"),
                "SyncApp closes, the background service restarts, and the new version opens. This takes a few seconds. Your settings and backups are not affected.".to_string(),
            ),
            Confirm::Cleanup(keys, bytes) => (
                format!("Permanently delete {} from the backup drive?", files_word(keys.len() as u64)),
                format!(
                    "This frees {}. These files no longer exist on any PC that uses this drive, so this backup is their last copy. This can't be undone.",
                    human_bytes(*bytes)
                ),
            ),
            Confirm::Uninstall => (
                "Uninstall SyncApp?".to_string(),
                "This stops the background service, removes it from Windows startup and the Start menu, and deletes the installed program. Backups on your drives are NOT touched.".to_string(),
            ),
            Confirm::Unregister { name, .. } => (
                format!("Stop using \"{name}\"?"),
                "SyncApp will no longer back up to this drive. Files already on it are NOT deleted.".to_string(),
            ),
            Confirm::RemoveSource(n) => (
                format!("Remove \"{n}\" from backups?"),
                "It won't be backed up any more. Copies already on your drives are kept.".to_string(),
            ),
            Confirm::RestoreOverwrite(files, _) => (
                "Replace existing files?".to_string(),
                format!("Restoring {} file(s). Files on the PC that differ from the backup will be replaced by the backup version. This can't be undone.", files.len()),
            ),
        };
        let mut answer = None;
        let is_uninstall = matches!(c, Confirm::Uninstall);
        let remove_data = &mut self.uninstall_remove_data;
        egui::Modal::new(egui::Id::unique("syncapp-confirm")).show(ctx, |ui| {
            ui.set_width(400.0);
            ui.heading(title);
            ui.label(body);
            if is_uninstall {
                ui.add_space(6.0);
                ui.checkbox(remove_data, "Also delete SyncApp's settings, history and log on this PC");
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button("Yes").clicked() {
                    answer = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    answer = Some(false);
                }
            });
        });
        let Some(yes) = answer else { return };
        let c = self.confirm.take().unwrap();
        if !yes {
            return;
        }
        match c {
            Confirm::Unregister { id, name } => match {
                self.flush_save();
                service::unregister_drive(&mut self.cfg, &id)
            } {
                Ok(()) => {
                    self.cfg_mtime = file_mtime(&config_path());
                    self.message = Some((format!("\"{name}\" is no longer a backup drive."), false));
                    crate::poke_tray(crate::tray::RELOAD_DRIVES);
                    self.selected_drive = None;
                    self.refresh();
                }
                Err(e) => self.message = Some((e, true)),
            },
            Confirm::RemoveSource(n) => {
                self.cfg.sources.retain(|s| s.name != n);
                for d in self.cfg.drives.iter_mut() {
                    if let Some(sel) = &mut d.sources {
                        sel.retain(|s| *s != n);
                    }
                }
                self.save();
            }
            Confirm::RestoreOverwrite(files, desc) => self.restore_files(ctx, files, desc),
            Confirm::Cleanup(keys, _) => self.start_cleanup(ctx, keys),
            Confirm::InstallUpdate(path, _) => {
                self.flush_save();
                match crate::install_update(&path) {
                    Ok(()) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                    Err(e) => self.message = Some((format!("Update not installed: {e}"), true)),
                }
            }
            Confirm::Uninstall => {
                self.flush_save();
                match crate::uninstall(self.uninstall_remove_data) {
                    Ok(msg) => {
                        self.message = Some((msg, false));
                        self.autostart = false;
                        self.save_due = None;
                        // Nothing left to manage: close the window.
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    Err(e) => self.message = Some((format!("Uninstall failed: {e}"), true)),
                }
            }
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.flush_save(); // don't lose a change made just before closing
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.ui_root(ui);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::kittest::Queryable;
    use egui_kittest::Harness;

    /// Opens the real window headlessly with empty settings and clicks through every tab.
    #[test]
    fn every_tab_opens_and_shows_its_content() {
        let home = tempfile::tempdir().unwrap();
        // Safe: the only test in this program; set before anything reads the environment.
        unsafe {
            std::env::set_var("SYNCAPP_HOME", home.path());
            std::env::set_var("SYNCAPP_INSTANCE", "gui-test");
        }
        let mut h = Harness::new_ui_state(|ui, app: &mut App| app.ui_root(ui), App::new());
        h.run_steps(3);
        let has = |h: &Harness<'_, App>, text: &str| h.query_all_by_label_contains(text).next().is_some();
        assert!(has(&h, "Getting started"), "Overview shows the getting-started steps");
        for (tab, expect) in [
            ("What to back up", "Add folder"),
            ("Backup drives", "Your backup drives"),
            ("Restore", "Connect a marked backup drive"),
            ("Clean up", "Files on the backup drive that no PC needs"),
            ("History", "Nothing yet"),
            ("Settings", "Check GitHub for updates"),
            ("Overview", "Getting started"),
        ] {
            h.get_by_label(tab).click();
            h.run_steps(3);
            assert!(has(&h, expect), "tab {tab:?} should show {expect:?}");
        }
    }
}
