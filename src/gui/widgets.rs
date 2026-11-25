//! Small shared drawing and formatting helpers.

use super::*;

pub(super) fn file_mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).ok().and_then(|m| m.modified().ok())
}

pub(super) fn when(unix: u64) -> String {
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
pub(super) fn until(unix: u64) -> String {
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

pub(super) fn files_word(n: u64) -> String {
    if n == 1 { "1 file".into() } else { format!("{n} files") }
}

pub(super) fn short_hash(h: &str) -> String {
    // chars(), not byte slicing: a damaged manifest must not be able to crash the window.
    if h.chars().count() > 16 { format!("{}...", h.chars().take(16).collect::<String>()) } else { h.to_string() }
}

/// Small filled status circle (drawn, so it doesn't depend on font glyphs).
pub(super) fn dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 5.0, color);
}

pub(super) fn capacity_bar(ui: &mut egui::Ui, free: u64, total: u64, with_text: bool) {
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
pub(super) fn thin_bar(ui: &mut egui::Ui, frac: f32, color: Color32) {
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
