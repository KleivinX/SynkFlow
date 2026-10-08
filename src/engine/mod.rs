//! The engine: one actor task that owns all mutable state.
//!
//! Network tasks, OS capture threads, discovery and the UI never touch that
//! state; they send [`CoreMsg`]s and the engine answers by publishing
//! immutable [`Snapshot`]s. Every queue in and out is bounded.

mod clip;
mod files;
mod pairing;

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::config::{AppPaths, Config, CrossModifier, LoadStatus, PeerPerms, now_secs};
use crate::control::{Action, Control, ControlConfig, Effect, Ev, Notice as CNotice, PeerCtl, State, Suspend};
use crate::discovery::{Announce, Candidate, Discovery, DiscoveryEvent, InterfacePolicy};
use crate::geometry::{Desk, EdgeDefaults};
use crate::identity::{Fingerprint, Identity, SecretStore, StoreKind, load_or_create};
use crate::keys::{Hotkey, Mods};
use crate::limits::*;
use crate::platform::{BackendCaps, Cap, Capture, CaptureEvent, CaptureSink, ClipboardBackend, InputBackend};
use crate::proto::{self, Capabilities, DisplayInfo, Grants, Hello, HelloAck, InputEvent, Msg, Platform, RejectReason, ReleaseReason, Wire};
use crate::session::{self, DialError, Dialed, EndReason, First, PairEvent, SessionChannels, SessionEvent, SessionId};
use crate::tls::{Accepted, ClientStream, ServerStream, TlsEndpoint, TrustSet};
use crate::view::*;

