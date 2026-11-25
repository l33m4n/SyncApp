//! "Backup drives" tab: drive list, drive details and the add-drive dialog.

use super::*;

pub(super) struct MarkDialog {
    pub(super) root: String,
    pub(super) info: Option<VolumeInfo>,
    pub(super) space: Option<(u64, u64)>,
    pub(super) previously_marked: bool,
    pub(super) other_pcs: Vec<String>,
    pub(super) name: String,
    pub(super) auto_backup: bool,
}

impl App {
    // ------------------------------------------------------------------ drives
    pub(super) fn ui_drives(&mut self, ui: &mut egui::Ui) {
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

    pub(super) fn ui_drive_detail(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, id: &str) {
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

        // Reminders and automatic checks
        ui.add_space(14.0);
        ui.heading("Reminders and checks");
        let d = self.cfg.drive_mut(id).unwrap();
        ui.horizontal(|ui| {
            let mut on = d.remind_after_days > 0;
            if ui.checkbox(&mut on, "Remind me if this drive hasn't been backed up for").changed() {
                d.remind_after_days = if on { 7 } else { 0 };
                changed = true;
            }
            ui.add_enabled_ui(on, |ui| {
                changed |= ui.add(egui::DragValue::new(&mut d.remind_after_days).range(1..=365).suffix(" days")).changed();
            });
        });
        ui.horizontal(|ui| {
            let mut on = d.verify_every_days > 0;
            if ui.checkbox(&mut on, "Verify the backup automatically every").changed() {
                d.verify_every_days = if on { 30 } else { 0 };
                changed = true;
            }
            ui.add_enabled_ui(on, |ui| {
                changed |= ui.add(egui::DragValue::new(&mut d.verify_every_days).range(1..=365).suffix(" days")).changed();
            });
        });
        ui.label(RichText::new("Checks run right after a backup while the drive is plugged in. Damaged files are copied again from this PC.").weak().small());

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

    pub(super) fn ui_mark_dialog(&mut self, ctx: &egui::Context) {
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
}
