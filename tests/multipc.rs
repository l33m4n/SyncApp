//! Several PCs sharing one backup drive, and clean-up of files no PC needs any more.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime};

use syncapp::config::Source;
use syncapp::engine::{self, OrphanReason, PcIdentity, Report, BACKUP_DIR};

/// Keep this test program's log/history out of the user's real SyncApp folders.
fn isolate() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let d = std::env::temp_dir().join(format!("syncapp-tests-{}", std::process::id()));
        // Safe: done once, before any test touches the environment (every test calls this first).
        unsafe { std::env::set_var("SYNCAPP_HOME", d) };
    });
}

struct Env {
    _tmp: tempfile::TempDir,
    a: PathBuf,
    b: PathBuf,
    drive: PathBuf,
}

fn env() -> Env {
    isolate();
    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("pcA").join("Docs");
    let b = tmp.path().join("pcB").join("Docs");
    let drive = tmp.path().join("drive");
    for d in [&a, &b, &drive] {
        fs::create_dir_all(d).unwrap();
    }
    Env { _tmp: tmp, a, b, drive }
}

fn write_at(p: &Path, data: &[u8], age_secs: u64) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    File::create(p).unwrap().write_all(data).unwrap();
    let t = SystemTime::now() - Duration::from_secs(age_secs);
    OpenOptions::new().write(true).open(p).unwrap().set_modified(t).unwrap();
}

fn pc(id: &str) -> PcIdentity {
    PcIdentity { id: id.into(), name: id.to_uppercase() }
}

/// Both PCs back up a folder called "Docs" to the same place on the drive.
fn backup(e: &Env, which: &str) -> Report {
    let path = if which == "a" { &e.a } else { &e.b };
    let src = vec![Source { name: "Docs".into(), path: path.clone(), exclude: vec![], enabled: true, kind: None }];
    let no = AtomicBool::new(false);
    engine::run_backup(&src, &e.drive, &pc(which), &no, None)
}

fn on_drive(e: &Env, rel: &str) -> Vec<u8> {
    fs::read(e.drive.join(BACKUP_DIR).join("Docs").join(rel)).unwrap()
}

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

#[test]
fn newest_version_wins_and_is_never_overwritten_by_an_older_one() {
    let e = env();
    write_at(&e.a.join("notes.txt"), b"v1 from A (old)", 3600);
    assert_eq!(backup(&e, "a").copied.len(), 1);

    // PC B has a newer version of the same file: it replaces A's.
    write_at(&e.b.join("notes.txt"), b"v2 from B (newer)", 60);
    let r = backup(&e, "b");
    assert_eq!(r.copied, vec!["Docs/notes.txt".to_string()]);
    assert_eq!(on_drive(&e, "notes.txt"), b"v2 from B (newer)");

    // A still has the old version and runs again: it must not put the old one back.
    let r = backup(&e, "a");
    assert!(r.copied.is_empty(), "{r:?}");
    assert_eq!(on_drive(&e, "notes.txt"), b"v2 from B (newer)");

    // A edits the file: now A's is the newest and wins.
    write_at(&e.a.join("notes.txt"), b"v3 from A (newest)", 0);
    assert_eq!(backup(&e, "a").copied.len(), 1);
    assert_eq!(on_drive(&e, "notes.txt"), b"v3 from A (newest)");

    // B hasn't touched its copy: B leaves A's newer version alone.
    let r = backup(&e, "b");
    assert!(r.copied.is_empty(), "{r:?}");
    assert_eq!(on_drive(&e, "notes.txt"), b"v3 from A (newest)");
}

#[test]
fn older_file_on_a_new_pc_is_reported_once_not_copied() {
    let e = env();
    write_at(&e.a.join("plan.txt"), b"current plan", 60);
    backup(&e, "a");
    // B joins later with an old copy it never backed up before.
    write_at(&e.b.join("plan.txt"), b"an old plan!", 7200);
    let r = backup(&e, "b");
    assert!(r.copied.is_empty());
    assert_eq!(r.newer_on_drive, vec![("Docs/plan.txt".to_string(), "A".to_string())]);
    assert_eq!(on_drive(&e, "plan.txt"), b"current plan");
    // Next time it's simply unchanged: no repeated warning, nothing written.
    let before = snapshot(&e.drive);
    let r = backup(&e, "b");
    assert!(r.newer_on_drive.is_empty() && r.copied.is_empty(), "{r:?}");
    assert_eq!(snapshot(&e.drive), before);
}

