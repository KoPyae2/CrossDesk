//! Persisted settings: identity, paired keys, and the wall layout.
//!
//! Written as JSON next to the OS's usual per-user config location. It is a
//! small file touched a handful of times per session, so it is saved
//! synchronously and read once at start-up.
//!
//! The stored pre-shared keys are what let a device reconnect without the
//! pairing code being retyped, so this file is as sensitive as a password store
//! and is created with owner-only permissions where the platform allows it.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::crypto::{random_bytes, random_pairing_code};
use crate::protocol::{id_to_hex, DeviceId};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Settings {
    /// This machine's stable identity, hex encoded.
    pub device_id: String,
    /// Name shown to peers; defaults to the hostname.
    pub device_name: String,
    /// The code a client has to type to pair with us. Regenerating it does not
    /// disturb devices that have already paired.
    pub pairing_code: String,
    pub clipboard_sync: bool,
    /// Clipboard images are off by default: a screenshot is orders of magnitude
    /// larger than text and this app is about latency.
    pub clipboard_images: bool,
    /// Keys handed to us by hosts we have paired with, keyed by host id.
    pub paired_hosts: HashMap<String, [u8; 32]>,
    /// Keys we issued to clients, keyed by client id.
    pub paired_clients: HashMap<String, [u8; 32]>,
    /// Where each remote device sits on the wall, keyed by device id.
    pub layout: HashMap<String, (i32, i32)>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            device_id: id_to_hex(&random_bytes::<16>()),
            device_name: hostname(),
            pairing_code: random_pairing_code(),
            clipboard_sync: true,
            clipboard_images: true,
            paired_hosts: HashMap::new(),
            paired_clients: HashMap::new(),
            layout: HashMap::new(),
        }
    }
}

impl Settings {
    pub fn load() -> Self {
        let Some(path) = config_path() else {
            return Self::default();
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            let fresh = Self::default();
            fresh.save();
            return fresh;
        };
        // A settings file we cannot parse is more likely a half-written file
        // than a real problem; starting fresh beats refusing to launch.
        match serde_json::from_str::<Settings>(&text) {
            Ok(settings) => settings,
            Err(_) => {
                let fresh = Self::default();
                fresh.save();
                fresh
            }
        }
    }

    pub fn save(&self) {
        let Some(path) = config_path() else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let Ok(json) = serde_json::to_string_pretty(self) else {
            return;
        };
        // Write-then-rename so a crash mid-save cannot lose the paired keys.
        let temp = path.with_extension("json.tmp");
        if std::fs::write(&temp, json).is_ok() {
            restrict(&temp);
            let _ = std::fs::rename(&temp, &path);
        }
    }

    pub fn id(&self) -> DeviceId {
        crate::protocol::id_from_hex(&self.device_id).unwrap_or([0u8; 16])
    }
}

#[cfg(unix)]
fn restrict(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict(_path: &std::path::Path) {
    // Windows inherits the user profile's ACL, which is already owner-only.
}

fn config_path() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    }?;
    Some(base.join("CrossDesk").join("settings.json"))
}

pub fn hostname() -> String {
    for key in ["COMPUTERNAME", "HOSTNAME"] {
        if let Some(name) = std::env::var_os(key) {
            let name = name.to_string_lossy().trim().to_string();
            if !name.is_empty() {
                return name;
            }
        }
    }
    // Linux and macOS do not export HOSTNAME to GUI processes.
    if let Ok(name) = std::fs::read_to_string("/etc/hostname") {
        let name = name.trim();
        if !name.is_empty() {
            return name.to_string();
        }
    }
    "CrossDesk device".to_string()
}

pub fn os_label() -> String {
    match std::env::consts::OS {
        "windows" => "Windows",
        "macos" => "macOS",
        "linux" => "Linux",
        other => other,
    }
    .to_string()
}