pub struct EngineDeps {
    pub paths: AppPaths,
    pub input: Arc<dyn InputBackend>,
    pub clipboard: Option<Box<dyn ClipboardBackend>>,
    pub stores: (SecretStore, SecretStore),
    pub bind: IpAddr,
    /// Advertise and browse over mDNS.
    pub discovery: bool,
    /// Tests only: accept loopback addresses as peers and as mDNS interfaces.
    pub allow_loopback: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("identity: {0}")]
    Identity(#[from] crate::identity::IdentityError),
    #[error("TLS: {0}")]
    Tls(#[from] crate::tls::TlsError),
    #[error("could not listen: {0}")]
    Listen(std::io::Error),
}

pub(crate) enum CoreMsg {
    Cmd(Command),
    Capture(CaptureEvent),
    Discovery(DiscoveryEvent),
    Session(SessionEvent),
    Pair(u64, PairEvent),
    IncomingControl { wire: Wire<ServerStream>, hello: Box<Hello>, peer: Fingerprint, addr: SocketAddr },
    IncomingBulk { wire: Wire<ServerStream>, session_id: [u8; 16], token: [u8; 16], transfer_id: u64, peer: Fingerprint },
    IncomingPairing { wire: Wire<ServerStream>, peer: Fingerprint, addr: SocketAddr },
    DialDone { peer: Fingerprint, addr: SocketAddr, result: Box<Result<Dialed, DialError>> },
    PairDialDone { id: u64, addr: SocketAddr, result: Box<Result<(Wire<ClientStream>, Fingerprint), DialError>> },
    BulkDialDone { transfer_id: u64, result: Box<Result<Wire<ClientStream>, DialError>> },
    TransferDone { id: u64, outcome: crate::transfer::Outcome },
    Clip(clip::ClipEvent),
    Shutdown(oneshot::Sender<()>),
}

#[derive(Clone)]
pub struct Engine {
    tx: mpsc::Sender<CoreMsg>,
    snap: watch::Receiver<Arc<Snapshot>>,
    task: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl Engine {
    pub async fn start(deps: EngineDeps) -> Result<Engine, EngineError> {
        let (cfg, status) = Config::load(&deps.paths.config_file());
        let (identity, store_kind) = load_or_create(&deps.stores.0, &deps.stores.1)?;
        let identity = Arc::new(identity);
        let trust = TrustSet::default();
        trust.replace(cfg.peers.iter().map(|p| p.fingerprint));
        let endpoint = Arc::new(TlsEndpoint::new(&identity, trust.clone())?);

        let mut notices = Vec::new();
        let listener = match TcpListener::bind((deps.bind, cfg.network.port)).await {
            Ok(l) => l,
            Err(e) if cfg.network.port != 0 => {
                tracing::warn!("port {} unavailable ({e}); choosing another", cfg.network.port);
                notices.push((
                    "Port unavailable".to_string(),
                    format!("The usual port ({}) is in use, so Synkflow picked another one. Nothing else changed.", cfg.network.port),
                ));
                TcpListener::bind((deps.bind, 0)).await.map_err(EngineError::Listen)?
            }
            Err(e) => return Err(EngineError::Listen(e)),
        };
        let port = listener.local_addr().map_err(EngineError::Listen)?.port();

        let (tx, rx) = mpsc::channel::<CoreMsg>(4096);
        let (snap_tx, snap_rx) = watch::channel(Arc::new(placeholder_snapshot(&cfg, &identity, port, store_kind)));

        let local_displays = deps.input.displays().unwrap_or_default();
        let injector = spawn_injector(deps.input.clone());
        let (clip_worker, clipboard_ok) = match deps.clipboard {
            Some(cb) => (Some(clip::spawn_worker(cb, tx.clone())), true),
            None => (None, false),
        };

        let mut core = Core {
            paths: deps.paths,
            input: deps.input,
            allow_loopback: deps.allow_loopback,
            identity: identity.clone(),
            trust,
            tx: tx.clone(),
            port,
            store_kind,
            control: Control::new(identity.fingerprint(), Platform::current(), ControlConfig::default(), Desk::default()),
            capture: None,
            capture_error: None,
            backend_caps: None,
            injector,
            inject_error: Arc::new(Mutex::new(None)),
            local_displays,
            sessions: HashMap::new(),
            next_sid: 1,
            peers: HashMap::new(),
            candidates: HashMap::new(),
            discovery: None,
            discovery_error: None,
            has_lan: true,
            notices: VecDeque::new(),
            next_notice: 1,
            snap_tx,
            rev: 0,
            dirty: true,
            pairing: pairing::PairingState::default(),
            xfers: files::Transfers::default(),
            clip: clip::ClipState::new(clip_worker, clipboard_ok),
            next_housekeeping: Instant::now(),
            last_displays_check: Instant::now(),
            shutting_down: false,
            shut_down: false,
            cfg,
        };
        match status {
            LoadStatus::Recovered(_) => core.notice(
                Level::Warn,
                "Settings could not be read",
                "Your old settings file was kept next to the new one. Devices must be paired again; nothing was shared in the meantime.",
                Some((NoticeAction::OpenPairing, "Pair a device".into())),
            ),
            LoadStatus::Loaded | LoadStatus::Fresh => {}
        }
        if store_kind == StoreKind::File {
            core.notice(Level::Info, "Identity key is stored in a file", "The operating system's secure key storage was not available, so the key sits in a private file in your settings folder. It is readable by programs running as you.", None);
        }
        for (what, safe) in notices {
            core.notice(Level::Info, &what, &safe, None);
        }
        core.init(endpoint.clone());
        tokio::spawn(accept_loop(listener, endpoint, tx.clone()));
        let task = tokio::spawn(core.run(rx));
        // Hand back a handle whose first snapshot is the real one, never the placeholder.
        let mut ready = snap_rx.clone();
        let _ = tokio::time::timeout(Duration::from_secs(3), ready.wait_for(|s| s.rev > 0)).await;
        Ok(Engine { tx, snap: snap_rx, task: Arc::new(Mutex::new(Some(task))) })
    }

    pub fn send(&self, c: Command) {
        if self.tx.try_send(CoreMsg::Cmd(c)).is_err() {
            tracing::warn!("engine is busy or stopped; command dropped");
        }
    }

    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.snap.borrow().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<Arc<Snapshot>> {
        self.snap.clone()
    }

    /// Release all input, say goodbye to peers, stop.
    pub async fn shutdown(&self) {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(CoreMsg::Shutdown(tx)).await.is_ok() {
            let _ = tokio::time::timeout(Duration::from_secs(2), rx).await;
        }
        let task = self.task.lock().ok().and_then(|mut t| t.take());
        if let Some(t) = task {
            let _ = tokio::time::timeout(Duration::from_secs(2), t).await;
        }
    }
}

fn spawn_injector(input: Arc<dyn InputBackend>) -> std::sync::mpsc::SyncSender<InputEvent> {
    let (tx, rx) = std::sync::mpsc::sync_channel::<InputEvent>(INJECT_QUEUE_DEPTH);
    let _ = std::thread::Builder::new().name("synkflow-inject".into()).spawn(move || {
        for ev in rx {
            if let Err(e) = input.inject(&ev) {
                tracing::debug!("inject failed: {e}");
            }
        }
    });
    tx
}

// ───────────────────────────── accept loop ─────────────────────────────

#[derive(Default)]
struct RateLimiter {
    hits: HashMap<IpAddr, VecDeque<Instant>>,
}

impl RateLimiter {
    fn allow(&mut self, ip: IpAddr, max: usize, window: Duration) -> bool {
        let now = Instant::now();
        let q = self.hits.entry(ip).or_default();
        while q.front().is_some_and(|t| now.duration_since(*t) > window) {
            q.pop_front();
        }
        if self.hits.len() > 512 {
            self.hits.retain(|_, q| q.back().is_some_and(|t| now.duration_since(*t) <= window));
        }
        let q = self.hits.entry(ip).or_default();
        if q.len() >= max {
            return false;
        }
        q.push_back(now);
        true
    }
}

async fn accept_loop(listener: TcpListener, endpoint: Arc<TlsEndpoint>, core: mpsc::Sender<CoreMsg>) {
    let limiter = Arc::new(Mutex::new(RateLimiter::default()));
    let slots = Arc::new(tokio::sync::Semaphore::new(ACCEPT_BACKLOG));
    loop {
        let (tcp, addr) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!("accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
        };
        // Cheap flood protection before any cryptography.
        let ok = limiter.lock().map(|mut l| l.allow(addr.ip(), 60, Duration::from_secs(10))).unwrap_or(false);
        let Ok(permit) = slots.clone().try_acquire_owned() else { continue };
        if !ok {
            continue;
        }
        let (endpoint, core, limiter) = (endpoint.clone(), core.clone(), limiter.clone());
        tokio::spawn(async move {
            let _permit = permit;
            let accepted = tokio::time::timeout(HANDSHAKE_TIMEOUT, endpoint.accept(tcp)).await;
            match accepted {
                Ok(Ok(Accepted::Session { stream, peer })) => match session::read_first(stream).await {
                    Ok(First::Control { wire, hello }) => {
                        let _ = core.send(CoreMsg::IncomingControl { wire, hello: Box::new(hello), peer, addr }).await;
                    }
                    Ok(First::Bulk { wire, session_id, token, transfer_id }) => {
                        let _ = core.send(CoreMsg::IncomingBulk { wire, session_id, token, transfer_id, peer }).await;
                    }
                    Err(e) => tracing::debug!("session handshake from {addr} failed: {e}"),
                },
                Ok(Ok(Accepted::Pairing { mut stream, peer })) => {
                    // Pairing is the only door open to strangers: rate limit it hard.
                    let allowed = limiter.lock().map(|mut l| l.allow(addr.ip(), PAIRING_RATE_MAX, PAIRING_RATE_WINDOW)).unwrap_or(false);
                    if !allowed {
                        tracing::debug!("pairing attempt from {addr} rate limited");
                        return;
                    }
                    let work = async {
                        proto::exchange_preamble(&mut stream).await?;
                        Ok::<_, proto::ProtoError>(proto::control_wire(stream))
                    };
                    if let Ok(Ok(wire)) = tokio::time::timeout(HANDSHAKE_TIMEOUT, work).await {
                        let _ = core.send(CoreMsg::IncomingPairing { wire, peer, addr }).await;
                    }
                }
                Ok(Err(e)) => tracing::debug!("TLS from {addr} refused: {e}"),
                Err(_) => tracing::debug!("TLS from {addr} timed out"),
            }
        });
    }
}

// ───────────────────────────── core state ─────────────────────────────

pub(crate) struct Remote {
    pub platform: Platform,
    pub caps: Capabilities,
    pub grants: Grants,
    pub displays: Vec<DisplayInfo>,
    pub paused: bool,
    pub locked: bool,
}

pub(crate) struct Sess {
    pub peer: Fingerprint,
    pub we_connected: bool,
    pub addr: SocketAddr,
    pub ch: SessionChannels,
    pub remote: Remote,
    pub session_key: [u8; 16],
    pub bulk_token: [u8; 16],
}

#[derive(Default)]
struct PeerRt {
    endpoints: Vec<SocketAddr>,
    session: Option<SessionId>,
    dialing: bool,
    backoff: Duration,
    next_try: Option<Instant>,
    error: Option<DialError>,
    /// The user disconnected this device; do not redial until asked.
    hold: bool,
}

pub(crate) struct Core {
    pub(crate) cfg: Config,
    paths: AppPaths,
    input: Arc<dyn InputBackend>,
    allow_loopback: bool,
    pub(crate) identity: Arc<Identity>,
    trust: TrustSet,
    pub(crate) tx: mpsc::Sender<CoreMsg>,
    port: u16,
    store_kind: StoreKind,
    control: Control,
    capture: Option<Box<dyn Capture>>,
    capture_error: Option<String>,
    backend_caps: Option<BackendCaps>,
    injector: std::sync::mpsc::SyncSender<InputEvent>,
    inject_error: Arc<Mutex<Option<String>>>,
    local_displays: Vec<DisplayInfo>,
    pub(crate) sessions: HashMap<SessionId, Sess>,
    next_sid: SessionId,
    peers: HashMap<Fingerprint, PeerRt>,
    candidates: HashMap<String, Candidate>,
    discovery: Option<Discovery>,
    discovery_error: Option<String>,
    has_lan: bool,
    notices: VecDeque<NoticeView>,
    next_notice: u64,
    snap_tx: watch::Sender<Arc<Snapshot>>,
    rev: u64,
    pub(crate) dirty: bool,
    pairing: pairing::PairingState,
    xfers: files::Transfers,
    clip: clip::ClipState,
    next_housekeeping: Instant,
    last_displays_check: Instant,
    shutting_down: bool,
    shut_down: bool,
}

impl Core {
    fn init(&mut self, _endpoint: Arc<TlsEndpoint>) {
        for p in &self.cfg.peers {
            let rt = self.peers.entry(p.fingerprint).or_default();
            if let Some(a) = p.last_endpoint.as_deref().and_then(|e| e.parse().ok()) {
                rt.endpoints.push(a);
            }
        }
        self.apply_config(true);
        if self.cfg.general.start_paused {
            self.control_event(Ev::Pause(Suspend::Paused));
        }
        self.rebuild_desk();
        self.dirty = true;
    }

    pub(crate) fn local(&self) -> Fingerprint {
        self.identity.fingerprint()
    }

    fn next_wake(&self) -> Instant {
        let mut w = self.next_housekeeping;
        if let Some(c) = self.control.next_deadline() {
            w = w.min(c);
        }
        if let Some(p) = self.xfers.next_progress() {
            w = w.min(p);
        }
        w
    }

    async fn run(mut self, mut rx: mpsc::Receiver<CoreMsg>) {
        let mut done: Option<oneshot::Sender<()>> = None;
        loop {
            let wake = self.next_wake();
            tokio::select! {
                msg = rx.recv() => {
                    let Some(msg) = msg else { break };
                    if let CoreMsg::Shutdown(ack) = msg {
                        done = Some(ack);
                        break;
                    }
                    self.handle(msg);
                    // Drain a bounded burst so one snapshot covers many events.
                    for _ in 0..256 {
                        match rx.try_recv() {
                            Ok(CoreMsg::Shutdown(ack)) => { done = Some(ack); break; }
                            Ok(m) => self.handle(m),
                            Err(_) => break,
                        }
                    }
                    if done.is_some() { break; }
                }
                _ = tokio::time::sleep_until(wake.into()) => self.tick(Instant::now()),
            }
            if self.shut_down {
                break;
            }
            self.publish();
        }
        self.teardown();
        self.publish();
        if let Some(ack) = done {
            let _ = ack.send(());
        }
    }

    fn handle(&mut self, msg: CoreMsg) {
        match msg {
            CoreMsg::Cmd(c) => self.on_command(c),
            CoreMsg::Capture(ev) => self.on_capture(ev),
            CoreMsg::Discovery(ev) => self.on_discovery(ev),
            CoreMsg::Session(SessionEvent::Msg { sid, msg }) => self.on_session_msg(sid, msg),
            CoreMsg::Session(SessionEvent::Ended { sid, reason }) => self.on_session_ended(sid, reason),
            CoreMsg::Pair(id, ev) => self.on_pair_event(id, ev),
            CoreMsg::IncomingControl { wire, hello, peer, addr } => self.on_incoming_control(wire, *hello, peer, addr),
            CoreMsg::IncomingBulk { wire, session_id, token, transfer_id, peer } => self.on_incoming_bulk(wire, session_id, token, transfer_id, peer),
            CoreMsg::IncomingPairing { wire, peer, addr } => self.on_incoming_pairing(wire, peer, addr),
            CoreMsg::DialDone { peer, addr, result } => self.on_dial_done(peer, addr, *result),
            CoreMsg::PairDialDone { id, addr, result } => self.on_pair_dial_done(id, addr, *result),
            CoreMsg::BulkDialDone { transfer_id, result } => self.on_bulk_dial_done(transfer_id, *result),
            CoreMsg::TransferDone { id, outcome } => self.on_transfer_done(id, outcome),
            CoreMsg::Clip(ev) => self.on_clip_event(ev),
            CoreMsg::Shutdown(_) => {}
        }
    }

    // ── notices ────────────────────────────────────────────────────────────

    pub(crate) fn notice(&mut self, level: Level, what: &str, safe: &str, action: Option<(NoticeAction, String)>) {
        // Collapse an identical, still-visible notice instead of stacking it.
        self.notices.retain(|n| !(n.what == what && n.safe == safe));
        let id = self.next_notice;
        self.next_notice += 1;
        self.notices.push_back(NoticeView { id, level, what: what.to_string(), safe: safe.to_string(), action, at: now_secs() });
        while self.notices.len() > 12 {
            self.notices.pop_front();
        }
        self.dirty = true;
    }

    pub(crate) fn label_of(&self, fp: &Fingerprint) -> String {
        self.cfg.peer(fp).map(|p| p.label.clone()).unwrap_or_else(|| "the other computer".into())
    }

    // ── config side effects ────────────────────────────────────────────────

    fn hotkeys(&mut self) -> (Hotkey, Hotkey) {
        let panic = Hotkey::parse(&self.cfg.input.panic_hotkey);
        let switch = Hotkey::parse(&self.cfg.input.switch_hotkey);
        if panic.is_err() || switch.is_err() {
            self.notice(
                Level::Warn,
                "A keyboard shortcut was not valid",
                "The default shortcuts are in use instead. Nothing else changed.",
                Some((NoticeAction::OpenSettings, "Open settings".into())),
            );
        }
        (panic.unwrap_or(Hotkey::PANIC), switch.unwrap_or(Hotkey::SWITCH))
    }

    fn control_cfg(&self) -> ControlConfig {
        let i = &self.cfg.input;
        ControlConfig {
            edge: EdgeDefaults {
                zone: i.edge_zone as f64,
                corner: 12.0,
                dwell_ms: i.edge_dwell_ms,
                require_modifier: i.require_modifier,
                block_corners: i.block_corners,
            },
            cross_modifier: match i.cross_modifier {
                CrossModifier::Shift => Mods::SHIFT,
                CrossModifier::Ctrl => Mods::CTRL,
                CrossModifier::Alt => Mods::ALT,
                CrossModifier::Meta => Mods::META,
            },
            sensitivity: i.pointer_sensitivity as f64,
            stay_local: i.stay_local,
            shortcut_translation: i.shortcut_translation,
            resume_after_lock: i.resume_after_lock,
        }
    }

    /// Re-derive everything that depends on settings.
    fn apply_config(&mut self, first: bool) {
        self.control.handle(Ev::ConfigChanged(self.control_cfg()), Instant::now());
        if !first {
            let (p, s) = self.hotkeys();
            if let Some(c) = &self.capture {
                c.set_hotkeys(p, s);
            }
        }
        self.restart_discovery();
        self.clip_reconcile();
        self.dirty = true;
    }

    fn save(&mut self) {
        if let Err(e) = self.cfg.save(&self.paths.config_file()) {
            tracing::warn!("could not save settings: {e}");
            self.notice(Level::Warn, "Settings could not be saved", "Your changes apply until you quit; nothing shared was affected.", None);
        }
    }

    fn restart_discovery(&mut self) {
        self.discovery = None;
        self.discovery_error = None;
        self.candidates.clear();
        if !self.cfg.network.discovery {
            return;
        }
        let (dtx, mut drx) = mpsc::channel(64);
        let policy = InterfacePolicy {
            only: self.cfg.network.interfaces.clone(),
            include_tunnels: self.cfg.network.include_tunnels,
            include_loopback: self.allow_loopback,
        };
        let announce = Announce { name: self.cfg.device_name.clone(), fingerprint: self.local(), platform: Platform::current(), port: self.port };
        match Discovery::start(&announce, &policy, dtx) {
            Ok(d) => {
                self.discovery = Some(d);
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    while let Some(ev) = drx.recv().await {
                        if tx.send(CoreMsg::Discovery(ev)).await.is_err() {
                            break;
                        }
                    }
                });
            }
            Err(e) => {
                tracing::warn!("discovery unavailable: {e}");
                self.discovery_error = Some(e.clone());
                self.notice(
                    Level::Warn,
                    "Automatic discovery is not available",
                    "Already paired devices still connect if their address is known, and you can pair by entering an address. Nothing was shared or changed.",
                    Some((NoticeAction::OpenPairing, "Pair by address".into())),
                );
            }
        }
    }

    // ── commands ───────────────────────────────────────────────────────────

    fn on_command(&mut self, c: Command) {
        self.dirty = true;
        match c {
            Command::SetPaused(true) => self.control_event(Ev::Pause(Suspend::Paused)),
            Command::SetPaused(false) => {
                self.capture_error = None;
                self.control_event(Ev::Resume);
            }
            Command::SetClipboardPaused(p) => {
                self.cfg.clipboard_paused = p;
                self.save();
                self.clip_reconcile();
            }
            Command::ReturnLocal => self.control_event(Ev::ReturnLocal),
            Command::OpenPairing => self.open_pairing_window(),
            Command::ClosePairing => self.close_pairing(),
            Command::PairWith(addr) => self.begin_pairing(addr),
            Command::PairManual(text) => match parse_endpoint(&text, self.port) {
                Some(addr) => self.begin_pairing(addr),
                None => {
                    self.notice(Level::Warn, "That address is not valid", "Use an address such as 192.168.1.20 or 192.168.1.20:24847. Nothing was sent.", None)
                }
            },
            Command::ApprovePairing { label, perms } => self.approve_pairing(label, perms),
            Command::RejectPairing => self.reject_pairing(),
            Command::Connect(fp) => {
                if let Some(rt) = self.peers.get_mut(&fp) {
                    rt.hold = false;
                    rt.next_try = None;
                    rt.backoff = Duration::ZERO;
                }
                self.dial_due(Instant::now());
            }
            Command::Disconnect(fp) => {
                if let Some(rt) = self.peers.get_mut(&fp) {
                    rt.hold = true;
                }
                if let Some(sid) = self.peers.get(&fp).and_then(|p| p.session) {
                    self.end_session(sid, true, EndReason::Cancelled);
                }
            }
            Command::Revoke(fp) => self.revoke(fp),
            Command::RenamePeer(fp, name) => {
                let name = proto::clean_text(&name, MAX_NAME_BYTES);
                if let (false, Some(p)) = (name.is_empty(), self.cfg.peer_mut(&fp)) {
                    p.label = name;
                    self.save();
                }
            }
            Command::SetPerms(fp, perms) => self.set_perms(fp, perms),
            Command::ApplyLayout(mut doc) => {
                if doc.validate().is_err() {
                    self.notice(Level::Warn, "That layout is not valid", "The previous layout is still in use.", None);
                    return;
                }
                doc.revision = self.cfg.layout.revision + 1;
                self.cfg.layout = doc;
                self.save();
                self.rebuild_desk();
                let msg = Msg::Layout(self.cfg.layout.clone());
                self.broadcast(msg);
            }
            Command::SendFiles { peer, paths } => self.send_files(peer, paths),
            Command::AcceptTransfer(id) => self.accept_transfer(id),
            Command::DeclineTransfer(id) => self.decline_transfer(id),
            Command::CancelTransfer(id) => self.cancel_transfer(id),
            Command::RetryTransfer(id) => self.retry_transfer(id),
            Command::ClearTransferHistory => self.xfers.clear_finished(),
            Command::UpdateConfig(edit) => {
                edit(&mut self.cfg);
                self.cfg.sanitize();
                self.save();
                self.apply_config(false);
                self.rebuild_desk();
            }
            Command::RecheckPermissions => {
                self.backend_caps = None;
                self.maybe_resume_after_permission();
            }
            Command::RequestPermissions => self.input.request_permissions(),
            Command::OpenPermissionSettings => self.input.open_permission_settings(),
            Command::DismissNotice(id) => self.notices.retain(|n| n.id != id),
            Command::DeleteAllLocalData => self.delete_all(),
        }
    }

    fn set_perms(&mut self, fp: Fingerprint, perms: PeerPerms) {
        let Some(p) = self.cfg.peer_mut(&fp) else { return };
        p.perms = perms;
        self.save();
        // Tell the peer what it may do to us, and re-evaluate control rights now.
        if let Some(g) = self.my_grants(&fp) {
            self.send_to(&fp, Msg::Grants(g));
        }
        self.refresh_peer_ctl(fp);
        self.clip_reconcile();
    }

    fn revoke(&mut self, fp: Fingerprint) {
        let label = self.label_of(&fp);
        // Order matters: stop trusting first so no new handshake can succeed,
        // then end the live session and everything bound to it.
        self.trust.remove(&fp);
        self.cfg.peers.retain(|p| p.fingerprint != fp);
        self.cfg.layout.forget(&fp);
        self.save();
        if let Some(sid) = self.peers.get(&fp).and_then(|p| p.session) {
            self.end_session(sid, true, EndReason::Cancelled);
        }
        self.peers.remove(&fp);
        self.fail_transfers_with(&fp);
        self.rebuild_desk();
        self.notice(
            Level::Info,
            &format!("{label} was removed"),
            "It can no longer connect, and any control it had ended immediately. You can pair it again at any time.",
            None,
        );
    }

    fn delete_all(&mut self) {
        let all: Vec<_> = self.cfg.peers.iter().map(|p| p.fingerprint).collect();
        for fp in all {
            self.trust.remove(&fp);
            if let Some(sid) = self.peers.get(&fp).and_then(|p| p.session) {
                self.end_session(sid, true, EndReason::Cancelled);
            }
        }
        let _ = std::fs::remove_file(self.paths.config_file());
        let (a, b) = self.paths.secret_stores();
        let _ = a.delete();
        let _ = b.delete();
        self.cfg = Config::default();
        self.peers.clear();
        self.shut_down = true;
    }

    // ── capture & control ──────────────────────────────────────────────────

    fn on_capture(&mut self, ev: CaptureEvent) {
        match ev {
            CaptureEvent::Input(raw) => self.control_event(Ev::Raw(raw)),
            CaptureEvent::Hotkey(a) => {
                self.control_event(match a {
                    Action::Panic => Ev::Hotkey(Action::Panic),
                    Action::Switch => Ev::Hotkey(Action::Switch),
                });
            }
            CaptureEvent::Lost => {
                // The OS turned capture off (permission revoked, tap disabled).
                self.capture = None;
                self.capture_error = Some("The operating system stopped delivering keyboard and mouse events.".into());
                self.control_event(Ev::Pause(Suspend::CaptureUnavailable));
                self.notice(
                    Level::Error,
                    "Keyboard and mouse sharing stopped",
                    "Control returned to this computer and every held key was released. Nothing is shared until you turn sharing on again.",
                    Some((NoticeAction::OpenPermissionSettings, "Check permissions".into())),
                );
            }
        }
    }

    pub(crate) fn control_event(&mut self, ev: Ev) {
        let before = self.control.state().clone();
        let fx = self.control.handle(ev, Instant::now());
        self.exec(fx);
        if *self.control.state() != before {
            self.on_state_change(&before);
        }
    }

    fn on_state_change(&mut self, before: &State) {
        self.dirty = true;
        self.reconcile_capture();
        self.clip_reconcile();
        let paused = |s: &State| matches!(s, State::Suspended(_));
        let locked = |s: &State| matches!(s, State::Suspended(Suspend::Locked));
        let now = self.control.state();
        if paused(now) != paused(before) || locked(now) != locked(before) {
            let msg = Msg::State { paused: paused(now), locked: locked(now) };
            self.broadcast(msg);
        }
    }

    fn reconcile_capture(&mut self) {
        let wanted = matches!(self.control.state(), State::Local | State::Pending { .. } | State::RemoteActive { .. } | State::BeingControlled { .. });
        if !wanted {
            // Not sharing: no hooks installed, nothing watching the keyboard.
            self.capture = None;
            return;
        }
        if self.capture.is_some() {
            return;
        }
        let (panic, switch) = self.hotkeys();
        let (ctx, mut crx) = mpsc::channel::<CaptureEvent>(CAPTURE_QUEUE_DEPTH);
        match self.input.start_capture(CaptureSink::new(ctx), panic, switch) {
            Ok(c) => {
                self.capture = Some(c);
                self.capture_error = None;
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    while let Some(ev) = crx.recv().await {
                        if tx.send(CoreMsg::Capture(ev)).await.is_err() {
                            break;
                        }
                    }
                });
            }
            Err(e) => {
                self.capture_error = Some(e.to_string());
                // Receiving control does not need capture, so only stop when we
                // were relying on it for the emergency shortcut or for sending.
                let can_send = self.cfg.peers.iter().any(|p| p.perms.share_input);
                let msg = format!("{e}");
                if can_send {
                    self.control_event(Ev::Pause(Suspend::CaptureUnavailable));
                    self.notice(
                        Level::Error,
                        "Synkflow needs permission to read the keyboard and mouse",
                        &format!("Sharing stayed off, so nothing left this computer. ({msg})"),
                        Some((NoticeAction::OpenPermissionSettings, "Open permission settings".into())),
                    );
                } else {
                    self.notice(
                        Level::Warn,
                        "The emergency shortcut is unavailable on this computer",
                        "Use the Pause button or the menu-bar item to stop sharing at any time. Nothing else changed.",
                        Some((NoticeAction::OpenPermissionSettings, "Open permission settings".into())),
                    );
                }
            }
        }
    }

    fn maybe_resume_after_permission(&mut self) {
        if matches!(self.control.state(), State::Suspended(Suspend::CaptureUnavailable)) {
            let caps = self.input.capabilities();
            if caps.capture.is_available() {
                self.capture_error = None;
                self.control_event(Ev::Resume);
                self.notice(Level::Info, "Permission granted", "Sharing is on again.", None);
            }
        }
        self.dirty = true;
    }

    fn exec(&mut self, fx: Vec<Effect>) {
        for e in fx {
            match e {
                Effect::Send { peer, msg } => self.send_to(&peer, msg),
                Effect::SendInput { peer, ev } => self.send_input(&peer, ev),
                Effect::Grab(g) => {
                    if let Some(c) = &self.capture {
                        c.set_grab(g);
                    }
                }
                Effect::Warp { x, y } => {
                    if let Some(c) = &self.capture {
                        c.warp(x, y);
                    }
                }
                Effect::Inject(ev) => {
                    if self.injector.try_send(ev).is_err() {
                        tracing::warn!("injector queue full");
                    }
                }
                Effect::Notice(n) => self.control_notice(n),
            }
        }
    }

    fn control_notice(&mut self, n: CNotice) {
        let who = self.control.active_peer().map(|p| self.label_of(&p));
        match n {
            CNotice::ConnectionLost => self.notice(
                Level::Warn,
                "Connection lost. Control has returned to this computer.",
                "Every key and button held on the other computer was released.",
                None,
            ),
            CNotice::EntryRejected(r) => {
                let (what, safe) = match r {
                    RejectReason::Locked => ("That computer is locked.".to_string(), "Control stayed on this computer."),
                    RejectReason::Busy => ("That computer is already being controlled by another device.".to_string(), "Control stayed on this computer."),
                    RejectReason::Paused => ("Sharing is paused on that computer.".to_string(), "Control stayed on this computer."),
                    _ => ("That computer does not allow this one to control it.".to_string(), "Control stayed on this computer; change this in Devices."),
                };
                self.notice(Level::Warn, &what, safe, Some((NoticeAction::OpenDevices, "Open devices".into())));
            }
            CNotice::EntryTimedOut => self.notice(Level::Warn, "The other computer did not respond in time.", "Control stayed on this computer.", None),
            CNotice::Suspended(Suspend::Panic) => self.notice(
                Level::Warn,
                "Emergency stop: sharing is paused.",
                "Control is back on this computer and nothing will cross over until you turn sharing on again.",
                None,
            ),
            CNotice::Suspended(Suspend::Locked) => self.notice(
                Level::Info,
                "Screen locked: sharing is paused.",
                "Held keys were released. Sharing stays off after unlocking unless you chose otherwise in Settings.",
                None,
            ),
            CNotice::Suspended(_) | CNotice::Resumed => {}
            CNotice::ControlReturned(r) => match r {
                ReleaseReason::PermissionRevoked => {
                    self.notice(Level::Info, "Permission was withdrawn, so control ended.", "Held keys and buttons were released.", None)
                }
                ReleaseReason::Locked => {
                    let name = who.unwrap_or_else(|| "The other computer".into());
                    self.notice(Level::Info, &format!("{name} was locked. Control returned to this computer."), "Nothing is being sent to it.", None)
                }
                ReleaseReason::Paused | ReleaseReason::Panic => {
                    self.notice(Level::Info, "The other computer paused sharing. Control returned to this computer.", "Nothing is being sent to it.", None)
                }
                ReleaseReason::Shutdown => self.notice(
                    Level::Info,
                    "The other computer quit Synkflow. Control returned to this computer.",
                    "Held keys and buttons were released.",
                    None,
                ),
                _ => {}
            },
        }
    }

    // ── layout / displays ──────────────────────────────────────────────────

    pub(crate) fn known_devices(&self) -> Vec<(Fingerprint, Vec<DisplayInfo>)> {
        let mut v = vec![(self.local(), self.local_displays.clone())];
        for p in &self.cfg.peers {
            let live = self.peers.get(&p.fingerprint).and_then(|r| r.session).and_then(|s| self.sessions.get(&s)).map(|s| s.remote.displays.clone());
            let displays = live.unwrap_or_else(|| p.displays.clone());
            if !displays.is_empty() {
                v.push((p.fingerprint, displays));
            }
        }
        v
    }

    pub(crate) fn rebuild_desk(&mut self) {
        let desk = Desk::build(&self.cfg.layout, &self.known_devices());
        self.control_event(Ev::DeskChanged(desk));
        self.dirty = true;
    }

    // ── peers & sessions ───────────────────────────────────────────────────

    pub(crate) fn my_caps(&mut self) -> Capabilities {
        let c = self.backend_caps.get_or_insert_with(|| self.input.capabilities());
        Capabilities {
            input_source: !matches!(c.capture, Cap::Unavailable(_)),
            input_inject: !matches!(c.inject, Cap::Unavailable(_)),
            clipboard_text: self.clip.available,
            clipboard_image: self.clip.available,
            files: true,
        }
    }

    pub(crate) fn my_grants(&self, peer: &Fingerprint) -> Option<Grants> {
        let p = self.cfg.peer(peer)?;
        Some(Grants { control: p.perms.accept_input, clipboard: p.perms.clipboard_receive, files: p.perms.files_receive })
    }

    fn local_paused(&self) -> bool {
        matches!(self.control.state(), State::Suspended(_))
    }

    fn my_hello(&mut self, session_id: [u8; 16], peer: &Fingerprint) -> Hello {
        Hello {
            name: self.cfg.device_name.clone(),
            platform: Platform::current(),
            app_version: env!("CARGO_PKG_VERSION").into(),
            caps: self.my_caps(),
            grants: self.my_grants(peer).unwrap_or_default(),
            session_id,
            displays: self.local_displays.clone(),
            paused: self.local_paused(),
        }
    }

    fn peer_ctl(&mut self, peer: &Fingerprint) -> Option<PeerCtl> {
        let sid = self.peers.get(peer)?.session?;
        let tp = self.cfg.peer(peer)?.clone();
        let mine = self.my_caps();
        let s = self.sessions.get(&sid)?;
        let neg = mine.intersect(s.remote.caps);
        Some(PeerCtl {
            platform: s.remote.platform,
            caps: neg,
            we_may_control: tp.perms.share_input && neg.input_source && s.remote.grants.control,
            they_may_control: tp.perms.accept_input && neg.input_inject,
            remote_paused: s.remote.paused,
            remote_locked: s.remote.locked,
            translate: tp.perms.translate_shortcuts,
            invert_scroll: tp.perms.invert_scroll,
        })
    }

    pub(crate) fn refresh_peer_ctl(&mut self, peer: Fingerprint) {
        if let Some(ctl) = self.peer_ctl(&peer) {
            self.control_event(Ev::PeerChanged { peer, ctl });
        }
        self.dirty = true;
    }

    pub(crate) fn send_to(&mut self, peer: &Fingerprint, msg: Msg) {
        if let Some(sid) = self.peers.get(peer).and_then(|p| p.session) {
            self.send_sid(sid, msg);
        }
    }

    pub(crate) fn send_sid(&mut self, sid: SessionId, msg: Msg) {
        let Some(s) = self.sessions.get(&sid) else { return };
        if s.ch.hi.try_send(msg).is_err() {
            // Full or closed: the peer cannot keep up. Fail safe rather than drop control messages.
            tracing::warn!("session {sid} overloaded; closing");
            s.ch.cancel.cancel();
        }
    }

    fn send_input(&mut self, peer: &Fingerprint, ev: InputEvent) {
        let Some(sid) = self.peers.get(peer).and_then(|p| p.session) else { return };
        let Some(s) = self.sessions.get(&sid) else { return };
        match s.ch.hi.try_send(Msg::Input(ev)) {
            Ok(()) => {}
            // A newer absolute position supersedes a dropped one; anything else must arrive.
            Err(mpsc::error::TrySendError::Full(_)) if matches!(ev, InputEvent::PointerAbs { .. }) => {}
            Err(_) => {
                tracing::warn!("input queue to {} overflowed; ending session", peer.short());
                s.ch.cancel.cancel();
            }
        }
    }

    pub(crate) fn broadcast(&mut self, msg: Msg) {
        let sids: Vec<_> = self.sessions.keys().copied().collect();
        for sid in sids {
            self.send_sid(sid, msg.clone());
        }
    }

    fn new_sid(&mut self) -> SessionId {
        let s = self.next_sid;
        self.next_sid += 1;
        s
    }

    /// Decide whether a new connection with `peer` replaces an existing one.
    /// Same connector ⇒ the peer restarted, replace. Different connectors (a
    /// simultaneous dial) ⇒ the connection initiated by the lower fingerprint wins.
    fn keep_new(&self, peer: &Fingerprint, new_connector: Fingerprint) -> bool {
        let Some(existing) = self.peers.get(peer).and_then(|p| p.session).and_then(|s| self.sessions.get(&s)) else { return true };
        let old_connector = if existing.we_connected { self.local() } else { *peer };
        old_connector == new_connector || new_connector < old_connector
    }

    fn on_incoming_control(&mut self, wire: Wire<ServerStream>, hello: Hello, peer: Fingerprint, addr: SocketAddr) {
        // The TLS layer already required an approved identity; check again
        // because trust may have changed while the handshake was in flight.
        if !self.trust.contains(&peer) || self.cfg.peer(&peer).is_none() {
            tokio::spawn(session::refuse(wire, RejectReason::NotAllowed));
            return;
        }
        if !self.keep_new(&peer, peer) {
            tokio::spawn(session::refuse(wire, RejectReason::AlreadyConnected));
            return;
        }
        if let Some(sid) = self.peers.get(&peer).and_then(|p| p.session) {
            self.end_session(sid, true, EndReason::Cancelled);
        }
        let bulk_token: [u8; 16] = rand::random();
        let ack = HelloAck {
            name: self.cfg.device_name.clone(),
            platform: Platform::current(),
            app_version: env!("CARGO_PKG_VERSION").into(),
            caps: self.my_caps(),
            grants: self.my_grants(&peer).unwrap_or_default(),
            displays: self.local_displays.clone(),
            paused: self.local_paused(),
            bulk_token,
        };
        let remote = Remote { platform: hello.platform, caps: hello.caps, grants: hello.grants, displays: hello.displays, paused: hello.paused, locked: false };
        let sid = self.new_sid();
        let ch = session::spawn_session(sid, wire, Some(Msg::HelloAck(ack)), self.session_events());
        self.register_session(sid, peer, false, addr, ch, remote, hello.session_id, bulk_token);
    }

    /// A sender whose events arrive in the core loop as `CoreMsg::Session`.
    fn session_events(&self) -> mpsc::Sender<SessionEvent> {
        let (tx, mut rx) = mpsc::channel::<SessionEvent>(256);
        let core = self.tx.clone();
        tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                if core.send(CoreMsg::Session(ev)).await.is_err() {
                    break;
                }
            }
        });
        tx
    }

    #[allow(clippy::too_many_arguments)]
    fn register_session(
        &mut self,
        sid: SessionId,
        peer: Fingerprint,
        we_connected: bool,
        addr: SocketAddr,
        ch: SessionChannels,
        remote: Remote,
        session_key: [u8; 16],
        bulk_token: [u8; 16],
    ) {
        let rt = self.peers.entry(peer).or_default();
        rt.session = Some(sid);
        rt.dialing = false;
        rt.error = None;
        rt.backoff = Duration::ZERO;
        rt.next_try = None;
        if !rt.endpoints.contains(&addr) && we_connected {
            rt.endpoints.insert(0, addr);
        }
        let displays = remote.displays.clone();
        let platform = remote.platform;
        self.sessions.insert(sid, Sess { peer, we_connected, addr, ch, remote, session_key, bulk_token });
        if let Some(p) = self.cfg.peer_mut(&peer) {
            p.last_connected = Some(now_secs());
            p.displays = displays;
            p.platform = platform;
            if we_connected {
                p.last_endpoint = Some(addr.to_string());
            }
        }
        self.save();
        if self.cfg.layout.revision > 0 {
            let layout = self.cfg.layout.clone();
            self.send_sid(sid, Msg::Layout(layout));
        }
        self.rebuild_desk();
        if let Some(ctl) = self.peer_ctl(&peer) {
            self.control_event(Ev::PeerUp { peer, ctl });
        }
        self.clip_reconcile();
        self.dirty = true;
    }

    fn on_session_ended(&mut self, sid: SessionId, reason: EndReason) {
        if !self.sessions.contains_key(&sid) {
            return; // already replaced or removed
        }
        let peer = self.sessions[&sid].peer;
        self.end_session(sid, false, reason);
        let label = self.label_of(&peer);
        match reason {
            EndReason::Lost | EndReason::Timeout => self.notice(
                Level::Warn,
                &format!("Connection to {label} was lost."),
                "Control is on this computer and nothing is being held down. Synkflow will reconnect automatically.",
                None,
            ),
            EndReason::Protocol => self.notice(
                Level::Warn,
                &format!("{label} sent something Synkflow could not understand."),
                "The connection was closed. Nothing it sent was acted on.",
                None,
            ),
            EndReason::PeerClosed(_) | EndReason::Cancelled => {}
        }
    }

    /// Remove a session and everything that depended on it.
    pub(crate) fn end_session(&mut self, sid: SessionId, say_bye: bool, _reason: EndReason) {
        let Some(s) = self.sessions.remove(&sid) else { return };
        if say_bye {
            let _ = s.ch.hi.try_send(Msg::Bye(ReleaseReason::Shutdown));
        }
        s.ch.cancel.cancel();
        let peer = s.peer;
        if let Some(rt) = self.peers.get_mut(&peer)
            && rt.session == Some(sid)
        {
            rt.session = None;
            rt.next_try = Some(Instant::now() + jitter(BACKOFF_MIN));
            rt.backoff = BACKOFF_MIN;
        }
        self.control_event(Ev::PeerDown { peer });
        self.fail_transfers_with(&peer);
        self.clip_session_gone(&peer);
        self.rebuild_desk();
        self.dirty = true;
    }

    fn on_session_msg(&mut self, sid: SessionId, msg: Msg) {
        let Some(peer) = self.sessions.get(&sid).map(|s| s.peer) else { return };
        match msg {
            Msg::Input(ev) => self.control_event(Ev::RemoteInput { peer, ev }),
            Msg::Enter { seq, display, x, y, held } => self.control_event(Ev::RemoteEnter { peer, seq, display, x, y, held }),
            Msg::EnterAck { seq, accepted, reason } => self.control_event(Ev::EnterAck { peer, seq, accepted, reason }),
            Msg::Leave { reason } => self.control_event(Ev::RemoteLeave { peer, reason }),
            Msg::Displays(d) => {
                if let Some(s) = self.sessions.get_mut(&sid) {
                    s.remote.displays = d.clone();
                }
                if let Some(p) = self.cfg.peer_mut(&peer) {
                    p.displays = d;
                }
                self.rebuild_desk();
            }
            Msg::Layout(doc) => {
                let related = self.cfg.peer(&peer).is_some_and(|p| p.perms.share_input || p.perms.accept_input);
                if related && doc.revision > self.cfg.layout.revision {
                    self.cfg.layout = doc;
                    self.save();
                    self.rebuild_desk();
                    let l = self.label_of(&peer);
                    self.notice(Level::Info, &format!("{l} updated the screen layout."), "Open Screen layout to review it.", None);
                }
            }
            Msg::Grants(g) => {
                if let Some(s) = self.sessions.get_mut(&sid) {
                    s.remote.grants = g;
                }
                self.refresh_peer_ctl(peer);
                self.clip_reconcile();
            }
            Msg::State { paused, locked } => {
                if let Some(s) = self.sessions.get_mut(&sid) {
                    s.remote.paused = paused;
                    s.remote.locked = locked;
                }
                self.refresh_peer_ctl(peer);
            }
            Msg::ClipOffer { id, origin, kind, size } => self.on_clip_offer(sid, peer, id, origin, kind, size),
            Msg::ClipChunk { id, data } => self.on_clip_chunk(peer, id, data),
            Msg::ClipAbort { id } => self.on_clip_abort(peer, id),
            Msg::FileOffer { transfer_id, files } => self.on_file_offer(sid, peer, transfer_id, files),
            Msg::FileAnswer { transfer_id, accepted, reason } => self.on_file_answer(peer, transfer_id, accepted, reason),
            Msg::FileCancel { transfer_id } => self.on_file_cancel(peer, transfer_id),
            // Handshake and pairing messages are never valid on an established control session.
            Msg::Hello(_) | Msg::HelloAck(_) | Msg::Reject(_) | Msg::Attach { .. } | Msg::PairHello { .. } | Msg::PairDecision { .. } => {
                tracing::warn!("unexpected message from {}", peer.short());
                if let Some(s) = self.sessions.get(&sid) {
                    s.ch.cancel.cancel();
                }
            }
            Msg::Ping(_) | Msg::Pong(_) | Msg::Bye(_) => {}
        }
        self.dirty = true;
    }

    // ── dialing ────────────────────────────────────────────────────────────

    fn dial_due(&mut self, now: Instant) {
        if self.local_shut() {
            return;
        }
        let peers: Vec<Fingerprint> = self.peers.keys().copied().collect();
        for fp in peers {
            let (local, trusted) = (self.local(), self.trust.contains(&fp));
            let Some(rt) = self.peers.get_mut(&fp) else { continue };
            if rt.session.is_some() || rt.dialing || rt.hold || rt.endpoints.is_empty() || !trusted {
                continue;
            }
            // Let the lower-fingerprint device dial first to avoid mutual dials.
            if rt.next_try.is_none() {
                rt.next_try = Some(now + if local < fp { Duration::ZERO } else { Duration::from_millis(1500) });
            }
            if rt.next_try.is_some_and(|t| now < t) {
                continue;
            }
            let rt = self.peers.get_mut(&fp).expect("exists");
            rt.dialing = true;
            let addrs = rt.endpoints.clone();
            let hello = self.my_hello(rand::random(), &fp);
            let (id, tx) = (self.identity.clone(), self.tx.clone());
            tokio::spawn(async move {
                let outcome = session::race(&addrs, DIAL_STAGGER, |addr| {
                    let (id, hello) = (id.clone(), hello.clone());
                    async move { session::dial(addr, &id, fp, hello).await }
                })
                .await;
                let (addr, result) = match outcome {
                    Ok((a, d)) => (a, Ok(d)),
                    Err(e) => (SocketAddr::from(([0, 0, 0, 0], 0)), Err(e)),
                };
                if let Err(e) = &result {
                    tracing::warn!("could not reach a paired computer at {addrs:?}: {e:?}");
                }
                let _ = tx.send(CoreMsg::DialDone { peer: fp, addr, result: Box::new(result) }).await;
            });
            self.dirty = true;
        }
    }

    fn local_shut(&self) -> bool {
        self.shutting_down
    }

    fn on_dial_done(&mut self, peer: Fingerprint, addr: SocketAddr, result: Result<Dialed, DialError>) {
        let Some(rt) = self.peers.get_mut(&peer) else { return };
        rt.dialing = false;
        self.dirty = true;
        match result {
            Ok(d) => {
                if !self.trust.contains(&peer) {
                    return;
                }
                if !self.keep_new(&peer, self.local()) {
                    return; // the other side's connection won the race
                }
                if let Some(sid) = self.peers.get(&peer).and_then(|p| p.session) {
                    self.end_session(sid, true, EndReason::Cancelled);
                }
                tracing::info!("connected to {} at {addr}", self.label_of(&peer));
                let ack = d.ack;
                let remote = Remote { platform: ack.platform, caps: ack.caps, grants: ack.grants, displays: ack.displays, paused: ack.paused, locked: false };
                let sid = self.new_sid();
                let ch = session::spawn_session(sid, d.wire, None, self.session_events());
                self.register_session(sid, peer, true, addr, ch, remote, d.session_id, ack.bulk_token);
            }
            Err(e) => {
                let rt = self.peers.get_mut(&peer).expect("exists");
                rt.error = Some(e.clone());
                rt.backoff = (rt.backoff.max(BACKOFF_MIN) * 2).min(BACKOFF_MAX);
                let long = matches!(e, DialError::Unauthorized | DialError::VersionMismatch(_));
                let wait = if long { BACKOFF_MAX } else { rt.backoff };
                rt.next_try = Some(Instant::now() + jitter(wait));
                let label = self.label_of(&peer);
                match e {
                    DialError::VersionMismatch(v) => self.notice(
                        Level::Error,
                        &format!("{label} runs an incompatible Synkflow (protocol v{v}, this one speaks v{PROTOCOL_MAJOR})."),
                        "Nothing was exchanged. Update Synkflow on both computers to the same version.",
                        None,
                    ),
                    DialError::Unauthorized => self.notice(
                        Level::Warn,
                        &format!("{label} does not recognise this computer."),
                        "It may have removed this device. Nothing was shared; pair again to reconnect.",
                        Some((NoticeAction::OpenPairing, "Pair again".into())),
                    ),
                    DialError::Rejected(RejectReason::AlreadyConnected) => {}
                    _ => {}
                }
            }
        }
    }

    // ── discovery ──────────────────────────────────────────────────────────

    fn on_discovery(&mut self, ev: DiscoveryEvent) {
        match ev {
            DiscoveryEvent::Found(c) => {
                tracing::info!("found {} ({:?}) at {:?}", c.name, c.platform, c.addrs);
                // Remember where approved devices are, so they reconnect on their own.
                let hint = c.fp_hint.clone();
                let matches: Vec<Fingerprint> =
                    self.cfg.peers.iter().filter(|p| p.fingerprint.hex().to_lowercase().starts_with(&hint)).map(|p| p.fingerprint).collect();
                for fp in matches {
                    let rt = self.peers.entry(fp).or_default();
                    merge_endpoints(&mut rt.endpoints, &c.addrs, self.allow_loopback);
                    if rt.session.is_none() && !rt.dialing {
                        rt.next_try = None;
                    }
                }
                self.candidates.insert(c.key.clone(), c);
                self.dial_due(Instant::now());
            }
            DiscoveryEvent::Removed(key) => {
                self.candidates.remove(&key);
            }
        }
        self.dirty = true;
    }

    // ── timers ─────────────────────────────────────────────────────────────

    fn tick(&mut self, now: Instant) {
        // Control deadlines (entry timeout, dwell).
        if self.control.next_deadline().is_some_and(|d| d <= now) {
            self.control_event(Ev::Tick);
        }
        if self.xfers.next_progress().is_some_and(|p| p <= now) {
            self.refresh_progress(now);
        }
        if now >= self.next_housekeeping {
            self.next_housekeeping = now + Duration::from_secs(1);
            self.housekeeping(now);
        }
    }

    fn housekeeping(&mut self, now: Instant) {
        self.dial_due(now);
        self.pair_tick(now);
        self.clip_housekeeping(now);
        self.files_housekeeping(now);
        // Screen lock follows the OS; resume only when the user chose that.
        if !matches!(self.control.state(), State::Suspended(Suspend::Paused | Suspend::Panic | Suspend::CaptureUnavailable | Suspend::Shutdown)) {
            let locked = self.input.session_locked();
            let is_locked = matches!(self.control.state(), State::Suspended(Suspend::Locked));
            if locked && !is_locked {
                self.control_event(Ev::Pause(Suspend::Locked));
            } else if !locked && is_locked {
                if self.cfg.input.resume_after_lock {
                    self.control_event(Ev::Resume);
                } else {
                    // Stay paused, but no longer "locked": the user resumes explicitly.
                    self.control_event(Ev::Pause(Suspend::Paused));
                }
            }
        }
        if matches!(self.control.state(), State::Suspended(Suspend::CaptureUnavailable))
            && now.duration_since(self.last_displays_check) > Duration::from_secs(2)
        {
            self.backend_caps = None;
            self.maybe_resume_after_permission();
        }
        if now.duration_since(self.last_displays_check) >= Duration::from_secs(5) {
            self.last_displays_check = now;
            self.check_environment();
        }
        if let Some(e) = self.inject_error.lock().ok().and_then(|mut g| g.take()) {
            self.notice(
                Level::Error,
                "This computer refused to accept input",
                &format!("Nothing was applied. ({e})"),
                Some((NoticeAction::OpenPermissionSettings, "Check permissions".into())),
            );
        }
        // Good news fades on its own; problems stay until dismissed (or ten minutes).
        let t = now_secs();
        let before = self.notices.len();
        self.notices.retain(|n| {
            let age = t.saturating_sub(n.at);
            match n.level {
                Level::Info => age < 12,
                Level::Warn => age < 90,
                Level::Error => age < 600,
            }
        });
        if self.notices.len() != before {
            self.dirty = true;
        }
    }

    fn check_environment(&mut self) {
        if let Ok(d) = self.input.displays()
            && d != self.local_displays
        {
            self.local_displays = d.clone();
            self.broadcast(Msg::Displays(d));
            self.rebuild_desk();
        }
        let lan = lan_present(self.allow_loopback);
        if lan != self.has_lan {
            self.has_lan = lan;
            if !lan {
                self.notice(
                    Level::Warn,
                    "No local network connection",
                    "Control stays on this computer. Synkflow reconnects by itself when a network is back.",
                    None,
                );
            }
            self.dirty = true;
        }
        let caps = self.input.capabilities();
        let changed = self.backend_caps.as_ref().map(|c| (c.capture.clone(), c.inject.clone())) != Some((caps.capture.clone(), caps.inject.clone()));
        if changed {
            self.backend_caps = Some(caps);
            self.dirty = true;
        }
    }

    // ── shutdown ───────────────────────────────────────────────────────────

    fn teardown(&mut self) {
        self.shutting_down = true;
        // Hand the pointer back and release anything injected.
        self.control_event(Ev::Pause(Suspend::Shutdown));
        self.capture = None;
        let sids: Vec<_> = self.sessions.keys().copied().collect();
        for sid in sids {
            if let Some(s) = self.sessions.get(&sid) {
                let _ = s.ch.hi.try_send(Msg::Bye(ReleaseReason::Shutdown));
            }
        }
        self.cancel_all_transfers();
        self.discovery = None;
    }

    // ── snapshot ───────────────────────────────────────────────────────────

    fn publish(&mut self) {
        if !self.dirty {
            return;
        }
        self.dirty = false;
        self.rev += 1;
        let snap = self.build_snapshot();
        let _ = self.snap_tx.send(Arc::new(snap));
    }

    fn build_snapshot(&mut self) -> Snapshot {
        let caps = self.backend_caps.get_or_insert_with(|| self.input.capabilities()).clone();
        let state = self.control.state().clone();
        let (label, summary) = self.state_summary(&state, &caps);
        let paused = matches!(state, State::Suspended(_));
        let mine = self.my_caps();
        let mut peers = Vec::new();
        for tp in self.cfg.peers.clone() {
            let rt = self.peers.get(&tp.fingerprint);
            let sess = rt.and_then(|r| r.session).and_then(|s| self.sessions.get(&s));
            let status = match (&sess, rt) {
                (Some(s), _) => {
                    if state == (State::RemoteActive { peer: tp.fingerprint }) || matches!(state, State::Pending { peer, .. } if peer == tp.fingerprint) {
                        PeerStatus::Controlling
                    } else if state == (State::BeingControlled { peer: tp.fingerprint }) {
                        PeerStatus::BeingControlled
                    } else if s.remote.locked {
                        PeerStatus::RemoteLocked
                    } else if s.remote.paused {
                        PeerStatus::RemotePaused
                    } else {
                        PeerStatus::Connected
                    }
                }
                (None, Some(r)) if r.dialing => PeerStatus::Connecting,
                (None, Some(r)) if r.error == Some(DialError::Unauthorized) => PeerStatus::NotTrustedByPeer,
                (None, Some(r)) if r.hold => PeerStatus::Offline,
                _ => PeerStatus::Offline,
            };
            let error = rt.and_then(|r| r.error.as_ref()).map(|e| match e {
                DialError::Unreachable => "Could not reach it. Is it on, on this network, and allowed through its firewall?".to_string(),
                DialError::WrongPeer => "A different device answered at that address.".to_string(),
                DialError::Unauthorized => "It does not recognise this computer. Pair again.".to_string(),
                DialError::VersionMismatch(v) => format!("It speaks protocol v{v}; this build speaks v{PROTOCOL_MAJOR}."),
                DialError::Rejected(r) => format!("It refused the connection ({r:?})."),
                DialError::Protocol => "It did not answer correctly.".to_string(),
            });
            let neg = sess.map(|s| mine.intersect(s.remote.caps));
            peers.push(PeerView {
                fingerprint: tp.fingerprint,
                label: tp.label.clone(),
                announced_name: tp.announced_name.clone(),
                platform: tp.platform,
                status,
                endpoint: sess.map(|s| s.addr.to_string()).or_else(|| tp.last_endpoint.clone()),
                last_connected: tp.last_connected,
                perms: tp.perms.clone(),
                caps: neg,
                error: if sess.is_some() { None } else { error },
                displays: sess.map(|s| s.remote.displays.clone()).unwrap_or_else(|| tp.displays.clone()),
                rtt_ms: None,
            });
        }
        let trusted_hints: Vec<(String, String)> =
            self.cfg.peers.iter().map(|p| (p.fingerprint.hex().to_lowercase()[..16].to_string(), p.label.clone())).collect();
        let mut candidates: Vec<CandidateView> = self
            .candidates
            .values()
            .filter(|c| c.addrs.iter().any(|a| !a.ip().is_loopback() || self.allow_loopback))
            .map(|c| {
                let already = trusted_hints.iter().any(|(h, _)| *h == c.fp_hint);
                let resembles = if already {
                    None
                } else {
                    self.cfg
                        .peers
                        .iter()
                        .find(|p| p.announced_name.eq_ignore_ascii_case(&c.name) || p.label.eq_ignore_ascii_case(&c.name))
                        .map(|p| p.label.clone())
                };
                CandidateView {
                    key: c.key.clone(),
                    name: c.name.clone(),
                    platform: c.platform,
                    addr: c.addrs[0],
                    fp_hint: c.fp_hint.clone(),
                    already_paired: already,
                    resembles,
                }
            })
            .collect();
        candidates.sort_by(|a, b| a.name.cmp(&b.name));

        let devices = {
            let mut v = vec![DeviceDisplays {
                fingerprint: self.local(),
                label: self.cfg.device_name.clone(),
                is_local: true,
                online: true,
                displays: self.local_displays.clone(),
            }];
            for p in &peers {
                v.push(DeviceDisplays {
                    fingerprint: p.fingerprint,
                    label: p.label.clone(),
                    is_local: false,
                    online: !matches!(p.status, PeerStatus::Offline | PeerStatus::Connecting | PeerStatus::NotTrustedByPeer),
                    displays: p.displays.clone(),
                });
            }
            v
        };
        let has_lan = self.has_lan;
        Snapshot {
            rev: self.rev,
            device: DeviceView {
                name: self.cfg.device_name.clone(),
                fingerprint: self.local(),
                platform: Platform::current(),
                listen_port: self.port,
                store: self.store_kind,
                config_dir_label: self.paths.dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
            },
            sharing: SharingView {
                state: label,
                paused,
                clipboard_paused: self.cfg.clipboard_paused,
                stay_local: self.cfg.input.stay_local,
                active_peer: self.control.active_peer(),
                panic_hotkey: self.cfg.input.panic_hotkey.clone(),
                switch_hotkey: self.cfg.input.switch_hotkey.clone(),
                panic_hotkey_live: self.capture.is_some(),
                summary,
            },
            backend: BackendView::from_caps(&caps, self.clip.available),
            peers,
            candidates,
            pairing: self.pairing_view(),
            pairing_window_secs: self.pairing_window_secs(),
            transfers: self.transfer_views(),
            layout: LayoutView { doc: self.cfg.layout.clone(), devices },
            notices: self.notices.iter().cloned().collect(),
            network: NetworkView {
                discovery_on: self.discovery.is_some(),
                discovery_error: self.discovery_error.clone(),
                has_lan,
                interfaces: crate::discovery::list_interfaces(),
                addresses: lan_addresses(self.port, self.allow_loopback),
            },
            config: self.cfg.clone(),
            shut_down: self.shut_down,
        }
    }

    fn state_summary(&self, state: &State, caps: &BackendCaps) -> (StateLabel, String) {
        let name = |p: &Fingerprint| self.label_of(p);
        match state {
            State::Disconnected => (StateLabel::Disconnected, "Waiting for a paired computer.".into()),
            State::Local => {
                let n = self.sessions.len();
                (
                    StateLabel::Local,
                    if n == 1 {
                        format!("Sharing with {}", self.sessions.values().next().map(|s| name(&s.peer)).unwrap_or_default())
                    } else {
                        format!("Sharing with {n} computers")
                    },
                )
            }
            State::Pending { peer, .. } => (StateLabel::Transitioning, format!("Moving to {}…", name(peer))),
            State::RemoteActive { peer } => (StateLabel::Controlling, format!("Controlling {}", name(peer))),
            State::BeingControlled { peer } => (StateLabel::BeingControlled, format!("{} is controlling this computer", name(peer))),
            State::Suspended(Suspend::Paused) => (StateLabel::Paused, "Sharing paused".into()),
            State::Suspended(Suspend::Panic) => (StateLabel::Panic, "Stopped by the emergency shortcut".into()),
            State::Suspended(Suspend::Locked) => (StateLabel::Locked, "Paused while the screen is locked".into()),
            State::Suspended(Suspend::CaptureUnavailable) => (StateLabel::NeedsPermission, format!("Needs permission: {}", caps.capture.text())),
            State::Suspended(Suspend::Shutdown) => (StateLabel::Paused, "Shutting down".into()),
        }
    }
}

