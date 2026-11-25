//! Thin, safe wrappers over the Win32 calls SyncApp needs.

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, SYSTEMTIME, FILETIME, WAIT_ABANDONED, WAIT_OBJECT_0};
use windows_sys::Win32::Security::Cryptography::{BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG};
use windows_sys::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetDriveTypeW, GetFileAttributesW, GetLogicalDrives, GetVolumeInformationW,
    GetVolumePathNameW, SetFileAttributesW, FILE_ATTRIBUTE_HIDDEN, INVALID_FILE_ATTRIBUTES,
};
use windows_sys::Win32::System::Diagnostics::Debug::{SetErrorMode, SEM_FAILCRITICALERRORS, SEM_NOOPENFILEERRORBOX};
use windows_sys::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ,
};
use windows_sys::Win32::System::SystemInformation::GetLocalTime;
use windows_sys::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};
use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

const DRIVE_REMOVABLE: u32 = 2;
const DRIVE_FIXED: u32 = 3;

pub fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
}

fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// "e", "E:", "e:\" -> "E:\"
pub fn normalize_root(s: &str) -> Result<String, String> {
    let t = s.trim().trim_end_matches(['\\', '/']);
    let b = t.as_bytes();
    let ok = (b.len() == 1 && b[0].is_ascii_alphabetic())
        || (b.len() == 2 && b[0].is_ascii_alphabetic() && b[1] == b':');
    if !ok {
        return Err(format!("'{s}' is not a drive letter (use e.g. E:)"));
    }
    Ok(format!("{}:\\", (b[0] as char).to_ascii_uppercase()))
}

pub fn system_root() -> String {
    std::env::var("SystemDrive")
        .ok()
        .and_then(|s| normalize_root(&s).ok())
        .unwrap_or_else(|| "C:\\".into())
}

/// Bitmask of present drive letters; cheap enough to poll.
pub fn drive_mask() -> u32 {
    unsafe { GetLogicalDrives() }
}

/// Roots of drives that could be backup targets: removable or fixed, never the system drive.
pub fn candidate_drives() -> Vec<String> {
    let mask = drive_mask();
    let sys = system_root();
    (0..26u8)
        .filter(|i| mask >> i & 1 == 1)
        .map(|i| format!("{}:\\", (b'A' + i) as char))
        .filter(|root| *root != sys)
        .filter(|root| {
            let t = unsafe { GetDriveTypeW(wide(root).as_ptr()) };
            t == DRIVE_REMOVABLE || t == DRIVE_FIXED
        })
        .collect()
}

#[derive(Clone, Debug)]
pub struct VolumeInfo {
    pub root: String,
    pub label: String,
    pub serial: u32,
    pub fs_name: String,
}

