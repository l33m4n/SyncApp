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

const OK: Color32 = Color32::from_rgb(34, 160, 90);
const WARN: Color32 = Color32::from_rgb(220, 140, 20);
const BAD: Color32 = Color32::from_rgb(210, 60, 60);
const ROW_H: f32 = 24.0;

pub fn run() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(crate::MANAGER_TITLE)
            .with_inner_size([1060.0, 700.0])
            .with_min_inner_size([820.0, 520.0])
            .with_icon(crate::icon_data()),
        ..Default::default()
    };
    eframe::run_native(crate::MANAGER_TITLE, options, Box::new(|_cc| Ok(Box::new(App::new()))))
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

// ------------------------------------------------------------------ restore browser

#[derive(Clone, Copy, PartialEq)]
enum PcState {
    Same,
    Changed,
    Missing,
    Unknown,
}

struct RFile {
    key: String,
    key_lc: String,
    size: u64,
    backed_up: Option<u64>, // unix seconds of the PC file's date at backup time
    original: Option<PathBuf>,
    state: PcState,
    from_pc: Option<String>,
}

#[derive(Clone, Copy, PartialEq)]
enum RFilter {
    All,
    Missing,
    Changed,
}

struct Row {
    dir: bool,
    name: String,
    key: String,
    range: (usize, usize),
    size: u64,
    count: usize,
    missing: usize,
    changed: usize,
    state: PcState,
    backed_up: Option<u64>,
    from_pc: Option<String>,
}

type Loaded = (Vec<RFile>, Vec<(String, String)>);

struct Browser {
    drive_id: String,
    root: String,
    loader: Option<Arc<Mutex<Option<Loaded>>>>,
    files: Vec<RFile>,
    errors: Vec<(String, String)>,
    cwd: String,
    search: String,
    filter: RFilter,
    selected: HashSet<usize>,
    rows: Vec<Row>,
    rows_dirty: bool,
    to_original: bool,
    target: String,
    overwrite: bool,
}

impl Browser {
    fn new(ctx: &egui::Context, cfg: &Config, drive_id: String, root: String) -> Self {
        let mut b = Browser {
            drive_id,
            root,
            loader: None,
            files: vec![],
            errors: vec![],
            cwd: String::new(),
            search: String::new(),
            filter: RFilter::All,
            selected: HashSet::new(),
            rows: vec![],
            rows_dirty: true,
            to_original: true,
            target: String::new(),
            overwrite: false,
        };
        b.reload(ctx, cfg);
        b
    }

    fn reload(&mut self, ctx: &egui::Context, cfg: &Config) {
        let slot = Arc::new(Mutex::new(None));
        let (s2, root, cfg, ctx) = (slot.clone(), self.root.clone(), cfg.clone(), ctx.clone());
        std::thread::spawn(move || {
            let (list, errors) = engine::list_backup(Path::new(&root));
            let files = list
                .into_iter()
                .map(|f| {
                    let original = service::original_path(&cfg, &f.key);
                    let state = match &original {
                        None => PcState::Unknown,
                        Some(p) => match engine::pc_stamp(p) {
                            None => PcState::Missing,
                            Some((size, mtime)) => {
                                let same = size == f.size && f.backed_up_mtime.is_none_or(|m| m == mtime);
                                if same { PcState::Same } else { PcState::Changed }
                            }
                        },
                    };
                    RFile {
                        key_lc: f.key.to_lowercase(),
                        key: f.key,
                        size: f.size,
                        backed_up: f.backed_up_mtime.map(engine::filetime_to_unix),
                        from_pc: f.from_pc.clone(),
                        original,
                        state,
                    }
                })
                .collect();
            *s2.lock().unwrap() = Some((files, errors));
            ctx.request_repaint();
        });
        self.loader = Some(slot);
    }

    /// Pick up the background listing when it's ready.
    fn poll(&mut self) {
        let done = self.loader.as_ref().and_then(|l| l.lock().unwrap().take());
        if let Some((files, errors)) = done {
            self.files = files;
            self.errors = errors;
            self.loader = None;
            self.selected.clear();
            self.rows_dirty = true;
            // Stay in the same folder if it still exists.
            if !self.cwd.is_empty() {
                let p = format!("{}/", self.cwd.to_lowercase());
                if !self.files.iter().any(|f| f.key_lc.starts_with(&p)) {
                    self.cwd.clear();
                }
            }
        }
    }

    fn loading(&self) -> bool {
        self.loader.is_some()
    }

    fn matches(&self, f: &RFile) -> bool {
        match self.filter {
            RFilter::All => true,
            RFilter::Missing => f.state == PcState::Missing,
            RFilter::Changed => f.state == PcState::Changed,
        }
    }

    fn rebuild_rows(&mut self) {
        self.rows_dirty = false;
        self.rows.clear();
        let search = self.search.trim().to_lowercase();
        if !search.is_empty() {
            for (i, f) in self.files.iter().enumerate() {
                if f.key_lc.contains(&search) && self.matches(f) {
                    self.rows.push(file_row(f, i, f.key.clone()));
                }
            }
            return;
        }
        let prefix_lc = if self.cwd.is_empty() { String::new() } else { format!("{}/", self.cwd.to_lowercase()) };
        let prefix_chars = prefix_lc.chars().count();
        let start = self.files.partition_point(|f| f.key_lc.as_str() < prefix_lc.as_str());
        let end = start + self.files[start..].partition_point(|f| f.key_lc.starts_with(&prefix_lc));
        let mut folders: Vec<Row> = Vec::new();
        let mut files: Vec<Row> = Vec::new();
        let mut i = start;
        while i < end {
            let f = &self.files[i];
            let cut = f.key.char_indices().nth(prefix_chars).map(|x| x.0).unwrap_or(f.key.len());
            let rest = &f.key[cut..];
            if let Some((dir, _)) = rest.split_once('/') {
                let dir_key = format!("{}{}", &f.key[..cut], dir);
                let dir_prefix = format!("{}/", dir_key.to_lowercase());
                let j = i + self.files[i..end].partition_point(|g| g.key_lc.starts_with(&dir_prefix));
                let mut row = Row {
                    dir: true,
                    name: dir.to_string(),
                    key: dir_key,
                    range: (i, j),
                    size: 0,
                    count: 0,
                    missing: 0,
                    changed: 0,
                    state: PcState::Same,
                    backed_up: None,
                    from_pc: None,
                };
                for g in &self.files[i..j] {
                    if self.matches(g) {
                        row.count += 1;
                        row.size += g.size;
                        row.missing += (g.state == PcState::Missing) as usize;
                        row.changed += (g.state == PcState::Changed) as usize;
                        row.backed_up = row.backed_up.max(g.backed_up);
                    }
                }
                // A single-file source is stored as "<name>/<name>": show it as the file itself.
                let single = self.cwd.is_empty() && j == i + 1 && self.files[i].key_lc == format!("{}/{}", dir.to_lowercase(), dir.to_lowercase());
                if single {
                    if self.matches(&self.files[i]) {
                        files.push(file_row(&self.files[i], i, dir.to_string()));
                    }
                } else if row.count > 0 {
                    folders.push(row);
                }
                i = j;
            } else {
                if self.matches(f) {
                    files.push(file_row(f, i, rest.to_string()));
                }
                i += 1;
            }
        }
        self.rows = folders;
        self.rows.extend(files);
    }

    /// Indices of files under a row that pass the filter.
    fn row_files(&self, r: &Row) -> Vec<usize> {
        (r.range.0..r.range.1).filter(|&i| self.matches(&self.files[i])).collect()
    }

    fn selected_bytes(&self) -> u64 {
        self.selected.iter().map(|&i| self.files[i].size).sum()
    }
}

fn file_row(f: &RFile, i: usize, name: String) -> Row {
    Row {
        dir: false,
        name,
        key: f.key.clone(),
        range: (i, i + 1),
        size: f.size,
        count: 1,
        missing: (f.state == PcState::Missing) as usize,
        changed: (f.state == PcState::Changed) as usize,
        state: f.state,
        backed_up: f.backed_up,
        from_pc: f.from_pc.clone(),
    }
}

// ------------------------------------------------------------------ verify report view

struct VerifyView {
    drive_name: String,
    finished_at: u64,
    report: VerifyReport,
    search: String,
    only_problems: bool,
    rows: Vec<usize>,
    dirty: bool,
}

impl VerifyView {
    fn new(drive_name: String, finished_at: u64, report: VerifyReport) -> Self {
        let only_problems = report.problems() > 0;
        VerifyView { drive_name, finished_at, report, search: String::new(), only_problems, rows: vec![], dirty: true }
    }

    fn rebuild(&mut self) {
        self.dirty = false;
        let s = self.search.trim().to_lowercase();
        self.rows = self
            .report
            .items
            .iter()
            .enumerate()
            .filter(|(_, it)| !self.only_problems || it.status != VerifyStatus::Ok)
            .filter(|(_, it)| s.is_empty() || it.key.to_lowercase().contains(&s))
            .map(|(i, _)| i)
            .collect();
    }
}

// ------------------------------------------------------------------ app

#[derive(Clone)]
struct DriveStat {
    files: u64,
    bytes: u64,
    /// PCs that back up to this drive: (name, last backup).
    pcs: Vec<(String, u64)>,
}

// ------------------------------------------------------------------ clean-up

struct Cleanup {
    drive_id: String,
    root: String,
    loader: Option<Arc<Mutex<Option<Vec<engine::Orphan>>>>>,
    items: Vec<engine::Orphan>,
    selected: HashSet<usize>,
    scanned: bool,
}

impl Cleanup {
    fn new(ctx: &egui::Context, cfg: &Config, drive_id: String, root: String) -> Self {
        let mut c = Cleanup { drive_id, root, loader: None, items: vec![], selected: HashSet::new(), scanned: false };
        c.scan(ctx, cfg);
        c
    }

