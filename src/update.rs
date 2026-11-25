//! Checking GitHub for a newer SyncApp release, and downloading it safely.
//!
//! * Only the project's own repository (Cargo.toml `repository`) is consulted, only
//!   over HTTPS, and only published (non-draft, non-prerelease) releases count.
//! * CI publishes every build of `main` as one rolling release with the tag `release`,
//!   titled "SyncApp X.Y.Z"; the version is read from the title.
//! * A release is only offered for installation if it includes `SyncApp.exe` and a
//!   `SyncApp.exe.sha256` checksum; the download must match the checksum exactly.
//! * Nothing is installed without the user clicking "Update now".

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use serde::{Deserialize, Serialize};

use crate::config::data_dir;
use crate::engine::atomic_write;

pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
const EXE_ASSET: &str = "SyncApp.exe";
const SUM_ASSET: &str = "SyncApp.exe.sha256";
/// Tag of the rolling release CI replaces on every build of main.
const ROLLING_TAG: &str = "release";
/// Background checks happen at most this often.
pub const CHECK_EVERY_S: u64 = 24 * 3600;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Release {
    pub version: String,
    pub page_url: String,
    pub exe_url: String,
    pub sha_url: String,
    pub size: u64,
    /// Release notes (Markdown), shortened.
    pub notes: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct UpdateState {
    /// Unix time of the last successful check.
    pub checked_at: u64,
    /// The newest release, if it is newer than this version.
    pub available: Option<Release>,
    /// Version the user has already been notified about.
    #[serde(default)]
    pub notified: Option<String>,
}

impl UpdateState {
    /// The available update, if it's still newer than the running version.
    pub fn newer(&self) -> Option<&Release> {
        self.available.as_ref().filter(|r| is_newer(&r.version, CURRENT))
    }
}

fn state_path() -> PathBuf {
    data_dir().join("update.json")
}

pub fn load_state() -> UpdateState {
    fs::read(state_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub fn save_state(s: &UpdateState) {
    if let Ok(b) = serde_json::to_vec_pretty(s) {
        let _ = atomic_write(&state_path(), &b);
    }
}

/// "v1.2.3" / "1.2" / "1.2.3-beta" -> (1, 2, 3).
pub fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim().trim_start_matches(['v', 'V']);
    let core = s.split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|p| p.parse::<u64>());
    let major = parts.next()?.ok()?;
    let minor = parts.next().unwrap_or(Ok(0)).ok()?;
    let patch = parts.next().unwrap_or(Ok(0)).ok()?;
    Some((major, minor, patch))
}

pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

/// ("owner", "repo") from Cargo.toml's `repository` URL.
fn repo() -> Option<(String, String)> {
    let url = env!("CARGO_PKG_REPOSITORY");
    let rest = url.strip_prefix("https://github.com/")?;
    let mut it = rest.trim_end_matches('/').trim_end_matches(".git").split('/');
    Some((it.next()?.to_string(), it.next()?.to_string()))
}

pub fn releases_page() -> String {
    format!("{}/releases", env!("CARGO_PKG_REPOSITORY").trim_end_matches('/'))
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
}

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    name: Option<String>,
    html_url: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

/// Parse GitHub's "latest release" JSON into an installable release, if it has the
/// exe and its checksum.
pub fn parse_release(json: &[u8]) -> Result<Option<Release>, String> {
    let r: GhRelease = serde_json::from_slice(json).map_err(|e| format!("unexpected reply from GitHub: {e}"))?;
    if r.draft || r.prerelease {
        return Ok(None);
    }
    let find = |n: &str| r.assets.iter().find(|a| a.name.eq_ignore_ascii_case(n));
    let (Some(exe), Some(sum)) = (find(EXE_ASSET), find(SUM_ASSET)) else {
        return Ok(None); // not installable automatically
    };
    if !exe.browser_download_url.starts_with("https://") || !sum.browser_download_url.starts_with("https://") {
        return Ok(None);
    }
    let mut notes = r.body.unwrap_or_default();
    if notes.chars().count() > 2000 {
        notes = notes.chars().take(2000).collect::<String>() + "...";
    }
    // Version from a "v1.2.3" tag, else from the title ("SyncApp 1.2.3") of the rolling release.
    let version = if parse_version(&r.tag_name).is_some() {
        r.tag_name.trim_start_matches(['v', 'V']).to_string()
    } else {
        match r.name.as_deref().and_then(|n| n.split_whitespace().find(|w| parse_version(w).is_some())) {
            Some(v) => v.trim_start_matches(['v', 'V']).to_string(),
            None => return Ok(None),
        }
    };
    Ok(Some(Release {
        version,
        page_url: r.html_url,
        exe_url: exe.browser_download_url.clone(),
        sha_url: sum.browser_download_url.clone(),
        size: exe.size,
        notes,
    }))
}

/// Ask GitHub for the latest release and record the result. Returns the release if
/// it's newer than this version.
pub fn check() -> Result<Option<Release>, String> {
    let (owner, name) = repo().ok_or("no GitHub repository configured")?;
    // The rolling "release" build first; fall back to the newest versioned release.
    let rolling = format!("https://api.github.com/repos/{owner}/{name}/releases/tags/{ROLLING_TAG}");
    let (mut status, mut body) = crate::net::get(&rolling, 1 << 20)?;
    if status == 404 {
        let latest = format!("https://api.github.com/repos/{owner}/{name}/releases/latest");
        (status, body) = crate::net::get(&latest, 1 << 20)?;
    }
    let latest = match status {
        200 => parse_release(&body)?,
        404 => None, // no releases published yet
        403 | 429 => return Err("GitHub is rate-limiting requests; try again later".into()),
        s => return Err(format!("GitHub returned HTTP {s}")),
    };
    let newer = latest.filter(|r| is_newer(&r.version, CURRENT));
    let mut st = load_state();
    st.checked_at = crate::win::now_unix();
    st.available = newer.clone();
    save_state(&st);
    crate::info!(
        "Update check: {}",
        match &newer {
            Some(r) => format!("version {} available", r.version),
            None => format!("{CURRENT} is up to date"),
        }
    );
    Ok(newer)
}

/// Expected SHA-256 from a "<hex>  SyncApp.exe" style checksum file.
pub fn parse_checksum(text: &str) -> Option<String> {
    let h = text.split_whitespace().next()?.to_ascii_lowercase();
    (h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit())).then_some(h)
}

