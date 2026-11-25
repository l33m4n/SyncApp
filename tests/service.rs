use std::path::PathBuf;

use syncapp::config::{Config, Source, SourceKind};
use syncapp::service::original_path;

fn cfg_with(path: &str, kind: Option<SourceKind>) -> Config {
    Config {
        sources: vec![Source { name: "Taxes".into(), path: PathBuf::from(path), exclude: vec![], enabled: true, kind }],
        ..Default::default()
    }
}

#[test]
fn restore_target_for_a_deleted_folder_source() {
    // The folder no longer exists on the PC: files must go back *inside* it.
    for kind in [Some(SourceKind::Folder), None] {
        let cfg = cfg_with(r"C:\NoSuchPlace\Docs\Taxes", kind);
        assert_eq!(
            original_path(&cfg, "Taxes/2024/a.pdf"),
            Some(PathBuf::from(r"C:\NoSuchPlace\Docs\Taxes\2024\a.pdf")),
            "kind {kind:?}"
        );
    }
}

#[test]
fn restore_target_for_a_single_file_source() {
    let cfg = Config {
        sources: vec![Source {
            name: "Db".into(),
            path: PathBuf::from(r"C:\NoSuchPlace\vault.kdbx"),
            exclude: vec![],
            enabled: true,
            kind: Some(SourceKind::File),
        }],
        ..Default::default()
    };
    assert_eq!(original_path(&cfg, "Db/vault.kdbx"), Some(PathBuf::from(r"C:\NoSuchPlace\vault.kdbx")));
    // Old config without a recorded kind: inferred from the key.
    let mut old = cfg.clone();
    old.sources[0].kind = None;
    assert_eq!(original_path(&old, "Db/vault.kdbx"), Some(PathBuf::from(r"C:\NoSuchPlace\vault.kdbx")));
}

#[test]
fn unknown_source_has_no_original_location() {
    let cfg = cfg_with(r"C:\x", None);
    assert_eq!(original_path(&cfg, "Other/file.txt"), None);
}