pub fn volume_root(path: &Path) -> Option<String> {
    let mut buf = [0u16; 1024];
    let ok = unsafe { GetVolumePathNameW(wide(path).as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
    (ok != 0).then(|| from_wide(&buf))
}

pub fn volume_info(path: &Path) -> Option<VolumeInfo> {
    let root = volume_root(path)?;
    let mut label = [0u16; 261];
    let mut fs = [0u16; 261];
    let (mut serial, mut maxlen, mut flags) = (0u32, 0u32, 0u32);
    let ok = unsafe {
        GetVolumeInformationW(
            wide(&root).as_ptr(),
            label.as_mut_ptr(),
            label.len() as u32,
            &mut serial,
            &mut maxlen,
            &mut flags,
            fs.as_mut_ptr(),
            fs.len() as u32,
        )
    };
    (ok != 0).then(|| VolumeInfo { root, label: from_wide(&label), serial, fs_name: from_wide(&fs) })
}

/// (free bytes available to this user, total bytes)
pub fn disk_space(path: &Path) -> io::Result<(u64, u64)> {
    let (mut free, mut total, mut total_free) = (0u64, 0u64, 0u64);
    let ok = unsafe { GetDiskFreeSpaceExW(wide(path).as_ptr(), &mut free, &mut total, &mut total_free) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((free, total))
}

pub fn set_hidden(path: &Path) {
    let w = wide(path);
    unsafe {
        let attrs = GetFileAttributesW(w.as_ptr());
        if attrs != INVALID_FILE_ATTRIBUTES {
            SetFileAttributesW(w.as_ptr(), attrs | FILE_ATTRIBUTE_HIDDEN);
        }
    }
}

/// Stop Windows from popping "insert a disk" dialogs for empty card-reader slots.
pub fn suppress_error_dialogs() {
    unsafe {
        SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOOPENFILEERRORBOX);
    }
}

/// 128-bit random hex id from the OS CSPRNG.
pub fn random_id() -> String {
    let mut b = [0u8; 16];
    let status = unsafe { BCryptGenRandom(null_mut(), b.as_mut_ptr(), b.len() as u32, BCRYPT_USE_SYSTEM_PREFERRED_RNG) };
    assert!(status == 0, "BCryptGenRandom failed: {status:#x}");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn fmt_systemtime(t: &SYSTEMTIME) -> String {
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}

pub fn local_now_string() -> String {
    let mut t: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut t) };
    fmt_systemtime(&t)
}

/// Unix seconds -> "YYYY-MM-DD HH:MM:SS" in local time.
pub fn local_time_string(unix: u64) -> String {
    let ticks = (unix + 11_644_473_600) * 10_000_000;
    let ft = FILETIME { dwLowDateTime: ticks as u32, dwHighDateTime: (ticks >> 32) as u32 };
    unsafe {
        let mut utc: SYSTEMTIME = std::mem::zeroed();
        let mut local: SYSTEMTIME = std::mem::zeroed();
        if FileTimeToSystemTime(&ft, &mut utc) == 0 || SystemTimeToTzSpecificLocalTime(null(), &utc, &mut local) == 0 {
            return format!("@{unix}");
        }
        fmt_systemtime(&local)
    }
}

/// Open a file/folder/URL with its default handler.
pub fn shell_open(target: &Path) {
    unsafe {
        ShellExecuteW(null_mut(), wide("open").as_ptr(), wide(target).as_ptr(), null(), null(), SW_SHOWNORMAL);
    }
}

// ---------------------------------------------------------------- autostart (HKCU Run key)

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "SyncApp";

pub fn set_autostart(command: Option<&str>) -> io::Result<()> {
    let key = wide(RUN_KEY);
    let name = wide(RUN_VALUE);
    let rc = unsafe {
        match command {
            Some(cmd) => {
                let data = wide(cmd);
                RegSetKeyValueW(
                    HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr(), REG_SZ,
                    data.as_ptr().cast(), (data.len() * 2) as u32,
                )
            }
            None => {
                let rc = RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr());
                if rc == 2 { 0 } else { rc } // ERROR_FILE_NOT_FOUND = already off
            }
        }
    };
    if rc == 0 { Ok(()) } else { Err(io::Error::from_raw_os_error(rc as i32)) }
}

pub fn get_autostart() -> Option<String> {
    let key = wide(RUN_KEY);
    let name = wide(RUN_VALUE);
    let mut buf = vec![0u16; 2048];
    let mut size = (buf.len() * 2) as u32;
    let rc = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr(), RRF_RT_REG_SZ, null_mut(), buf.as_mut_ptr().cast(), &mut size)
    };
    (rc == 0).then(|| from_wide(&buf))
}

// ---------------------------------------------------------------- named mutex

/// System-wide named mutex, released automatically if the process dies.
/// Keeps two backups from ever writing to the same drive at once,
/// without writing a lock file to the drive.
pub struct NamedLock {
    h: HANDLE,
    held: bool,
}

unsafe impl Send for NamedLock {}

impl NamedLock {
    pub fn try_acquire(name: &str) -> io::Result<Option<NamedLock>> {
        let h = unsafe { CreateMutexW(null(), 0, wide(name).as_ptr()) };
        if h.is_null() {
            return Err(io::Error::last_os_error());
        }
        let r = unsafe { WaitForSingleObject(h, 0) };
        if r == WAIT_OBJECT_0 || r == WAIT_ABANDONED {
            Ok(Some(NamedLock { h, held: true }))
        } else {
            unsafe { CloseHandle(h) };
            Ok(None)
        }
    }

    /// Wait up to `ms` milliseconds for the lock.
    pub fn acquire_wait(name: &str, ms: u32) -> io::Result<Option<NamedLock>> {
        let h = unsafe { CreateMutexW(null(), 0, wide(name).as_ptr()) };
        if h.is_null() {
            return Err(io::Error::last_os_error());
        }
        let r = unsafe { WaitForSingleObject(h, ms) };
        if r == WAIT_OBJECT_0 || r == WAIT_ABANDONED {
            Ok(Some(NamedLock { h, held: true }))
        } else {
            unsafe { CloseHandle(h) };
            Ok(None)
        }
    }
}

impl Drop for NamedLock {
    fn drop(&mut self) {
        unsafe {
            if self.held {
                ReleaseMutex(self.h);
            }
            CloseHandle(self.h);
        }
    }
}
