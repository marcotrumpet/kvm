//! Settings and where things live: %APPDATA%\KVM on Windows, $XDG_CONFIG_HOME/kvm elsewhere.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use kvm_platform::Hotkey;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// The paired computer.
    pub peer_id: Option<String>,
    pub peer_name: Option<String>,
    /// Paired remote controls (iPhone, iPad): id -> name.
    pub remotes: BTreeMap<String, String>,
    pub share_clipboard: bool,
    /// The Mac's Command keys act as Ctrl here and Ctrl as Command there, so shortcuts
    /// (copy, paste, ...) work under the same fingers on both.
    pub swap_ctrl_cmd: bool,
    /// Locking either computer locks the other (like the Mac's setting).
    pub lock_together: bool,
    pub hotkey: HotkeyConfig,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            peer_id: None,
            peer_name: None,
            remotes: BTreeMap::new(),
            share_clipboard: true,
            swap_ctrl_cmd: true,
            lock_together: true,
            hotkey: HotkeyConfig::from(Hotkey::DEFAULT),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct HotkeyConfig {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub meta: bool,
    /// A USB HID keyboard usage.
    pub key: u16,
}

impl From<Hotkey> for HotkeyConfig {
    fn from(h: Hotkey) -> Self {
        HotkeyConfig { ctrl: h.ctrl, alt: h.alt, shift: h.shift, meta: h.meta, key: h.hid }
    }
}

impl From<HotkeyConfig> for Hotkey {
    fn from(h: HotkeyConfig) -> Self {
        Hotkey { ctrl: h.ctrl, alt: h.alt, shift: h.shift, meta: h.meta, hid: h.key }
    }
}

impl Config {
    pub fn load(dir: &Path) -> Config {
        std::fs::read(dir.join("config.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, dir: &Path) {
        let result = std::fs::create_dir_all(dir)
            .and_then(|_| std::fs::write(dir.join("config.json"), serde_json::to_vec_pretty(self).unwrap_or_default()));
        if let Err(err) = result {
            tracing::warn!("saving settings: {err}");
        }
    }
}

/// Settings and this machine's identity.
pub fn config_dir() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(std::env::var_os("APPDATA").unwrap_or_default()).join("KVM")
    } else if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        PathBuf::from(xdg).join("kvm")
    } else {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config/kvm")
    }
}

/// The log file's folder.
pub fn log_dir() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_default()).join("KVM")
    } else {
        config_dir()
    }
}

/// Where files from the peer go: the user's Downloads folder.
pub fn downloads_dir() -> PathBuf {
    if cfg!(windows) {
        return PathBuf::from(std::env::var_os("USERPROFILE").unwrap_or_default()).join("Downloads");
    }
    let xdg = std::process::Command::new("xdg-user-dir").arg("DOWNLOAD").output().ok();
    match xdg.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()) {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join("Downloads"),
    }
}

/// How this machine introduces itself to the peer.
pub fn computer_name() -> String {
    let name = if cfg!(windows) {
        std::env::var("COMPUTERNAME").ok()
    } else {
        std::fs::read_to_string("/etc/hostname").ok().map(|s| s.trim().to_string())
    };
    name.filter(|n| !n.is_empty()).unwrap_or_else(|| "PC".into())
}
