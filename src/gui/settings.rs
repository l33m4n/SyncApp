//! Settings tab, including updates.

use super::*;

impl App {
    // ------------------------------------------------------------------ updates
    pub(super) fn check_for_update(&mut self, ctx: &egui::Context) {
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

    pub(super) fn poll_update_check(&mut self) {
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

    pub(super) fn start_update(&mut self, ctx: &egui::Context) {
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

    pub(super) fn ui_update_banner(&mut self, ui: &mut egui::Ui) {
        if self.update_banner_hidden || self.job.as_ref().is_some_and(|j| j.kind == JobKind::Update) {
            return;
        }
        // Development builds only show it after a manual check this session.
        if syncapp::update::is_dev_build() && self.update_status.is_none() {
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
                ui.label(RichText::new(format!("(you have {})", syncapp::update::version_label())).weak());
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

    // ------------------------------------------------------------------ settings
    pub(super) fn ui_settings(&mut self, ui: &mut egui::Ui) {
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
        ui.label(format!("Version {}", syncapp::update::version_label()));
        if syncapp::update::is_dev_build() {
            ui.label(RichText::new("Development builds don't check for updates automatically; use Check now.").weak().small());
        }
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
        ui.label(RichText::new("Updates come only from this project's GitHub releases, and must match the published SHA-256 checksum and carry the project's digital signature.").weak().small());

        ui.add_space(14.0);
        ui.heading("Install");
        let exe = std::env::current_exe().unwrap_or_default();
        let target = crate::install_dir().join(crate::EXE_NAME);
        ui.label(format!("Running from: {}", exe.display()));
        if exe != target {
            ui.label("Install SyncApp to your user profile so it has a permanent home, a Start menu entry, and starts with Windows.");
            if ui.button(format!("Install to {}", target.parent().unwrap().display())).clicked() {
                self.flush_save();
                match crate::install() {
                    Ok(msg) => {
                        self.message = Some((msg, false));
                        self.autostart = true;
                        self.save_due = None;
                        // The installed copy opens its own window with the same settings.
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
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
}
