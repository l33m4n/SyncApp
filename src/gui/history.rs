//! History tab.

use super::*;

impl App {
    // ------------------------------------------------------------------ history
    pub(super) fn ui_history(&mut self, ui: &mut egui::Ui) {
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
}