    fn scan(&mut self, ctx: &egui::Context, cfg: &Config) {
        let slot = Arc::new(Mutex::new(None));
        let (s2, root, cfg, ctx) = (slot.clone(), self.root.clone(), cfg.clone(), ctx.clone());
        std::thread::spawn(move || {
            let found = service::find_orphans(&cfg, &root);
            *s2.lock().unwrap() = Some(found);
            ctx.request_repaint();
        });
        self.loader = Some(slot);
    }

    fn poll(&mut self) {
        let done = self.loader.as_ref().and_then(|l| l.lock().unwrap().take());
        if let Some(items) = done {
            self.items = items;
            self.loader = None;
            self.scanned = true;
            self.selected = (0..self.items.len()).collect(); // all selected by default
        }
    }

    fn selected_bytes(&self) -> u64 {
        self.selected.iter().map(|&i| self.items[i].size).sum()
    }
}

enum Confirm {
    InstallUpdate(PathBuf, String),
    Cleanup(Vec<String>, u64),
    Uninstall,
    Unregister { id: String, name: String },
    RemoveSource(String),
    RestoreOverwrite(Vec<(String, PathBuf)>, String),
}

struct MarkDialog {
    root: String,
    info: Option<VolumeInfo>,
    space: Option<(u64, u64)>,
    previously_marked: bool,
    other_pcs: Vec<String>,
    name: String,
    auto_backup: bool,
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

fn file_mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).ok().and_then(|m| m.modified().ok())
}

fn when(unix: u64) -> String {
    let now = win::now_unix();
    let ago = now.saturating_sub(unix);
    let rel = match ago {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{} min ago", ago / 60),
        3600..=86_399 => format!("{} h ago", ago / 3600),
        _ => format!("{} days ago", ago / 86_400),
    };
    format!("{} ({rel})", win::local_time_string(unix))
}

/// Future time as "YYYY-MM-DD HH:MM:SS (in 3 h)".
fn until(unix: u64) -> String {
    let now = win::now_unix();
    let d = unix.saturating_sub(now);
    let rel = match d {
        0..=59 => "now".to_string(),
        60..=3599 => format!("in {} min", d / 60),
        3600..=86_399 => format!("in {} h {} min", d / 3600, d % 3600 / 60),
        _ => format!("in {} days", d / 86_400),
    };
    format!("{} ({rel})", win::local_time_string(unix))
}

fn files_word(n: u64) -> String {
    if n == 1 { "1 file".into() } else { format!("{n} files") }
}

fn short_hash(h: &str) -> String {
    // chars(), not byte slicing: a damaged manifest must not be able to crash the window.
    if h.chars().count() > 16 { format!("{}...", h.chars().take(16).collect::<String>()) } else { h.to_string() }
}

/// Small filled status circle (drawn, so it doesn't depend on font glyphs).
fn dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 5.0, color);
}

fn capacity_bar(ui: &mut egui::Ui, free: u64, total: u64, with_text: bool) {
    if total == 0 {
        return;
    }
    let used = total.saturating_sub(free);
    let frac = used as f32 / total as f32;
    if with_text {
        ui.horizontal(|ui| {
            ui.label(format!("{} used of {}", human_bytes(used), human_bytes(total)));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(format!("{} free", human_bytes(free))).weak());
            });
        });
    }
    thin_bar(ui, frac, if frac > 0.9 { WARN } else { ui.visuals().selection.bg_fill });
}