#[test]
fn identical_files_on_two_pcs_cause_no_ping_pong() {
    let e = env();
    // Same content, different dates (e.g. copied between PCs).
    write_at(&e.a.join("photo.jpg"), &vec![1u8; 50_000], 500);
    write_at(&e.b.join("photo.jpg"), &vec![1u8; 50_000], 100);
    backup(&e, "a");
    let r = backup(&e, "b");
    assert!(r.copied.is_empty(), "identical content must not be copied again: {r:?}");
    // From now on, alternating between the PCs writes nothing at all.
    let before = snapshot(&e.drive);
    for which in ["a", "b", "a", "b"] {
        let r = backup(&e, which);
        assert!(r.copied.is_empty() && r.refreshed == 0, "{which}: {r:?}");
    }
    assert_eq!(snapshot(&e.drive), before);
}

#[test]
fn orphans_are_files_no_pc_has_any_more() {
    let e = env();
    write_at(&e.a.join("shared.txt"), b"on both", 100);
    write_at(&e.b.join("shared.txt"), b"on both", 100);
    write_at(&e.a.join("sub/only-a.txt"), b"only on A", 100);
    backup(&e, "a");
    backup(&e, "b");
    let mine = vec!["Docs".to_string()];
    assert!(engine::find_orphans(&e.drive, &mine, "a").is_empty());

    // A deletes both files. "shared.txt" is still on B, so it isn't an orphan.
    fs::remove_file(e.a.join("shared.txt")).unwrap();
    fs::remove_file(e.a.join("sub/only-a.txt")).unwrap();
    backup(&e, "a");
    let o = engine::find_orphans(&e.drive, &mine, "a");
    assert_eq!(o.len(), 1);
    assert_eq!(o[0].key, "Docs/sub/only-a.txt");
    assert_eq!(o[0].reason, OrphanReason::GoneEverywhere);

    // Once B deletes it too, it becomes an orphan.
    fs::remove_file(e.b.join("shared.txt")).unwrap();
    backup(&e, "b");
    let keys: Vec<String> = engine::find_orphans(&e.drive, &mine, "a").into_iter().map(|o| o.key).collect();
    assert_eq!(keys, vec!["Docs/shared.txt".to_string(), "Docs/sub/only-a.txt".to_string()]);

    // Clean up: files and the now-empty folder are removed, and forgotten.
    let no = AtomicBool::new(false);
    let r = engine::remove_backed_up(&e.drive, &keys, &no, None);
    assert_eq!((r.removed, r.failed.len()), (2, 0));
    assert!(!e.drive.join(BACKUP_DIR).join("Docs").join("sub").exists());
    assert!(engine::find_orphans(&e.drive, &mine, "a").is_empty());
    // A backup afterwards doesn't bring anything back or complain.
    let r = backup(&e, "a");
    assert!(r.ok() && r.copied.is_empty(), "{r:?}");
}

#[test]
fn files_from_a_source_this_pc_stopped_backing_up_are_orphans() {
    let e = env();
    write_at(&e.a.join("old-project.txt"), b"done", 100);
    backup(&e, "a");
    // "Docs" was removed from A's sources (and no other PC has it).
    let o = engine::find_orphans(&e.drive, &[], "a");
    assert_eq!(o.len(), 1);
    assert_eq!(o[0].reason, OrphanReason::SourceRemoved);
    // But if B still backs up a "Docs" with that file, it's kept.
    write_at(&e.b.join("old-project.txt"), b"done", 100);
    backup(&e, "b");
    assert!(engine::find_orphans(&e.drive, &[], "a").is_empty());
}

#[test]
fn a_source_folder_that_is_temporarily_missing_is_not_treated_as_deleted() {
    let e = env();
    write_at(&e.a.join("keep.txt"), b"keep me", 100);
    backup(&e, "a");
    // The folder is unavailable (e.g. an unplugged external disk on the PC).
    let moved = e.a.with_file_name("Docs-away");
    fs::rename(&e.a, &moved).unwrap();
    let r = backup(&e, "a");
    assert_eq!(r.missing_sources.len(), 1);
    assert!(engine::find_orphans(&e.drive, &["Docs".to_string()], "a").is_empty());
}