fn update_dir() -> PathBuf {
    data_dir().join("update")
}

/// Download the release's exe and verify it against the published checksum.
/// Returns the path of the verified file, ready to install.
pub fn download(rel: &Release, cancel: &AtomicBool, progress: &mut dyn FnMut(u64, u64)) -> Result<PathBuf, String> {
    let dir = update_dir();
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let (status, sum) = crate::net::get(&rel.sha_url, 4096)?;
    if status != 200 {
        return Err(format!("couldn't get the checksum (HTTP {status})"));
    }
    let expected = parse_checksum(&String::from_utf8_lossy(&sum)).ok_or("the release's checksum file is invalid")?;
    let part = dir.join(format!("SyncApp-{}.exe.part", rel.version));
    let done = dir.join(format!("SyncApp-{}.exe", rel.version));
    let res = (|| {
        crate::net::download(&rel.exe_url, &part, cancel, progress)?;
        let actual = crate::net::sha256_file(&part)?;
        if actual != expected {
            return Err("the download doesn't match the published checksum, so it was discarded".to_string());
        }
        let head = fs::read(&part).map_err(|e| e.to_string())?;
        if !head.starts_with(b"MZ") {
            return Err("the download isn't a Windows program".to_string());
        }
        let _ = fs::remove_file(&done);
        fs::rename(&part, &done).map_err(|e| e.to_string())
    })();
    if let Err(e) = res {
        let _ = fs::remove_file(&part);
        crate::warn!("Update download failed: {e}");
        return Err(e);
    }
    crate::info!("Downloaded and verified SyncApp {}", rel.version);
    Ok(done)
}

