//! The control state machine: who currently owns the pointer and keyboard.
//!
//! Pure logic – no I/O, no clocks, no threads. The engine feeds it [`Ev`]ents
//! and executes the [`Effect`]s it returns, which makes every transition and
//! every safe-failure path unit-testable.
//!
//! ```text
//!             ┌──────────── Disconnected ◄── last peer lost
//!   peer up   ▼                                   ▲
//!          Local ──edge push──► Pending ──ack──► RemoteActive ──edge back──► Local
//!            │ ▲                   │ reject/timeout          │
//!            │ └───────────────────┘                         │ panic / pause / lock / loss
//!            │ peer Enter (we accept)                        ▼
//!            └──────────────► BeingControlled ──Leave──► Suspended ──resume──► Local
//! ```

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::time::{Duration, Instant};

use crate::geometry::{Desk, EdgeDefaults, Hit, Probe};
use crate::identity::Fingerprint;
use crate::keys::{self, Mapping, Mods, Translator};
use crate::proto::{Capabilities, InputEvent, MouseButton, Msg, Platform, RejectReason, ReleaseReason};

pub type PeerId = Fingerprint;

/// How long we wait for `EnterAck` before giving the pointer back.
pub const ENTER_TIMEOUT: Duration = Duration::from_secs(1);
/// Pointer events queued while an `Enter` is in flight.
const PENDING_QUEUE: usize = 256;
/// A pointer this far from the edge cancels a dwell (hysteresis, in points).
const DWELL_HYSTERESIS: f64 = 4.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Suspend {
    Paused,
    Panic,
    Locked,
    CaptureUnavailable,
    /// This app is quitting.
    Shutdown,
}

#[derive(Debug, Clone, PartialEq)]
pub enum State {
    /// Sharing is on but no peer can exchange input.
    Disconnected,
    /// This computer's own keyboard and mouse are in use here.
    Local,
    /// `Enter` sent; waiting for the peer to accept.
    Pending {
        peer: PeerId,
        seq: u64,
        since: Instant,
    },
    /// This computer's keyboard and mouse currently drive `peer`.
    RemoteActive {
        peer: PeerId,
    },
    /// `peer` is driving this computer.
    BeingControlled {
        peer: PeerId,
    },
    Suspended(Suspend),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Panic,
    Switch,
}

