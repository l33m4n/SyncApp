use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime};

use syncapp::config::Source;
use syncapp::engine::{self, wildcard, Manifest, BACKUP_DIR};

struct Env {
    _tmp: tempfile::TempDir,
    pc: PathBuf,
    drive: PathBuf,
}

fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let pc = tmp.path().join("pc");
    let drive = tmp.path().join("drive");
    fs::create_dir_all(pc.join("docs/sub")).unwrap();
    fs::create_dir_all(&drive).unwrap();
    write(&pc.join("docs/a.txt"), b"alpha");
    write(&pc.join("docs/sub/b.bin"), &vec![7u8; 300_000]);
    write(&pc.join("docs/~$lock.docx"), b"office lock file");
    write(&pc.join("single.txt"), b"just me");
    Env { _tmp: tmp, pc, drive }
}

fn write(p: &Path, data: &[u8]) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    let mut f = File::create(p).unwrap();
    f.write_all(data).unwrap();
}

fn set_mtime(p: &Path, t: SystemTime) {
    OpenOptions::new().write(true).open(p).unwrap().set_modified(t).unwrap();
}

fn sources(e: &Env) -> Vec<Source> {
    vec![
        Source { name: "Docs".into(), path: e.pc.join("docs"), exclude: vec!["*.log".into()], enabled: true, kind: None },
        Source { name: "One".into(), path: e.pc.join("single.txt"), exclude: vec![], enabled: true, kind: None },
    ]
}

fn backup(e: &Env) -> engine::Report {
    let no = AtomicBool::new(false);
    engine::run_backup(&sources(e), &e.drive, &no, None)
}

/// Every file on the drive with its size and mtime: proves whether anything was written.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, (u64, SystemTime)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap() {
            let e = e.unwrap();
            let md = e.metadata().unwrap();
            if md.is_dir() {
                stack.push(e.path());
            } else {
                out.insert(e.path(), (md.len(), md.modified().unwrap()));
            }
        }
    }
    out
}

fn on_drive(e: &Env, rel: &str) -> PathBuf {
    e.drive.join(BACKUP_DIR).join(rel)
}

#[test]
fn first_backup_copies_everything_except_excluded() {
    let e = env();
    let r = backup(&e);
    assert!(r.ok(), "{r:?}");
    assert_eq!(r.copied.len(), 3);
    assert_eq!(fs::read(on_drive(&e, "Docs/a.txt")).unwrap(), b"alpha");
    assert_eq!(fs::read(on_drive(&e, "Docs/sub/b.bin")).unwrap(), vec![7u8; 300_000]);
    assert_eq!(fs::read(on_drive(&e, "One/single.txt")).unwrap(), b"just me");
    assert!(!on_drive(&e, "Docs/~$lock.docx").exists(), "office lock files are excluded by default");
    // Modification time is preserved.
    assert_eq!(
        fs::metadata(on_drive(&e, "Docs/a.txt")).unwrap().modified().unwrap(),
        fs::metadata(e.pc.join("docs/a.txt")).unwrap().modified().unwrap()
    );
}

#[test]
fn no_changes_means_nothing_is_written() {
    let e = env();
    backup(&e);
    let before = snapshot(&e.drive);
    std::thread::sleep(Duration::from_millis(50));
    let r = backup(&e);
    assert!(r.copied.is_empty());
    assert_eq!(r.unchanged, 3);
    assert_eq!(snapshot(&e.drive), before, "drive contents (including manifest) must be untouched");
}

#[test]
fn only_changed_files_are_copied() {
    let e = env();
    backup(&e);
    write(&e.pc.join("docs/a.txt"), b"alpha v2, longer");
    let r = backup(&e);
    assert_eq!(r.copied, vec!["Docs/a.txt".to_string()]);
    assert_eq!(fs::read(on_drive(&e, "Docs/a.txt")).unwrap(), b"alpha v2, longer");
}

#[test]
fn same_size_edit_is_detected() {
    let e = env();
    backup(&e);
    write(&e.pc.join("docs/a.txt"), b"ALPHA"); // same length, different content
    set_mtime(&e.pc.join("docs/a.txt"), SystemTime::now() + Duration::from_secs(5));
    let r = backup(&e);
    assert_eq!(r.copied, vec!["Docs/a.txt".to_string()]);
    assert_eq!(fs::read(on_drive(&e, "Docs/a.txt")).unwrap(), b"ALPHA");
}