fn placeholder_snapshot(cfg: &Config, id: &Identity, port: u16, store: StoreKind) -> Snapshot {
    Snapshot {
        rev: 0,
        device: DeviceView {
            name: cfg.device_name.clone(),
            fingerprint: id.fingerprint(),
            platform: Platform::current(),
            listen_port: port,
            store,
            config_dir_label: String::new(),
        },
        sharing: SharingView {
            state: StateLabel::Paused,
            paused: true,
            clipboard_paused: cfg.clipboard_paused,
            stay_local: false,
            active_peer: None,
            panic_hotkey: cfg.input.panic_hotkey.clone(),
            switch_hotkey: cfg.input.switch_hotkey.clone(),
            panic_hotkey_live: false,
            summary: "Starting…".into(),
        },
        backend: BackendView {
            api: String::new(),
            capture: String::new(),
            capture_ok: false,
            inject: String::new(),
            inject_ok: false,
            notes: vec![],
            clipboard_ok: false,
        },
        peers: vec![],
        candidates: vec![],
        pairing: None,
        pairing_window_secs: None,
        transfers: vec![],
        layout: LayoutView { doc: cfg.layout.clone(), devices: vec![] },
        notices: vec![],
        network: NetworkView { discovery_on: false, discovery_error: None, has_lan: true, interfaces: vec![], addresses: vec![] },
        config: cfg.clone(),
        shut_down: false,
    }
}

