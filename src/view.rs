//! What the UI sees and what it can ask for. Plain data, no logic: the engine
//! owns all state and publishes immutable [`Snapshot`]s.

use std::path::PathBuf;

use crate::config::{Config, PeerPerms};
use crate::geometry::LayoutDoc;
use crate::identity::{Fingerprint, StoreKind};
use crate::platform::BackendCaps;
use crate::proto::{Capabilities, DisplayInfo, Platform};

pub type ConfigEdit = Box<dyn FnOnce(&mut Config) + Send>;

pub enum Command {
    // sharing
    /// `true` pauses everything (the tray's "Pause all sharing").
    SetPaused(bool),
    SetClipboardPaused(bool),
    /// "Return control locally".
    ReturnLocal,
    // pairing
    OpenPairing,
    ClosePairing,
    PairWith(std::net::SocketAddr),
    /// `ip`, `ip:port` or `[v6]:port`.
    PairManual(String),
    ApprovePairing {
        label: String,
        perms: PeerPerms,
    },
    RejectPairing,
    // devices
    Connect(Fingerprint),
    Disconnect(Fingerprint),
    Revoke(Fingerprint),
    RenamePeer(Fingerprint, String),
    SetPerms(Fingerprint, PeerPerms),
    ApplyLayout(LayoutDoc),
    // files
    SendFiles {
        peer: Fingerprint,
        paths: Vec<PathBuf>,
    },
    AcceptTransfer(u64),
    DeclineTransfer(u64),
    CancelTransfer(u64),
    RetryTransfer(u64),
    ClearTransferHistory,
    // settings
    UpdateConfig(ConfigEdit),
    RecheckPermissions,
    RequestPermissions,
    OpenPermissionSettings,
    DismissNotice(u64),
    /// Forget every device and delete identity and settings, then stop.
    DeleteAllLocalData,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateLabel {
    Disconnected,
    Local,
    Transitioning,
    Controlling,
    BeingControlled,
    Paused,
    Panic,
    Locked,
    NeedsPermission,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerStatus {
    Offline,
    Connecting,
    Connected,
    RemotePaused,
    RemoteLocked,
    Controlling,
    BeingControlled,
    /// The other computer answered but does not trust this one.
    NotTrustedByPeer,
    Blocked,
}

#[derive(Debug, Clone)]
pub struct PeerView {
    pub fingerprint: Fingerprint,
    pub label: String,
    pub announced_name: String,
    pub platform: Platform,
    pub status: PeerStatus,
    pub endpoint: Option<String>,
    pub last_connected: Option<u64>,
    pub perms: PeerPerms,
    /// Negotiated with the live session, if any.
    pub caps: Option<Capabilities>,
    pub error: Option<String>,
    pub displays: Vec<DisplayInfo>,
    pub rtt_ms: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct CandidateView {
    pub key: String,
    pub name: String,
    pub platform: Platform,
    pub addr: std::net::SocketAddr,
    pub fp_hint: String,
    /// The hint matches a device that is already approved.
    pub already_paired: bool,
    /// Same name as an approved device but a *different* identity hint.
    pub resembles: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairStage {
    Connecting,
    /// Both fingerprints are on screen; waiting for this user's decision.
    Verify,
    /// This user approved; waiting for the other computer's decision.
    WaitingForPeer,
    Done,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairError {
    Expired,
    RejectedByPeer,
    RejectedHere,
    /// Nothing answered.
    Unreachable,
    /// The other computer is not accepting pairing requests right now.
    NotInvited,
    VersionMismatch,
    Protocol,
    Busy,
    AlreadyPaired,
}

#[derive(Debug, Clone)]
pub struct PairingView {
    pub id: u64,
    pub initiator: bool,
    pub stage: PairStage,
    pub peer_name: Option<String>,
    pub peer_platform: Option<Platform>,
    pub peer_fingerprint: Option<Fingerprint>,
    pub local_fingerprint: Fingerprint,
    pub expires_in_secs: u32,
    pub error: Option<PairError>,
    /// Same name as a device that is already approved.
    pub resembles: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Send,
    Receive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferStatus {
    WaitingForAcceptance,
    Preparing,
    Transferring,
    Verifying,
    Complete,
    Cancelled,
    Declined,
    Failed,
}

impl TransferStatus {
    pub fn is_finished(self) -> bool {
        matches!(self, Self::Complete | Self::Cancelled | Self::Declined | Self::Failed)
    }
}

#[derive(Debug, Clone)]
pub struct TransferView {
    pub id: u64,
    pub direction: Direction,
    pub peer: Fingerprint,
    pub peer_label: String,
    pub status: TransferStatus,
    pub files: Vec<(String, u64)>,
    pub total: u64,
    pub done: u64,
    pub rate_bps: f64,
    pub eta_secs: Option<u32>,
    pub error: Option<String>,
    /// Receive folder name only; the full path is never shown or stored.
    pub folder: Option<String>,
    pub can_retry: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeAction {
    OpenPermissionSettings,
    OpenPairing,
    OpenDevices,
    OpenSettings,
    Retry,
}

/// Every failure explains: what happened, what stayed safe, what to do.
#[derive(Debug, Clone)]
pub struct NoticeView {
    pub id: u64,
    pub level: Level,
    pub what: String,
    pub safe: String,
    pub action: Option<(NoticeAction, String)>,
    pub at: u64,
}

#[derive(Debug, Clone)]
pub struct DeviceView {
    pub name: String,
    pub fingerprint: Fingerprint,
    pub platform: Platform,
    pub listen_port: u16,
    pub store: StoreKind,
    pub config_dir_label: String,
}

#[derive(Debug, Clone)]
pub struct SharingView {
    pub state: StateLabel,
    /// Everything paused (by the user, panic, lock or missing permission).
    pub paused: bool,
    pub clipboard_paused: bool,
    pub stay_local: bool,
    pub active_peer: Option<Fingerprint>,
    pub panic_hotkey: String,
    pub switch_hotkey: String,
    /// True when the OS delivers the panic chord to us right now.
    pub panic_hotkey_live: bool,
    pub summary: String,
}

#[derive(Debug, Clone)]
pub struct BackendView {
    pub api: String,
    pub capture: String,
    pub capture_ok: bool,
    pub inject: String,
    pub inject_ok: bool,
    pub notes: Vec<String>,
    pub clipboard_ok: bool,
}

impl BackendView {
    pub fn from_caps(c: &BackendCaps, clipboard_ok: bool) -> Self {
        Self {
            api: c.api.to_string(),
            capture: c.capture.text(),
            capture_ok: c.capture.is_available(),
            inject: c.inject.text(),
            inject_ok: c.inject.is_available(),
            notes: c.notes.clone(),
            clipboard_ok,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DeviceDisplays {
    pub fingerprint: Fingerprint,
    pub label: String,
    pub is_local: bool,
    pub online: bool,
    pub displays: Vec<DisplayInfo>,
}

#[derive(Debug, Clone)]
pub struct LayoutView {
    pub doc: LayoutDoc,
    pub devices: Vec<DeviceDisplays>,
}

#[derive(Debug, Clone)]
pub struct NetworkView {
    pub discovery_on: bool,
    pub discovery_error: Option<String>,
    pub has_lan: bool,
    pub interfaces: Vec<(String, bool)>,
    /// `ip:port` of this computer on the local network, to type on the other computer when pairing from there.
    pub addresses: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub rev: u64,
    pub device: DeviceView,
    pub sharing: SharingView,
    pub backend: BackendView,
    pub peers: Vec<PeerView>,
    pub candidates: Vec<CandidateView>,
    pub pairing: Option<PairingView>,
    /// Seconds left in the open pairing window, if any.
    pub pairing_window_secs: Option<u32>,
    pub transfers: Vec<TransferView>,
    pub layout: LayoutView,
    pub notices: Vec<NoticeView>,
    pub network: NetworkView,
    pub config: Config,
    /// Set after `DeleteAllLocalData`; the UI should quit.
    pub shut_down: bool,
}