#[test]
fn timestamp_only_change_does_not_copy() {
    let e = env();
    backup(&e);
    set_mtime(&e.pc.join("docs/sub/b.bin"), SystemTime::now() + Duration::from_secs(60));
    let r = backup(&e);
    assert!(r.copied.is_empty());
    assert_eq!(r.refreshed, 1);
    let snap = snapshot(&e.drive);
    let r = backup(&e);
    assert!(r.copied.is_empty() && r.refreshed == 0);
    assert_eq!(snapshot(&e.drive), snap);
}

#[test]
fn file_deleted_from_drive_is_recopied() {
    let e = env();
    backup(&e);
    fs::remove_file(on_drive(&e, "Docs/a.txt")).unwrap();
    let r = backup(&e);
    assert_eq!(r.copied, vec!["Docs/a.txt".to_string()]);
}

#[test]
fn deleting_on_pc_never_deletes_backup() {
    let e = env();
    backup(&e);
    fs::remove_file(e.pc.join("docs/a.txt")).unwrap();
    let r = backup(&e);
    assert!(r.ok());
    assert_eq!(fs::read(on_drive(&e, "Docs/a.txt")).unwrap(), b"alpha");
}

#[test]
fn lost_manifest_does_not_cause_recopy() {
    let e = env();
    backup(&e);
    fs::write(Manifest::path_for(&e.drive), b"{ garbage").unwrap();
    let r = backup(&e);
    assert!(r.copied.is_empty(), "identical files must be recognised by checksum: {r:?}");
    assert_eq!(r.refreshed, 3);
}

#[test]
fn verify_finds_corruption_and_next_backup_repairs_it() {
    let e = env();
    backup(&e);
    let p = on_drive(&e, "Docs/a.txt");
    let mtime = fs::metadata(&p).unwrap().modified().unwrap();
    write(&p, b"alphX"); // same size, keep timestamp: invisible to a metadata check
    set_mtime(&p, mtime);
    let no = AtomicBool::new(false);
    let v = engine::verify_drive(&e.drive, &no, None);
    assert_eq!(v.corrupted, vec!["Docs/a.txt".to_string()]);
    assert_eq!(v.ok, 2);
    let r = backup(&e);
    assert_eq!(r.copied, vec!["Docs/a.txt".to_string()]);
    assert_eq!(fs::read(&p).unwrap(), b"alpha");
}

#[test]
fn cancelled_copy_leaves_old_backup_intact_and_no_temp_files() {
    let e = env();
    backup(&e);
    write(&e.pc.join("docs/sub/b.bin"), &vec![9u8; 400_000]);
    let cancel = AtomicBool::new(true);
    let r = engine::run_backup(&sources(&e), &e.drive, &cancel, None);
    assert_eq!(r.aborted.as_deref(), Some("cancelled"));
    assert_eq!(fs::read(on_drive(&e, "Docs/sub/b.bin")).unwrap(), vec![7u8; 300_000]);
    assert!(snapshot(&e.drive).keys().all(|p| !p.to_string_lossy().contains(".syncapp-tmp")));
}

#[test]
fn read_only_backup_copy_is_updated() {
    let e = env();
    backup(&e);
    let p = on_drive(&e, "Docs/a.txt");
    let mut perm = fs::metadata(&p).unwrap().permissions();
    perm.set_readonly(true);
    fs::set_permissions(&p, perm).unwrap();
    write(&e.pc.join("docs/a.txt"), b"changed!!");
    let r = backup(&e);
    assert!(r.ok(), "{r:?}");
    assert_eq!(fs::read(&p).unwrap(), b"changed!!");
}

