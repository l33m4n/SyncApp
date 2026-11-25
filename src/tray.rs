//! Background tray service: watches for marked drives and backs them up automatically.
//!
//! Built to cost next to nothing while idle (it runs 24/7):
//! * Pure Win32, no GUI framework; runs in Windows' efficiency mode.
//! * Event-driven: Windows tells us when drives arrive or leave; settings are kept in
//!   memory and only re-read when the manager window says they changed.
//! * Schedules use one exact timer for the next due backup instead of polling.
//! * Only drives known to be backup drives are ever touched after they're identified,
//!   so sleeping internal disks aren't woken up.
//! * A one-minute safety-net check costs one GetLogicalDrives call and one file stat.

use std::collections::HashMap;
use std::path::Path;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIIF_WARNING, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use syncapp::config::{config_path, Config, DriveReg};
use syncapp::engine::{human_bytes, Event};
use syncapp::planner::Planner;
use syncapp::schedule::Clock;
use syncapp::service::{self, Identity};
use syncapp::win::{self, wide, NamedLock};
use syncapp::{info, warn};

pub const TRAY_LOCK: &str = "Local\\SyncApp-Tray";
pub const TRAY_CLASS: &str = "SyncAppTrayHost";
/// Posted by the manager window when settings changed. WPARAM flags say what else changed.
pub const WM_SYNCAPP_RELOAD: u32 = WM_APP + 2;
/// A drive was marked or unmarked: re-identify plugged-in drives.
pub const RELOAD_DRIVES: usize = 1;
/// A backup/verify/restore finished: re-read last-backup times.
pub const RELOAD_HISTORY: usize = 2;
/// How long an automatic backup waits for a drive the manager window is busy with.
const MAX_WAIT_FOR_BUSY: Duration = Duration::from_secs(30 * 60);

/// Why a backup is running; decides priority and how chatty notifications are.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Trigger {
    PlugIn,
    Scheduled,
    Manual,
}

const WM_TRAY: u32 = WM_APP + 1;
const WM_BACKUP_DONE: u32 = WM_APP + 3;
const TIMER_SAFETY: usize = 1;
const TIMER_ARRIVAL: usize = 2;
const TIMER_SCHEDULE: usize = 3;
const SAFETY_POLL_MS: u32 = 60_000;
/// Re-check the schedule at least this often, in case the clock jumps or the PC slept.
const MAX_SCHEDULE_WAIT_S: u64 = 30 * 60;
const DBT_DEVICEARRIVAL: usize = 0x8000;
const DBT_DEVICEREMOVECOMPLETE: usize = 0x8004;
const PBT_APMRESUMEAUTOMATIC: usize = 0x12;
const ID_OPEN: u16 = 100;
const ID_BACKUP_NOW: u16 = 101;
const ID_PAUSE: u16 = 102;
const ID_LOG: u16 = 103;
const ID_EXIT: u16 = 104;

static HWND_G: AtomicIsize = AtomicIsize::new(0);
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
static CANCEL: AtomicBool = AtomicBool::new(false);
static LAST_LOGGED_NEXT: AtomicU64 = AtomicU64::new(0);
static BATTERY_LOGGED: AtomicBool = AtomicBool::new(false);

struct State {
    cfg: Config,
    cfg_mtime: Option<SystemTime>,
    /// Drive letters present last time we looked.
    known_mask: u32,
    /// Plugged-in backup drives: root ("E:\") -> drive id.
    backup_drives: HashMap<String, String>,
    /// (drive_id, status text) of running backups.
    busy: Vec<(String, String)>,
    /// When a backup to each drive last failed to run (for retry throttling).
    last_attempt: HashMap<String, Instant>,
    /// Unix time of each drive's last completed backup (from history, then kept up to date).
    last_backup: HashMap<String, u64>,
    /// Files that failed in each drive's last backup (to avoid repeating the same warning).
    last_failed: HashMap<String, Vec<String>>,
    workers: Vec<JoinHandle<()>>,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| {
    Mutex::new(State {
        cfg: Config::default(),
        cfg_mtime: None,
        known_mask: 0,
        backup_drives: HashMap::new(),
        busy: Vec::new(),
        last_attempt: HashMap::new(),
        last_backup: HashMap::new(),
        last_failed: HashMap::new(),
        workers: Vec::new(),
    })
});

