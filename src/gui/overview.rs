//! Overview tab: each backup drive at a glance.

use super::*;

impl App {
    // ------------------------------------------------------------------ overview
    pub(super) fn ui_overview(&mut self, ui: &mut egui::Ui) {
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
        let last_backups: HashMap<String, u64> = self
            .history
            .iter()
            .filter(|h| h.kind == "backup")
            .map(|h| (h.drive_id.clone(), h.finished_at))
            .collect();
        let overdue = syncapp::planner::overdue(&self.cfg, &last_backups, win::now_unix());
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
                if let Some(days) = overdue.iter().find(|(id, _)| *id == reg.drive_id).map(|x| x.1) {
                    ui.horizontal(|ui| {
                        dot(ui, WARN);
                        ui.colored_label(WARN, format!("Not backed up for {days} day(s). Plug it in to back up."));
                    });
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
}