/// How many addresses are remembered per peer.
const MAX_ENDPOINTS: usize = 8;

/// A fresh announcement is authoritative and arrives best-first: keep that order, then the older addresses (an
/// announcement can be partial). Inserting one by one at the front would have reversed it, putting the worst first.
fn merge_endpoints(known: &mut Vec<SocketAddr>, announced: &[SocketAddr], allow_loopback: bool) {
    let mut merged: Vec<SocketAddr> = announced.iter().copied().filter(|a| allow_loopback || !a.ip().is_loopback()).collect();
    let old: Vec<SocketAddr> = known.iter().copied().filter(|a| !merged.contains(a)).collect();
    merged.extend(old);
    merged.truncate(MAX_ENDPOINTS);
    *known = merged;
}

/// `±25 %` so devices that lost the network together do not redial in lockstep.
fn jitter(d: Duration) -> Duration {
    let f = 0.75 + rand::random::<f64>() * 0.5;
    d.mul_f64(f)
}

/// This computer's IPv4 addresses on the local network as `ip:port`, home-style networks first. Virtual, tunnel and
/// link-local interfaces are left out: nobody on the other computer could use them.
fn lan_addresses(port: u16, allow_loopback: bool) -> Vec<String> {
    let mut v: Vec<std::net::Ipv4Addr> = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|i| (allow_loopback || !i.is_loopback()) && !crate::discovery::is_tunnel_like(&i.name))
        .filter_map(|i| match i.addr.ip() {
            IpAddr::V4(a) if !a.is_link_local() && !a.is_unspecified() => Some(a),
            _ => None,
        })
        .collect();
    v.sort_by_key(|a| (!(a.octets()[0] == 192 && a.octets()[1] == 168), !a.is_private(), *a));
    v.dedup();
    v.truncate(3);
    v.into_iter().map(|a| format!("{a}:{port}")).collect()
}

