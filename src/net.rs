//! Minimal HTTPS client on WinHTTP (built into Windows): uses the system's proxy settings
//! and certificate validation, follows redirects, and adds no dependencies.
//! Only https:// URLs are accepted.

use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};

use windows_sys::Win32::Networking::WinHttp::*;
use windows_sys::Win32::Security::Cryptography::{
    BCryptCreateHash, BCryptDestroyHash, BCryptDestroyKey, BCryptFinishHash, BCryptHashData, BCryptImportKeyPair,
    BCryptVerifySignature, BCRYPT_ECCKEY_BLOB, BCRYPT_ECCPUBLIC_BLOB, BCRYPT_ECDSA_P256_ALG_HANDLE,
    BCRYPT_ECDSA_PUBLIC_P256_MAGIC, BCRYPT_SHA256_ALG_HANDLE,
};

use crate::win::wide;

struct Handle(*mut core::ffi::c_void);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { WinHttpCloseHandle(self.0) };
        }
    }
}

fn last_err(what: &str) -> String {
    format!("{what} failed: {}", std::io::Error::last_os_error())
}

/// Open a GET request and wait for the response headers.
/// Returns (session, connection, request, status code, content length).
fn open(url: &str) -> Result<(Handle, Handle, Handle, u32, Option<u64>), String> {
    let rest = url.strip_prefix("https://").ok_or("only https:// URLs are allowed")?;
    let (host, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let agent = format!("SyncApp/{}", env!("CARGO_PKG_VERSION"));
    unsafe {
        let session = Handle(WinHttpOpen(wide(&agent).as_ptr(), WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, null(), null(), 0));
        if session.0.is_null() {
            return Err(last_err("WinHttpOpen"));
        }
        WinHttpSetTimeouts(session.0, 15_000, 15_000, 30_000, 60_000);
        let conn = Handle(WinHttpConnect(session.0, wide(host).as_ptr(), INTERNET_DEFAULT_HTTPS_PORT, 0));
        if conn.0.is_null() {
            return Err(last_err("connect"));
        }
        let req = Handle(WinHttpOpenRequest(
            conn.0,
            wide("GET").as_ptr(),
            wide(path).as_ptr(),
            null(),
            null(),
            null(),
            WINHTTP_FLAG_SECURE,
        ));
        if req.0.is_null() {
            return Err(last_err("request"));
        }
        let headers = wide("Accept: application/vnd.github+json, application/octet-stream;q=0.9, */*;q=0.8");
        WinHttpAddRequestHeaders(req.0, headers.as_ptr(), u32::MAX, WINHTTP_ADDREQ_FLAG_ADD);
        if WinHttpSendRequest(req.0, null(), 0, null(), 0, 0, 0) == 0 {
            return Err(last_err("send"));
        }
        if WinHttpReceiveResponse(req.0, null_mut()) == 0 {
            return Err(last_err("receive"));
        }
        let mut status: u32 = 0;
        let mut size = 4u32;
        WinHttpQueryHeaders(
            req.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            null(),
            (&mut status as *mut u32).cast(),
            &mut size,
            null_mut(),
        );
        let mut len: u32 = 0;
        let mut size = 4u32;
        let has_len = WinHttpQueryHeaders(
            req.0,
            WINHTTP_QUERY_CONTENT_LENGTH | WINHTTP_QUERY_FLAG_NUMBER,
            null(),
            (&mut len as *mut u32).cast(),
            &mut size,
            null_mut(),
        ) != 0;
        Ok((session, conn, req, status, has_len.then_some(len as u64)))
    }
}

fn read_chunk(req: &Handle, buf: &mut [u8]) -> Result<usize, String> {
    let mut n: u32 = 0;
    let ok = unsafe { WinHttpReadData(req.0, buf.as_mut_ptr().cast(), buf.len() as u32, &mut n) };
    if ok == 0 {
        return Err(last_err("read"));
    }
    Ok(n as usize)
}

/// GET a small resource (JSON, checksum file) into memory. Returns (status, body).
pub fn get(url: &str, max_bytes: usize) -> Result<(u32, Vec<u8>), String> {
    let (_s, _c, req, status, _) = open(url)?;
    let mut body = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = read_chunk(&req, &mut buf)?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&buf[..n]);
        if body.len() > max_bytes {
            return Err("response too large".into());
        }
    }
    Ok((status, body))
}

