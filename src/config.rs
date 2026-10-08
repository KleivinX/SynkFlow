//! Settings and the trust store, persisted as one small TOML file.
//!
//! No database: this is a handful of settings and a short list of approved
//! devices. The file is written atomically with owner-only permissions.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::geometry::LayoutDoc;
use crate::identity::{Fingerprint, SecretStore, write_private_file};
use crate::limits::DEFAULT_PORT;
use crate::proto::Platform;

/// Provisional reverse-DNS application ID. `.example` is reserved by RFC 2606
/// and can never be registered, so this cannot collide with a real domain.
/// Replace with a real identifier once the project owns one.
pub const APP_ID: &str = "example.synkflow.Synkflow";

pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub dir: PathBuf,
}

impl AppPaths {
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// OS-conventional config directory, or `SYNKFLOW_CONFIG_DIR` (used to run
    /// several instances on one machine for testing).
    pub fn discover() -> Self {
        if let Some(d) = std::env::var_os("SYNKFLOW_CONFIG_DIR") {
            return Self::at(d);
        }
        let dir = directories::ProjectDirs::from("example", "synkflow", "Synkflow")
            .map(|p| p.config_dir().to_path_buf())
            .unwrap_or_else(|| PathBuf::from(".synkflow"));
        Self { dir }
    }

    pub fn config_file(&self) -> PathBuf {
        self.dir.join("config.toml")
    }
    pub fn identity_file(&self) -> PathBuf {
        self.dir.join("identity.bin")
    }

