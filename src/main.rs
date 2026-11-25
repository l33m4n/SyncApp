//! SyncApp.exe
//!   (no args)  open the manager window (and start the background service if needed)
//!   --tray     run only the background service (used at Windows sign-in)

#![windows_subsystem = "windows"]

mod gui;
mod tray;

use std::path::PathBuf;
use std::process::Command;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::time::{Duration, Instant};

use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateIcon, FindWindowW, MessageBoxW, PostMessageW, SetForegroundWindow, ShowWindow, HICON, MB_ICONERROR,
    SW_RESTORE, WM_CLOSE,
};

use syncapp::win::{self, wide, NamedLock};

pub const MANAGER_TITLE: &str = "SyncApp";
pub const EXE_NAME: &str = "SyncApp.exe";
const MANAGER_LOCK: &str = "Local\\SyncApp-Manager";

fn main() {
    win::suppress_error_dialogs();
    if std::env::args().nth(1).as_deref() == Some("--tray") {
        tray::run();
        return;
    }
    let Ok(Some(_lock)) = NamedLock::try_acquire(MANAGER_LOCK) else {
        focus_existing_manager();
        return;
    };
    if !tray_running() {
        spawn_tray();
    }
    if let Err(e) = gui::run() {
        error_box(&format!("SyncApp couldn't open its window:\n\n{e}"));
    }
}

fn error_box(msg: &str) {
    unsafe { MessageBoxW(null_mut(), wide(msg).as_ptr(), wide("SyncApp").as_ptr(), MB_ICONERROR) };
}

fn focus_existing_manager() {
    unsafe {
        let h = FindWindowW(null(), wide(MANAGER_TITLE).as_ptr());
        if !h.is_null() {
            ShowWindow(h, SW_RESTORE);
            SetForegroundWindow(h);
        }
    }
}

pub fn tray_running() -> bool {
    !matches!(NamedLock::try_acquire(tray::TRAY_LOCK), Ok(Some(_)))
}

/// Tell the background service that settings changed (cheap; no polling needed).
/// `flags`: tray::RELOAD_DRIVES and/or tray::RELOAD_HISTORY for what else changed.
pub fn poke_tray(flags: usize) {
    unsafe {
        let h = FindWindowW(wide(tray::TRAY_CLASS).as_ptr(), null());
        if !h.is_null() {
            PostMessageW(h, tray::WM_SYNCAPP_RELOAD, flags, 0);
        }
    }
}

pub fn spawn_tray() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = Command::new(exe).arg("--tray").spawn();
    }
}

// ------------------------------------------------------------------ install

pub fn install_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|| ".".into());
    base.join("Programs").join("SyncApp")
}

/// Copy the exe to %LOCALAPPDATA%\Programs\SyncApp, add a Start menu shortcut,
/// start at sign-in, and restart the background service from there.
pub fn install() -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let dir = install_dir();
    let target = dir.join(EXE_NAME);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;

    // The running background service must exit so its exe can be replaced.
    stop_tray()?;

    std::fs::copy(&exe, &target).map_err(|e| format!("copying to {}: {e}", target.display()))?;
    win::set_autostart(Some(&format!("\"{}\" --tray", target.display()))).map_err(|e| e.to_string())?;

    // Start menu shortcut (WScript.Shell via PowerShell; values passed as env vars, not code).
    let programs = start_menu_dir();
    let _ = Command::new("powershell.exe")
        .args([
            "-NoProfile", "-NonInteractive", "-WindowStyle", "Hidden", "-Command",
            "$s=(New-Object -ComObject WScript.Shell).CreateShortcut($env:SA_LNK);$s.TargetPath=$env:SA_EXE;$s.WorkingDirectory=$env:SA_DIR;$s.Description='SyncApp backup manager';$s.Save()",
        ])
        .env("SA_LNK", programs.join("SyncApp.lnk"))
        .env("SA_EXE", &target)
        .env("SA_DIR", &dir)
        .creation_flags_no_window()
        .status();

    Command::new(&target).arg("--tray").spawn().map_err(|e| e.to_string())?;
    syncapp::info!("Installed to {}", target.display());
    Ok(format!(
        "Installed to {}. SyncApp now starts with Windows and is in your Start menu. You can delete the copy you ran this from.",
        dir.display()
    ))
}

fn start_menu_dir() -> PathBuf {
    std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_default().join(r"Microsoft\Windows\Start Menu\Programs")
}

