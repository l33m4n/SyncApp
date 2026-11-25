//! Restore tab: browse the backup, compare with this PC, restore selected files.

use super::*;

// ------------------------------------------------------------------ restore browser

#[derive(Clone, Copy, PartialEq)]
pub(super) enum PcState {
    Same,
    Changed,
    Missing,
    Unknown,
}

pub(super) struct RFile {
    pub(super) key: String,
    pub(super) key_lc: String,
    pub(super) size: u64,
    pub(super) backed_up: Option<u64>, // unix seconds of the PC file's date at backup time
    pub(super) original: Option<PathBuf>,
    pub(super) state: PcState,
    pub(super) from_pc: Option<String>,
}

#[derive(Clone, Copy, PartialEq)]
pub(super) enum RFilter {
    All,
    Missing,
    Changed,
}

pub(super) struct Row {
    pub(super) dir: bool,
    pub(super) name: String,
    pub(super) key: String,
    pub(super) range: (usize, usize),
    pub(super) size: u64,
    pub(super) count: usize,
    pub(super) missing: usize,
    pub(super) changed: usize,
    pub(super) state: PcState,
    pub(super) backed_up: Option<u64>,
    pub(super) from_pc: Option<String>,
}

pub(super) type Loaded = (Vec<RFile>, Vec<(String, String)>);

pub(super) struct Browser {
    pub(super) drive_id: String,
    pub(super) root: String,
    /// Sources on the drive this PC doesn't have yet, and the PC they come from.
    pub(super) adoptable: (Vec<syncapp::config::Source>, String),
    pub(super) loader: Option<Arc<Mutex<Option<Loaded>>>>,
    pub(super) files: Vec<RFile>,
    pub(super) errors: Vec<(String, String)>,
    pub(super) cwd: String,
    pub(super) search: String,
    pub(super) filter: RFilter,
    pub(super) selected: HashSet<usize>,
    pub(super) rows: Vec<Row>,
    pub(super) rows_dirty: bool,
    pub(super) to_original: bool,
    pub(super) target: String,
    pub(super) overwrite: bool,
}

impl Browser {
    pub(super) fn new(ctx: &egui::Context, cfg: &Config, drive_id: String, root: String) -> Self {
        let adoptable = service::sources_to_adopt(cfg, Path::new(&root));
        let mut b = Browser {
            adoptable,
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

    pub(super) fn reload(&mut self, ctx: &egui::Context, cfg: &Config) {
        let slot = Arc::new(Mutex::new(None));
        let (s2, root, cfg, ctx) = (slot.clone(), self.root.clone(), cfg.clone(), ctx.clone());
        std::thread::spawn(move || {
            let (list, errors) = engine::list_backup(Path::new(&root));
            // Sources this PC doesn't have, as another PC set them up (for restoring onto a new PC).
            let (others, _) = service::sources_to_adopt(&cfg, Path::new(&root));
            let files = list
                .into_iter()
                .map(|f| {
                    let original = service::original_path(&cfg, &f.key)
                        .or_else(|| service::original_path_from(&others, &f.key));
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
    pub(super) fn poll(&mut self) {
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

    pub(super) fn loading(&self) -> bool {
        self.loader.is_some()
    }

    pub(super) fn matches(&self, f: &RFile) -> bool {
        match self.filter {
            RFilter::All => true,
            RFilter::Missing => f.state == PcState::Missing,
            RFilter::Changed => f.state == PcState::Changed,
        }
    }

    pub(super) fn rebuild_rows(&mut self) {
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
    pub(super) fn row_files(&self, r: &Row) -> Vec<usize> {
        (r.range.0..r.range.1).filter(|&i| self.matches(&self.files[i])).collect()
    }

    pub(super) fn selected_bytes(&self) -> u64 {
        self.selected.iter().map(|&i| self.files[i].size).sum()
    }
}

pub(super) fn file_row(f: &RFile, i: usize, name: String) -> Row {
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

impl App {
    pub(super) fn restore_files(&mut self, ctx: &egui::Context, files: Vec<(String, PathBuf)>, description: String) {
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

    // ------------------------------------------------------------------ restore
    pub(super) fn ui_restore(&mut self, ui: &mut egui::Ui) {
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

        // Restoring onto a new PC: offer to set it up like the PC the backups came from.
        let mut adopt = false;
        {
            let b = self.browser.as_ref().unwrap();
            if !b.adoptable.0.is_empty() {
                egui::Frame::group(ui.style()).fill(ui.visuals().selection.bg_fill.gamma_multiply(0.2)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    let names: Vec<&str> = b.adoptable.0.iter().map(|s| s.name.as_str()).collect();
                    ui.label(format!(
                        "This drive has backups from {} that aren't set up on this PC: {}.",
                        b.adoptable.1,
                        names.join(", ")
                    ));
                    ui.horizontal(|ui| {
                        if ui.button(RichText::new(format!("Set this PC up like {}", b.adoptable.1)).strong()).clicked() {
                            adopt = true;
                        }
                        ui.label(RichText::new("Adds the same folders to \"What to back up\". Then select \"Missing on this PC\" and restore to where they came from.").weak().small());
                    });
                });
                ui.add_space(4.0);
            }
        }
        if adopt {
            let (sources, from) = self.browser.as_ref().unwrap().adoptable.clone();
            let n = sources.len();
            for s in sources {
                if !self.cfg.sources.iter().any(|c| c.name.eq_ignore_ascii_case(&s.name)) {
                    self.cfg.sources.push(s);
                }
            }
            self.save();
            self.flush_save();
            self.message = Some((format!("Added {n} folder(s) from {from}. Files missing on this PC can now be restored to where they came from."), false));
            let (id, root) = { let b = self.browser.as_ref().unwrap(); (b.drive_id.clone(), b.root.clone()) };
            self.browser = Some(Browser::new(&ctx, &self.cfg, id, root));
            return;
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
}