/// The previous exe kept during an update (deleted once the new version is running).
pub fn old_exe_path(exe: &Path) -> PathBuf {
    exe.with_extension("exe.old")
}

/// After an update: remove the previous version and downloaded files (retries while the
/// old process finishes exiting).
pub fn cleanup_after_update() {
    let Ok(exe) = std::env::current_exe() else { return };
    let old = old_exe_path(&exe);
    if !old.exists() {
        return;
    }
    std::thread::spawn(move || {
        for _ in 0..20 {
            if fs::remove_file(&old).is_ok() || !old.exists() {
                let _ = fs::remove_dir_all(update_dir());
                crate::info!("Update finished: now running SyncApp {CURRENT}");
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("0.10"), Some((0, 10, 0)));
        assert_eq!(parse_version("2.0.0-beta.1"), Some((2, 0, 0)));
        assert_eq!(parse_version("banana"), None);
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(is_newer("0.1.10", "0.1.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.0.9", "0.1.0"));
        assert!(!is_newer("junk", "0.1.0"));
    }

    #[test]
    fn checksum_file() {
        let h = "a".repeat(64);
        assert_eq!(parse_checksum(&format!("{h}  SyncApp.exe\n")), Some(h.clone()));
        assert_eq!(parse_checksum(&h.to_uppercase()), Some(h));
        assert_eq!(parse_checksum("not a hash"), None);
    }

    #[test]
    fn release_json() {
        let json = br#"{
            "tag_name": "v0.2.0", "html_url": "https://github.com/o/r/releases/tag/v0.2.0",
            "body": "Fixes", "draft": false, "prerelease": false,
            "assets": [
                {"name": "SyncApp.exe", "browser_download_url": "https://github.com/o/r/releases/download/v0.2.0/SyncApp.exe", "size": 123},
                {"name": "SyncApp.exe.sha256", "browser_download_url": "https://github.com/o/r/releases/download/v0.2.0/SyncApp.exe.sha256", "size": 80}
            ]}"#;
        let r = parse_release(json).unwrap().unwrap();
        assert_eq!(r.version, "0.2.0");
        assert_eq!(r.size, 123);
        // Without a checksum asset, it's not offered for automatic install.
        let no_sum = br#"{"tag_name":"v0.2.0","html_url":"x","assets":[{"name":"SyncApp.exe","browser_download_url":"https://x/SyncApp.exe"}]}"#;
        assert_eq!(parse_release(no_sum).unwrap(), None);
        let pre = br#"{"tag_name":"v9.0.0","html_url":"x","prerelease":true,"assets":[]}"#;
        assert_eq!(parse_release(pre).unwrap(), None);
        // The rolling release: fixed tag, version in the title.
        let rolling = br#"{"tag_name":"release","name":"SyncApp 0.1.7","html_url":"x","assets":[
            {"name":"SyncApp.exe","browser_download_url":"https://x/SyncApp.exe","size":5},
            {"name":"SyncApp.exe.sha256","browser_download_url":"https://x/SyncApp.exe.sha256","size":5}]}"#;
        assert_eq!(parse_release(rolling).unwrap().unwrap().version, "0.1.7");
        let untitled = br#"{"tag_name":"release","name":"nightly","html_url":"x","assets":[
            {"name":"SyncApp.exe","browser_download_url":"https://x/SyncApp.exe"},
            {"name":"SyncApp.exe.sha256","browser_download_url":"https://x/SyncApp.exe.sha256"}]}"#;
        assert_eq!(parse_release(untitled).unwrap(), None);
    }
}