    /// Preferred and fallback identity stores. `SYNKFLOW_SECRET_STORE=file`
    /// forces the file store (headless boxes, tests, two instances per user).
    pub fn secret_stores(&self) -> (SecretStore, SecretStore) {
        let file = SecretStore::File { path: self.identity_file() };
        if std::env::var("SYNKFLOW_SECRET_STORE").as_deref() == Ok("file") {
            return (file.clone(), file);
        }
        let account = format!("device-identity:{}", self.dir.display());
        (SecretStore::Keyring { service: APP_ID.to_string(), account }, file)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    System,
    Dark,
    Light,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Tri {
    /// Follow the operating system.
    #[default]
    System,
    On,
    Off,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum CrossModifier {
    #[default]
    Shift,
    Ctrl,
    Alt,
    Meta,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum AcceptMode {
    /// Ask for every offer.
    #[default]
    Ask,
    /// Decline everything.
    Never,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(default)]
pub struct General {
    pub start_paused: bool,
    pub close_to_tray: bool,
    pub start_at_login: bool,
}

impl Default for General {
    fn default() -> Self {
        // Pairing is the consent. After it, Synkflow just works on every launch; "Start paused" is there for those who want it.
        Self { start_paused: false, close_to_tray: true, start_at_login: false }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(default)]
pub struct InputSettings {
    pub edge_dwell_ms: u32,
    pub edge_zone: u32,
    pub block_corners: bool,
    pub require_modifier: bool,
    pub cross_modifier: CrossModifier,
    pub pointer_sensitivity: f32,
    pub shortcut_translation: bool,
    pub panic_hotkey: String,
    pub switch_hotkey: String,
    /// After the screen unlocks, resume sharing automatically. Off by default.
    pub resume_after_lock: bool,
    /// "Stay on this computer": the pointer never crosses.
    pub stay_local: bool,
}

impl Default for InputSettings {
    fn default() -> Self {
        Self {
            edge_dwell_ms: 0,
            edge_zone: 2,
            block_corners: true,
            require_modifier: false,
            cross_modifier: CrossModifier::Shift,
            pointer_sensitivity: 1.0,
            // Only acts between a Mac and a PC (⌘C there is Ctrl+C here); between two Macs or two PCs it does nothing.
            shortcut_translation: true,
            panic_hotkey: "Ctrl+Alt+Shift+Escape".into(),
            switch_hotkey: "Ctrl+Alt+Shift+S".into(),
            resume_after_lock: false,
            stay_local: false,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(default)]
pub struct FileSettings {
    /// Folder for received files. `None` ⇒ `<Downloads>/Synkflow`.
    pub inbox_dir: Option<PathBuf>,
    pub accept_mode: AcceptMode,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(default)]
pub struct NetworkSettings {
    pub discovery: bool,
    /// Interface names to use; empty ⇒ all non-tunnel interfaces.
    pub interfaces: Vec<String>,
    pub include_tunnels: bool,
    /// 0 ⇒ pick a free port.
    pub port: u16,
}

impl Default for NetworkSettings {
    fn default() -> Self {
        Self { discovery: true, interfaces: vec![], include_tunnels: false, port: DEFAULT_PORT }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(default)]
pub struct Appearance {
    pub theme: Theme,
    pub reduced_motion: Tri,
    pub reduced_transparency: Tri,
    /// Text scale in percent: 100, 115 or 130.
    pub text_scale: u16,
}

impl Default for Appearance {
    fn default() -> Self {
        Self { theme: Theme::System, reduced_motion: Tri::System, reduced_transparency: Tri::System, text_scale: 100 }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(default)]
pub struct Diagnostics {
    /// error | warn | info | debug
    pub log_level: String,
}

impl Default for Diagnostics {
    fn default() -> Self {
        // "info" records connections and discoveries, which is what is needed when two computers will not meet.
        Self { log_level: "info".into() }
    }
}

/// What this computer lets one specific approved device do.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(default)]
#[derive(Default)]
pub struct PeerPerms {
    /// This computer may take over that device's keyboard and mouse.
    pub share_input: bool,
    /// That device may control this computer's keyboard and mouse.
    pub accept_input: bool,
    pub clipboard_send: bool,
    pub clipboard_receive: bool,
    /// That device may offer files to this computer (each still needs
    /// acceptance unless `files_auto_accept`).
    pub files_receive: bool,
    pub files_auto_accept: bool,
    /// `None` ⇒ automatic (on when platform families differ and the global
    /// setting is on).
    pub translate_shortcuts: Option<bool>,
    pub invert_scroll: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct TrustedPeer {
    pub fingerprint: Fingerprint,
    /// Local label chosen by this user. Not authenticated.
    pub label: String,
    /// Name the device announced when paired. Not authenticated.
    pub announced_name: String,
    pub platform: Platform,
    pub paired_at: u64,
    #[serde(default)]
    pub last_connected: Option<u64>,
    /// Last address that worked, as `ip:port`.
    #[serde(default)]
    pub last_endpoint: Option<String>,
    #[serde(default)]
    pub perms: PeerPerms,
    /// Displays the device reported last time, so the layout can be edited
    /// while it is offline.
    #[serde(default)]
    pub displays: Vec<crate::proto::DisplayInfo>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(default)]
pub struct Config {
    pub version: u32,
    pub device_name: String,
    pub onboarding_done: bool,
    pub general: General,
    pub input: InputSettings,
    pub files: FileSettings,
    pub network: NetworkSettings,
    pub appearance: Appearance,
    pub diagnostics: Diagnostics,
    pub clipboard_paused: bool,
    pub layout: LayoutDoc,
    pub peers: Vec<TrustedPeer>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            device_name: default_device_name(),
            onboarding_done: false,
            general: General::default(),
            input: InputSettings::default(),
            files: FileSettings::default(),
            network: NetworkSettings::default(),
            appearance: Appearance::default(),
            diagnostics: Diagnostics::default(),
            clipboard_paused: false,
            layout: LayoutDoc::default(),
            peers: vec![],
        }
    }
}

pub fn default_device_name() -> String {
    for var in ["SYNKFLOW_DEVICE_NAME", "COMPUTERNAME", "HOSTNAME"] {
        if let Ok(v) = std::env::var(var) {
            let v = crate::proto::clean_text(&v, crate::limits::MAX_NAME_BYTES);
            if !v.is_empty() {
                return v;
            }
        }
    }
    if let Ok(out) = std::process::Command::new("hostname").output() {
        let v = crate::proto::clean_text(String::from_utf8_lossy(&out.stdout).trim(), crate::limits::MAX_NAME_BYTES);
        if !v.is_empty() {
            return v.trim_end_matches(".local").to_string();
        }
    }
    "This computer".into()
}

#[derive(Debug, PartialEq, Eq)]
pub enum LoadStatus {
    Loaded,
    /// No file yet.
    Fresh,
    /// File was unreadable; it was kept under this name and defaults are in use.
    Recovered(PathBuf),
}

/// Settings files written before this version carry the old start-up defaults (paused on every launch, no ⌘/Ctrl
/// translation), which made a freshly paired Mac and Windows laptop look broken. They are moved to the new defaults once.
const CONFIG_VERSION: u32 = 2;

impl Config {
    fn migrate(&mut self) {
        if self.version < 2 {
            self.general.start_paused = false;
            self.input.shortcut_translation = true;
            if self.diagnostics.log_level == "warn" {
                self.diagnostics.log_level = "info".into();
            }
        }
        self.version = CONFIG_VERSION;
    }

    pub fn load(path: &Path) -> (Config, LoadStatus) {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Config::default(), LoadStatus::Fresh),
            Err(_) => return (Config::default(), quarantine(path)),
        };
        match toml::from_str::<Config>(&text) {
            Ok(mut c) => {
                c.migrate();
                c.sanitize();
                (c, LoadStatus::Loaded)
            }
            Err(_) => (Config::default(), quarantine(path)),
        }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let text = toml::to_string_pretty(self).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        write_private_file(path, text.as_bytes())
    }

    /// Clamp values a hand-edited file could set to something unusable.
    pub fn sanitize(&mut self) {
        self.device_name = crate::proto::clean_text(&self.device_name, crate::limits::MAX_NAME_BYTES);
        if self.device_name.is_empty() {
            self.device_name = default_device_name();
        }
        self.input.pointer_sensitivity = self.input.pointer_sensitivity.clamp(0.25, 4.0);
        self.input.edge_dwell_ms = self.input.edge_dwell_ms.min(2000);
        self.input.edge_zone = self.input.edge_zone.clamp(1, 16);
        self.appearance.text_scale = self.appearance.text_scale.clamp(100, 130);
        for p in &mut self.peers {
            p.label = crate::proto::clean_text(&p.label, crate::limits::MAX_NAME_BYTES);
            p.announced_name = crate::proto::clean_text(&p.announced_name, crate::limits::MAX_NAME_BYTES);
        }
        // Duplicate fingerprints would make revocation ambiguous.
        let mut seen = std::collections::HashSet::new();
        self.peers.retain(|p| seen.insert(p.fingerprint));
        if self.layout.validate().is_err() {
            self.layout = LayoutDoc::default();
        }
    }

    pub fn peer(&self, fp: &Fingerprint) -> Option<&TrustedPeer> {
        self.peers.iter().find(|p| &p.fingerprint == fp)
    }
    pub fn peer_mut(&mut self, fp: &Fingerprint) -> Option<&mut TrustedPeer> {
        self.peers.iter_mut().find(|p| &p.fingerprint == fp)
    }

    pub fn inbox_dir(&self) -> PathBuf {
        self.files.inbox_dir.clone().unwrap_or_else(|| {
            directories::UserDirs::new().and_then(|d| d.download_dir().map(|p| p.join("Synkflow"))).unwrap_or_else(|| PathBuf::from("Synkflow"))
        })
    }
}

fn quarantine(path: &Path) -> LoadStatus {
    let bad = path.with_extension(format!("toml.unreadable-{}", now_secs()));
    match std::fs::rename(path, &bad) {
        Ok(()) => LoadStatus::Recovered(bad),
        Err(_) => LoadStatus::Recovered(path.to_path_buf()),
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    fn peer(n: u8) -> TrustedPeer {
        TrustedPeer {
            fingerprint: Fingerprint([n; 32]),
            label: format!("Peer {n}"),
            announced_name: "Studio PC".into(),
            platform: Platform::Windows,
            paired_at: 1,
            last_connected: None,
            last_endpoint: Some("192.168.1.5:24847".into()),
            perms: PeerPerms { share_input: true, ..PeerPerms::default() },
            displays: vec![],
        }
    }

    #[test]
    fn defaults_are_safe_but_seamless() {
        let c = Config::default();
        assert!(!c.general.start_paused, "pairing is the consent; launching must not silently stop sharing");
        assert!(!c.general.start_at_login);
        assert!(c.input.shortcut_translation);
        assert!(!c.input.resume_after_lock);
        assert_eq!(c.files.accept_mode, AcceptMode::Ask);
        assert_eq!(
            PeerPerms::default(),
            PeerPerms {
                share_input: false,
                accept_input: false,
                clipboard_send: false,
                clipboard_receive: false,
                files_receive: false,
                files_auto_accept: false,
                translate_shortcuts: None,
                invert_scroll: false
            }
        );
    }

    #[test]
    fn config_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut c = Config::default();
        c.device_name = "Desk".into();
        c.peers.push(peer(1));
        c.layout.set_placement(Fingerprint([1; 32]), 100, 0);
        c.save(&path).unwrap();
        let (back, status) = Config::load(&path);
        assert_eq!(status, LoadStatus::Loaded);
        assert_eq!(back, c);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o077, 0);
        }
    }

    #[test]
    fn missing_file_is_fresh_and_corrupt_file_is_kept_not_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert_eq!(Config::load(&path).1, LoadStatus::Fresh);
        std::fs::write(&path, "this is [not valid toml").unwrap();
        let (c, status) = Config::load(&path);
        assert!(c.peers.is_empty());
        let LoadStatus::Recovered(kept) = status else { panic!("expected recovery") };
        assert!(kept.exists(), "the unreadable file must be preserved for the user");
        assert!(!path.exists());
    }

    #[test]
    fn hand_edited_values_are_clamped_and_duplicates_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut c = Config::default();
        c.input.pointer_sensitivity = 99.0;
        c.input.edge_dwell_ms = 999_999;
        c.device_name = "   ".into();
        c.peers.push(peer(1));
        c.peers.push(peer(1));
        c.save(&path).unwrap();
        let (b, _) = Config::load(&path);
        assert_eq!(b.input.pointer_sensitivity, 4.0);
        assert_eq!(b.input.edge_dwell_ms, 2000);
        assert!(!b.device_name.is_empty());
        assert_eq!(b.peers.len(), 1);
    }

    #[test]
    fn unknown_and_missing_fields_do_not_break_loading() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "device_name = \"X\"\nfuture_setting = 1\n[input]\nedge_dwell_ms = 100\n").unwrap();
        let (c, s) = Config::load(&path);
        assert_eq!(s, LoadStatus::Loaded);
        assert_eq!(c.device_name, "X");
        assert_eq!(c.input.edge_dwell_ms, 100);
        assert!(!c.general.start_paused);
    }

    #[test]
    fn old_start_up_defaults_are_migrated_once_and_later_choices_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "version = 1\n[general]\nstart_paused = true\n[input]\nshortcut_translation = false\n[diagnostics]\nlog_level = \"warn\"\n")
            .unwrap();
        let (c, _) = Config::load(&path);
        assert!(!c.general.start_paused && c.input.shortcut_translation && c.version == 2);
        assert_eq!(c.diagnostics.log_level, "info");
        // Saved as version 2, then deliberately set back: that choice now sticks.
        let mut mine = c.clone();
        mine.general.start_paused = true;
        mine.input.shortcut_translation = false;
        mine.save(&path).unwrap();
        let (again, _) = Config::load(&path);
        assert!(again.general.start_paused && !again.input.shortcut_translation);
    }
}