/// Download a file, reporting (done, total) progress. Writes to `dest`.
pub fn download(
    url: &str,
    dest: &Path,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<u64, String> {
    let (_s, _c, req, status, len) = open(url)?;
    if status != 200 {
        return Err(format!("download failed (HTTP {status})"));
    }
    let total = len.unwrap_or(0);
    let mut f = File::create(dest).map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; 256 * 1024];
    let mut done = 0u64;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        let n = read_chunk(&req, &mut buf)?;
        if n == 0 {
            break;
        }
        f.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        done += n as u64;
        progress(done, total);
    }
    f.sync_all().map_err(|e| e.to_string())?;
    if total > 0 && done != total {
        return Err(format!("download incomplete ({done} of {total} bytes)"));
    }
    Ok(done)
}

/// SHA-256 of a file, lowercase hex (Windows CNG; no extra dependencies).
pub fn sha256_file(path: &Path) -> Result<String, String> {
    sha256_file_raw(path).map(|d| d.iter().map(|b| format!("{b:02x}")).collect())
}

/// Convert a DER-encoded ECDSA signature (as made by OpenSSL/.NET) to the raw 64-byte
/// r||s form Windows expects.
pub fn ecdsa_der_to_raw(der: &[u8]) -> Option<[u8; 64]> {
    fn int(b: &[u8]) -> Option<(&[u8], &[u8])> {
        let (&tag, rest) = b.split_first()?;
        let (&len, rest) = rest.split_first()?;
        if tag != 0x02 || len as usize > rest.len() || len > 33 {
            return None;
        }
        Some(rest.split_at(len as usize))
    }
    let (&tag, rest) = der.split_first()?;
    let (&len, body) = rest.split_first()?;
    if tag != 0x30 || len as usize != body.len() {
        return None;
    }
    let (r, rest) = int(body)?;
    let (s, rest) = int(rest)?;
    if !rest.is_empty() {
        return None;
    }
    let mut out = [0u8; 64];
    for (i, v) in [r, s].iter().enumerate() {
        let v = &v[v.iter().position(|&x| x != 0).unwrap_or(v.len())..]; // strip sign padding
        if v.len() > 32 {
            return None;
        }
        out[i * 32 + 32 - v.len()..(i + 1) * 32].copy_from_slice(v);
    }
    Some(out)
}

/// Verify an ECDSA P-256 / SHA-256 signature (DER) over `digest` with a public key
/// given as raw X||Y (64 bytes). Uses Windows CNG.
pub fn verify_p256(public_xy: &[u8; 64], digest: &[u8; 32], der_sig: &[u8]) -> bool {
    let Some(sig) = ecdsa_der_to_raw(der_sig) else { return false };
    let mut blob = Vec::with_capacity(8 + 64);
    let header = BCRYPT_ECCKEY_BLOB { dwMagic: BCRYPT_ECDSA_PUBLIC_P256_MAGIC, cbKey: 32 };
    blob.extend_from_slice(&header.dwMagic.to_le_bytes());
    blob.extend_from_slice(&header.cbKey.to_le_bytes());
    blob.extend_from_slice(public_xy);
    unsafe {
        let mut key = null_mut();
        if BCryptImportKeyPair(BCRYPT_ECDSA_P256_ALG_HANDLE, null_mut(), BCRYPT_ECCPUBLIC_BLOB, &mut key, blob.as_ptr(), blob.len() as u32, 0) != 0 {
            return false;
        }
        let ok = BCryptVerifySignature(key, null(), digest.as_ptr(), 32, sig.as_ptr(), 64, 0) == 0;
        BCryptDestroyKey(key);
        ok
    }
}

/// SHA-256 of a file as raw bytes.
pub fn sha256_file_raw(path: &Path) -> Result<[u8; 32], String> {
    use std::io::Read;
    let mut f = File::open(path).map_err(|e| e.to_string())?;
    unsafe {
        let mut h = null_mut();
        if BCryptCreateHash(BCRYPT_SHA256_ALG_HANDLE, &mut h, null_mut(), 0, null(), 0, 0) != 0 {
            return Err("hash init failed".into());
        }
        let mut buf = vec![0u8; 1 << 20];
        let res = loop {
            let n = match f.read(&mut buf) {
                Ok(n) => n,
                Err(e) => break Err(e.to_string()),
            };
            if n == 0 {
                let mut out = [0u8; 32];
                break if BCryptFinishHash(h, out.as_mut_ptr(), 32, 0) == 0 {
                    Ok(out)
                } else {
                    Err("hash failed".into())
                };
            }
            if BCryptHashData(h, buf.as_ptr(), n as u32, 0) != 0 {
                break Err("hash failed".into());
            }
        };
        BCryptDestroyHash(h);
        res
    }
}