/// Slim progress bar with no text inside (labels go next to it instead).
fn thin_bar(ui: &mut egui::Ui, frac: f32, color: Color32) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 6.0), Sense::hover());
    let r = rect.height() / 2.0;
    ui.painter().rect_filled(rect, r, ui.visuals().extreme_bg_color);
    if frac > 0.0 {
        let mut fill = rect;
        fill.set_width((rect.width() * frac.clamp(0.0, 1.0)).max(rect.height()));
        ui.painter().rect_filled(fill, r, color);
    }
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

    fn restore_files(&mut self, ctx: &egui::Context, files: Vec<(String, PathBuf)>, description: String) {
        let Some(b) = &self.browser else { return };
        let Some(reg) = self.cfg.drive(&b.drive_id).cloned() else { return };
        let (root, overwrite) = (b.root.clone(), b.overwrite);
        self.start_job(ctx, JobKind::Restore, format!("Restoring {} file(s)", files.len()), move |cancel, cb| {
            match service::restore_files(&root, &reg, &files, overwrite, &description, cancel, Some(cb)) {
                Ok(r) => {
                    let msg = match &r.aborted {
                        Some(a) => format!("Restore stopped: {a}. {} file(s) restored before that.", r.copied.len()),
                        None => format!(
                            "Restored {} file(s) ({}). {} were already identical.",
                            r.copied.len(),
                            human_bytes(r.bytes_copied),
                            r.unchanged
                        ),
                    };
                    JobResult { ok: r.ok(), msg, errors: r.failed, verify: None }
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

    fn open_last_verify_any(&mut self) {
        if let Some(r) = self.history.iter().rev().find(|h| h.kind == "verify") {
            self.open_last_verify(&r.drive_id.clone());
        }
    }

    fn open_last_verify(&mut self, drive_id: &str) {
        match service::last_verify(drive_id) {
            Some(s) => self.verify_view = Some(VerifyView::new(s.drive_name, s.finished_at, s.report)),
            None => self.message = Some(("No saved verification report for this drive yet.".into(), true)),
        }
    }

    // ------------------------------------------------------------------ overview

    fn ui_overview(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        if self.cfg.sources.is_empty() || self.cfg.drives.is_empty() {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.heading("Getting started");
                ui.label("1. Under \"What to back up\", add the folders and files you want copied.");
                ui.label("2. Plug in your flash drive or external disk and mark it under \"Backup drives\".");
                ui.label("3. That's it. Whenever that drive is plugged in, SyncApp copies anything that changed.");
                ui.label("   Other drives are never touched.");
            });
            ui.add_space(10.0);
        }

        for (root, reg) in &self.mismatched {
            ui.colored_label(BAD, format!(
                "Warning: {} looks like \"{}\" but its volume ID doesn't match, so it is not trusted. If you reformatted it, mark it again under Backup drives.",
                &root[..2], reg.name
            ));
        }

        ui.heading("Backup drives");
        ui.add_space(4.0);
        if self.cfg.drives.is_empty() {
            ui.label("No drives marked yet.");
        }
        let busy = self.job_running();
        let mut action: Option<(u8, String, DriveReg)> = None;
        for reg in self.cfg.drives.clone() {
            let conn = self.connected.iter().find(|(_, r)| r.drive_id == reg.drive_id).map(|(c, _)| (c.root.clone(), c.space));
            let last = self.history.iter().rev().find(|h| h.drive_id == reg.drive_id && h.kind == "backup").cloned();
            let last_verify = self.history.iter().rev().find(|h| h.drive_id == reg.drive_id && h.kind == "verify").cloned();
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&reg.name).size(17.0).strong());
                    match &conn {
                        Some((root, _)) => {
                            dot(ui, OK);
                            ui.label(format!("Connected as {}", &root[..2]));
                        }
                        None => {
                            ui.label(RichText::new("Not connected").weak());
                        }
                    }
                });
                if let Some((_, Some((free, total)))) = conn {
                    capacity_bar(ui, free, total, true);
                }
                match &last {
                    Some(h) => {
                        ui.horizontal(|ui| {
                            dot(ui, if h.ok { OK } else { WARN });
                            ui.label(format!(
                                "Last backup {}: {} updated, {} unchanged{}",
                                when(h.finished_at),
                                h.copied,
                                h.unchanged,
                                if h.errors.is_empty() { String::new() } else { format!(", {} problem(s)", h.errors.len()) }
                            ));
                        });
                    }
                    None => {
                        ui.label(RichText::new("Never backed up").weak());
                    }
                }
                if let Some(v) = &last_verify {
                    ui.horizontal(|ui| {
                        dot(ui, if v.ok { OK } else { BAD });
                        ui.label(format!(
                            "Last verified {}: {}",
                            when(v.finished_at),
                            if v.ok { format!("all {} intact", files_word(v.unchanged)) } else { format!("{} problem(s)", v.errors.len()) }
                        ));
                        if ui.small_button("View report").clicked() {
                            action = Some((2, String::new(), reg.clone()));
                        }
                    });
                }
                if let Some((root, _)) = &conn {
                    ui.horizontal(|ui| {
                        if ui.add_enabled(!busy, egui::Button::new("Back up now")).clicked() {
                            action = Some((0, root.clone(), reg.clone()));
                        }
                        if ui
                            .add_enabled(!busy, egui::Button::new("Verify integrity"))
                            .on_hover_text("Reads every backed-up file back from the drive and checks it against its checksum")
                            .clicked()
                        {
                            action = Some((1, root.clone(), reg.clone()));
                        }
                        if ui.button("Open drive").clicked() {
                            win::shell_open(&Path::new(root).join(engine::BACKUP_DIR));
                        }
                    });
                }
            });
            ui.add_space(6.0);
        }
        match action {
            Some((0, root, reg)) => self.backup(&ctx, root, reg),
            Some((1, root, reg)) => self.verify(&ctx, root, reg),
            Some((_, _, reg)) => self.open_last_verify(&reg.drive_id),
            None => {}
        }
    }

    // ------------------------------------------------------------------ sources

    fn ui_sources(&mut self, ui: &mut egui::Ui) {
        ui.heading("What to back up");
        ui.label("Folders and individual files copied to your backup drives. Only new or changed files are copied.");
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui.button("+ Add folder...").clicked() {
                if let Some(p) = rfd::FileDialog::new().set_title("Choose a folder to back up").pick_folder() {
                    self.add_source(&p);
                }
            }
            if ui.button("+ Add files...").clicked() {
                if let Some(files) = rfd::FileDialog::new().set_title("Choose files to back up").pick_files() {
                    for p in files {
                        self.add_source(&p);
                    }
                }
            }
        });
        ui.add_space(8.0);

        let mut changed = false;
        let mut remove = None;
        let sizes = self.sizes.lock().unwrap().clone();
        for s in self.cfg.sources.iter_mut() {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    changed |= ui.checkbox(&mut s.enabled, "").on_hover_text("Include in backups").changed();
                    ui.label(RichText::new(&s.name).strong().size(15.0));
                    ui.label(s.path.display().to_string());
                    if !s.path.exists() {
                        ui.colored_label(BAD, "(not found)");
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Remove").clicked() {
                            remove = Some(s.name.clone());
                        }
                        if ui.button("Open").clicked() {
                            win::shell_open(&s.path);
                        }
                        match sizes.get(&s.name) {
                            Some((n, b)) => ui.label(RichText::new(format!("{}, {}", files_word(*n), human_bytes(*b))).weak()),
                            None => ui.label(RichText::new("measuring...").weak()),
                        };
                    });
                });
                if s.path.is_dir() {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new("Skip:").weak());
                        let mut del = None;
                        for (j, pat) in s.exclude.iter().enumerate() {
                            if ui.small_button(format!("{pat}  x")).on_hover_text("Click to stop skipping").clicked() {
                                del = Some(j);
                            }
                        }
                        if let Some(j) = del {
                            s.exclude.remove(j);
                            changed = true;
                        }
                        let input = self.exclude_input.entry(s.name.clone()).or_default();
                        let r = ui.add(egui::TextEdit::singleline(input).hint_text("e.g. *.iso or node_modules").desired_width(170.0));
                        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        if (ui.small_button("Add").clicked() || enter) && !input.trim().is_empty() {
                            s.exclude.push(input.trim().to_string());
                            input.clear();
                            changed = true;
                        }
                    });
                }
            });
            ui.add_space(4.0);
        }
        if let Some(name) = remove {
            self.confirm = Some(Confirm::RemoveSource(name));
        }
        if changed {
            self.save();
        }
        ui.add_space(6.0);
        ui.label(RichText::new(format!("Always skipped: {}  (temporary and lock files)", engine::DEFAULT_EXCLUDES.join(", "))).weak().small());
    }

    fn add_source(&mut self, p: &Path) {
        match self.cfg.add_source(p, None, vec![]) {
            Ok(s) => {
                self.message = Some((format!("Added \"{}\"", s.name), false));
                self.save();
            }
            Err(e) => self.message = Some((e, true)),
        }
    }

    // ------------------------------------------------------------------ drives

    fn ui_drives(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        egui::Panel::left("drive_list").resizable(false).exact_size(300.0).show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
                ui.heading("Your backup drives");
                ui.add_space(4.0);
                if self.cfg.drives.is_empty() {
                    ui.label(RichText::new("None yet. Mark a drive below.").weak());
                }
                let mut select = None;
                for reg in &self.cfg.drives {
                    let selected = self.selected_drive.as_deref() == Some(reg.drive_id.as_str());
                    let conn = self.connected.iter().find(|(_, r)| r.drive_id == reg.drive_id).map(|(c, _)| c);
                    let last = self.history.iter().rev().find(|h| h.drive_id == reg.drive_id && h.kind == "backup");
                    let mut frame = egui::Frame::group(ui.style());
                    if selected {
                        frame = frame.fill(ui.visuals().selection.bg_fill.gamma_multiply(0.35)).stroke(ui.visuals().selection.stroke);
                    }
                    let resp = frame
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                dot(ui, if conn.is_some() { OK } else { Color32::GRAY });
                                ui.label(RichText::new(&reg.name).strong().size(15.0));
                                if let Some(c) = conn {
                                    ui.label(RichText::new(&c.root[..2]).weak());
                                }
                            });
                            match conn.and_then(|c| c.space) {
                                Some((free, total)) => {
                                    capacity_bar(ui, free, total, false);
                                    ui.label(RichText::new(format!("{} free of {}", human_bytes(free), human_bytes(total))).small().weak());
                                }
                                None => {
                                    ui.label(RichText::new("Not connected").small().weak());
                                }
                            }
                            let line = match last {
                                Some(h) => format!("Last backup {}", when(h.finished_at)),
                                None => "Never backed up".into(),
                            };
                            ui.label(RichText::new(line).small().weak());
                            let plan = if !reg.auto_backup { "Manual backups only".to_string() } else { reg.schedule.describe() };
                            ui.label(RichText::new(plan).small().weak());
                        })
                        .response
                        .interact(Sense::click());
                    if resp.clicked() {
                        select = Some(reg.drive_id.clone());
                    }
                    ui.add_space(4.0);
                }
                if let Some(id) = select {
                    self.selected_drive = Some(id);
                    self.renaming = None;
                }

                ui.add_space(14.0);
                ui.heading("Add a drive");
                ui.add_space(4.0);
                if self.unmarked.is_empty() {
                    ui.label(RichText::new("Plug in a flash drive or external disk and it will show up here.").weak());
                }
                let mut open = None;
                for (i, d) in self.unmarked.iter().enumerate() {
                    egui::Frame::group(ui.style()).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        let label = d.info.as_ref().map(|i| i.label.clone()).filter(|l| !l.is_empty()).unwrap_or("Unnamed drive".into());
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(format!("{} ({})", label, &d.root[..2])).strong());
                        });
                        if let Some((free, total)) = d.space {
                            ui.label(RichText::new(format!(
                                "{}  |  {} free of {}",
                                d.info.as_ref().map(|i| i.fs_name.as_str()).unwrap_or(""),
                                human_bytes(free),
                                human_bytes(total)
                            )).small().weak());
                        }
                        if ui.button("Use as backup drive...").clicked() {
                            open = Some(i);
                        }
                    });
                    ui.add_space(4.0);
                }
                if let Some(i) = open {
                    let d = &self.unmarked[i];
                    let label = d.info.as_ref().map(|i| i.label.clone()).filter(|l| !l.is_empty()).unwrap_or_default();
                    self.mark = Some(MarkDialog {
                        root: d.root.clone(),
                        info: d.info.clone(),
                        space: d.space,
                        previously_marked: d.previously_marked,
                        other_pcs: if d.previously_marked {
                            let me = self.cfg.pc().id;
                            Manifest::load(Path::new(&d.root))
                                .pcs
                                .iter()
                                .filter(|(id, _)| **id != me)
                                .map(|(_, p)| p.name.clone())
                                .collect()
                        } else {
                            vec![]
                        },
                        name: label,
                        auto_backup: true,
                    });
                }
            });
        });

        egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
            ui.add_space(4.0);
            let Some(id) = self.selected_drive.clone() else {
                ui.add_space(40.0);
                ui.vertical_centered(|ui| {
                    ui.heading("No backup drive yet");
                    ui.label("Plug in a flash drive or external disk, then choose \"Use as backup drive...\" on the left.");
                });
                return;
            };
            self.ui_drive_detail(ui, &ctx, &id);
        });
    }

    fn ui_drive_detail(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, id: &str) {
        let Some(reg) = self.cfg.drive(id).cloned() else { return };
        let conn = self.connected.iter().find(|(_, r)| r.drive_id == id).map(|(c, _)| (c.root.clone(), c.space, c.info.clone()));
        let stats = self.drive_stat(ctx, id);
        let busy = self.job_running() || self.busy_drives.contains(id);
        let mut changed = false;

        // Header
        ui.horizontal(|ui| {
            match &mut self.renaming {
                Some((rid, text)) if rid == id => {
                    let r = ui.add(egui::TextEdit::singleline(text).desired_width(240.0).font(egui::TextStyle::Heading));
                    let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if ui.button("Save").clicked() || enter {
                        let new = text.trim().to_string();
                        if !new.is_empty() {
                            if let Some(d) = self.cfg.drive_mut(id) {
                                d.name = new;
                            }
                            changed = true;
                        }
                        self.renaming = None;
                    } else if ui.button("Cancel").clicked() {
                        self.renaming = None;
                    }
                }
                _ => {
                    ui.heading(RichText::new(&reg.name).size(22.0).strong());
                    if ui.small_button("Rename").clicked() {
                        self.renaming = Some((id.to_string(), reg.name.clone()));
                    }
                }
            }
        });
        ui.horizontal(|ui| match &conn {
            Some((root, _, info)) => {
                dot(ui, OK);
                ui.label(format!("Connected as {}", &root[..2]));
                if let Some(i) = info {
                    ui.label(RichText::new(format!("|  {}  |  volume label \"{}\"", i.fs_name, i.label)).weak());
                }
            }
            None => {
                dot(ui, Color32::GRAY);
                ui.label("Not connected. Plug it in to back up, verify or restore.");
            }
        });
        ui.add_space(6.0);
        if let Some((_, Some((free, total)), _)) = &conn {
            capacity_bar(ui, *free, *total, true);
        }
        if let Some(st) = &stats {
            ui.label(format!("SyncApp backup on this drive: {}, {}", files_word(st.files), human_bytes(st.bytes)));
            if st.pcs.len() > 1 {
                let list = st
                    .pcs
                    .iter()
                    .map(|(n, t)| format!("{n} (last backup {})", when(*t)))
                    .collect::<Vec<_>>()
                    .join(", ");
                ui.label(RichText::new(format!("Shared by {} PCs: {list}. The newest version of each file is kept.", st.pcs.len())).weak());
            }
        }
        if let Some(h) = self.history.iter().rev().find(|h| h.drive_id == id && h.kind == "backup") {
            ui.horizontal(|ui| {
                dot(ui, if h.ok { OK } else { WARN });
                ui.label(format!("Last backup {}: {} updated, {} unchanged", when(h.finished_at), h.copied, h.unchanged));
            });
        }
        let last_verify = self.history.iter().rev().find(|h| h.drive_id == id && h.kind == "verify").cloned();
        if let Some(v) = &last_verify {
            ui.horizontal(|ui| {
                dot(ui, if v.ok { OK } else { BAD });
                ui.label(format!("Last verified {}", when(v.finished_at)));
            });
        }

        // Actions
        ui.add_space(8.0);
        let mut act = 0u8;
        ui.horizontal(|ui| {
            let connected = conn.is_some();
            if ui.add_enabled(connected && !busy, egui::Button::new(RichText::new("Back up now").strong())).clicked() {
                act = 1;
            }
            if ui
                .add_enabled(connected && !busy, egui::Button::new("Verify integrity"))
                .on_hover_text("Reads every backed-up file back from the drive and compares its checksum")
                .clicked()
            {
                act = 2;
            }
            if ui.add_enabled(connected, egui::Button::new("Browse & restore")).clicked() {
                act = 3;
            }
            if ui.add_enabled(connected, egui::Button::new("Open backup folder")).clicked() {
                act = 4;
            }
            if last_verify.is_some() && ui.button("Last verification report").clicked() {
                act = 5;
            }
        });
        if busy && conn.is_some() {
            ui.label(RichText::new("A backup or check is running on this drive.").weak());
        }

        // Automatic backup
        ui.add_space(14.0);
        ui.heading("Automatic backup");
        let last_backup = self.history.iter().rev().find(|h| h.drive_id == id && h.kind == "backup").map(|h| h.finished_at);
        let unit_hours = self.interval_hours.entry(id.to_string()).or_insert_with(|| {
            matches!(reg.schedule, Schedule::Every { minutes } if minutes >= 60 && minutes % 60 == 0)
        });
        let new_time = &mut self.new_time;
        let paused = self.cfg.paused;
        let d = self.cfg.drive_mut(id).unwrap();
        changed |= ui.checkbox(&mut d.auto_backup, "Back up automatically when this drive is plugged in").changed();
        ui.add_enabled_ui(d.auto_backup, |ui| {
            ui.add_space(4.0);
            ui.label("While it stays plugged in:");
            let mode = match d.schedule {
                Schedule::Off => 0,
                Schedule::Every { .. } => 1,
                Schedule::Daily { .. } => 2,
            };
            ui.indent("sched", |ui| {
                if ui.radio(mode == 0, "Don't back up again until it's plugged in next time").clicked() && mode != 0 {
                    d.schedule = Schedule::Off;
                    changed = true;
                }
                if ui.radio(mode == 1, "Back up at regular intervals").clicked() && mode != 1 {
                    d.schedule = Schedule::Every { minutes: 120 };
                    *unit_hours = true;
                    changed = true;
                }
                if let Schedule::Every { minutes } = &mut d.schedule {
                    ui.indent("every", |ui| {
                        ui.horizontal(|ui| {
                            ui.label("Every");
                            let mut amount = if *unit_hours { (*minutes / 60).max(1) } else { *minutes };
                            let range = if *unit_hours { 1..=24 } else { schedule::MIN_INTERVAL..=1440 };
                            let r1 = ui.add(egui::DragValue::new(&mut amount).range(range)).changed();
                            let mut hours = *unit_hours;
                            egui::ComboBox::from_id_salt("interval_unit")
                                .width(80.0)
                                .selected_text(if hours { "hours" } else { "minutes" })
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(&mut hours, false, "minutes");
                                    ui.selectable_value(&mut hours, true, "hours");
                                });
                            if hours != *unit_hours {
                                *unit_hours = hours;
                                amount = if hours { (*minutes).div_ceil(60).clamp(1, 24) } else { *minutes };
                            }
                            let new = if *unit_hours { amount.clamp(1, 24) * 60 } else { amount.max(schedule::MIN_INTERVAL) };
                            if r1 || new != *minutes {
                                changed |= new != *minutes;
                                *minutes = new;
                            }
                        });
                    });
                }
                if ui.radio(mode == 2, "Back up at set times of day").clicked() && mode != 2 {
                    d.schedule = Schedule::times_per_day(3);
                    changed = true;
                }
                if let Schedule::Daily { times, days } = &mut d.schedule {
                    ui.indent("daily", |ui| {
                        ui.horizontal(|ui| {
                            ui.label("Quick set:");
                            for (n, label) in [(1, "Once a day"), (2, "2x a day"), (3, "3x a day"), (4, "4x a day")] {
                                if ui.small_button(label).clicked() {
                                    if let Schedule::Daily { times: t, .. } = Schedule::times_per_day(n) {
                                        *times = t;
                                        changed = true;
                                    }
                                }
                            }
                        });
                        ui.horizontal_wrapped(|ui| {
                            ui.label("Times:");
                            times.sort();
                            let mut remove = None;
                            let can_remove = times.len() > 1;
                            for (i, t) in times.iter().enumerate() {
                                let b = ui.add_enabled(can_remove, egui::Button::new(format!("{}  x", schedule::fmt_time(*t))).small());
                                if b.on_hover_text("Remove this time").clicked() {
                                    remove = Some(i);
                                }
                            }
                            if let Some(i) = remove {
                                times.remove(i);
                                changed = true;
                            }
                            ui.add_space(12.0);
                            ui.add(egui::DragValue::new(&mut new_time.0).range(0..=23).custom_formatter(|n, _| format!("{:02}", n as i64)));
                            ui.label(":");
                            ui.add(egui::DragValue::new(&mut new_time.1).range(0..=59).custom_formatter(|n, _| format!("{:02}", n as i64)));
                            let t = (new_time.0 * 60 + new_time.1) as u16;
                            if ui.add_enabled(!times.contains(&t) && times.len() < 24, egui::Button::new("Add time").small()).clicked() {
                                times.push(t);
                                times.sort();
                                changed = true;
                            }
                        });
                        ui.horizontal(|ui| {
                            ui.label("Days:");
                            for (i, name) in schedule::DAY_NAMES.iter().enumerate() {
                                let on = days.contains(&(i as u8));
                                if ui.selectable_label(on, *name).clicked() {
                                    if on {
                                        days.retain(|x| *x != i as u8);
                                    } else {
                                        days.push(i as u8);
                                        days.sort();
                                    }
                                    changed = true;
                                }
                            }
                            ui.add_space(8.0);
                            if ui.small_button("Every day").clicked() {
                                *days = (0..7).collect();
                                changed = true;
                            }
                            if ui.small_button("Weekdays").clicked() {
                                *days = (0..5).collect();
                                changed = true;
                            }
                        });
                        if days.is_empty() {
                            ui.colored_label(WARN, "Pick at least one day, or no scheduled backups will happen.");
                        }
                    });
                }
            });
            ui.add_space(4.0);
            match d.schedule.next_run(last_backup, &Clock::now()) {
                Some(next) if d.auto_backup && !paused => {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("{}.", d.schedule.describe())).weak());
                        ui.label(RichText::new(format!("Next scheduled backup: {}", until(next))).strong());
                    });
                    ui.label(RichText::new("Scheduled backups only run while the drive is plugged in; a missed time is caught up as soon as it's plugged in again (plugging in always backs up anyway).").weak().small());
                }
                _ if paused && !d.schedule.is_off() => {
                    ui.colored_label(WARN, "Automatic backups are paused (Settings), so scheduled backups won't run.");
                }
                _ => {}
            }
        });

        // What goes to this drive
        ui.add_space(14.0);
        ui.heading("What goes to this drive");
        let names: Vec<String> = self.cfg.sources.iter().map(|s| s.name.clone()).collect();
        let all_sources = self.cfg.sources.clone();
        let sizes = self.sizes.lock().unwrap().clone();
        let d = self.cfg.drive_mut(id).unwrap();
        ui.horizontal(|ui| {
            if ui.radio(d.sources.is_none(), "Everything in \"What to back up\"").clicked() && d.sources.is_some() {
                d.sources = None;
                changed = true;
            }
            if ui.radio(d.sources.is_some(), "Only the sources I pick").clicked() && d.sources.is_none() {
                d.sources = Some(names.clone());
                changed = true;
            }
        });
        let mut total = 0u64;
        let mut unknown = false;
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_width(ui.available_width());
            if names.is_empty() {
                ui.label(RichText::new("No sources yet. Add some under \"What to back up\".").weak());
            }
            egui::Grid::new("drive_sources").num_columns(3).spacing([16.0, 6.0]).show(ui, |ui| {
                for s in &all_sources {
                    let included = s.enabled && d.sources.as_ref().is_none_or(|sel| sel.iter().any(|n| n.eq_ignore_ascii_case(&s.name)));
                    let mut on = included;
                    let enabled = d.sources.is_some() && s.enabled;
                    if ui.add_enabled(enabled, egui::Checkbox::new(&mut on, RichText::new(&s.name).strong())).changed() {
                        let sel = d.sources.get_or_insert_with(Vec::new);
                        if on {
                            sel.push(s.name.clone());
                        } else {
                            sel.retain(|n| !n.eq_ignore_ascii_case(&s.name));
                        }
                        changed = true;
                    }
                    ui.label(RichText::new(s.path.display().to_string()).weak());
                    match sizes.get(&s.name) {
                        Some((n, b)) => {
                            if included {
                                total += b;
                            }
                            ui.label(format!("{}, {}", files_word(*n), human_bytes(*b)));
                        }
                        None => {
                            unknown |= included;
                            ui.label(RichText::new("measuring...").weak());
                        }
                    }
                    ui.end_row();
                    if !s.enabled {
                        ui.label(RichText::new("  (turned off in What to back up)").weak().small());
                        ui.end_row();
                    }
                }
            });
        });
        if !unknown {
            let room = match (&conn, stats.as_ref().map(|s| s.bytes)) {
                (Some((_, Some((free, _)), _)), Some(on_drive)) => Some(free + on_drive),
                (Some((_, Some((free, _)), _)), None) => Some(*free),
                _ => None,
            };
            ui.horizontal(|ui| {
                ui.label(format!("Selected: {}", human_bytes(total)));
                if let Some(room) = room {
                    if total > room {
                        dot(ui, BAD);
                        ui.colored_label(BAD, format!("Won't fit: about {} available for SyncApp on this drive", human_bytes(room)));
                    } else {
                        dot(ui, OK);
                        ui.label(RichText::new(format!("Fits ({} available)", human_bytes(room))).weak());
                    }
                }
            });
        }

        // Danger zone
        ui.add_space(18.0);
        ui.separator();
        ui.horizontal(|ui| {
            if ui.button(RichText::new("Stop using this drive").color(BAD)).clicked() {
                self.confirm = Some(Confirm::Unregister { id: id.to_string(), name: reg.name.clone() });
            }
            ui.label(RichText::new("Backups already on the drive are kept.").weak());
        });
        ui.label(RichText::new(format!("Drive ID {}", reg.drive_id)).weak().small());

        if changed {
            self.save();
        }
        match (act, conn) {
            (1, Some((root, _, _))) => self.backup(ctx, root, reg),
            (2, Some((root, _, _))) => self.verify(ctx, root, reg),
            (3, Some((root, _, _))) => {
                self.browser = Some(Browser::new(ctx, &self.cfg, id.to_string(), root));
                self.tab = Tab::Restore;
            }
            (4, Some((root, _, _))) => win::shell_open(&Path::new(&root).join(engine::BACKUP_DIR)),
            (5, _) => self.open_last_verify(id),
            _ => {}
        }
    }

    fn ui_mark_dialog(&mut self, ctx: &egui::Context) {
        let Some(m) = &mut self.mark else { return };
        let mut result: Option<bool> = None;
        egui::Modal::new(egui::Id::unique("syncapp-mark")).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.heading(format!("Use {} as a backup drive", &m.root[..2]));
            ui.add_space(6.0);
            egui::Grid::new("mark_info").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                ui.label("Volume label:");
                ui.label(m.info.as_ref().map(|i| i.label.clone()).filter(|l| !l.is_empty()).unwrap_or("(none)".into()));
                ui.end_row();
                ui.label("File system:");
                let fs = m.info.as_ref().map(|i| i.fs_name.clone()).unwrap_or_default();
                if fs.eq_ignore_ascii_case("FAT32") {
                    ui.colored_label(WARN, "FAT32 (can't hold files over 4 GB; exFAT or NTFS recommended)");
                } else {
                    ui.label(fs);
                }
                ui.end_row();
                if let Some((free, total)) = m.space {
                    ui.label("Space:");
                    ui.label(format!("{} free of {}", human_bytes(free), human_bytes(total)));
                    ui.end_row();
                }
                ui.label("Name in SyncApp:");
                ui.add(egui::TextEdit::singleline(&mut m.name).hint_text("e.g. Blue USB stick").desired_width(240.0));
                ui.end_row();
            });
            ui.add_space(4.0);
            ui.checkbox(&mut m.auto_backup, "Back up automatically whenever it's plugged in");
            ui.add_space(8.0);
            ui.label(RichText::new(
                "SyncApp will add a hidden \".syncapp\" folder holding a random ID for this drive, and put your backups in a \"SyncApp Backup\" folder. Nothing else on the drive is touched.",
            ).weak());
            if !m.other_pcs.is_empty() {
                ui.label(RichText::new(format!(
                    "This drive is already used by SyncApp on {}. This PC will share the same backup folders: for each file, the newest version is kept, and an older copy never replaces a newer one.",
                    m.other_pcs.join(", ")
                )).color(WARN));
            } else if m.previously_marked {
                ui.label(RichText::new("This drive already has SyncApp backups from before; they'll be reused.").weak());
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button(RichText::new("Use this drive").strong()).clicked() {
                    result = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    result = Some(false);
                }
            });
        });
        let Some(yes) = result else { return };
        let m = self.mark.take().unwrap();
        if !yes {
            return;
        }
        let name = (!m.name.trim().is_empty()).then(|| m.name.clone());
        self.flush_save();
        match service::register_drive(&mut self.cfg, &m.root, name.as_deref()) {
            Ok(r) => {
                if let Some(d) = self.cfg.drive_mut(&r.drive_id) {
                    d.auto_backup = m.auto_backup;
                }
                self.save();
                self.flush_save();
                self.selected_drive = Some(r.drive_id.clone());
                crate::poke_tray(crate::tray::RELOAD_DRIVES);
                self.message = Some((format!("\"{}\" ({}) is now a backup drive. Click \"Back up now\" to make the first backup.", r.name, &m.root[..2]), false));
                self.refresh();
            }
            Err(e) => self.message = Some((e, true)),
        }
    }

    // ------------------------------------------------------------------ restore

    fn ui_restore(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        if self.connected.is_empty() {
            ui.heading("Restore files from a backup drive");
            ui.label("Connect a marked backup drive to browse and restore its files.");
            self.browser = None;
            return;
        }
        // Make sure the browser points at a connected drive.
        let valid = self.browser.as_ref().is_some_and(|b| self.connected_root(&b.drive_id).as_deref() == Some(b.root.as_str()));
        if !valid {
            let (c, r) = &self.connected[0];
            self.browser = Some(Browser::new(&ctx, &self.cfg, r.drive_id.clone(), c.root.clone()));
        }

        // Top bar: drive, search, filter.
        let mut switch_to = None;
        let mut reload = false;
        {
            let b = self.browser.as_mut().unwrap();
            b.poll();
            ui.horizontal(|ui| {
                ui.heading("Restore");
                ui.add_space(12.0);
                ui.label("From:");
                let cur = self.connected.iter().find(|(_, r)| r.drive_id == b.drive_id).map(|(c, r)| format!("{} ({})", r.name, &c.root[..2])).unwrap_or_default();
                egui::ComboBox::from_id_salt("rdrive").selected_text(cur).show_ui(ui, |ui| {
                    for (c, r) in &self.connected {
                        if ui.selectable_label(r.drive_id == b.drive_id, format!("{} ({})", r.name, &c.root[..2])).clicked() {
                            switch_to = Some((r.drive_id.clone(), c.root.clone()));
                        }
                    }
                });
                ui.add_space(12.0);
                let r = ui.add(egui::TextEdit::singleline(&mut b.search).hint_text("Search all backed-up files...").desired_width(240.0));
                if r.changed() {
                    b.rows_dirty = true;
                }
                if !b.search.is_empty() && ui.small_button("Clear").clicked() {
                    b.search.clear();
                    b.rows_dirty = true;
                }
                if ui.button("Refresh").on_hover_text("Re-read the drive and compare with this PC again").clicked() {
                    reload = true;
                }
            });
            ui.horizontal(|ui| {
                ui.label("Show:");
                let (all, missing, changed) = b.files.iter().fold((0, 0, 0), |a, f| {
                    (a.0 + 1, a.1 + (f.state == PcState::Missing) as usize, a.2 + (f.state == PcState::Changed) as usize)
                });
                for (f, label) in [
                    (RFilter::All, format!("All files ({all})")),
                    (RFilter::Missing, format!("Missing on this PC ({missing})")),
                    (RFilter::Changed, format!("Different on this PC ({changed})")),
                ] {
                    if ui.selectable_label(b.filter == f, label).clicked() && b.filter != f {
                        b.filter = f;
                        b.rows_dirty = true;
                    }
                }
            });
        }
        if let Some((id, root)) = switch_to {
            self.browser = Some(Browser::new(&ctx, &self.cfg, id, root));
            return;
        }
        if reload {
            let cfg = self.cfg.clone();
            self.browser.as_mut().unwrap().reload(&ctx, &cfg);
        }

        let busy = self.job_running();
        let mut do_restore: Option<(Vec<(String, PathBuf)>, String, bool)> = None;
        let b = self.browser.as_mut().unwrap();
        if b.rows_dirty {
            b.rebuild_rows();
        }

        // Bottom bar: selection summary and restore options.
        egui::Panel::bottom("restore_bar").show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.strong(format!("{} file(s) selected", b.selected.len()));
                ui.label(RichText::new(human_bytes(b.selected_bytes())).weak());
                if ui.small_button("Select all missing on PC").clicked() {
                    b.selected = b.files.iter().enumerate().filter(|(_, f)| f.state == PcState::Missing).map(|(i, _)| i).collect();
                }
                if !b.selected.is_empty() && ui.small_button("Clear selection").clicked() {
                    b.selected.clear();
                }
            });
            ui.horizontal(|ui| {
                ui.label("Restore to:");
                ui.radio_value(&mut b.to_original, true, "Where they came from");
                ui.radio_value(&mut b.to_original, false, "This folder:");
                ui.add_enabled(!b.to_original, egui::TextEdit::singleline(&mut b.target).desired_width(260.0));
                if ui.add_enabled(!b.to_original, egui::Button::new("Browse...")).clicked() {
                    if let Some(p) = rfd::FileDialog::new().pick_folder() {
                        b.target = p.display().to_string();
                    }
                }
            });
            ui.horizontal(|ui| {
                ui.checkbox(&mut b.overwrite, "Replace files on the PC that differ from the backup");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let can = !b.selected.is_empty() && !busy && (b.to_original || !b.target.trim().is_empty());
                    if ui.add_enabled(can, egui::Button::new(RichText::new(format!("Restore {} file(s)", b.selected.len())).strong())).clicked() {
                        let mut idx: Vec<usize> = b.selected.iter().copied().collect();
                        idx.sort();
                        let mut files = Vec::new();
                        let mut unknown = 0;
                        for i in idx {
                            let f = &b.files[i];
                            if b.to_original {
                                match &f.original {
                                    Some(p) => files.push((f.key.clone(), p.clone())),
                                    None => unknown += 1,
                                }
                            } else {
                                files.push((f.key.clone(), engine::dest_path(Path::new(b.target.trim()), &f.key)));
                            }
                        }
                        let desc = if b.to_original { "to original locations".to_string() } else { format!("to {}", b.target.trim()) };
                        if unknown > 0 {
                            do_restore = Some((vec![], format!("{unknown} selected file(s) belong to a source that's no longer in \"What to back up\", so their original location is unknown. Restore them to a folder instead."), false));
                        } else {
                            do_restore = Some((files, desc, b.overwrite));
                        }
                    }
                });
            });
            ui.add_space(4.0);
        });

        // Breadcrumb
        ui.add_space(4.0);
        let mut nav: Option<String> = None;
        ui.horizontal_wrapped(|ui| {
            if !b.search.trim().is_empty() {
                ui.label(format!("{} result(s) for \"{}\"", b.rows.len(), b.search.trim()));
                return;
            }
            if ui.link(RichText::new("All backups").strong()).clicked() {
                nav = Some(String::new());
            }
            let mut acc = String::new();
            for part in b.cwd.split('/').filter(|s| !s.is_empty()) {
                ui.label(">");
                if !acc.is_empty() {
                    acc.push('/');
                }
                acc.push_str(part);
                if ui.link(part).clicked() {
                    nav = Some(acc.clone());
                }
            }
        });
        ui.add_space(4.0);

        if b.loading() && b.files.is_empty() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Reading the backup and comparing it with this PC...");
            });
        } else if b.files.is_empty() {
            ui.label("There are no backed-up files on this drive yet.");
        } else {
            // Table (virtualised: only visible rows are drawn, so thousands of files are fine).
            let mut toggle_rows: Vec<usize> = Vec::new();
            let mut toggle_all = false;
            let in_view: Vec<usize> = b.rows.iter().flat_map(|r| b.row_files(r)).collect();
            let all_sel = !in_view.is_empty() && in_view.iter().all(|i| b.selected.contains(i));
            let some_sel = in_view.iter().any(|i| b.selected.contains(i));
            TableBuilder::new(ui)
                .id_salt("restore_table")
                .striped(true)
                .auto_shrink([false; 2])
                .sense(Sense::click())
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .column(Column::exact(28.0))
                .column(Column::remainder().at_least(220.0).clip(true))
                .column(Column::exact(190.0))
                .column(Column::exact(90.0))
                .column(Column::exact(170.0))
                .column(Column::exact(120.0).clip(true))
                .header(22.0, |mut h| {
                    h.col(|ui| {
                        let mut v = all_sel;
                        if ui.add(egui::Checkbox::without_text(&mut v).indeterminate(some_sel && !all_sel)).changed() {
                            toggle_all = true;
                        }
                    });
                    h.col(|ui| {
                        ui.strong("Name");
                    });
                    h.col(|ui| {
                        ui.strong("Compared with this PC");
                    });
                    h.col(|ui| {
                        ui.strong("Size");
                    });
                    h.col(|ui| {
                        ui.strong("File date");
                    });
                    h.col(|ui| {
                        ui.strong("Backed up from");
                    });
                })
                .body(|body| {
                    body.rows(ROW_H, b.rows.len(), |mut row| {
                        let ri = row.index();
                        let r = &b.rows[ri];
                        let files = b.row_files(r);
                        let n_sel = files.iter().filter(|i| b.selected.contains(i)).count();
                        row.set_selected(n_sel > 0 && n_sel == files.len());
                        row.col(|ui| {
                            let mut v = n_sel > 0 && n_sel == files.len();
                            if ui.add(egui::Checkbox::without_text(&mut v).indeterminate(n_sel > 0 && n_sel < files.len())).changed() {
                                toggle_rows.push(ri);
                            }
                        });
                        row.col(|ui| {
                            if r.dir {
                                if ui.link(RichText::new(format!("{}  >", r.name)).strong()).on_hover_text("Open folder").clicked() {
                                    nav = Some(r.key.clone());
                                }
                                ui.label(RichText::new(format!("{} files", r.count)).weak().small());
                            } else {
                                ui.label(&r.name).on_hover_text(&r.key);
                            }
                        });
                        row.col(|ui| {
                            if r.dir {
                                if r.missing > 0 {
                                    dot(ui, BAD);
                                    ui.label(format!("{} missing", r.missing));
                                }
                                if r.changed > 0 {
                                    dot(ui, WARN);
                                    ui.label(format!("{} different", r.changed));
                                }
                                if r.missing == 0 && r.changed == 0 {
                                    ui.label(RichText::new("All present").weak());
                                }
                            } else {
                                let (c, t) = match r.state {
                                    PcState::Same => (OK, "Same as on PC"),
                                    PcState::Changed => (WARN, "PC copy is different"),
                                    PcState::Missing => (BAD, "Missing on PC"),
                                    PcState::Unknown => (Color32::GRAY, "Source no longer set up"),
                                };
                                dot(ui, c);
                                ui.label(t);
                            }
                        });
                        row.col(|ui| {
                            ui.label(human_bytes(r.size));
                        });
                        row.col(|ui| {
                            ui.label(r.backed_up.map(win::local_time_string).unwrap_or_default());
                        });
                        row.col(|ui| {
                            if let Some(pc) = &r.from_pc {
                                ui.label(RichText::new(pc).weak());
                            }
                        });
                        if !r.dir && row.response().clicked() {
                            toggle_rows.push(ri);
                        }
                    });
                });
            // Apply selection changes.
            if toggle_all {
                if all_sel {
                    for i in &in_view {
                        b.selected.remove(i);
                    }
                } else {
                    b.selected.extend(in_view.iter().copied());
                }
            }
            toggle_rows.dedup();
            for ri in toggle_rows {
                let files = b.row_files(&b.rows[ri]);
                if files.iter().all(|i| b.selected.contains(i)) {
                    for i in files {
                        b.selected.remove(&i);
                    }
                } else {
                    b.selected.extend(files);
                }
            }
        }
        if let Some(p) = nav {
            b.cwd = p;
            b.search.clear();
            b.rows_dirty = true;
        }

        if let Some((files, desc, overwrite)) = do_restore {
            if files.is_empty() {
                self.message = Some((desc, true));
            } else if overwrite {
                self.confirm = Some(Confirm::RestoreOverwrite(files, desc));
            } else {
                self.restore_files(&ctx, files, desc);
            }
        }
    }

    // ------------------------------------------------------------------ updates

    fn check_for_update(&mut self, ctx: &egui::Context) {
        if self.update_check.is_some() {
            return;
        }
        let slot = Arc::new(Mutex::new(None));
        let (s2, ctx) = (slot.clone(), ctx.clone());
        std::thread::spawn(move || {
            *s2.lock().unwrap() = Some(syncapp::update::check());
            ctx.request_repaint();
        });
        self.update_check = Some(slot);
        self.update_status = Some(("Checking GitHub for updates...".into(), false));
    }

    fn poll_update_check(&mut self) {
        let done = self.update_check.as_ref().and_then(|s| s.lock().unwrap().take());
        if let Some(res) = done {
            self.update_check = None;
            self.update_state = syncapp::update::load_state();
            self.update_banner_hidden = false;
            self.update_status = Some(match res {
                Ok(Some(r)) => (format!("SyncApp {} is available.", r.version), false),
                Ok(None) => (format!("You have the latest version ({}).", syncapp::update::CURRENT), false),
                Err(e) => (format!("Couldn't check for updates: {e}"), true),
            });
        }
    }

    fn start_update(&mut self, ctx: &egui::Context) {
        let Some(rel) = self.update_state.newer().cloned() else { return };
        let ready = self.update_ready.clone();
        self.start_job(ctx, JobKind::Update, format!("Downloading SyncApp {}", rel.version), move |cancel, cb| {
            cb(Event::Planned { files: 1, bytes: rel.size });
            cb(Event::File { index: 1, total: 1, key: "SyncApp.exe" });
            let mut progress = |done: u64, total: u64| cb(Event::Bytes { done, total: total.max(rel.size) });
            match syncapp::update::download(&rel, cancel, &mut progress) {
                Ok(path) => {
                    *ready.lock().unwrap() = Some(path);
                    JobResult {
                        ok: true,
                        msg: format!("SyncApp {} downloaded and verified.", rel.version),
                        errors: vec![],
                        verify: None,
                    }
                }
                Err(e) => JobResult { ok: false, msg: format!("Update failed: {e}"), errors: vec![], verify: None },
            }
        });
    }

    fn ui_update_banner(&mut self, ui: &mut egui::Ui) {
        if self.update_banner_hidden || self.job.as_ref().is_some_and(|j| j.kind == JobKind::Update) {
            return;
        }
        let Some(rel) = self.update_state.newer().cloned() else { return };
        let ctx = ui.ctx().clone();
        let busy = self.job_running();
        egui::Frame::group(ui.style()).fill(ui.visuals().selection.bg_fill.gamma_multiply(0.25)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                dot(ui, OK);
                ui.label(RichText::new(format!("SyncApp {} is available", rel.version)).strong());
                ui.label(RichText::new(format!("(you have {})", syncapp::update::CURRENT)).weak());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("Later").clicked() {
                        self.update_banner_hidden = true;
                    }
                    if ui.small_button("What's new").clicked() {
                        win::shell_open(Path::new(&rel.page_url));
                    }
                    if ui.add_enabled(!busy, egui::Button::new(RichText::new("Update now").strong())).clicked() {
                        self.start_update(&ctx);
                    }
                });
            });
        });
        ui.add_space(6.0);
    }

    // ------------------------------------------------------------------ clean-up

    fn start_cleanup(&mut self, ctx: &egui::Context, keys: Vec<String>) {
        let Some(c) = &self.cleanup else { return };
        let Some(reg) = self.cfg.drive(&c.drive_id).cloned() else { return };
        let root = c.root.clone();
        self.start_job(ctx, JobKind::Cleanup, format!("Cleaning up {}", reg.name), move |cancel, cb| {
            match service::clean_up(&root, &reg, &keys, cancel, Some(cb)) {
                Ok(r) => {
                    let msg = match &r.aborted {
                        Some(a) => format!("Clean-up stopped: {a}. Removed {} ({}).", files_word(r.removed), human_bytes(r.bytes)),
                        None => format!("Removed {} from {}, freeing {}.", files_word(r.removed), reg.name, human_bytes(r.bytes)),
                    };
                    JobResult { ok: r.failed.is_empty() && r.aborted.is_none(), msg, errors: r.failed, verify: None }
                }
                Err(e) => JobResult { ok: false, msg: e, errors: vec![], verify: None },
            }
        });
    }

    fn ui_cleanup(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.heading("Clean up");
        ui.label("Files on the backup drive that no PC needs any more: deleted, renamed or moved on every PC that had them, or from folders you no longer back up. SyncApp never deletes them on its own; review them here and remove the ones you don't want.");
        ui.add_space(6.0);
        if self.connected.is_empty() {
            ui.label("Connect a marked backup drive to clean it up.");
            self.cleanup = None;
            return;
        }
        let valid = self.cleanup.as_ref().is_some_and(|c| self.connected_root(&c.drive_id).as_deref() == Some(c.root.as_str()));
        if !valid {
            let (c, r) = &self.connected[0];
            self.cleanup = Some(Cleanup::new(&ctx, &self.cfg, r.drive_id.clone(), c.root.clone()));
        }
        let mut switch_to = None;
        let mut rescan = false;
        let busy = self.job_running();
        {
            let c = self.cleanup.as_mut().unwrap();
            c.poll();
            ui.horizontal(|ui| {
                ui.label("Drive:");
                let cur = self
                    .connected
                    .iter()
                    .find(|(_, r)| r.drive_id == c.drive_id)
                    .map(|(d, r)| format!("{} ({})", r.name, &d.root[..2]))
                    .unwrap_or_default();
                egui::ComboBox::from_id_salt("cdrive").selected_text(cur).show_ui(ui, |ui| {
                    for (d, r) in &self.connected {
                        if ui.selectable_label(r.drive_id == c.drive_id, format!("{} ({})", r.name, &d.root[..2])).clicked() {
                            switch_to = Some((r.drive_id.clone(), d.root.clone()));
                        }
                    }
                });
                if ui.add_enabled(c.loader.is_none(), egui::Button::new("Scan again")).clicked() {
                    rescan = true;
                }
            });
        }
        if let Some((id, root)) = switch_to {
            self.cleanup = Some(Cleanup::new(&ctx, &self.cfg, id, root));
            return;
        }
        if rescan {
            let cfg = self.cfg.clone();
            self.cleanup.as_mut().unwrap().scan(&ctx, &cfg);
        }
        let c = self.cleanup.as_mut().unwrap();
        ui.add_space(6.0);
        if !c.scanned {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Looking for files no PC needs...");
            });
            return;
        }
        if c.items.is_empty() {
            ui.horizontal(|ui| {
                dot(ui, OK);
                ui.label("Nothing to clean up: every file on this drive still exists on at least one PC.");
            });
            return;
        }
        let total: u64 = c.items.iter().map(|o| o.size).sum();
        let mut confirm = None;
        egui::Panel::bottom("cleanup_bar").show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.strong(format!("{} selected", files_word(c.selected.len() as u64)));
                ui.label(RichText::new(human_bytes(c.selected_bytes())).weak());
                if ui.small_button("Select all").clicked() {
                    c.selected = (0..c.items.len()).collect();
                }
                if ui.small_button("Select none").clicked() {
                    c.selected.clear();
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let label = format!("Delete {} from drive...", files_word(c.selected.len() as u64));
                    if ui.add_enabled(!c.selected.is_empty() && !busy, egui::Button::new(RichText::new(label).color(BAD))).clicked() {
                        let mut idx: Vec<usize> = c.selected.iter().copied().collect();
                        idx.sort();
                        let keys = idx.iter().map(|&i| c.items[i].key.clone()).collect();
                        confirm = Some(Confirm::Cleanup(keys, c.selected_bytes()));
                    }
                });
            });
            ui.add_space(4.0);
        });
        ui.label(format!("{} can be removed, freeing {}.", files_word(c.items.len() as u64), human_bytes(total)));
        ui.add_space(4.0);
        let mut toggle = None;
        let all = c.selected.len() == c.items.len();
        let mut toggle_all = false;
        TableBuilder::new(ui)
            .id_salt("cleanup_table")
            .striped(true)
            .auto_shrink([false; 2])
            .sense(Sense::click())
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::exact(28.0))
            .column(Column::remainder().at_least(220.0).clip(true))
            .column(Column::exact(230.0))
            .column(Column::exact(90.0))
            .column(Column::exact(170.0))
            .header(22.0, |mut h| {
                h.col(|ui| {
                    let mut v = all;
                    if ui.add(egui::Checkbox::without_text(&mut v).indeterminate(!all && !c.selected.is_empty())).changed() {
                        toggle_all = true;
                    }
                });
                for t in ["File", "Why it's no longer needed", "Size", "Last version"] {
                    h.col(|ui| {
                        ui.strong(t);
                    });
                }
            })
            .body(|body| {
                body.rows(ROW_H, c.items.len(), |mut row| {
                    let i = row.index();
                    let o = &c.items[i];
                    let sel = c.selected.contains(&i);
                    row.set_selected(sel);
                    row.col(|ui| {
                        let mut v = sel;
                        if ui.add(egui::Checkbox::without_text(&mut v)).changed() {
                            toggle = Some(i);
                        }
                    });
                    row.col(|ui| {
                        ui.label(&o.key).on_hover_text(&o.key);
                    });
                    row.col(|ui| {
                        ui.label(match o.reason {
                            engine::OrphanReason::GoneEverywhere => "Deleted or moved on every PC",
                            engine::OrphanReason::SourceRemoved => "Folder no longer backed up",
                        });
                    });
                    row.col(|ui| {
                        ui.label(human_bytes(o.size));
                    });
                    row.col(|ui| {
                        let date = o.modified.map(win::local_time_string).unwrap_or_default();
                        match &o.last_pc {
                            Some(pc) => ui.label(format!("{date}, {pc}")),
                            None => ui.label(date),
                        };
                    });
                    if row.response().clicked() {
                        toggle = Some(i);
                    }
                });
            });
        if toggle_all {
            if all {
                c.selected.clear();
            } else {
                c.selected = (0..c.items.len()).collect();
            }
        }
        if let Some(i) = toggle {
            if !c.selected.remove(&i) {
                c.selected.insert(i);
            }
        }
        if confirm.is_some() {
            self.confirm = confirm;
        }
    }

    // ------------------------------------------------------------------ verification report

    fn ui_verify_report(&mut self, ui: &mut egui::Ui) {
        let v = self.verify_view.as_mut().unwrap();
        let r = &v.report;
        let mut close = false;
        ui.horizontal(|ui| {
            if ui.button("< Back").clicked() {
                close = true;
            }
            ui.heading(format!("Verification report: {}", v.drive_name));
            ui.label(RichText::new(when(v.finished_at)).weak());
        });
        ui.add_space(6.0);

        // Verdict
        let problems = r.problems();
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let (c, t) = if let Some(a) = &r.aborted {
                    (WARN, format!("Verification was stopped early ({a}). Files checked so far are listed below."))
                } else if problems == 0 {
                    (OK, format!("All {} on the drive are bit-for-bit identical to what was backed up.", files_word(r.ok)))
                } else {
                    (BAD, format!("{problems} file(s) failed the check. They'll be re-copied from your PC on the next backup."))
                };
                dot(ui, c);
                ui.label(RichText::new(t).size(16.0).strong());
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                for (k, val) in [
                    ("Files checked", r.items.len().to_string()),
                    ("Passed", r.ok.to_string()),
                    ("Problems", problems.to_string()),
                    ("Data read", human_bytes(r.bytes_read)),
                    ("Read speed", format!("{:.1} MB/s", r.mb_per_s())),
                    ("Took", format!("{:.1} s", r.duration_ms as f64 / 1000.0)),
                ] {
                    ui.allocate_ui(egui::vec2(130.0, 46.0), |ui| {
                        ui.set_min_width(130.0);
                        ui.vertical(|ui| {
                            ui.label(RichText::new(k).weak().small());
                            ui.add(egui::Label::new(RichText::new(val).size(18.0).strong()).extend());
                        });
                    });
                }
            });
        });

        // How it was checked
        egui::CollapsingHeader::new(RichText::new("How SyncApp checked this").strong()).default_open(true).show(ui, |ui| {
            ui.label("1. When each file was backed up, SyncApp computed its BLAKE3 checksum: a 256-bit fingerprint of the file's contents. Changing even a single bit of the file produces a completely different checksum. The checksum was saved on the drive in .syncapp\\manifest.json.");
            ui.label(if r.direct_io {
                "2. Just now, every file was read back in full directly from the drive, bypassing Windows' memory cache, so what was checked is what is physically stored on the drive."
            } else {
                "2. Just now, every file was read back in full from the drive. (This drive's file system didn't allow bypassing Windows' cache for some files.)"
            });
            ui.label("3. A new checksum was computed from those bytes and compared with the recorded one. Identical checksums mean the file is exactly as it was when backed up.");
            ui.label("4. Any file that didn't match, couldn't be read, or was missing is forgotten by SyncApp, so the next backup copies it again from your PC.");
            if !r.untracked.is_empty() {
                ui.label(RichText::new(format!(
                    "Note: {} file(s) in the backup folder weren't put there by SyncApp, so there's no recorded checksum to compare against: {}",
                    r.untracked.len(),
                    r.untracked.iter().take(5).cloned().collect::<Vec<_>>().join(", ")
                )).color(WARN));
            }
        });
        ui.add_space(6.0);

        // Per-file table
        ui.horizontal(|ui| {
            if ui.add(egui::TextEdit::singleline(&mut v.search).hint_text("Filter files...").desired_width(240.0)).changed() {
                v.dirty = true;
            }
            if ui.checkbox(&mut v.only_problems, "Only show problems").changed() {
                v.dirty = true;
            }
            ui.label(RichText::new("Hover a checksum to see all 64 characters").weak().small());
        });
        if v.dirty {
            v.rebuild();
        }
        let r = &v.report;
        if v.rows.is_empty() {
            ui.label(RichText::new(if v.only_problems { "No problems to show." } else { "No files match." }).weak());
        } else {
            TableBuilder::new(ui)
                .id_salt("verify_table")
                .striped(true)
                .auto_shrink([false; 2])
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .column(Column::exact(120.0))
                .column(Column::remainder().at_least(200.0).clip(true))
                .column(Column::exact(80.0))
                .column(Column::exact(170.0))
                .column(Column::exact(170.0))
                .header(22.0, |mut h| {
                    for t in ["Result", "File", "Size", "Checksum at backup", "Checksum read now"] {
                        h.col(|ui| {
                            ui.strong(t);
                        });
                    }
                })
                .body(|body| {
                    body.rows(ROW_H, v.rows.len(), |mut row| {
                        let it = &r.items[v.rows[row.index()]];
                        row.col(|ui| {
                            let (c, t) = match it.status {
                                VerifyStatus::Ok => (OK, "Match"),
                                VerifyStatus::Corrupted => (BAD, "MISMATCH"),
                                VerifyStatus::Missing => (BAD, "Missing"),
                                VerifyStatus::Unreadable => (BAD, "Read error"),
                            };
                            dot(ui, c);
                            let l = ui.label(t);
                            if let Some(e) = &it.error {
                                l.on_hover_text(e);
                            }
                        });
                        row.col(|ui| {
                            ui.label(&it.key).on_hover_text(&it.key);
                        });
                        row.col(|ui| {
                            ui.label(human_bytes(it.size));
                        });
                        row.col(|ui| {
                            ui.monospace(short_hash(&it.expected)).on_hover_text(&it.expected);
                        });
                        row.col(|ui| match &it.actual {
                            Some(a) => {
                                let t = RichText::new(short_hash(a)).monospace();
                                let t = if *a == it.expected { t } else { t.color(BAD) };
                                ui.label(t).on_hover_text(a);
                            }
                            None => {
                                ui.label(RichText::new("-").weak());
                            }
                        });
                    });
                });
        }
        if close {
            self.verify_view = None;
        }
    }

    // ------------------------------------------------------------------ history

    fn ui_history(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("History");
            if ui.button("Open log file").clicked() {
                win::shell_open(&syncapp::log::log_path());
            }
        });
        if self.history.is_empty() {
            ui.label("Nothing yet.");
            return;
        }
        let mut open_verify = None;
        egui::Grid::new("hist").striped(true).num_columns(7).spacing([14.0, 4.0]).show(ui, |ui| {
            for h in ["", "When", "What", "Drive", "Updated", "Unchanged / OK", "Size"] {
                ui.strong(h);
            }
            ui.end_row();
            let n = self.history.len();
            for (i, h) in self.history.iter().rev().enumerate().take(300) {
                let idx = n - 1 - i;
                dot(ui, if h.ok { OK } else { WARN });
                if ui.selectable_label(self.history_selected == Some(idx), win::local_time_string(h.finished_at)).clicked() {
                    self.history_selected = if self.history_selected == Some(idx) { None } else { Some(idx) };
                }
                ui.label(&h.kind);
                ui.label(&h.drive_name);
                ui.label(h.copied.to_string());
                ui.label(h.unchanged.to_string());
                ui.label(human_bytes(h.bytes));
                ui.end_row();
                if self.history_selected == Some(idx) {
                    ui.label("");
                    ui.vertical(|ui| {
                        if !h.detail.is_empty() {
                            ui.label(&h.detail);
                        }
                        ui.label(format!("Took {:.1}s", h.duration_ms as f64 / 1000.0));
                        if h.errors.is_empty() {
                            ui.label("No problems.");
                        }
                        for (k, e) in &h.errors {
                            ui.colored_label(WARN, format!("{k}: {e}"));
                        }
                        if h.kind == "verify" && ui.small_button("Open latest report for this drive").clicked() {
                            open_verify = Some(h.drive_id.clone());
                        }
                    });
                    ui.end_row();
                }
            }
        });
        if let Some(id) = open_verify {
            self.open_last_verify(&id);
        }
    }

    // ------------------------------------------------------------------ settings

    fn ui_settings(&mut self, ui: &mut egui::Ui) {
        ui.heading("Settings");
        ui.add_space(6.0);
        if ui.checkbox(&mut self.cfg.notifications, "Show a notification when a backup starts and finishes").changed() {
            self.save();
        }
        ui.label(RichText::new("Problems are always notified.").weak().small());
        ui.add_space(4.0);
        if ui.checkbox(&mut self.cfg.paused, "Pause automatic backups").changed() {
            self.save();
        }
        ui.add_space(4.0);
        if ui
            .checkbox(&mut self.cfg.skip_on_battery_saver, "Skip scheduled backups while Battery saver is on")
            .on_hover_text("Backups when a drive is plugged in still happen.")
            .changed()
        {
            self.save();
        }
        ui.add_space(4.0);
        if ui.checkbox(&mut self.autostart, "Start SyncApp's background service when I sign in to Windows").changed() {
            let exe = std::env::current_exe().unwrap_or_default();
            let cmd = format!("\"{}\" --tray", exe.display());
            if let Err(e) = win::set_autostart(self.autostart.then_some(cmd.as_str())) {
                self.message = Some((format!("Couldn't change startup setting: {e}"), true));
                self.autostart = !self.autostart;
            }
        }
        if let Some(cmd) = win::get_autostart() {
            ui.label(RichText::new(format!("Starts: {cmd}")).weak().small());
        }

        ui.add_space(14.0);
        ui.heading("Updates");
        let ctx = ui.ctx().clone();
        ui.label(format!("Version {}", syncapp::update::CURRENT));
        if ui
            .checkbox(&mut self.cfg.check_updates, "Check GitHub for updates once a day and notify me")
            .on_hover_text("Only asks GitHub for the latest release; nothing is installed without your click.")
            .changed()
        {
            self.save();
        }
        ui.horizontal(|ui| {
            if ui.add_enabled(self.update_check.is_none(), egui::Button::new("Check now")).clicked() {
                self.check_for_update(&ctx);
            }
            if let Some(rel) = self.update_state.newer().cloned() {
                if ui.add_enabled(!self.job_running(), egui::Button::new(RichText::new(format!("Update to {}", rel.version)).strong())).clicked() {
                    self.start_update(&ctx);
                }
                if ui.button("What's new").clicked() {
                    win::shell_open(Path::new(&rel.page_url));
                }
            }
            if ui.button("All releases").clicked() {
                win::shell_open(Path::new(&syncapp::update::releases_page()));
            }
        });
        if let Some((m, bad)) = &self.update_status {
            ui.colored_label(if *bad { WARN } else { ui.visuals().text_color() }, m);
        } else if self.update_state.checked_at > 0 {
            ui.label(RichText::new(format!("Last checked {}", when(self.update_state.checked_at))).weak().small());
        }
        if let Some(rel) = self.update_state.newer() {
            if !rel.notes.trim().is_empty() {
                egui::CollapsingHeader::new("Release notes").show(ui, |ui| {
                    ui.label(rel.notes.trim());
                });
            }
        }
        ui.label(RichText::new("Updates come only from this project's GitHub releases and must match the published SHA-256 checksum.").weak().small());

        ui.add_space(14.0);
        ui.heading("Install");
        let exe = std::env::current_exe().unwrap_or_default();
        let target = crate::install_dir().join(crate::EXE_NAME);
        ui.label(format!("Running from: {}", exe.display()));
        if exe != target {
            ui.label("Install SyncApp to your user profile so it has a permanent home, a Start menu entry, and starts with Windows.");
            if ui.button(format!("Install to {}", target.parent().unwrap().display())).clicked() {
                match crate::install() {
                    Ok(msg) => {
                        self.message = Some((msg, false));
                        self.autostart = true;
                    }
                    Err(e) => self.message = Some((format!("Install failed: {e}"), true)),
                }
            }
        } else {
            ui.horizontal(|ui| {
                dot(ui, OK);
                ui.label("Installed");
            });
        }

        ui.add_space(8.0);
        if ui.button("Uninstall SyncApp...").clicked() {
            self.uninstall_remove_data = false;
            self.confirm = Some(Confirm::Uninstall);
        }

        ui.add_space(14.0);
        ui.heading("Files");
        ui.horizontal(|ui| {
            if ui.button("Open settings folder").clicked() {
                win::shell_open(&syncapp::config::config_dir());
            }
            if ui.button("Open log file").clicked() {
                win::shell_open(&syncapp::log::log_path());
            }
        });

        ui.add_space(14.0);
        ui.heading("How your files are protected");
        for line in [
            "- Only drives you mark are used. A copied ID file isn't enough: the volume serial must match too.",
            "- Files on your PC are only read, never changed, during a backup.",
            "- Nothing on the backup drive is deleted unless you choose to in Clean up. Files you delete on the PC stay in the backup until then.",
            "- Every copy is written to a temp file, flushed to the drive, read back directly from the drive, checksum-verified, and only then swapped in. Unplugging mid-backup can't damage an existing backup.",
            "- If nothing changed, nothing is written to the drive.",
            "- For encryption, turn on BitLocker To Go for the drive (right-click it in Explorer > Turn on BitLocker). SyncApp works unchanged on encrypted drives.",
        ] {
            ui.label(line);
        }
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
