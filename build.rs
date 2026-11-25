//! Embeds the app icon and version information into SyncApp.exe, so Explorer, the
//! Start menu, Task Manager and file properties show them.

fn main() {
    println!("cargo:rerun-if-changed=assets/syncapp.ico");
    // CI sets SYNCAPP_RELEASE_BUILD for published builds; everything else is a
    // development build (its version number may lag behind the published releases).
    println!("cargo:rerun-if-env-changed=SYNCAPP_RELEASE_BUILD");
    let release = std::env::var("SYNCAPP_RELEASE_BUILD").is_ok_and(|v| !v.is_empty());
    println!("cargo:rustc-env=SYNCAPP_BUILD={}", if release { "release" } else { "dev" });
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/syncapp.ico")
        .set("FileDescription", "SyncApp - automatic backups to your marked drives")
        .set("ProductName", "SyncApp")
        .set("OriginalFilename", "SyncApp.exe")
        .set("InternalName", "SyncApp")
        .set("LegalCopyright", "Copyright (c) 2026 banana. MIT License.");
    // Version fields come from Cargo.toml's `version` automatically.
    if let Err(e) = res.compile() {
        // Don't fail the build over cosmetics (e.g. no resource compiler available).
        println!("cargo:warning=could not embed icon/version info: {e}");
    }
}