/// Ask the background service to exit and wait for it (it refuses while a backup runs).
fn stop_tray() -> Result<(), String> {
    unsafe {
        let h = FindWindowW(wide(tray::TRAY_CLASS).as_ptr(), null());
        if !h.is_null() {
            PostMessageW(h, WM_CLOSE, 0, 0);
        }
    }
    let t0 = Instant::now();
    while tray_running() {
        if t0.elapsed() > Duration::from_secs(10) {
            return Err("the background service is busy with a backup; try again when it finishes".into());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}

/// Remove SyncApp from this PC: background service, startup entry, Start menu shortcut
/// and installed program. Backups on drives are never touched. With `remove_data`,
/// settings, history and the log are deleted too.
pub fn uninstall(remove_data: bool) -> Result<String, String> {
    stop_tray()?;
    syncapp::info!("Uninstalling (remove settings: {remove_data})");
    win::set_autostart(None).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(start_menu_dir().join("SyncApp.lnk"));

    let dir = install_dir();
    let exe = std::env::current_exe().unwrap_or_default();
    if dir.exists() {
        if exe.starts_with(&dir) {
            // Can't delete a running exe: remove the folder a moment after this window closes.
            use std::os::windows::process::CommandExt;
            let _ = Command::new("cmd.exe")
                .raw_arg(format!("/C ping -n 4 127.0.0.1 >nul & rmdir /s /q \"{}\"", dir.display()))
                .creation_flags_no_window()
                .spawn();
        } else {
            std::fs::remove_dir_all(&dir).map_err(|e| format!("removing {}: {e}", dir.display()))?;
        }
    }
    if remove_data {
        let _ = std::fs::remove_dir_all(syncapp::config::config_dir());
        let _ = std::fs::remove_dir_all(syncapp::config::data_dir());
    }
    Ok("SyncApp has been uninstalled. Your backups on your drives are untouched.".into())
}

trait NoWindow {
    fn creation_flags_no_window(&mut self) -> &mut Self;
}

impl NoWindow for Command {
    fn creation_flags_no_window(&mut self) -> &mut Self {
        use std::os::windows::process::CommandExt;
        self.creation_flags(0x0800_0000) // CREATE_NO_WINDOW
    }
}

// ------------------------------------------------------------------ icon (drawn in code, no resources)

const ICON_SIZE: usize = 32;

const BLUE: (u8, u8, u8) = (37, 99, 235);
const GREEN: (u8, u8, u8) = (22, 163, 74);

/// 32x32 RGBA: rounded square in `bg` with a white "download into drive" glyph.
fn icon_rgba(bg: (u8, u8, u8)) -> Vec<u8> {
    let n = ICON_SIZE as i32;
    let mut px = vec![0u8; ICON_SIZE * ICON_SIZE * 4];
    for y in 0..n {
        for x in 0..n {
            // rounded-rect mask, radius 7
            let r = 7;
            let cx = if x < r { r } else if x > n - 1 - r { n - 1 - r } else { x };
            let cy = if y < r { r } else if y > n - 1 - r { n - 1 - r } else { y };
            let d2 = (x - cx).pow(2) + (y - cy).pow(2);
            if d2 > r * r {
                continue;
            }
            let arrow_shaft = (14..18).contains(&x) && (6..17).contains(&y);
            let half = 22 - y;
            let arrow_head = (15..22).contains(&y) && (x - 16).abs() <= half && x >= 16 - half && x < 16 + half;
            let tray_base = (8..24).contains(&x) && (23..26).contains(&y);
            let tray_side = ((8..11).contains(&x) || (21..24).contains(&x)) && (19..26).contains(&y);
            let white = arrow_shaft || arrow_head || tray_base || tray_side;
            let i = ((y * n + x) * 4) as usize;
            let (rr, gg, bb) = if white { (255, 255, 255) } else { bg };
            px[i..i + 4].copy_from_slice(&[rr, gg, bb, 255]);
        }
    }
    px
}

pub fn icon_data() -> eframe::egui::IconData {
    eframe::egui::IconData { rgba: icon_rgba(BLUE), width: ICON_SIZE as u32, height: ICON_SIZE as u32 }
}

static ICON: AtomicIsize = AtomicIsize::new(0);
static ICON_BUSY: AtomicIsize = AtomicIsize::new(0);

/// Tray/window icon: blue when idle, green while a backup is running.
pub fn app_icon(busy: bool) -> HICON {
    let slot = if busy { &ICON_BUSY } else { &ICON };
    let cached = slot.load(Ordering::Relaxed);
    if cached != 0 {
        return cached as HICON;
    }
    let rgba = icon_rgba(if busy { GREEN } else { BLUE });
    let bgra: Vec<u8> = rgba.chunks(4).flat_map(|p| [p[2], p[1], p[0], p[3]]).collect();
    let and_mask = vec![0u8; ICON_SIZE * ICON_SIZE / 8];
    let h = unsafe {
        CreateIcon(null_mut(), ICON_SIZE as i32, ICON_SIZE as i32, 1, 32, and_mask.as_ptr(), bgra.as_ptr())
    };
    slot.store(h as isize, Ordering::Relaxed);
    h
}