fn lan_present(allow_loopback: bool) -> bool {
    if_addrs::get_if_addrs().unwrap_or_default().iter().any(|i| {
        (allow_loopback || !i.is_loopback())
            && !crate::discovery::is_tunnel_like(&i.name)
            && !i.addr.ip().is_unspecified()
            && !matches!(i.addr.ip(), IpAddr::V6(v6) if v6.segments()[0] & 0xffc0 == 0xfe80)
    })
}

/// `ip`, `ip:port`, `[v6]` or `[v6]:port`. Never resolves names, never scans.
pub fn parse_endpoint(text: &str, default_port: u16) -> Option<SocketAddr> {
    let t = text.trim();
    if let Ok(a) = t.parse::<SocketAddr>() {
        return Some(a);
    }
    let bare = t.trim_start_matches('[').trim_end_matches(']');
    bare.parse::<IpAddr>().ok().map(|ip| SocketAddr::new(ip, default_port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_announcement_keeps_its_best_first_order_and_older_addresses_follow() {
        let ip = |s: &str| SocketAddr::new(s.parse().unwrap(), 1);
        let mut known = vec![ip("192.168.1.9"), ip("192.168.1.30")];
        merge_endpoints(&mut known, &[ip("192.168.1.23"), ip("172.20.64.1"), ip("192.168.1.9"), ip("127.0.0.1")], false);
        assert_eq!(known, vec![ip("192.168.1.23"), ip("172.20.64.1"), ip("192.168.1.9"), ip("192.168.1.30")]);
        let many: Vec<SocketAddr> = (1..=20).map(|n| SocketAddr::from(([10, 0, 0, n], 1))).collect();
        merge_endpoints(&mut known, &many, false);
        assert_eq!(known.len(), MAX_ENDPOINTS);
        assert_eq!(known[0], many[0]);
    }

    #[test]
    fn manual_addresses_parse_without_name_resolution() {
        assert_eq!(parse_endpoint("192.168.1.20", 24847), Some("192.168.1.20:24847".parse().unwrap()));
        assert_eq!(parse_endpoint(" 192.168.1.20:9000 ", 1), Some("192.168.1.20:9000".parse().unwrap()));
        assert_eq!(parse_endpoint("[fe80::1]:7", 1).map(|a| a.port()), Some(7));
        assert_eq!(parse_endpoint("fe80::1", 5).map(|a| a.port()), Some(5));
        assert_eq!(parse_endpoint("example.com", 1), None);
        assert_eq!(parse_endpoint("", 1), None);
        assert_eq!(parse_endpoint("999.1.1.1", 1), None);
    }

    #[test]
    fn rate_limiter_blocks_bursts_and_recovers() {
        let mut l = RateLimiter::default();
        let ip: IpAddr = "10.0.0.5".parse().unwrap();
        let w = Duration::from_millis(60);
        for _ in 0..3 {
            assert!(l.allow(ip, 3, w));
        }
        assert!(!l.allow(ip, 3, w));
        assert!(l.allow("10.0.0.6".parse().unwrap(), 3, w), "limits are per address");
        std::thread::sleep(Duration::from_millis(80));
        assert!(l.allow(ip, 3, w));
    }

    #[test]
    fn jitter_stays_within_a_quarter() {
        for _ in 0..200 {
            let j = jitter(Duration::from_secs(8));
            assert!(j >= Duration::from_secs(6) && j <= Duration::from_secs(10));
        }
    }
}
