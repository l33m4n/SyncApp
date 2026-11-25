//! Verification report page.

use super::*;

// ------------------------------------------------------------------ verify report view

pub(super) struct VerifyView {
    pub(super) drive_name: String,
    pub(super) finished_at: u64,
    pub(super) report: VerifyReport,
    pub(super) search: String,
    pub(super) only_problems: bool,
    pub(super) rows: Vec<usize>,
    pub(super) dirty: bool,
}

impl VerifyView {
    pub(super) fn new(drive_name: String, finished_at: u64, report: VerifyReport) -> Self {
        let only_problems = report.problems() > 0;
        VerifyView { drive_name, finished_at, report, search: String::new(), only_problems, rows: vec![], dirty: true }
    }

    pub(super) fn rebuild(&mut self) {
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

impl App {
    pub(super) fn open_last_verify_any(&mut self) {
        if let Some(r) = self.history.iter().rev().find(|h| h.kind == "verify") {
            self.open_last_verify(&r.drive_id.clone());
        }
    }

    pub(super) fn open_last_verify(&mut self, drive_id: &str) {
        match service::last_verify(drive_id) {
            Some(s) => self.verify_view = Some(VerifyView::new(s.drive_name, s.finished_at, s.report)),
            None => self.message = Some(("No saved verification report for this drive yet.".into(), true)),
        }
    }

    // ------------------------------------------------------------------ verification report
    pub(super) fn ui_verify_report(&mut self, ui: &mut egui::Ui) {
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
        egui::CollapsingHeader::new(RichText::new("How SyncApp checked this").strong()).default_open(false).show(ui, |ui| {
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
}