/// Raw local input as captured by a backend (device-local global points).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Raw {
    Pointer { x: f64, y: f64, dx: f64, dy: f64 },
    Button { button: MouseButton, down: bool },
    Scroll { dx: f64, dy: f64, pixels: bool },
    Key { usage: u16, down: bool, repeat: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notice {
    /// Control came back to this computer.
    ControlReturned(ReleaseReason),
    ConnectionLost,
    EntryRejected(RejectReason),
    EntryTimedOut,
    Suspended(Suspend),
    Resumed,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    Send {
        peer: PeerId,
        msg: Msg,
    },
    /// Forward one input event to the controlled peer (ordered with `Send`).
    SendInput {
        peer: PeerId,
        ev: InputEvent,
    },
    /// Suppress local delivery and freeze the cursor (true) or undo it (false).
    Grab(bool),
    /// Put the local cursor at a device-local global point.
    Warp {
        x: f64,
        y: f64,
    },
    /// Apply an event received from the controller to this computer.
    Inject(InputEvent),
    Notice(Notice),
}

/// What this computer knows and allows about one connected device.
#[derive(Debug, Clone)]
pub struct PeerCtl {
    pub platform: Platform,
    /// Capabilities negotiated with this peer.
    pub caps: Capabilities,
    /// This computer may drive the peer (permission + the peer's grant).
    pub we_may_control: bool,
    /// The peer may drive this computer.
    pub they_may_control: bool,
    pub remote_paused: bool,
    pub remote_locked: bool,
    /// `None` = automatic.
    pub translate: Option<bool>,
    pub invert_scroll: bool,
}

#[derive(Debug, Clone)]
pub struct ControlConfig {
    pub edge: EdgeDefaults,
    /// One of `Mods::*`.
    pub cross_modifier: u8,
    pub sensitivity: f64,
    pub stay_local: bool,
    pub shortcut_translation: bool,
    pub resume_after_lock: bool,
}

impl Default for ControlConfig {
    fn default() -> Self {
        Self {
            edge: EdgeDefaults::default(),
            cross_modifier: Mods::SHIFT,
            sensitivity: 1.0,
            stay_local: false,
            shortcut_translation: false,
            resume_after_lock: false,
        }
    }
}

#[derive(Debug)]
pub enum Ev {
    Raw(Raw),
    Hotkey(Action),
    PeerUp {
        peer: PeerId,
        ctl: PeerCtl,
    },
    PeerDown {
        peer: PeerId,
    },
    PeerChanged {
        peer: PeerId,
        ctl: PeerCtl,
    },
    /// The device layout or the local/remote displays changed.
    DeskChanged(Desk),
    ConfigChanged(ControlConfig),
    EnterAck {
        peer: PeerId,
        seq: u64,
        accepted: bool,
        reason: Option<RejectReason>,
    },
    RemoteEnter {
        peer: PeerId,
        seq: u64,
        display: u32,
        x: f32,
        y: f32,
        held: Vec<u16>,
    },
    RemoteLeave {
        peer: PeerId,
        reason: ReleaseReason,
    },
    RemoteInput {
        peer: PeerId,
        ev: InputEvent,
    },
    Pause(Suspend),
    Resume,
    /// Return the pointer to this computer ("Return control locally").
    ReturnLocal,
    Tick,
}

/// Tracks what *this computer's injector* currently holds down so a
/// disconnect, pause or panic can always release it.
#[derive(Debug, Default)]
pub struct HeldInput {
    keys: BTreeSet<u16>,
    buttons: BTreeSet<u8>,
}

pub fn button_index_pub(b: MouseButton) -> u8 {
    button_index(b)
}

fn button_index(b: MouseButton) -> u8 {
    match b {
        MouseButton::Left => 0,
        MouseButton::Right => 1,
        MouseButton::Middle => 2,
        MouseButton::Back => 3,
        MouseButton::Forward => 4,
        MouseButton::Other(n) => 5u8.saturating_add(n.min(200)),
    }
}

fn button_from_index(i: u8) -> MouseButton {
    match i {
        0 => MouseButton::Left,
        1 => MouseButton::Right,
        2 => MouseButton::Middle,
        3 => MouseButton::Back,
        4 => MouseButton::Forward,
        n => MouseButton::Other(n - 5),
    }
}

impl HeldInput {
    pub fn apply(&mut self, ev: &InputEvent) {
        match *ev {
            InputEvent::Key { usage, down, .. } => {
                if down {
                    self.keys.insert(usage);
                } else {
                    self.keys.remove(&usage);
                }
            }
            InputEvent::Button { button, down } => {
                let i = button_index(button);
                if down {
                    self.buttons.insert(i);
                } else {
                    self.buttons.remove(&i);
                }
            }
            _ => {}
        }
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty() && self.buttons.is_empty()
    }

    /// Releases for everything held: buttons first, then non-modifier keys,
    /// modifiers last.
    pub fn release_all(&mut self) -> Vec<InputEvent> {
        let mut out: Vec<InputEvent> = self.buttons.iter().map(|i| InputEvent::Button { button: button_from_index(*i), down: false }).collect();
        let (mods, rest): (Vec<u16>, Vec<u16>) = self.keys.iter().copied().partition(|k| keys::is_modifier(*k));
        out.extend(rest.into_iter().chain(mods).map(|usage| InputEvent::Key { usage, down: false, repeat: false }));
        self.keys.clear();
        self.buttons.clear();
        out
    }
}

struct Dwell {
    hit: Hit,
    since: Instant,
}

struct Remote {
    /// Layout tile and position of the virtual pointer.
    tile: usize,
    pos: (f64, f64),
    /// Where the local cursor goes when control returns (device-local points).
    ret: (f64, f64),
    translator: Option<Translator>,
}

pub struct Control {
    local: PeerId,
    local_platform: Platform,
    cfg: ControlConfig,
    base_desk: Desk,
    /// `base_desk` with devices we cannot currently reach disabled.
    desk: Desk,
    peers: HashMap<PeerId, PeerCtl>,
    state: State,
    seq: u64,
    sharing_enabled: bool,
    local_pos: (f64, f64),
    /// Physically held keys on this computer.
    held_local: BTreeSet<u16>,
    mods: u8,
    dwell: Option<Dwell>,
    remote: Option<Remote>,
    queued: VecDeque<Raw>,
    /// Destination side.
    injected: HeldInput,
    /// Peers in a stable order for the manual-switch hotkey.
    order: Vec<PeerId>,
}

impl Control {
    pub fn new(local: PeerId, local_platform: Platform, cfg: ControlConfig, desk: Desk) -> Self {
        let mut c = Self {
            local,
            local_platform,
            cfg,
            desk: desk.clone(),
            base_desk: desk,
            peers: HashMap::new(),
            state: State::Disconnected,
            seq: 0,
            sharing_enabled: true,
            local_pos: (0.0, 0.0),
            held_local: BTreeSet::new(),
            mods: 0,
            dwell: None,
            remote: None,
            queued: VecDeque::new(),
            injected: HeldInput::default(),
            order: vec![],
        };
        c.rebuild_desk();
        c
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    pub fn peer_in_control_of_us(&self) -> Option<PeerId> {
        match self.state {
            State::BeingControlled { peer } => Some(peer),
            _ => None,
        }
    }

    pub fn active_peer(&self) -> Option<PeerId> {
        match self.state {
            State::Pending { peer, .. } | State::RemoteActive { peer } | State::BeingControlled { peer } => Some(peer),
            _ => None,
        }
    }

    /// When the engine should call [`Ev::Tick`] next, if anything is waiting.
    pub fn next_deadline(&self) -> Option<Instant> {
        let pending = match self.state {
            State::Pending { since, .. } => Some(since + ENTER_TIMEOUT),
            _ => None,
        };
        let dwell = self.dwell.as_ref().map(|d| d.since + Duration::from_millis(d.hit.dwell_ms as u64));
        [pending, dwell].into_iter().flatten().min()
    }

    fn peers_ready(&self) -> bool {
        self.peers.values().any(|p| p.we_may_control || p.they_may_control)
    }

    fn controllable(&self, p: &PeerId) -> bool {
        self.peers.get(p).is_some_and(|c| c.we_may_control && !c.remote_paused && !c.remote_locked)
    }

    fn rebuild_desk(&mut self) {
        let mut d = self.base_desk.clone();
        for t in &mut d.tiles {
            if t.device != self.local && !self.controllable(&t.device) {
                t.enabled = false;
            }
        }
        self.desk = d;
    }

    fn mapping_for(&self, peer: &PeerId) -> Option<Mapping> {
        let c = self.peers.get(peer)?;
        let differs = self.local_platform.is_mac() != c.platform.is_mac();
        let on = c.translate.unwrap_or(self.cfg.shortcut_translation && differs);
        if !on || !differs {
            return None;
        }
        Some(if self.local_platform.is_mac() { Mapping::MacToPc } else { Mapping::PcToMac })
    }

    fn settle_idle_state(&mut self) {
        if matches!(self.state, State::Local | State::Disconnected) {
            self.state = if self.peers_ready() { State::Local } else { State::Disconnected };
        }
    }

    pub fn handle(&mut self, ev: Ev, now: Instant) -> Vec<Effect> {
        let mut fx = Vec::new();
        match ev {
            Ev::Raw(raw) => self.on_raw(raw, now, &mut fx),
            Ev::Hotkey(Action::Panic) => self.suspend(Suspend::Panic, &mut fx),
            Ev::Hotkey(Action::Switch) => self.on_switch(now, &mut fx),
            Ev::PeerUp { peer, ctl } => {
                if !self.order.contains(&peer) {
                    self.order.push(peer);
                }
                self.peers.insert(peer, ctl);
                self.rebuild_desk();
                self.settle_idle_state();
            }
            Ev::PeerChanged { peer, ctl } => {
                self.peers.insert(peer, ctl);
                self.rebuild_desk();
                self.recheck_active_peer(peer, &mut fx);
                self.settle_idle_state();
            }
            Ev::PeerDown { peer } => {
                let was_active = self.active_peer() == Some(peer);
                if was_active {
                    self.end_with(ReleaseReason::SessionLost, false, &mut fx);
                    fx.push(Effect::Notice(Notice::ConnectionLost));
                }
                self.peers.remove(&peer);
                self.order.retain(|p| *p != peer);
                self.rebuild_desk();
                self.settle_idle_state();
            }
            Ev::DeskChanged(d) => {
                self.base_desk = d;
                self.rebuild_desk();
                // The tile the virtual pointer is on may be gone.
                if matches!(self.state, State::RemoteActive { .. } | State::Pending { .. }) {
                    let stale = self.remote.as_ref().is_none_or(|r| r.tile >= self.desk.tiles.len() || !self.desk.tiles[r.tile].enabled);
                    if stale {
                        self.end_with(ReleaseReason::Manual, true, &mut fx);
                    }
                }
            }
            Ev::ConfigChanged(c) => {
                self.cfg = c;
                self.dwell = None;
                if self.cfg.stay_local && matches!(self.state, State::RemoteActive { .. } | State::Pending { .. }) {
                    self.end_with(ReleaseReason::Manual, true, &mut fx);
                }
            }
            Ev::EnterAck { peer, seq, accepted, reason } => self.on_enter_ack(peer, seq, accepted, reason, now, &mut fx),
            Ev::RemoteEnter { peer, seq, display, x, y, held } => self.on_remote_enter(peer, seq, display, x, y, held, &mut fx),
            Ev::RemoteLeave { peer, reason } => {
                if self.state == (State::BeingControlled { peer }) {
                    self.release_injected(&mut fx);
                    self.state = State::Local;
                    self.settle_idle_state();
                    fx.push(Effect::Notice(Notice::ControlReturned(reason)));
                } else if matches!(self.state, State::RemoteActive { peer: p } | State::Pending { peer: p, .. } if p == peer) {
                    // The peer asked to end control (locked, paused, panic on its side).
                    self.end_with(reason, true, &mut fx);
                    fx.push(Effect::Notice(Notice::ControlReturned(reason)));
                }
            }
            Ev::RemoteInput { peer, ev } => {
                if self.state == (State::BeingControlled { peer }) {
                    self.injected.apply(&ev);
                    fx.push(Effect::Inject(ev));
                }
            }
            Ev::Pause(why) => self.suspend(why, &mut fx),
            Ev::Resume => {
                if let State::Suspended(_) = self.state {
                    self.state = State::Local;
                    self.settle_idle_state();
                    fx.push(Effect::Notice(Notice::Resumed));
                }
            }
            Ev::ReturnLocal => {
                if matches!(self.state, State::RemoteActive { .. } | State::Pending { .. }) {
                    self.end_with(ReleaseReason::Manual, true, &mut fx);
                    fx.push(Effect::Notice(Notice::ControlReturned(ReleaseReason::Manual)));
                } else if let State::BeingControlled { peer } = self.state {
                    self.release_injected(&mut fx);
                    fx.push(Effect::Send { peer, msg: Msg::Leave { reason: ReleaseReason::Manual } });
                    self.state = State::Local;
                    self.settle_idle_state();
                    fx.push(Effect::Notice(Notice::ControlReturned(ReleaseReason::Manual)));
                }
            }
            Ev::Tick => self.on_tick(now, &mut fx),
        }
        fx
    }

    // ── source side ────────────────────────────────────────────────────────

    fn on_raw(&mut self, raw: Raw, now: Instant, fx: &mut Vec<Effect>) {
        // Track physical modifier state in every state.
        if let Raw::Key { usage, down, .. } = raw {
            if down {
                self.held_local.insert(usage);
            } else {
                self.held_local.remove(&usage);
            }
            self.mods = self.held_local.iter().fold(0, |m, k| m | Mods::bit_of(*k));
        }
        match self.state.clone() {
            State::Local => {
                if let Raw::Pointer { x, y, dx, dy } = raw {
                    self.local_pos = (x, y);
                    self.probe_edge((x, y), (dx, dy), now, fx);
                }
            }
            State::Pending { .. } => {
                if self.queued.len() < PENDING_QUEUE {
                    self.queued.push_back(raw);
                }
            }
            State::RemoteActive { peer } => self.route_remote(peer, raw, now, fx),
            State::BeingControlled { .. } => {
                if let Raw::Pointer { x, y, .. } = raw {
                    self.local_pos = (x, y);
                }
            }
            State::Disconnected | State::Suspended(_) => {
                if let Raw::Pointer { x, y, .. } = raw {
                    self.local_pos = (x, y);
                }
            }
        }
    }

    fn probe_edge(&mut self, p: (f64, f64), d: (f64, f64), now: Instant, fx: &mut Vec<Effect>) {
        if self.cfg.stay_local || !self.sharing_enabled {
            self.dwell = None;
            return;
        }
        let Some(off) = self.desk.device_offset(&self.local) else { return };
        let lp = (p.0 + off.0, p.1 + off.1);
        let Some(tile) = self.desk.tile_at(lp.0, lp.1) else { return };
        let edge = EdgeDefaults { zone: self.cfg.edge.zone, ..self.cfg.edge };
        match self.desk.probe(tile, lp, d, &edge) {
            Probe::Pushing(hit) => {
                if hit.require_modifier && self.mods & self.cfg.cross_modifier == 0 {
                    self.dwell = None;
                    return;
                }
                if hit.dwell_ms == 0 {
                    self.begin_cross(hit, now, fx);
                    return;
                }
                let same = self.dwell.as_ref().is_some_and(|dw| dw.hit.side == hit.side && dw.hit.to == hit.to);
                if !same {
                    self.dwell = Some(Dwell { hit, since: now });
                } else if let Some(dw) = &self.dwell
                    && now.duration_since(dw.since) >= Duration::from_millis(dw.hit.dwell_ms as u64)
                {
                    let hit = dw.hit;
                    self.begin_cross(hit, now, fx);
                }
            }
            _ => {
                if let Some(dw) = &self.dwell {
                    // Hysteresis: a small drift away from the edge keeps the dwell alive.
                    let r = self.desk.tiles[dw.hit.from].rect;
                    let dist = match dw.hit.side {
                        crate::geometry::Side::Right => r.right() - lp.0,
                        crate::geometry::Side::Left => lp.0 - r.x,
                        crate::geometry::Side::Bottom => r.bottom() - lp.1,
                        crate::geometry::Side::Top => lp.1 - r.y,
                    };
                    if dist > edge.zone + DWELL_HYSTERESIS {
                        self.dwell = None;
                    }
                }
            }
        }
    }

    fn begin_cross(&mut self, hit: Hit, now: Instant, fx: &mut Vec<Effect>) {
        self.dwell = None;
        let to = &self.desk.tiles[hit.to];
        let peer = to.device;
        if !self.controllable(&peer) {
            return;
        }
        self.seq += 1;
        let ret = self.local_pos;
        let held: Vec<u16> = self.held_local.iter().copied().filter(|k| keys::is_modifier(*k)).take(16).collect();
        fx.push(Effect::Grab(true));
        fx.push(Effect::Send {
            peer,
            msg: Msg::Enter { seq: self.seq, display: to.display.id, x: (hit.entry.0 - to.rect.x) as f32, y: (hit.entry.1 - to.rect.y) as f32, held },
        });
        self.remote = Some(Remote { tile: hit.to, pos: hit.entry, ret, translator: self.mapping_for(&peer).map(Translator::new) });
        self.queued.clear();
        self.state = State::Pending { peer, seq: self.seq, since: now };
    }

    fn on_enter_ack(&mut self, peer: PeerId, seq: u64, accepted: bool, reason: Option<RejectReason>, now: Instant, fx: &mut Vec<Effect>) {
        let State::Pending { peer: p, seq: s, .. } = self.state else {
            // A late accept after we already gave up: tell the peer to let go.
            if accepted {
                fx.push(Effect::Send { peer, msg: Msg::Leave { reason: ReleaseReason::Manual } });
            }
            return;
        };
        if p != peer || s != seq {
            return;
        }
        if !accepted {
            self.remote_failed(fx);
            fx.push(Effect::Notice(Notice::EntryRejected(reason.unwrap_or(RejectReason::NotAllowed))));
            return;
        }
        self.state = State::RemoteActive { peer };
        // The pointer already sits at the entry point on the peer; flush what
        // the user did in the meantime, in order.
        while let Some(raw) = self.queued.pop_front() {
            self.route_remote(peer, raw, now, fx);
        }
    }

    fn remote_failed(&mut self, fx: &mut Vec<Effect>) {
        let ret = self.remote.take().map(|r| r.ret);
        self.queued.clear();
        self.state = State::Local;
        fx.push(Effect::Grab(false));
        if let Some((x, y)) = ret {
            fx.push(Effect::Warp { x, y });
        }
        self.settle_idle_state();
    }

    fn route_remote(&mut self, peer: PeerId, raw: Raw, now: Instant, fx: &mut Vec<Effect>) {
        let invert = self.peers.get(&peer).is_some_and(|c| c.invert_scroll);
        match raw {
            Raw::Pointer { dx, dy, .. } => {
                let Some(r) = &mut self.remote else { return };
                let d = (dx * self.cfg.sensitivity, dy * self.cfg.sensitivity);
                let (tile, pos) = self.desk.step(r.tile, r.pos, d);
                if (tile, pos) == (r.tile, r.pos) {
                    return;
                }
                let device = self.desk.tiles[tile].device;
                if device == self.local {
                    // Walked back onto this computer: return control.
                    let off = self.desk.device_offset(&self.local).unwrap_or((0.0, 0.0));
                    let local_pos = (pos.0 - off.0, pos.1 - off.1);
                    self.flush_translator(peer, fx);
                    fx.push(Effect::Send { peer, msg: Msg::Leave { reason: ReleaseReason::EdgeReturn } });
                    fx.push(Effect::Grab(false));
                    fx.push(Effect::Warp { x: local_pos.0, y: local_pos.1 });
                    self.local_pos = local_pos;
                    self.remote = None;
                    self.state = State::Local;
                    self.settle_idle_state();
                    fx.push(Effect::Notice(Notice::ControlReturned(ReleaseReason::EdgeReturn)));
                } else if device != peer {
                    // Straight from one remote computer to another.
                    self.flush_translator(peer, fx);
                    fx.push(Effect::Send { peer, msg: Msg::Leave { reason: ReleaseReason::EdgeReturn } });
                    let t = &self.desk.tiles[tile];
                    self.seq += 1;
                    fx.push(Effect::Send {
                        peer: device,
                        msg: Msg::Enter { seq: self.seq, display: t.display.id, x: (pos.0 - t.rect.x) as f32, y: (pos.1 - t.rect.y) as f32, held: vec![] },
                    });
                    let translator = self.mapping_for(&device).map(Translator::new);
                    if let Some(r) = &mut self.remote {
                        r.tile = tile;
                        r.pos = pos;
                        r.translator = translator;
                    }
                    self.queued.clear();
                    self.state = State::Pending { peer: device, seq: self.seq, since: now };
                } else {
                    r.tile = tile;
                    r.pos = pos;
                    let t = &self.desk.tiles[tile];
                    fx.push(Effect::SendInput {
                        peer,
                        ev: InputEvent::PointerAbs { display: t.display.id, x: (pos.0 - t.rect.x) as f32, y: (pos.1 - t.rect.y) as f32 },
                    });
                }
            }
            Raw::Button { button, down } => self.forward(peer, InputEvent::Button { button, down }, fx),
            Raw::Scroll { dx, dy, pixels } => {
                let s = if invert { -1.0 } else { 1.0 };
                self.forward(peer, InputEvent::Scroll { dx: (dx * s) as f32, dy: (dy * s) as f32, pixels }, fx)
            }
            Raw::Key { usage, down, repeat } => self.forward(peer, InputEvent::Key { usage, down, repeat }, fx),
        }
    }

    fn forward(&mut self, peer: PeerId, ev: InputEvent, fx: &mut Vec<Effect>) {
        match self.remote.as_mut().and_then(|r| r.translator.as_mut()) {
            Some(t) => {
                let mut out = Vec::new();
                t.feed(ev, &mut out);
                fx.extend(out.into_iter().map(|ev| Effect::SendInput { peer, ev }));
            }
            None => fx.push(Effect::SendInput { peer, ev }),
        }
    }

    fn flush_translator(&mut self, peer: PeerId, fx: &mut Vec<Effect>) {
        if let Some(t) = self.remote.as_mut().and_then(|r| r.translator.as_mut()) {
            fx.extend(t.release_all().into_iter().map(|ev| Effect::SendInput { peer, ev }));
        }
    }

    /// Leave the remote computer and take the pointer back.
    fn end_with(&mut self, reason: ReleaseReason, notify_peer: bool, fx: &mut Vec<Effect>) {
        match self.state.clone() {
            State::RemoteActive { peer } | State::Pending { peer, .. } => {
                if notify_peer {
                    self.flush_translator(peer, fx);
                    fx.push(Effect::Send { peer, msg: Msg::Leave { reason } });
                }
                self.remote_failed(fx);
            }
            State::BeingControlled { peer } => {
                self.release_injected(fx);
                if notify_peer {
                    fx.push(Effect::Send { peer, msg: Msg::Leave { reason } });
                }
                self.state = State::Local;
                self.settle_idle_state();
            }
            _ => {}
        }
    }

    fn suspend(&mut self, why: Suspend, fx: &mut Vec<Effect>) {
        let reason = match why {
            Suspend::Panic => ReleaseReason::Panic,
            Suspend::Locked => ReleaseReason::Locked,
            Suspend::Paused => ReleaseReason::Paused,
            Suspend::CaptureUnavailable => ReleaseReason::PermissionRevoked,
            Suspend::Shutdown => ReleaseReason::Shutdown,
        };
        self.end_with(reason, true, fx);
        self.dwell = None;
        self.queued.clear();
        self.state = State::Suspended(why);
        fx.push(Effect::Notice(Notice::Suspended(why)));
    }

    fn on_switch(&mut self, now: Instant, fx: &mut Vec<Effect>) {
        match self.state.clone() {
            State::RemoteActive { .. } | State::Pending { .. } => {
                self.end_with(ReleaseReason::Manual, true, fx);
                fx.push(Effect::Notice(Notice::ControlReturned(ReleaseReason::Manual)));
            }
            State::Local if !self.cfg.stay_local => {
                // Next controllable peer in connection order.
                let target = self.order.iter().find(|p| self.controllable(p)).copied();
                let Some(peer) = target else { return };
                let Some((tile, center)) = self.desk.center_of_device(&peer) else { return };
                let t = &self.desk.tiles[tile];
                self.seq += 1;
                let held: Vec<u16> = self.held_local.iter().copied().filter(|k| keys::is_modifier(*k)).take(16).collect();
                fx.push(Effect::Grab(true));
                fx.push(Effect::Send {
                    peer,
                    msg: Msg::Enter { seq: self.seq, display: t.display.id, x: (center.0 - t.rect.x) as f32, y: (center.1 - t.rect.y) as f32, held },
                });
                self.remote = Some(Remote { tile, pos: center, ret: self.local_pos, translator: self.mapping_for(&peer).map(Translator::new) });
                self.queued.clear();
                self.state = State::Pending { peer, seq: self.seq, since: now };
            }
            _ => {}
        }
    }

    fn recheck_active_peer(&mut self, peer: PeerId, fx: &mut Vec<Effect>) {
        match self.state {
            State::RemoteActive { peer: p } | State::Pending { peer: p, .. } if p == peer && !self.controllable(&peer) => {
                let reason = if self.peers.get(&peer).is_some_and(|c| c.remote_locked) {
                    ReleaseReason::Locked
                } else if self.peers.get(&peer).is_some_and(|c| c.remote_paused) {
                    ReleaseReason::Paused
                } else {
                    ReleaseReason::PermissionRevoked
                };
                self.end_with(reason, true, fx);
                fx.push(Effect::Notice(Notice::ControlReturned(reason)));
            }
            State::BeingControlled { peer: p } if p == peer && !self.peers.get(&peer).is_some_and(|c| c.they_may_control) => {
                self.end_with(ReleaseReason::PermissionRevoked, true, fx);
                fx.push(Effect::Notice(Notice::ControlReturned(ReleaseReason::PermissionRevoked)));
            }
            _ => {}
        }
    }

    fn on_tick(&mut self, now: Instant, fx: &mut Vec<Effect>) {
        if let State::Pending { peer, since, .. } = self.state
            && now.duration_since(since) >= ENTER_TIMEOUT
        {
            // If the late ack arrives we answer it with a Leave.
            fx.push(Effect::Send { peer, msg: Msg::Leave { reason: ReleaseReason::Manual } });
            self.remote_failed(fx);
            fx.push(Effect::Notice(Notice::EntryTimedOut));
        }
        if let Some(dw) = &self.dwell
            && matches!(self.state, State::Local)
            && now.duration_since(dw.since) >= Duration::from_millis(dw.hit.dwell_ms as u64)
        {
            // Still holding at the edge after the dwell: cross.
            let hit = dw.hit;
            if self.mods & self.cfg.cross_modifier != 0 || !hit.require_modifier {
                self.begin_cross(hit, now, fx);
            } else {
                self.dwell = None;
            }
        }
    }

    // ── destination side ───────────────────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    fn on_remote_enter(&mut self, peer: PeerId, seq: u64, display: u32, x: f32, y: f32, held: Vec<u16>, fx: &mut Vec<Effect>) {
        let reject = |fx: &mut Vec<Effect>, reason| {
            fx.push(Effect::Send { peer, msg: Msg::EnterAck { seq, accepted: false, reason: Some(reason) } });
        };
        let allowed = self.peers.get(&peer).is_some_and(|c| c.they_may_control && c.caps.input_inject);
        match &self.state {
            State::Suspended(Suspend::Locked) => return reject(fx, RejectReason::Locked),
            State::Suspended(_) => return reject(fx, RejectReason::Paused),
            State::BeingControlled { peer: p } if *p != peer => return reject(fx, RejectReason::Busy),
            State::RemoteActive { .. } | State::Pending { .. } => return reject(fx, RejectReason::Busy),
            _ => {}
        }
        if !allowed {
            return reject(fx, RejectReason::NotAllowed);
        }
        // A same-peer re-enter replaces the previous control without a gap.
        if self.state == (State::BeingControlled { peer }) {
            self.release_injected(fx);
        }
        self.state = State::BeingControlled { peer };
        fx.push(Effect::Send { peer, msg: Msg::EnterAck { seq, accepted: true, reason: None } });
        let warp = InputEvent::PointerAbs { display, x, y };
        self.injected.apply(&warp);
        fx.push(Effect::Inject(warp));
        for usage in held.into_iter().filter(|k| keys::is_modifier(*k)) {
            let ev = InputEvent::Key { usage, down: true, repeat: false };
            self.injected.apply(&ev);
            fx.push(Effect::Inject(ev));
        }
    }

    fn release_injected(&mut self, fx: &mut Vec<Effect>) {
        for ev in self.injected.release_all() {
            fx.push(Effect::Inject(ev));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::LayoutDoc;
    use crate::proto::DisplayInfo;

    fn fp(n: u8) -> Fingerprint {
        Fingerprint([n; 32])
    }
    fn disp(w: u32, h: u32) -> DisplayInfo {
        DisplayInfo { id: 1, name: "D".into(), x: 0, y: 0, width: w, height: h, scale: 1.0, rotation: 0, primary: true }
    }
    fn ctl(platform: Platform) -> PeerCtl {
        PeerCtl {
            platform,
            caps: Capabilities { input_source: true, input_inject: true, clipboard_text: true, clipboard_image: true, files: true },
            we_may_control: true,
            they_may_control: true,
            remote_paused: false,
            remote_locked: false,
            translate: None,
            invert_scroll: false,
        }
    }
    /// A (local, 1440x900) | B (right, 1920x1080) | C (further right, 800x600)
    fn desk3() -> Desk {
        let mut doc = LayoutDoc::default();
        doc.set_placement(fp(1), 0, 0);
        doc.set_placement(fp(2), 1440, 0);
        doc.set_placement(fp(3), 3360, 0);
        Desk::build(&doc, &[(fp(1), vec![disp(1440, 900)]), (fp(2), vec![disp(1920, 1080)]), (fp(3), vec![disp(800, 600)])])
    }
    fn setup() -> (Control, Instant) {
        let mut c = Control::new(fp(1), Platform::MacOs, ControlConfig::default(), desk3());
        let now = Instant::now();
        c.handle(Ev::PeerUp { peer: fp(2), ctl: ctl(Platform::Windows) }, now);
        (c, now)
    }
    fn push_right(c: &mut Control, now: Instant) -> Vec<Effect> {
        c.handle(Ev::Raw(Raw::Pointer { x: 1439.0, y: 450.0, dx: 5.0, dy: 0.0 }), now)
    }
    fn enter_seq(fx: &[Effect]) -> u64 {
        fx.iter()
            .find_map(|e| match e {
                Effect::Send { msg: Msg::Enter { seq, .. }, .. } => Some(*seq),
                _ => None,
            })
            .expect("Enter was sent")
    }
    fn go_remote(c: &mut Control, now: Instant) {
        let fx = push_right(c, now);
        let seq = enter_seq(&fx);
        c.handle(Ev::EnterAck { peer: fp(2), seq, accepted: true, reason: None }, now);
        assert_eq!(*c.state(), State::RemoteActive { peer: fp(2) });
    }

    #[test]
    fn connects_then_crosses_the_edge_and_grabs_input() {
        let mut c = Control::new(fp(1), Platform::MacOs, ControlConfig::default(), desk3());
        let now = Instant::now();
        assert_eq!(*c.state(), State::Disconnected);
        c.handle(Ev::PeerUp { peer: fp(2), ctl: ctl(Platform::Windows) }, now);
        assert_eq!(*c.state(), State::Local);
        let fx = push_right(&mut c, now);
        assert!(matches!(c.state(), State::Pending { .. }));
        assert_eq!(fx[0], Effect::Grab(true));
        let Effect::Send { peer, msg: Msg::Enter { display, x, y, .. } } = &fx[1] else { panic!("{fx:?}") };
        assert_eq!((*peer, *display), (fp(2), 1));
        assert_eq!((*x, *y), (2.0, 450.0)); // entry inset, same height
    }

    #[test]
    fn ack_activates_and_pointer_motion_is_forwarded_in_remote_coordinates() {
        let (mut c, now) = setup();
        go_remote(&mut c, now);
        let fx = c.handle(Ev::Raw(Raw::Pointer { x: 0.0, y: 0.0, dx: 100.0, dy: 10.0 }), now);
        assert_eq!(fx, vec![Effect::SendInput { peer: fp(2), ev: InputEvent::PointerAbs { display: 1, x: 102.0, y: 460.0 } }]);
        let fx = c.handle(Ev::Raw(Raw::Key { usage: 4, down: true, repeat: false }), now);
        assert_eq!(fx, vec![Effect::SendInput { peer: fp(2), ev: InputEvent::Key { usage: 4, down: true, repeat: false } }]);
    }

    #[test]
    fn walking_back_over_the_boundary_returns_control_and_warps_home() {
        let (mut c, now) = setup();
        go_remote(&mut c, now);
        let fx = c.handle(Ev::Raw(Raw::Pointer { x: 0.0, y: 0.0, dx: -10.0, dy: 0.0 }), now);
        assert_eq!(*c.state(), State::Local);
        assert!(fx.contains(&Effect::Send { peer: fp(2), msg: Msg::Leave { reason: ReleaseReason::EdgeReturn } }));
        assert!(fx.contains(&Effect::Grab(false)));
        let Some(Effect::Warp { x, y }) = fx.iter().find(|e| matches!(e, Effect::Warp { .. })) else { panic!() };
        assert!(*x < 1440.0 && *x > 1425.0 && *y == 450.0, "lands just inside the local screen: {x},{y}");
    }

    #[test]
    fn rejected_entry_gives_the_pointer_back() {
        let (mut c, now) = setup();
        let seq = enter_seq(&push_right(&mut c, now));
        let fx = c.handle(Ev::EnterAck { peer: fp(2), seq, accepted: false, reason: Some(RejectReason::Locked) }, now);
        assert_eq!(*c.state(), State::Local);
        assert!(fx.contains(&Effect::Grab(false)));
        assert!(fx.contains(&Effect::Notice(Notice::EntryRejected(RejectReason::Locked))));
        assert!(fx.iter().any(|e| matches!(e, Effect::Warp { .. })));
    }

    #[test]
    fn unanswered_entry_times_out_and_a_late_accept_is_undone() {
        let (mut c, now) = setup();
        let seq = enter_seq(&push_right(&mut c, now));
        assert_eq!(c.next_deadline(), Some(now + ENTER_TIMEOUT));
        let fx = c.handle(Ev::Tick, now + ENTER_TIMEOUT + Duration::from_millis(1));
        assert_eq!(*c.state(), State::Local);
        assert!(fx.contains(&Effect::Notice(Notice::EntryTimedOut)));
        let late = c.handle(Ev::EnterAck { peer: fp(2), seq, accepted: true, reason: None }, now);
        assert_eq!(late, vec![Effect::Send { peer: fp(2), msg: Msg::Leave { reason: ReleaseReason::Manual } }]);
        assert_eq!(*c.state(), State::Local);
    }

    #[test]
    fn input_during_pending_is_kept_in_order_and_flushed_after_the_ack() {
        let (mut c, now) = setup();
        let seq = enter_seq(&push_right(&mut c, now));
        assert!(c.handle(Ev::Raw(Raw::Key { usage: 4, down: true, repeat: false }), now).is_empty());
        assert!(c.handle(Ev::Raw(Raw::Key { usage: 4, down: false, repeat: false }), now).is_empty());
        let fx = c.handle(Ev::EnterAck { peer: fp(2), seq, accepted: true, reason: None }, now);
        let sent: Vec<_> = fx.iter().filter_map(|e| if let Effect::SendInput { ev, .. } = e { Some(*ev) } else { None }).collect();
        assert_eq!(sent, vec![InputEvent::Key { usage: 4, down: true, repeat: false }, InputEvent::Key { usage: 4, down: false, repeat: false }]);
    }

    #[test]
    fn held_modifiers_are_handed_over_on_entry_and_nothing_else() {
        let (mut c, now) = setup();
        c.handle(Ev::Raw(Raw::Key { usage: keys::LSHIFT, down: true, repeat: false }), now);
        c.handle(Ev::Raw(Raw::Key { usage: 0x04, down: true, repeat: false }), now); // 'A' is not handed over
        let fx = push_right(&mut c, now);
        let Effect::Send { msg: Msg::Enter { held, .. }, .. } = &fx[1] else { panic!() };
        assert_eq!(held, &vec![keys::LSHIFT]);
    }

    #[test]
    fn panic_returns_control_releases_and_blocks_reentry_until_resumed() {
        let (mut c, now) = setup();
        go_remote(&mut c, now);
        let fx = c.handle(Ev::Hotkey(Action::Panic), now);
        assert_eq!(*c.state(), State::Suspended(Suspend::Panic));
        assert!(fx.contains(&Effect::Send { peer: fp(2), msg: Msg::Leave { reason: ReleaseReason::Panic } }));
        assert!(fx.contains(&Effect::Grab(false)));
        // Pushing against the edge now does nothing.
        assert!(push_right(&mut c, now).is_empty());
        assert_eq!(*c.state(), State::Suspended(Suspend::Panic));
        c.handle(Ev::Resume, now);
        assert_eq!(*c.state(), State::Local);
        assert!(matches!(push_right(&mut c, now)[0], Effect::Grab(true)));
    }

    #[test]
    fn losing_the_peer_mid_control_restores_local_control_with_a_notice() {
        let (mut c, now) = setup();
        go_remote(&mut c, now);
        let fx = c.handle(Ev::PeerDown { peer: fp(2) }, now);
        assert_eq!(*c.state(), State::Disconnected);
        assert!(fx.contains(&Effect::Grab(false)));
        assert!(fx.contains(&Effect::Notice(Notice::ConnectionLost)));
        // No Leave is sent into a dead session.
        assert!(!fx.iter().any(|e| matches!(e, Effect::Send { msg: Msg::Leave { .. }, .. })));
    }

    #[test]
    fn revoking_permission_mid_session_ends_control() {
        let (mut c, now) = setup();
        go_remote(&mut c, now);
        let mut revoked = ctl(Platform::Windows);
        revoked.we_may_control = false;
        let fx = c.handle(Ev::PeerChanged { peer: fp(2), ctl: revoked }, now);
        assert!(matches!(c.state(), State::Disconnected | State::Local));
        assert!(fx.contains(&Effect::Grab(false)));
        assert!(fx.contains(&Effect::Send { peer: fp(2), msg: Msg::Leave { reason: ReleaseReason::PermissionRevoked } }));
        // And the edge no longer leads anywhere.
        assert!(push_right(&mut c, now).is_empty());
    }

    #[test]
    fn locked_or_paused_peers_are_not_targets() {
        let (mut c, now) = setup();
        let mut locked = ctl(Platform::Windows);
        locked.remote_locked = true;
        c.handle(Ev::PeerChanged { peer: fp(2), ctl: locked }, now);
        assert!(push_right(&mut c, now).is_empty());
    }

    #[test]
    fn stay_on_this_computer_blocks_crossing_and_ends_active_control() {
        let (mut c, now) = setup();
        go_remote(&mut c, now);
        let cfg = ControlConfig { stay_local: true, ..ControlConfig::default() };
        let fx = c.handle(Ev::ConfigChanged(cfg), now);
        assert_eq!(*c.state(), State::Local);
        assert!(fx.contains(&Effect::Grab(false)));
        assert!(push_right(&mut c, now).is_empty());
    }

    #[test]
    fn modifier_requirement_and_dwell_are_honoured() {
        let mut doc = LayoutDoc::default();
        doc.set_placement(fp(1), 0, 0);
        doc.set_placement(fp(2), 1440, 0);
        let r = doc.rule_mut(fp(1), 1, crate::geometry::Side::Right);
        r.require_modifier = Some(true);
        r.dwell_ms = Some(200);
        let desk = Desk::build(&doc, &[(fp(1), vec![disp(1440, 900)]), (fp(2), vec![disp(1920, 1080)])]);
        let mut c = Control::new(fp(1), Platform::MacOs, ControlConfig::default(), desk);
        let t0 = Instant::now();
        c.handle(Ev::PeerUp { peer: fp(2), ctl: ctl(Platform::Windows) }, t0);
        // Without Shift nothing happens.
        assert!(push_right(&mut c, t0).is_empty());
        c.handle(Ev::Raw(Raw::Key { usage: keys::LSHIFT, down: true, repeat: false }), t0);
        // With Shift the dwell starts, and crossing waits for it.
        assert!(push_right(&mut c, t0).is_empty());
        assert!(push_right(&mut c, t0 + Duration::from_millis(100)).is_empty());
        assert_eq!(c.next_deadline(), Some(t0 + Duration::from_millis(200)));
        let fx = push_right(&mut c, t0 + Duration::from_millis(250));
        assert!(matches!(fx[0], Effect::Grab(true)));
    }

    #[test]
    fn dwell_resets_when_the_pointer_leaves_the_edge() {
        let mut doc = LayoutDoc::default();
        doc.set_placement(fp(1), 0, 0);
        doc.set_placement(fp(2), 1440, 0);
        doc.rule_mut(fp(1), 1, crate::geometry::Side::Right).dwell_ms = Some(200);
        let desk = Desk::build(&doc, &[(fp(1), vec![disp(1440, 900)]), (fp(2), vec![disp(1920, 1080)])]);
        let mut c = Control::new(fp(1), Platform::MacOs, ControlConfig::default(), desk);
        let t0 = Instant::now();
        c.handle(Ev::PeerUp { peer: fp(2), ctl: ctl(Platform::Windows) }, t0);
        push_right(&mut c, t0);
        assert!(c.next_deadline().is_some());
        c.handle(Ev::Raw(Raw::Pointer { x: 1000.0, y: 450.0, dx: -400.0, dy: 0.0 }), t0 + Duration::from_millis(50));
        assert_eq!(c.next_deadline(), None, "moving well away cancels the dwell");
        assert!(push_right(&mut c, t0 + Duration::from_millis(300)).is_empty(), "a fresh dwell must start over");
    }

    #[test]
    fn manual_switch_goes_to_the_peer_and_back() {
        let (mut c, now) = setup();
        let fx = c.handle(Ev::Hotkey(Action::Switch), now);
        assert!(matches!(c.state(), State::Pending { .. }));
        let seq = enter_seq(&fx);
        c.handle(Ev::EnterAck { peer: fp(2), seq, accepted: true, reason: None }, now);
        assert_eq!(*c.state(), State::RemoteActive { peer: fp(2) });
        let fx = c.handle(Ev::Hotkey(Action::Switch), now);
        assert_eq!(*c.state(), State::Local);
        assert!(fx.contains(&Effect::Grab(false)));
    }

    #[test]
    fn pointer_can_pass_through_one_remote_into_the_next() {
        let mut c = Control::new(fp(1), Platform::MacOs, ControlConfig::default(), desk3());
        let now = Instant::now();
        c.handle(Ev::PeerUp { peer: fp(2), ctl: ctl(Platform::Windows) }, now);
        c.handle(Ev::PeerUp { peer: fp(3), ctl: ctl(Platform::LinuxX11) }, now);
        go_remote(&mut c, now);
        // Fling across B (1920 wide) into C.
        let fx = c.handle(Ev::Raw(Raw::Pointer { x: 0.0, y: 0.0, dx: 1950.0, dy: 0.0 }), now);
        assert!(fx.contains(&Effect::Send { peer: fp(2), msg: Msg::Leave { reason: ReleaseReason::EdgeReturn } }));
        assert!(fx.iter().any(|e| matches!(e, Effect::Send { peer, msg: Msg::Enter { .. } } if *peer == fp(3))));
        assert!(!fx.contains(&Effect::Grab(false)), "stays grabbed while hopping between remotes");
        assert!(matches!(c.state(), State::Pending { peer, .. } if *peer == fp(3)));
    }

    #[test]
    fn pointer_cannot_cross_to_an_unreachable_or_gapped_device() {
        // C is not connected: the fling stops at B's far edge.
        let (mut c, now) = setup();
        go_remote(&mut c, now);
        c.handle(Ev::Raw(Raw::Pointer { x: 0.0, y: 0.0, dx: 5000.0, dy: 0.0 }), now);
        assert_eq!(*c.state(), State::RemoteActive { peer: fp(2) });
    }

    #[test]
    fn scroll_inversion_and_sensitivity_apply_per_peer() {
        let (mut c, now) = setup();
        let mut inv = ctl(Platform::Windows);
        inv.invert_scroll = true;
        c.handle(Ev::PeerChanged { peer: fp(2), ctl: inv }, now);
        go_remote(&mut c, now);
        let fx = c.handle(Ev::Raw(Raw::Scroll { dx: 0.0, dy: 3.0, pixels: false }), now);
        assert_eq!(fx, vec![Effect::SendInput { peer: fp(2), ev: InputEvent::Scroll { dx: -0.0, dy: -3.0, pixels: false } }]);
        c.handle(Ev::ConfigChanged(ControlConfig { sensitivity: 2.0, ..ControlConfig::default() }), now);
        let fx = c.handle(Ev::Raw(Raw::Pointer { x: 0.0, y: 0.0, dx: 10.0, dy: 0.0 }), now);
        let Effect::SendInput { ev: InputEvent::PointerAbs { x, .. }, .. } = &fx[0] else { panic!("{fx:?}") };
        assert_eq!(*x, 22.0); // 2 inset + 20
    }

    #[test]
    fn shortcut_translation_applies_only_across_platform_families_when_enabled() {
        let cfg = ControlConfig { shortcut_translation: true, ..ControlConfig::default() };
        let mut c = Control::new(fp(1), Platform::MacOs, cfg, desk3());
        let now = Instant::now();
        c.handle(Ev::PeerUp { peer: fp(2), ctl: ctl(Platform::Windows) }, now);
        go_remote(&mut c, now);
        let mut keys_sent = vec![];
        for (u, d) in [(keys::LMETA, true), (0x06, true), (0x06, false), (keys::LMETA, false)] {
            for e in c.handle(Ev::Raw(Raw::Key { usage: u, down: d, repeat: false }), now) {
                if let Effect::SendInput { ev: InputEvent::Key { usage, down, .. }, .. } = e {
                    keys_sent.push((usage, down));
                }
            }
        }
        assert_eq!(keys_sent, vec![(keys::LCTRL, true), (0x06, true), (0x06, false), (keys::LCTRL, false)]);
        // Same family ⇒ untouched.
        let mut c = Control::new(fp(1), Platform::MacOs, ControlConfig { shortcut_translation: true, ..ControlConfig::default() }, desk3());
        c.handle(Ev::PeerUp { peer: fp(2), ctl: ctl(Platform::MacOs) }, now);
        go_remote(&mut c, now);
        let fx = c.handle(Ev::Raw(Raw::Key { usage: keys::LMETA, down: true, repeat: false }), now);
        assert_eq!(fx, vec![Effect::SendInput { peer: fp(2), ev: InputEvent::Key { usage: keys::LMETA, down: true, repeat: false } }]);
    }

    // ── destination ────────────────────────────────────────────────────────

    fn dest() -> (Control, Instant) {
        // We are device 2 (Windows); device 1 may control us.
        let mut c = Control::new(fp(2), Platform::Windows, ControlConfig::default(), desk3());
        let now = Instant::now();
        c.handle(Ev::PeerUp { peer: fp(1), ctl: ctl(Platform::MacOs) }, now);
        (c, now)
    }
    fn enter_dest(c: &mut Control, now: Instant) -> Vec<Effect> {
        c.handle(Ev::RemoteEnter { peer: fp(1), seq: 7, display: 1, x: 2.0, y: 450.0, held: vec![keys::LSHIFT, 0x04] }, now)
    }

    #[test]
    fn destination_accepts_warps_pointer_and_applies_held_modifiers_only() {
        let (mut c, now) = dest();
        let fx = enter_dest(&mut c, now);
        assert_eq!(*c.state(), State::BeingControlled { peer: fp(1) });
        assert_eq!(fx[0], Effect::Send { peer: fp(1), msg: Msg::EnterAck { seq: 7, accepted: true, reason: None } });
        assert_eq!(fx[1], Effect::Inject(InputEvent::PointerAbs { display: 1, x: 2.0, y: 450.0 }));
        assert_eq!(fx[2], Effect::Inject(InputEvent::Key { usage: keys::LSHIFT, down: true, repeat: false }));
        assert_eq!(fx.len(), 3, "a non-modifier 'held' key must not be injected");
    }

    #[test]
    fn leaving_releases_every_injected_key_and_button() {
        let (mut c, now) = dest();
        enter_dest(&mut c, now);
        c.handle(Ev::RemoteInput { peer: fp(1), ev: InputEvent::Key { usage: 0x04, down: true, repeat: false } }, now);
        c.handle(Ev::RemoteInput { peer: fp(1), ev: InputEvent::Button { button: MouseButton::Left, down: true } }, now);
        let fx = c.handle(Ev::RemoteLeave { peer: fp(1), reason: ReleaseReason::EdgeReturn }, now);
        let released: Vec<_> = fx.iter().filter_map(|e| if let Effect::Inject(ev) = e { Some(*ev) } else { None }).collect();
        assert_eq!(
            released,
            vec![
                InputEvent::Button { button: MouseButton::Left, down: false },
                InputEvent::Key { usage: 0x04, down: false, repeat: false },
                InputEvent::Key { usage: keys::LSHIFT, down: false, repeat: false },
            ]
        );
        assert_eq!(*c.state(), State::Local);
    }

    #[test]
    fn session_loss_while_controlled_releases_held_input() {
        let (mut c, now) = dest();
        enter_dest(&mut c, now);
        c.handle(Ev::RemoteInput { peer: fp(1), ev: InputEvent::Key { usage: 0x06, down: true, repeat: false } }, now);
        let fx = c.handle(Ev::PeerDown { peer: fp(1) }, now);
        assert!(fx.contains(&Effect::Inject(InputEvent::Key { usage: 0x06, down: false, repeat: false })));
        assert!(fx.contains(&Effect::Inject(InputEvent::Key { usage: keys::LSHIFT, down: false, repeat: false })));
        assert_eq!(*c.state(), State::Disconnected);
    }

    #[test]
    fn local_panic_while_controlled_reclaims_the_machine_and_releases_input() {
        let (mut c, now) = dest();
        enter_dest(&mut c, now);
        c.handle(Ev::RemoteInput { peer: fp(1), ev: InputEvent::Key { usage: 0x06, down: true, repeat: false } }, now);
        let fx = c.handle(Ev::Hotkey(Action::Panic), now);
        assert!(fx.contains(&Effect::Send { peer: fp(1), msg: Msg::Leave { reason: ReleaseReason::Panic } }));
        assert!(fx.contains(&Effect::Inject(InputEvent::Key { usage: 0x06, down: false, repeat: false })));
        assert_eq!(*c.state(), State::Suspended(Suspend::Panic));
        // Anything the controller still sends is ignored.
        assert!(c.handle(Ev::RemoteInput { peer: fp(1), ev: InputEvent::Key { usage: 0x07, down: true, repeat: false } }, now).is_empty());
    }

    #[test]
    fn destination_refuses_when_not_permitted_busy_locked_or_paused() {
        // Not permitted.
        let mut c = Control::new(fp(2), Platform::Windows, ControlConfig::default(), desk3());
        let now = Instant::now();
        let mut deny = ctl(Platform::MacOs);
        deny.they_may_control = false;
        c.handle(Ev::PeerUp { peer: fp(1), ctl: deny }, now);
        let fx = enter_dest(&mut c, now);
        assert_eq!(fx, vec![Effect::Send { peer: fp(1), msg: Msg::EnterAck { seq: 7, accepted: false, reason: Some(RejectReason::NotAllowed) } }]);
        // Unknown peer.
        let fx = c.handle(Ev::RemoteEnter { peer: fp(9), seq: 1, display: 1, x: 0.0, y: 0.0, held: vec![] }, now);
        assert!(matches!(fx[0], Effect::Send { msg: Msg::EnterAck { accepted: false, .. }, .. }));
        // Busy: a second controller while one is active.
        let (mut c, now) = dest();
        c.handle(Ev::PeerUp { peer: fp(3), ctl: ctl(Platform::LinuxX11) }, now);
        enter_dest(&mut c, now);
        let fx = c.handle(Ev::RemoteEnter { peer: fp(3), seq: 2, display: 1, x: 0.0, y: 0.0, held: vec![] }, now);
        assert_eq!(fx, vec![Effect::Send { peer: fp(3), msg: Msg::EnterAck { seq: 2, accepted: false, reason: Some(RejectReason::Busy) } }]);
        assert_eq!(*c.state(), State::BeingControlled { peer: fp(1) });
        // Locked / paused.
        let (mut c, now) = dest();
        c.handle(Ev::Pause(Suspend::Locked), now);
        let fx = enter_dest(&mut c, now);
        assert!(matches!(fx[0], Effect::Send { msg: Msg::EnterAck { reason: Some(RejectReason::Locked), .. }, .. }));
        let (mut c, now) = dest();
        c.handle(Ev::Pause(Suspend::Paused), now);
        let fx = enter_dest(&mut c, now);
        assert!(matches!(fx[0], Effect::Send { msg: Msg::EnterAck { reason: Some(RejectReason::Paused), .. }, .. }));
    }

    #[test]
    fn revoking_the_controllers_permission_releases_input_immediately() {
        let (mut c, now) = dest();
        enter_dest(&mut c, now);
        let mut revoked = ctl(Platform::MacOs);
        revoked.they_may_control = false;
        let fx = c.handle(Ev::PeerChanged { peer: fp(1), ctl: revoked }, now);
        assert!(fx.contains(&Effect::Inject(InputEvent::Key { usage: keys::LSHIFT, down: false, repeat: false })));
        assert!(fx.contains(&Effect::Send { peer: fp(1), msg: Msg::Leave { reason: ReleaseReason::PermissionRevoked } }));
    }

    #[test]
    fn held_input_release_order_is_buttons_then_keys_then_modifiers() {
        let mut h = HeldInput::default();
        for ev in [
            InputEvent::Key { usage: keys::LCTRL, down: true, repeat: false },
            InputEvent::Key { usage: 0x06, down: true, repeat: false },
            InputEvent::Button { button: MouseButton::Right, down: true },
            InputEvent::Button { button: MouseButton::Other(2), down: true },
        ] {
            h.apply(&ev);
        }
        let out = h.release_all();
        assert!(h.is_empty());
        assert!(matches!(out[0], InputEvent::Button { .. }) && matches!(out[1], InputEvent::Button { button: MouseButton::Other(2), .. }));
        assert_eq!(out[2], InputEvent::Key { usage: 0x06, down: false, repeat: false });
        assert_eq!(out[3], InputEvent::Key { usage: keys::LCTRL, down: false, repeat: false });
    }

    #[test]
    fn repeated_key_downs_do_not_create_extra_holds() {
        let mut h = HeldInput::default();
        for _ in 0..5 {
            h.apply(&InputEvent::Key { usage: 4, down: true, repeat: true });
        }
        assert_eq!(h.release_all().len(), 1);
    }
}
