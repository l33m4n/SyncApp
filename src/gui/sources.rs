//! "What to back up" tab.

use super::*;

impl App {
    // ------------------------------------------------------------------ sources
    pub(super) fn ui_sources(&mut self, ui: &mut egui::Ui) {
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

    pub(super) fn add_source(&mut self, p: &Path) {
        match self.cfg.add_source(p, None, vec![]) {
            Ok(s) => {
                self.message = Some((format!("Added \"{}\"", s.name), false));
                self.save();
            }
            Err(e) => self.message = Some((e, true)),
        }
    }
}
