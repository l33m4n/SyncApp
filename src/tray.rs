//! Background tray service: watches for marked drives and backs them up automatically.
//!
//! Pure Win32 (no GUI framework) so it idles at a few MB of RAM and ~0% CPU.

use std::path::Path;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIIF_WARNING, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use syncapp::config::{Config, DriveReg};
use syncapp::engine::{human_bytes, Event};
use syncapp::service::{self, Identity};
use syncapp::win::{self, wide, NamedLock};
use syncapp::{info, warn};

pub const TRAY_LOCK: &str = "Local\\SyncApp-Tray";
pub const TRAY_CLASS: &str = "SyncAppTrayHost";

const WM_TRAY: u32 = WM_APP + 1;
const TIMER_POLL: usize = 1;
const TIMER_ARRIVAL: usize = 2;
const DBT_DEVICEARRIVAL: usize = 0x8000;
const ID_OPEN: u16 = 100;
const ID_BACKUP_NOW: u16 = 101;
const ID_PAUSE: u16 = 102;
const ID_LOG: u16 = 103;
const ID_EXIT: u16 = 104;

static HWND_G: AtomicIsize = AtomicIsize::new(0);
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
static CANCEL: AtomicBool = AtomicBool::new(false);
static STATE: Mutex<State> = Mutex::new(State {
    known_mask: 0,
    busy: Vec::new(),
    last_run: Vec::new(),
    workers: Vec::new(),
    last_repeat_check: None,
});

struct State {
    known_mask: u32,
    /// (drive_id, status text) of running backups
    busy: Vec<(String, String)>,
    last_run: Vec<(String, Instant)>,
    workers: Vec<JoinHandle<()>>,
    last_repeat_check: Option<Instant>,
}

fn state() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

fn hwnd() -> HWND {
    HWND_G.load(Ordering::Relaxed) as HWND
}

fn copy_wide(dst: &mut [u16], s: &str) {
    let w: Vec<u16> = s.encode_utf16().take(dst.len() - 1).collect();
    dst[..w.len()].copy_from_slice(&w);
    dst[w.len()] = 0;
}

fn base_nid() -> NOTIFYICONDATAW {
    let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd();
    nid.uID = 1;
    nid
}

fn add_icon() {
    let mut nid = base_nid();
    nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    nid.uCallbackMessage = WM_TRAY;
    nid.hIcon = crate::app_icon(!state().busy.is_empty());
    copy_wide(&mut nid.szTip, "SyncApp: watching for backup drives");
    unsafe { Shell_NotifyIconW(NIM_ADD, &nid) };
}

pub fn notify(title: &str, body: &str, warning: bool) {
    let mut nid = base_nid();
    nid.uFlags = NIF_INFO;
    copy_wide(&mut nid.szInfoTitle, title);
    copy_wide(&mut nid.szInfo, body);
    nid.dwInfoFlags = if warning { NIIF_WARNING } else { NIIF_INFO };
    unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) };
}

/// Refresh the tooltip and icon (green while any backup is running).
fn update_tip() {
    let (text, busy) = {
        let st = state();
        if st.busy.is_empty() {
            ("SyncApp: watching for backup drives".to_string(), false)
        } else {
            (st.busy.iter().map(|b| b.1.clone()).collect::<Vec<_>>().join("\n"), true)
        }
    };
    let mut nid = base_nid();
    nid.uFlags = NIF_TIP | NIF_ICON;
    nid.hIcon = crate::app_icon(busy);
    copy_wide(&mut nid.szTip, &text);
    unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) };
}

pub fn run() {
    let Ok(Some(_lock)) = NamedLock::try_acquire(TRAY_LOCK) else {
        return; // already running
    };
    info!("Background service started");
    unsafe {
        let hinst = GetModuleHandleW(null());
        let class = wide(TRAY_CLASS);
        let mut wc: WNDCLASSEXW = std::mem::zeroed();
        wc.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
        wc.lpfnWndProc = Some(wndproc);
        wc.hInstance = hinst;
        wc.lpszClassName = class.as_ptr();
        RegisterClassExW(&wc);
        // A real (hidden) top-level window: message-only windows don't get WM_DEVICECHANGE.
        let h = CreateWindowExW(
            0, class.as_ptr(), wide("SyncApp Tray").as_ptr(), WS_OVERLAPPED,
            0, 0, 0, 0, null_mut(), null_mut(), hinst, null(),
        );
        if h.is_null() {
            warn!("Could not create tray window");
            return;
        }
        HWND_G.store(h as isize, Ordering::Relaxed);
        TASKBAR_CREATED.store(RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()), Ordering::Relaxed);
        add_icon();
        // known_mask starts at 0, so drives already plugged in at login count as arrivals.
        SetTimer(h, TIMER_ARRIVAL, 3000, None);
        SetTimer(h, TIMER_POLL, 2000, None);

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let nid = base_nid();
        Shell_NotifyIconW(NIM_DELETE, &nid);
    }
    info!("Background service stopped");
}

