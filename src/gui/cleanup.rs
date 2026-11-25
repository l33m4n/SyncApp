//! Clean up tab: files no PC needs any more.

use super::*;

// ------------------------------------------------------------------ clean-up

pub(super) struct Cleanup {
    pub(super) drive_id: String,
    pub(super) root: String,
    pub(super) loader: Option<Arc<Mutex<Option<Vec<engine::Orphan>>>>>,
    pub(super) items: Vec<engine::Orphan>,
    pub(super) selected: HashSet<usize>,
    pub(super) scanned: bool,
}

impl Cleanup {
    pub(super) fn new(ctx: &egui::Context, cfg: &Config, drive_id: String, root: String) -> Self {
        let mut c = Cleanup { drive_id, root, loader: None, items: vec![], selected: HashSet::new(), scanned: false };
        c.scan(ctx, cfg);
        c
    }

    pub(super) fn scan(&mut self, ctx: &egui::Context, cfg: &Config) {
        let slot = Arc::new(Mutex::new(None));
        let (s2, root, cfg, ctx) = (slot.clone(), self.root.clone(), cfg.clone(), ctx.clone());
        std::thread::spawn(move || {
            let found = service::find_orphans(&cfg, &root);
            *s2.lock().unwrap() = Some(found);
            ctx.request_repaint();
        });
        self.loader = Some(slot);
    }

    pub(super) fn poll(&mut self) {
        let done = self.loader.as_ref().and_then(|l| l.lock().unwrap().take());
        if let Some(items) = done {
            self.items = items;
            self.loader = None;
            self.scanned = true;
            self.selected = (0..self.items.len()).collect(); // all selected by default
        }
    }

    pub(super) fn selected_bytes(&self) -> u64 {
        self.selected.iter().map(|&i| self.items[i].size).sum()
    }
}

impl App {
    // ------------------------------------------------------------------ clean-up
    pub(super) fn start_cleanup(&mut self, ctx: &egui::Context, keys: Vec<String>) {
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

    pub(super) fn ui_cleanup(&mut self, ui: &mut egui::Ui) {
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
}