fn state() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

fn cfg() -> Config {
    state().cfg.clone()
}

fn hwnd() -> HWND {
    HWND_G.load(Ordering::Relaxed) as HWND
}

fn file_mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).ok().and_then(|m| m.modified().ok())
}

/// Re-read settings if the file changed (a single stat when it hasn't).
fn reload_config(force: bool) {
    let mtime = file_mtime(&config_path());
    if !force && mtime == state().cfg_mtime {
        return;
    }
    match Config::load() {
        Ok(c) => {
            let mut st = state();
            st.cfg = c;
            st.cfg_mtime = mtime;
        }
        Err(e) => {
            warn!("{e}");
            state().cfg_mtime = mtime; // don't re-parse a broken file every minute
        }
    }
}

// ------------------------------------------------------------------ tray icon

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

// ------------------------------------------------------------------ main loop

pub fn run() {
    let Ok(Some(_lock)) = NamedLock::try_acquire(TRAY_LOCK) else {
        return; // already running
    };
    win::enable_efficiency_mode();
    info!("Background service started");
    reload_config(true);
    state().last_backup = service::last_backups();
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
        // known_mask starts at 0, so drives already plugged in at sign-in count as arrivals.
        SetTimer(h, TIMER_ARRIVAL, 3000, None);
        SetTimer(h, TIMER_SAFETY, SAFETY_POLL_MS, None);
        win::trim_memory();

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
                match wp {
                    TIMER_ARRIVAL => {
                        KillTimer(h, TIMER_ARRIVAL);
                        drives_changed();
                    }
                    TIMER_SCHEDULE => {
                        KillTimer(h, TIMER_SCHEDULE);
                        run_due_schedules();
                    }
                    _ => {
                        // Safety net, once a minute: catches anything the events missed.
                        drives_changed();
                    }
                }
                0
            }
            WM_DEVICECHANGE => {
                match wp {
                    // Give Windows a moment to mount the volume, then look.
                    DBT_DEVICEARRIVAL => {
                        SetTimer(h, TIMER_ARRIVAL, 1500, None);
                    }
                    DBT_DEVICEREMOVECOMPLETE => drives_changed(),
                    _ => {}
                }
                1
            }
            WM_SYNCAPP_RELOAD => {
                reload_config(true);
                if wp & RELOAD_DRIVES != 0 {
                    // Rare (a drive was marked/unmarked): the only time all drives are re-read.
                    rescan_backup_drives();
                } else {
                    let mut st = state();
                    let ids: Vec<String> = st.cfg.drives.iter().map(|d| d.drive_id.clone()).collect();
                    st.backup_drives.retain(|_, id| ids.contains(id));
                }
                if wp & RELOAD_HISTORY != 0 {
                    state().last_backup = service::last_backups();
                }
                reschedule();
                0
            }
            WM_BACKUP_DONE => {
                let idle = {
                    let mut st = state();
                    st.workers.retain(|w| !w.is_finished());
                    st.busy.is_empty()
                };
                if idle {
                    win::trim_memory();
                }
                reschedule();
                0
            }
            WM_POWERBROADCAST => {
                if wp == PBT_APMRESUMEAUTOMATIC {
                    drives_changed();
                }
                1
            }
            WM_TIMECHANGE => {
                reschedule();
                0
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
        let paused = state().cfg.paused;
        let status = {
            let st = state();
            if !st.busy.is_empty() {
                st.busy.iter().map(|b| b.1.clone()).collect::<Vec<_>>().join("; ")
            } else if paused {
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
        let pause_flags = MF_STRING | if paused { MF_CHECKED } else { MF_UNCHECKED };
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
            reload_config(false);
            let cfg = cfg();
            let drives = service::connected_drives(&cfg);
            if drives.is_empty() {
                notify("SyncApp", "No marked backup drive is connected.", false);
            }
            for (root, reg) in drives {
                start_backup(cfg.clone(), root, reg, Trigger::Manual);
            }
        }
        ID_PAUSE => {
            reload_config(false);
            let mut c = cfg();
            c.paused = !c.paused;
            if c.save().is_ok() {
                info!("Automatic backups {}", if c.paused { "paused" } else { "resumed" });
            }
            reload_config(true);
            reschedule();
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

// ------------------------------------------------------------------ drives

/// React to drive letters appearing or disappearing. Only newly arrived drives are
/// identified; nothing else is read.
fn drives_changed() {
    reload_config(false);
    let mask = win::drive_mask();
    let arrived = {
        let mut st = state();
        let arrived = mask & !st.known_mask;
        let removed = st.known_mask & !mask;
        st.known_mask = mask;
        if removed != 0 {
            st.backup_drives.retain(|root, _| mask >> (root.as_bytes()[0] - b'A') & 1 == 1);
        }
        st.workers.retain(|w| !w.is_finished());
        arrived
    };
    if arrived != 0 {
        let cfg = cfg();
        for root in win::candidate_drives_in(arrived) {
            match service::identify(&cfg, Path::new(&root)) {
                Identity::Registered(reg) => {
                    state().backup_drives.insert(root.clone(), reg.drive_id.clone());
                    if !reg.auto_backup {
                        info!("Drive '{}' connected at {root} (automatic backup off)", reg.name);
                    } else if cfg.paused {
                        info!("Drive '{}' connected at {root} (automatic backups paused)", reg.name);
                    } else {
                        info!("Drive '{}' connected at {root}", reg.name);
                        start_backup(cfg.clone(), root, reg, Trigger::PlugIn);
                    }
                }
                Identity::SerialMismatch(reg) => {
                    warn!("{root} carries the marker for '{}' but its volume serial differs; refusing", reg.name);
                    notify(
                        "Drive not trusted",
                        &format!(
                            "{} looks like '{}' but its volume ID doesn't match, so nothing was backed up. If you reformatted it, mark it again in SyncApp.",
                            &root[..2],
                            reg.name
                        ),
                        true,
                    );
                }
                Identity::Unknown => {} // not a backup drive: never touched again
            }
        }
    }
    reschedule();
}

/// Re-identify plugged-in drives (after the manager marks or unmarks one). Rare.
fn rescan_backup_drives() {
    let cfg = cfg();
    let found: HashMap<String, String> =
        service::connected_drives(&cfg).into_iter().map(|(root, reg)| (root, reg.drive_id)).collect();
    let mut st = state();
    st.backup_drives = found;
    st.known_mask = win::drive_mask();
}

// ------------------------------------------------------------------ schedules

/// Run `f` with the scheduling decisions for the current state (see `planner`).
fn with_planner<R>(f: impl FnOnce(&Planner, &Clock) -> R) -> R {
    let clock = Clock::now();
    let st = state();
    let busy: Vec<String> = st.busy.iter().map(|b| b.0.clone()).collect();
    let failed_ago: HashMap<String, u64> =
        st.last_attempt.iter().map(|(id, t)| (id.clone(), t.elapsed().as_secs())).collect();
    let p = Planner {
        cfg: &st.cfg,
        backup_drives: &st.backup_drives,
        busy: &busy,
        last_backup: &st.last_backup,
        failed_ago: &failed_ago,
        battery_saver: win::battery_saver_on(),
    };
    f(&p, &clock)
}

/// Set one timer for the earliest scheduled backup among plugged-in drives.
fn reschedule() {
    let h = hwnd();
    let next = with_planner(|p, clock| {
        p.next_wake(clock).map(|t| {
            // Log when the next scheduled time changes (rare), for transparency.
            if LAST_LOGGED_NEXT.swap(t, Ordering::Relaxed) != t && !p.postponed_by_battery(clock) {
                info!("Next scheduled backup at {}", win::local_time_string(t));
            }
            t.saturating_sub(clock.now)
        })
    });
    unsafe {
        match next {
            None => {
                KillTimer(h, TIMER_SCHEDULE);
            }
            Some(wait_s) => {
                let ms = (wait_s.clamp(1, MAX_SCHEDULE_WAIT_S) * 1000) as u32;
                SetTimer(h, TIMER_SCHEDULE, ms, None);
            }
        }
    }
}

fn run_due_schedules() {
    reload_config(false);
    let (due, postponed) = with_planner(|p, clock| (p.due(clock), p.postponed_by_battery(clock)));
    if postponed && !BATTERY_LOGGED.swap(true, Ordering::Relaxed) {
        info!("Scheduled backup postponed: Battery saver is on");
    } else if !postponed {
        BATTERY_LOGGED.store(false, Ordering::Relaxed);
    }
    let cfg = cfg();
    let due: Vec<(String, DriveReg)> =
        due.into_iter().filter_map(|(root, id)| cfg.drive(&id).cloned().map(|r| (root, r))).collect();
    for (root, reg) in due {
        info!("Scheduled backup to '{}' ({})", reg.name, reg.schedule.describe());
        start_backup(cfg.clone(), root, reg, Trigger::Scheduled);
    }
    reschedule();
}

// ------------------------------------------------------------------ backups

/// Run a backup on a worker thread. Automatic backups (plug-in and scheduled) use low disk
/// and memory priority so they don't slow down whatever you're doing.
fn start_backup(cfg: Config, root: String, reg: DriveReg, trigger: Trigger) {
    {
        let mut st = state();
        if st.busy.iter().any(|b| b.0 == reg.drive_id) {
            return;
        }
        st.busy.push((reg.drive_id.clone(), format!("Checking {}...", reg.name)));
    }
    update_tip();
    let handle = std::thread::spawn(move || {
        win::thread_full_speed();
        if trigger != Trigger::Manual {
            win::thread_background_mode();
        }
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

        // If the manager window is verifying or restoring this drive, wait for it
        // (while the drive stays plugged in) instead of skipping the backup.
        let started = Instant::now();
        let mut said_waiting = false;
        let result = loop {
            let r = service::backup_drive(&cfg, &root, &reg, &CANCEL, Some(&mut cb));
            match &r {
                Err(e)
                    if e.contains("already running")
                        && !CANCEL.load(Ordering::Relaxed)
                        && started.elapsed() < MAX_WAIT_FOR_BUSY
                        && Path::new(&root).exists() =>
                {
                    if !said_waiting {
                        info!("'{}' is busy; waiting to back it up", reg.name);
                        set_status(format!("Waiting for {} (busy)...", reg.name));
                        said_waiting = true;
                    }
                    std::thread::sleep(Duration::from_secs(5));
                }
                _ => break r,
            }
        };
        let completed = result.is_ok();
        let first_failure = !state().last_attempt.contains_key(&reg.drive_id);
        match result {
            Ok(r) => {
                let (title, body, problem) = service::describe(&reg, &root, &r);
                // Scheduled runs that found nothing to do stay silent, and don't repeat a
                // warning about exactly the same files as last time (e.g. one that's always open).
                let mut failed: Vec<String> = r.failed.iter().map(|f| f.0.clone()).collect();
                failed.extend(r.missing_sources.iter().cloned());
                failed.sort();
                let same_problem = state().last_failed.insert(reg.drive_id.clone(), failed.clone()) == Some(failed)
                    && r.aborted.is_none();
                let quiet = trigger == Trigger::Scheduled && r.copied.is_empty();
                let repeat_warning = trigger == Trigger::Scheduled && same_problem;
                if (problem && !repeat_warning) || (notify_on && !quiet && !problem) {
                    notify(&title, &body, problem);
                }
            }
            Err(e) if e.contains("already running") => info!("Skipped '{}': {e}", reg.name),
            Err(e) => {
                warn!("Backup to '{}' did not run: {e}", reg.name);
                // A scheduled backup that keeps failing is announced once, not every retry.
                if trigger != Trigger::Scheduled || first_failure {
                    notify(&format!("Backup to {} didn't run", reg.name), &e, true);
                }
            }
        }
        {
            let mut st = state();
            st.busy.retain(|b| b.0 != reg.drive_id);
            if completed {
                st.last_attempt.remove(&reg.drive_id);
                st.last_backup.insert(reg.drive_id.clone(), win::now_unix());
            } else {
                // Only failed attempts are throttled.
                st.last_attempt.insert(reg.drive_id.clone(), Instant::now());
            }
        }
        update_tip();
        unsafe { PostMessageW(hwnd(), WM_BACKUP_DONE, 0, 0) };
    });
    state().workers.push(handle);
}