unsafe extern "system" fn wndproc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_TIMER => {
                if wp == TIMER_ARRIVAL {
                    KillTimer(h, TIMER_ARRIVAL);
                }
                check_drives();
                0
            }
            WM_DEVICECHANGE => {
                if wp == DBT_DEVICEARRIVAL {
                    // Give Windows a moment to mount the volume, then look.
                    SetTimer(h, TIMER_ARRIVAL, 1500, None);
                }
                1
            }
            WM_TRAY => {
                match lp as u32 {
                    WM_LBUTTONDBLCLK => open_manager(),
                    WM_RBUTTONUP => show_menu(h),
                    _ => {}
                }
                0
            }
            WM_COMMAND => {
                on_command(h, (wp & 0xffff) as u16);
                0
            }
            WM_CLOSE => {
                // Sent by the manager (e.g. after installing). Never interrupts a running backup.
                if state().busy.is_empty() {
                    DestroyWindow(h);
                }
                0
            }
            WM_ENDSESSION => {
                if wp != 0 {
                    CANCEL.store(true, Ordering::Relaxed);
                    join_workers(Duration::from_secs(4));
                }
                0
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                0
            }
            m if m != 0 && m == TASKBAR_CREATED.load(Ordering::Relaxed) => {
                add_icon(); // Explorer restarted
                0
            }
            _ => DefWindowProcW(h, msg, wp, lp),
        }
    }
}

fn join_workers(max: Duration) {
    let start = Instant::now();
    loop {
        let done = {
            let mut st = state();
            st.workers.retain(|w| !w.is_finished());
            st.workers.is_empty()
        };
        if done || start.elapsed() > max {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn open_manager() {
    let exe = std::env::current_exe().unwrap_or_default();
    let _ = std::process::Command::new(exe).spawn();
}

unsafe fn show_menu(h: HWND) {
    unsafe {
        let cfg = Config::load().unwrap_or_default();
        let status = {
            let st = state();
            if !st.busy.is_empty() {
                st.busy.iter().map(|b| b.1.clone()).collect::<Vec<_>>().join("; ")
            } else if cfg.paused {
                "Automatic backups paused".into()
            } else {
                "Watching for backup drives".into()
            }
        };
        let menu = CreatePopupMenu();
        AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, wide(&status).as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, null());
        AppendMenuW(menu, MF_STRING, ID_OPEN as usize, wide("Open SyncApp").as_ptr());
        AppendMenuW(menu, MF_STRING, ID_BACKUP_NOW as usize, wide("Back up connected drives now").as_ptr());
        let pause_flags = MF_STRING | if cfg.paused { MF_CHECKED } else { MF_UNCHECKED };
        AppendMenuW(menu, pause_flags, ID_PAUSE as usize, wide("Pause automatic backups").as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, null());
        AppendMenuW(menu, MF_STRING, ID_LOG as usize, wide("Open log file").as_ptr());
        AppendMenuW(menu, MF_STRING, ID_EXIT as usize, wide("Exit").as_ptr());
        SetMenuDefaultItem(menu, ID_OPEN as u32, 0);

        let mut pt = POINT { x: 0, y: 0 };
        GetCursorPos(&mut pt);
        SetForegroundWindow(h);
        TrackPopupMenu(menu, TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, pt.x, pt.y, 0, h, null());
        PostMessageW(h, WM_NULL, 0, 0);
        DestroyMenu(menu);
    }
}

unsafe fn on_command(h: HWND, id: u16) {
    match id {
        ID_OPEN => open_manager(),
        ID_BACKUP_NOW => {
            let Ok(cfg) = Config::load() else { return };
            let drives = service::connected_drives(&cfg);
            if drives.is_empty() {
                notify("SyncApp", "No marked backup drive is connected.", false);
            }
            for (root, reg) in drives {
                start_backup(cfg.clone(), root, reg);
            }
        }
        ID_PAUSE => {
            if let Ok(mut cfg) = Config::load() {
                cfg.paused = !cfg.paused;
                let _ = cfg.save();
                info!("Automatic backups {}", if cfg.paused { "paused" } else { "resumed" });
            }
        }
        ID_LOG => win::shell_open(&syncapp::log::log_path()),
        ID_EXIT => {
            let busy = !state().busy.is_empty();
            if busy {
                let r = unsafe {
                    MessageBoxW(
                        h,
                        wide("A backup is running. Stop it and exit?\n\nFiles already on the drive stay intact; the file being copied is simply not updated.").as_ptr(),
                        wide("SyncApp").as_ptr(),
                        MB_YESNO | MB_ICONWARNING,
                    )
                };
                if r != IDYES {
                    return;
                }
                CANCEL.store(true, Ordering::Relaxed);
                join_workers(Duration::from_secs(30));
            }
            unsafe { DestroyWindow(h) };
        }
        _ => {}
    }
}

fn check_drives() {
    let cfg = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            warn!("{e}");
            return;
        }
    };
    let mask = win::drive_mask();
    let (arrived, repeat_check) = {
        let mut st = state();
        let arrived = mask & !st.known_mask;
        st.known_mask = mask;
        st.workers.retain(|w| !w.is_finished());
        let due = st.last_repeat_check.is_none_or(|t| t.elapsed() > Duration::from_secs(60));
        if due {
            st.last_repeat_check = Some(Instant::now());
        }
        (arrived, due)
    };

    if arrived != 0 {
        for root in win::candidate_drives() {
            let bit = (root.as_bytes()[0] - b'A') as u32;
            if arrived >> bit & 1 == 0 {
                continue;
            }
            match service::identify(&cfg, Path::new(&root)) {
                Identity::Registered(reg) => {
                    if !reg.auto_backup {
                        info!("Drive '{}' connected at {root} (automatic backup off)", reg.name);
                    } else if cfg.paused {
                        info!("Drive '{}' connected at {root} (automatic backups paused)", reg.name);
                    } else {
                        info!("Drive '{}' connected at {root}", reg.name);
                        start_backup(cfg.clone(), root, reg);
                    }
                }
                Identity::SerialMismatch(reg) => {
                    warn!("{root} carries the marker for '{}' but its volume serial differs; refusing", reg.name);
                    notify(
                        "Drive not trusted",
                        &format!(
                            "{} looks like '{}' but its volume ID doesn't match, so nothing was backed up. If you reformatted it, mark it again in SyncApp.",
                            &root[..2], reg.name
                        ),
                        true,
                    );
                }
                Identity::Unknown => {} // not a backup drive: never touched
            }
        }
    }

    if repeat_check && !cfg.paused && cfg.drives.iter().any(|d| d.repeat_minutes > 0 && d.auto_backup) {
        for (root, reg) in service::connected_drives(&cfg) {
            if reg.repeat_minutes == 0 || !reg.auto_backup {
                continue;
            }
            let due = {
                let st = state();
                st.last_run
                    .iter()
                    .find(|(id, _)| *id == reg.drive_id)
                    .is_none_or(|(_, t)| t.elapsed() >= Duration::from_secs(reg.repeat_minutes as u64 * 60))
            };
            if due {
                start_backup(cfg.clone(), root, reg);
            }
        }
    }
}