#[test]
fn restore_round_trip_and_no_silent_overwrite() {
    let e = env();
    backup(&e);
    let no = AtomicBool::new(false);
    let target = e.pc.join("restored");
    let r = engine::restore(&e.drive, "Docs", &target, false, &no, None);
    assert!(r.ok(), "{r:?}");
    assert_eq!(fs::read(target.join("a.txt")).unwrap(), b"alpha");
    assert_eq!(fs::read(target.join("sub/b.bin")).unwrap(), vec![7u8; 300_000]);

    // Restoring again: identical files are skipped.
    let r = engine::restore(&e.drive, "Docs", &target, false, &no, None);
    assert_eq!((r.copied.len(), r.unchanged), (0, 2));

    // A different local file is not overwritten unless asked.
    write(&target.join("a.txt"), b"local edit");
    let r = engine::restore(&e.drive, "Docs", &target, false, &no, None);
    assert_eq!(r.failed.len(), 1);
    assert_eq!(fs::read(target.join("a.txt")).unwrap(), b"local edit");
    let r = engine::restore(&e.drive, "Docs", &target, true, &no, None);
    assert!(r.ok());
    assert_eq!(fs::read(target.join("a.txt")).unwrap(), b"alpha");
}

#[test]
fn restore_refuses_corrupted_backup() {
    let e = env();
    backup(&e);
    let p = on_drive(&e, "Docs/a.txt");
    write(&p, b"alphX");
    let no = AtomicBool::new(false);
    let target = e.pc.join("restored");
    let r = engine::restore(&e.drive, "Docs", &target, false, &no, None);
    assert_eq!(r.failed.len(), 1);
    assert!(r.failed[0].1.contains("corrupted"));
    assert!(!target.join("a.txt").exists());
}

#[test]
fn missing_source_is_reported_not_fatal() {
    let e = env();
    let mut s = sources(&e);
    s.push(Source { name: "Gone".into(), path: e.pc.join("nope"), exclude: vec![], enabled: true, kind: None });
    let no = AtomicBool::new(false);
    let r = engine::run_backup(&s, &e.drive, &no, None);
    assert_eq!(r.missing_sources.len(), 1);
    assert_eq!(r.copied.len(), 3);
}

#[test]
fn wildcard_matching() {
    assert!(wildcard("*.tmp", "FILE.TMP"));
    assert!(wildcard("~$*", "~$report.docx"));
    assert!(wildcard("node_modules", "node_modules"));
    assert!(wildcard("build/*", "build/x/y.o"));
    assert!(wildcard("a?c", "abc"));
    assert!(!wildcard("*.tmp", "file.tmpx"));
    assert!(!wildcard("a?c", "ac"));
}

#[test]
fn restore_selected_files_to_chosen_paths() {
    let e = env();
    backup(&e);
    let (files, errs) = engine::list_backup(&e.drive);
    assert!(errs.is_empty());
    let keys: Vec<&str> = files.iter().map(|f| f.key.as_str()).collect();
    assert_eq!(keys, vec!["Docs/a.txt", "Docs/sub/b.bin", "One/single.txt"]);
    assert!(files.iter().all(|f| f.hash.is_some()));

    // Simulate losing one file on the PC and restore just that file to where it was.
    fs::remove_file(e.pc.join("docs/sub/b.bin")).unwrap();
    let no = AtomicBool::new(false);
    let pick = vec![("Docs/sub/b.bin".to_string(), e.pc.join("docs/sub/b.bin"))];
    let r = engine::restore_files(&e.drive, &pick, false, &no, None);
    assert!(r.ok(), "{r:?}");
    assert_eq!(r.copied.len(), 1);
    assert_eq!(fs::read(e.pc.join("docs/sub/b.bin")).unwrap(), vec![7u8; 300_000]);
}

#[test]
fn verify_report_lists_every_file_with_checksums() {
    let e = env();
    backup(&e);
    let no = AtomicBool::new(false);
    let v = engine::verify_drive(&e.drive, &no, None);
    assert_eq!(v.items.len(), 3);
    assert!(v.items.iter().all(|i| i.status == engine::VerifyStatus::Ok && i.actual.as_deref() == Some(i.expected.as_str())));
    assert_eq!(v.bytes_read, 5 + 300_000 + 7);
    assert!(v.direct_io, "verification should bypass the Windows cache on NTFS");
    assert!(v.untracked.is_empty());

    // A file dropped onto the drive by hand has no recorded checksum.
    write(&on_drive(&e, "Docs/extra.txt"), b"hand-copied");
    fs::remove_file(on_drive(&e, "One/single.txt")).unwrap();
    let v = engine::verify_drive(&e.drive, &no, None);
    assert_eq!(v.untracked, vec!["Docs/extra.txt".to_string()]);
    assert_eq!(v.missing, vec!["One/single.txt".to_string()]);
    // Missing file is re-copied by the next backup.
    let r = backup(&e);
    assert_eq!(r.copied, vec!["One/single.txt".to_string()]);
}

