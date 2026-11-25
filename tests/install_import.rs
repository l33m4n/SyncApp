//! Installing carries settings and history over to the standard folders.
//! Runs in its own test program because it points APPDATA/LOCALAPPDATA at temp folders.

use std::fs;

use syncapp::config::{default_config_dir, default_data_dir, import_into_standard_folders, Config};

#[test]
fn install_carries_settings_and_history_over() {
    let tmp = tempfile::tempdir().unwrap();
    let portable = tmp.path().join("portable-home");
    let appdata = tmp.path().join("Roaming");
    let local = tmp.path().join("Local");
    // Safe: the only test in this program, before anything reads the environment.
    unsafe {
        std::env::set_var("APPDATA", &appdata);
        std::env::set_var("LOCALAPPDATA", &local);
        std::env::set_var("SYNCAPP_HOME", &portable);
    }

    // The copy being installed keeps everything in one folder (SYNCAPP_HOME).
    fs::create_dir_all(portable.join("update")).unwrap();
    fs::write(portable.join("config.json"), br#"{"sources":[],"drives":[],"pc_id":"abc"}"#).unwrap();
    fs::write(portable.join("history.json"), b"[1,2,3]").unwrap();
    fs::write(portable.join("syncapp.log"), b"log").unwrap();
    fs::write(portable.join("verify-x.json"), b"{}").unwrap();
    fs::write(portable.join("update").join("SyncApp-9.exe"), b"MZ").unwrap();
    // A stale, different settings file already sits in the standard folder.
    fs::create_dir_all(default_config_dir()).unwrap();
    fs::write(default_config_dir().join("config.json"), b"{\"old\":true}").unwrap();

    let n = import_into_standard_folders().unwrap();
    assert_eq!(n, 4, "config, history, log and verify report");
    assert_eq!(fs::read(default_config_dir().join("config.json")).unwrap(), br#"{"sources":[],"drives":[],"pc_id":"abc"}"#);
    assert_eq!(fs::read(default_config_dir().join("config.json.before-install")).unwrap(), b"{\"old\":true}");
    assert_eq!(fs::read(default_data_dir().join("history.json")).unwrap(), b"[1,2,3]");
    assert!(default_data_dir().join("verify-x.json").exists());
    assert!(!default_data_dir().join("config.json").exists(), "settings go to the settings folder only");
    assert!(!default_data_dir().join("update").exists(), "downloads aren't carried over");

    // Running it again changes nothing.
    assert_eq!(import_into_standard_folders().unwrap(), 0);

    // A copy that already uses the standard folders has nothing to import.
    unsafe { std::env::remove_var("SYNCAPP_HOME") };
    assert_eq!(Config::import_into_standard_folders_logged().unwrap(), 0);

    // Defaults for a brand-new settings file match the documented defaults.
    let c = Config::default();
    assert!(c.notifications && c.skip_on_battery_saver && c.check_updates && !c.paused);
}