fn start_backup(cfg: Config, root: String, reg: DriveReg) {
    {
        let mut st = state();
        if st.busy.iter().any(|b| b.0 == reg.drive_id) {
            return;
        }
        st.busy.push((reg.drive_id.clone(), format!("Checking {}...", reg.name)));
    }
    update_tip();
    let handle = std::thread::spawn(move || {
        let notify_on = cfg.notifications;
        let id = reg.drive_id.clone();
        let set_status = |s: String| {
            if let Some(b) = state().busy.iter_mut().find(|b| b.0 == id) {
                b.1 = s;
            }
            update_tip();
        };
        let mut last_tip = Instant::now();
        let mut cb = |ev: Event| match ev {
            Event::Planned { files, bytes } if files > 0 => {
                if notify_on {
                    notify(
                        &format!("Backing up to {}", reg.name),
                        &format!("{files} changed file(s), {}. Please don't unplug {} yet.", human_bytes(bytes), &root[..2]),
                        false,
                    );
                }
            }
            Event::File { index, total, .. } if last_tip.elapsed() > Duration::from_millis(500) => {
                last_tip = Instant::now();
                set_status(format!("Backing up to {}: {index}/{total}", reg.name));
            }
            _ => {}
        };

        let mut result = Err(String::new());
        for _ in 0..3 {
            result = service::backup_drive(&cfg, &root, &reg, &CANCEL, Some(&mut cb));
            match &result {
                Err(e) if e.contains("already running") => std::thread::sleep(Duration::from_secs(3)),
                _ => break,
            }
        }
        match result {
            Ok(r) => {
                let (title, body, problem) = service::describe(&reg, &root, &r);
                if notify_on || problem {
                    notify(&title, &body, problem);
                }
            }
            Err(e) if e.contains("already running") => info!("Skipped '{}': {e}", reg.name),
            Err(e) => {
                warn!("Backup to '{}' did not run: {e}", reg.name);
                notify(&format!("Backup to {} didn't run", reg.name), &e, true);
            }
        }
        {
            let mut st = state();
            st.busy.retain(|b| b.0 != reg.drive_id);
            st.last_run.retain(|(i, _)| *i != reg.drive_id);
            st.last_run.push((reg.drive_id.clone(), Instant::now()));
        }
        update_tip();
    });
    state().workers.push(handle);
}
