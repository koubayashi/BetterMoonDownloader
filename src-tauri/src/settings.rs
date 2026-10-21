//! User settings: persisted as JSON, and clamped at one boundary so nothing the
//! page sends can reach a semaphore unchecked.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    /// Destination folder for finished files.
    pub out_dir: String,
    /// Files downloading at the same time.
    pub max_active: usize,
    /// Connections per file (segmented download).
    pub connections: usize,
    /// Links resolving at the same time.
    pub max_resolving: usize,
    /// Extraction retries per link.
    pub retries: u32,
    /// Browser tabs open at once on the shared window (datanodes / fuckingfast fallback).
    pub browser_pages: usize,
    /// Seconds to wait for a manual captcha tick before giving up on a link.
    pub captcha_wait: u64,
    /// Explicit chrome.exe / msedge.exe; empty = autodetect.
    pub browser_path: String,
    /// datanodes premium API key; empty = browser flow.
    pub dn_api_key: String,
    /// One proxy per line: `http://h:p`, `socks5://h:p`, `ip:port`, `ip:port:user:pass`.
    pub proxies: String,
    /// `download` or `links` (resolve direct links only).
    pub mode: String,
    /// Re-extract a file stuck on a slow CDN lane.
    pub stall_killer: bool,
    /// Global speed limit in MB/s; 0 = unlimited.
    pub speed_limit_mbs: f64,
    /// `system`, `light` or `dark`.
    pub theme: String,
    /// Start the queue as soon as links are added.
    pub auto_start: bool,
    /// Put each release (ExampleGame, ...) in its own subfolder of `out_dir`.
    pub release_folders: bool,
    /// Resolve links in the app's own WebView2 window instead of an external Chrome/Edge.
    pub builtin_browser: bool,
}

impl Default for Settings {
    fn default() -> Self {
        let out = dirs::download_dir()
            .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join("Downloads"))
            .join("BetterMoon");
        Self {
            out_dir: out.to_string_lossy().into_owned(),
            max_active: 4,
            connections: 4,
            max_resolving: 3,
            retries: 3,
            browser_pages: 3,
            captcha_wait: 240,
            browser_path: String::new(),
            dn_api_key: String::new(),
            proxies: String::new(),
            mode: "download".into(),
            stall_killer: true,
            speed_limit_mbs: 0.0,
            theme: "system".into(),
            auto_start: true,
            release_folders: true,
            builtin_browser: true,
        }
    }
}

impl Settings {
    pub fn clamped(mut self) -> Self {
        self.max_active = self.max_active.clamp(1, 32);
        self.connections = self.connections.clamp(1, 16);
        self.max_resolving = self.max_resolving.clamp(1, 16);
        self.retries = self.retries.min(10);
        self.browser_pages = self.browser_pages.clamp(1, 8);
        self.captcha_wait = self.captcha_wait.clamp(0, 1800);
        if !self.speed_limit_mbs.is_finite() || self.speed_limit_mbs < 0.0 {
            self.speed_limit_mbs = 0.0;
        }
        if self.mode != "links" {
            self.mode = "download".into();
        }
        if !matches!(self.theme.as_str(), "light" | "dark") {
            self.theme = "system".into();
        }
        self.out_dir = self.out_dir.trim().to_string();
        if self.out_dir.is_empty() {
            self.out_dir = Settings::default().out_dir;
        }
        self.browser_path = self.browser_path.trim().to_string();
        self.dn_api_key = self.dn_api_key.trim().to_string();
        self
    }
}

/// `%APPDATA%\BetterMoonDownloader`
pub fn data_dir() -> PathBuf {
    let d = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("BetterMoonDownloader");
    let _ = std::fs::create_dir_all(&d);
    d
}

/// `%LOCALAPPDATA%\BetterMoonDownloader\browser-profile`: never the user's live
/// profile; Chrome refuses a debugging port on a profile another instance holds.
pub fn browser_profile_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(data_dir)
        .join("BetterMoonDownloader")
        .join("browser-profile")
}

pub fn load() -> Settings {
    let mut s = std::fs::read(data_dir().join("settings.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Settings>(&b).ok())
        .unwrap_or_default();
    s.dn_api_key = secret::open(&s.dn_api_key);
    s.proxies = secret::open(&s.proxies);
    s.clamped()
}

/// Secrets (API key, proxy passwords) are sealed with Windows DPAPI: only
/// this Windows account can read them back, not other programs scanning
/// `%APPDATA%` files or a copy of the file on another machine. Elsewhere the
/// file is owner-only (0600), the usual protection for app tokens on Linux.
pub fn save(s: &Settings) {
    let mut on_disk = s.clone();
    on_disk.dn_api_key = secret::seal(&s.dn_api_key);
    on_disk.proxies = secret::seal(&s.proxies);
    if let Ok(b) = serde_json::to_vec_pretty(&on_disk) {
        let path = data_dir().join("settings.json");
        let tmp = path.with_extension("json.tmp");
        if write_private(&tmp, &b).is_ok() {
            let _ = std::fs::rename(tmp, path);
        }
    }
}

#[cfg(unix)]
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
    // `mode` only applies to new files; a leftover tmp keeps its old bits.
    f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    f.write_all(bytes)
}

#[cfg(not(unix))]
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
}

mod secret {
    const PREFIX: &str = "dpapi:";

    pub fn seal(plain: &str) -> String {
        if plain.is_empty() {
            return String::new();
        }
        match protect(plain.as_bytes(), true) {
            Some(blob) => format!("{PREFIX}{}", blob.iter().map(|b| format!("{b:02x}")).collect::<String>()),
            None => plain.to_string(),
        }
    }

    /// Values saved by older versions are plain text and pass straight through.
    pub fn open(stored: &str) -> String {
        let Some(hex) = stored.strip_prefix(PREFIX) else { return stored.to_string() };
        let bytes: Option<Vec<u8>> = (0..hex.len())
            .step_by(2)
            .map(|i| hex.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok()))
            .collect();
        bytes
            .and_then(|b| protect(&b, false))
            .and_then(|p| String::from_utf8(p).ok())
            .unwrap_or_default()
    }

    #[cfg(windows)]
    fn protect(data: &[u8], seal: bool) -> Option<Vec<u8>> {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Cryptography::{
            CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
        };
        let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
        // SAFETY: input points at `data` for the call; DPAPI allocates `out`,
        // which is copied and then released with LocalFree.
        unsafe {
            let ok = if seal {
                CryptProtectData(&input, std::ptr::null(), std::ptr::null(), std::ptr::null(), std::ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out)
            } else {
                CryptUnprotectData(&input, std::ptr::null_mut(), std::ptr::null(), std::ptr::null(), std::ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out)
            };
            if ok == 0 || out.pbData.is_null() {
                return None;
            }
            let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
            LocalFree(out.pbData as _);
            Some(v)
        }
    }

    #[cfg(not(windows))]
    fn protect(_data: &[u8], _seal: bool) -> Option<Vec<u8>> {
        None
    }

    #[cfg(all(test, windows))]
    mod tests {
        #[test]
        fn round_trip_and_legacy() {
            let sealed = super::seal("key-123:proxy-pass");
            assert!(sealed.starts_with("dpapi:"), "{sealed}");
            assert!(!sealed.contains("key-123"));
            assert_eq!(super::open(&sealed), "key-123:proxy-pass");
            assert_eq!(super::open("plain-old-value"), "plain-old-value");
            assert_eq!(super::seal(""), "");
        }
    }
}
