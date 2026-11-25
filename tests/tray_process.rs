//! Runs the real SyncApp.exe as a background service and checks its lifecycle.
//! Uses its own instance name and settings folder, so it doesn't interfere with a
//! SyncApp that's already running, and skips registering anything with Windows.

use std::path::Path;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowW, PostMessageW, WM_CLOSE};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn start(home: &Path, instance: &str) -> Child {
    Command::new(env!("CARGO_BIN_EXE_SyncApp"))
        .arg("--tray")
        .env("SYNCAPP_HOME", home)
        .env("SYNCAPP_INSTANCE", instance)
        .env("SYNCAPP_NO_OS_INTEGRATION", "1")
        .spawn()
        .expect("start SyncApp.exe")
}

fn wait_until(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
    let t = Instant::now();
    while !f() {
        assert!(t.elapsed() < Duration::from_secs(secs), "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn log(home: &Path) -> String {
    std::fs::read_to_string(home.join("syncapp.log")).unwrap_or_default()
}

#[test]
fn background_service_starts_is_single_instance_and_stops_cleanly() {
    let home = tempfile::tempdir().unwrap();
    let instance = format!("test{}", std::process::id());
    let class = wide(&format!("SyncAppTrayHost-{instance}"));

    let mut svc = start(home.path(), &instance);
    wait_until("the service's window", 15, || unsafe { !FindWindowW(class.as_ptr(), std::ptr::null()).is_null() });
    wait_until("'started' in the log", 15, || log(home.path()).contains("Background service started"));

    // A second copy notices the first and exits straight away.
    let mut second = start(home.path(), &instance);
    wait_until("the second copy to exit", 10, || second.try_wait().unwrap().is_some());
    assert!(svc.try_wait().unwrap().is_none(), "the first copy keeps running");

    // Asked to close (as the installer/updater does), it shuts down cleanly.
    unsafe { PostMessageW(FindWindowW(class.as_ptr(), std::ptr::null()), WM_CLOSE, 0, 0) };
    wait_until("the service to exit", 15, || svc.try_wait().unwrap().is_some());
    assert!(svc.wait().unwrap().success());
    assert!(log(home.path()).contains("Background service stopped"));
    // A fresh settings file was created with the documented defaults.
    let cfg: serde_json::Value = serde_json::from_slice(&std::fs::read(home.path().join("config.json")).unwrap()).unwrap();
    assert_eq!(cfg["check_updates"], true);
    assert_eq!(cfg["skip_on_battery_saver"], true);
}