#[test]
fn refuses_to_write_through_a_junction_on_the_drive() {
    let e = env();
    // A tampered drive: "SyncApp Backup\Docs" is a junction pointing somewhere else.
    let elsewhere = e.pc.join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    fs::create_dir_all(e.drive.join(BACKUP_DIR)).unwrap();
    let link = e.drive.join(BACKUP_DIR).join("Docs");
    let ok = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&link)
        .arg(&elsewhere)
        .output()
        .unwrap()
        .status
        .success();
    assert!(ok, "could not create a junction for the test");

    let r = backup(&e);
    assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0, "nothing may be written through the junction");
    assert!(r.failed.iter().any(|(k, m)| k.starts_with("Docs/") && m.contains("junction")), "{r:?}");
    // Other sources are still backed up.
    assert!(r.copied.contains(&"One/single.txt".to_string()));
}

#[test]
fn leftover_temp_files_are_swept_after_an_interrupted_backup() {
    let e = env();
    backup(&e);
    // Simulate a drive pulled mid-copy: an orphan temp file plus the in-progress marker.
    let orphan = on_drive(&e, "Docs/sub/.gone.bin.syncapp-tmp");
    write(&orphan, b"half a file");
    write(&e.drive.join(".syncapp").join("backup-in-progress"), b"1");
    let r = backup(&e); // nothing changed on the PC
    assert!(r.copied.is_empty());
    assert!(!orphan.exists(), "orphaned temp file should be removed");
    assert!(!e.drive.join(".syncapp").join("backup-in-progress").exists());
    // And with no interruption, an unchanged backup still writes nothing.
    let before = snapshot(&e.drive);
    backup(&e);
    assert_eq!(snapshot(&e.drive), before);
}

#[test]
fn exclusively_locked_file_is_reported_and_old_backup_kept() {
    use std::os::windows::fs::OpenOptionsExt;
    let e = env();
    backup(&e);
    write(&e.pc.join("docs/a.txt"), b"new version, locked by an app");
    // Simulate an app holding the file open with no sharing (like Outlook with a .pst).
    let _lock = OpenOptions::new().read(true).share_mode(0).open(e.pc.join("docs/a.txt")).unwrap();
    let r = backup(&e);
    assert_eq!(r.failed.len(), 1, "{r:?}");
    assert!(r.failed[0].0 == "Docs/a.txt");
    assert!(r.failed[0].1.contains("in use"), "message should say the file is in use: {}", r.failed[0].1);
    // The previous backup copy is untouched and other files were still processed.
    assert_eq!(fs::read(on_drive(&e, "Docs/a.txt")).unwrap(), b"alpha");
    drop(_lock);
    // Once the app closes it, the next backup picks it up.
    let r = backup(&e);
    assert_eq!(r.copied, vec!["Docs/a.txt".to_string()]);
}

#[test]
fn file_changed_between_scan_and_copy_is_copied_as_its_latest_version() {
    let e = env();
    backup(&e);
    write(&e.pc.join("docs/a.txt"), b"edit 1");
    let no = AtomicBool::new(false);
    let path = e.pc.join("docs/a.txt");
    let mut edited = false;
    // Edit the file again right after the scan, before it's copied.
    let mut cb = |ev: engine::Event| {
        if let engine::Event::Planned { .. } = ev {
            if !edited {
                write(&path, b"edit 2, saved mid-backup");
                set_mtime(&path, SystemTime::now() + Duration::from_secs(30));
                edited = true;
            }
        }
    };
    let r = engine::run_backup(&sources(&e), &e.drive, &no, Some(&mut cb));
    assert!(r.ok(), "{r:?}");
    assert_eq!(fs::read(on_drive(&e, "Docs/a.txt")).unwrap(), b"edit 2, saved mid-backup");
    // And the manifest matches what's on the drive: the next run sees no changes.
    let r = backup(&e);
    assert!(r.copied.is_empty() && r.refreshed == 0, "{r:?}");
}
