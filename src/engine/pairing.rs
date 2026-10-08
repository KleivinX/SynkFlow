//! Pairing: turning a stranger into an approved device.
//!
//! Discovery and a click only *start* a conversation on the pairing channel
//! (which grants nothing). Trust is created only when **both** people have
//! compared the full fingerprints and approved, and the approval is recorded
//! only after the other side's approval has also arrived.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use super::*;
use crate::config::TrustedPeer;
use crate::session::{PairChannels, PairEnd, spawn_pairing};

struct PeerHello {
    name: String,
    platform: Platform,
    listen_port: u16,
}

struct Pairing {
    id: u64,
    initiator: bool,
    addr: SocketAddr,
    peer_fp: Option<Fingerprint>,
    hello: Option<PeerHello>,
    local: Option<bool>,
    remote: Option<bool>,
    chosen: Option<(String, PeerPerms)>,
    deadline: Instant,
    ch: Option<PairChannels>,
    stage: PairStage,
    error: Option<PairError>,
    finished_at: Option<Instant>,
}

impl Pairing {
    fn active(&self) -> bool {
        !matches!(self.stage, PairStage::Done | PairStage::Failed)
    }
}

#[derive(Default)]
pub(super) struct PairingState {
    window_until: Option<Instant>,
    cur: Option<Pairing>,
    next_id: u64,
}

impl From<DialError> for PairError {
    fn from(e: DialError) -> Self {
        match e {
            DialError::Unreachable => PairError::Unreachable,
            DialError::VersionMismatch(_) => PairError::VersionMismatch,
            _ => PairError::Protocol,
        }
    }
}

impl Core {
    pub(super) fn open_pairing_window(&mut self) {
        self.pairing.window_until = Some(Instant::now() + PAIRING_TTL);
        self.dirty = true;
    }

    pub(super) fn close_pairing(&mut self) {
        self.pairing.window_until = None;
        if let Some(p) = &self.pairing.cur
            && p.active()
        {
            self.fail_pairing(PairError::RejectedHere, false);
        }
        self.pairing.cur = None;
        self.dirty = true;
    }

    pub(super) fn pairing_window_secs(&self) -> Option<u32> {
        self.pairing.window_until.map(|t| t.saturating_duration_since(Instant::now()).as_secs() as u32).filter(|s| *s > 0)
    }

    fn pair_events(&self, id: u64) -> mpsc::Sender<PairEvent> {
        let (tx, mut rx) = mpsc::channel::<PairEvent>(16);
        let core = self.tx.clone();
        tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                if core.send(CoreMsg::Pair(id, ev)).await.is_err() {
                    break;
                }
            }
        });
        tx
    }

    fn my_pair_hello(&self) -> Msg {
        Msg::PairHello {
            name: self.cfg.device_name.clone(),
            platform: Platform::current(),
            app_version: env!("CARGO_PKG_VERSION").into(),
            listen_port: self.port,
        }
    }

    fn new_pairing(&mut self, initiator: bool, addr: SocketAddr, peer_fp: Option<Fingerprint>) -> u64 {
        self.pairing.next_id += 1;
        let id = self.pairing.next_id;
        self.pairing.cur = Some(Pairing {
            id,
            initiator,
            addr,
            peer_fp,
            hello: None,
            local: None,
            remote: None,
            chosen: None,
            deadline: Instant::now() + PAIRING_TTL,
            ch: None,
            stage: PairStage::Connecting,
            error: None,
            finished_at: None,
        });
        self.dirty = true;
        id
    }

    /// The user picked a nearby device or typed an address.
    pub(super) fn begin_pairing(&mut self, addr: SocketAddr) {
        if self.pairing.cur.as_ref().is_some_and(|p| p.active()) {
            self.notice(Level::Warn, "A pairing is already in progress.", "Finish or cancel it first. Nothing else changed.", None);
            return;
        }
        if addr.ip().is_loopback() && !self.allow_loopback {
            self.notice(Level::Warn, "That address is this computer.", "Enter the address of the other computer.", None);
            return;
        }
        let id = self.new_pairing(true, addr, None);
        // The one picked is tried first; the other addresses the same computer announced are tried a beat later.
        let mut addrs = vec![addr];
        if let Some(c) = self.candidates.values().find(|c| c.addrs.contains(&addr)) {
            addrs.extend(c.addrs.iter().copied().filter(|a| *a != addr && (self.allow_loopback || !a.ip().is_loopback())));
        }
        let (identity, tx) = (self.identity.clone(), self.tx.clone());
        tokio::spawn(async move {
            let outcome = crate::session::race(&addrs, DIAL_STAGGER, |a| {
                let identity = identity.clone();
                async move {
                    let work = async {
                        let (mut stream, fp) = crate::tls::connect_pairing(a, &identity).await?;
                        proto::exchange_preamble(&mut stream).await.map_err(DialError::from)?;
                        Ok::<_, DialError>((proto::control_wire(stream), fp))
                    };
                    tokio::time::timeout(HANDSHAKE_TIMEOUT, work).await.unwrap_or(Err(DialError::Unreachable))
                }
            })
            .await;
            let (addr, result) = match outcome {
                Ok((a, r)) => (a, Ok(r)),
                Err(e) => {
                    tracing::warn!("could not start pairing at {addrs:?}: {e:?}");
                    (addr, Err(e))
                }
            };
            let _ = tx.send(CoreMsg::PairDialDone { id, addr, result: Box::new(result) }).await;
        });
    }

    pub(super) fn on_pair_dial_done(&mut self, id: u64, addr: SocketAddr, result: Result<(Wire<ClientStream>, Fingerprint), DialError>) {
        if self.pairing.cur.as_ref().map(|p| p.id) != Some(id) {
            return;
        }
        match result {
            Err(e) => self.fail_pairing(e.into(), true),
            Ok((wire, fp)) => {
                if self.trust.contains(&fp) {
                    self.fail_pairing(PairError::AlreadyPaired, true);
                    return;
                }
                let hello = self.my_pair_hello();
                let ttl = self.pairing.cur.as_ref().map(|p| p.deadline.saturating_duration_since(Instant::now())).unwrap_or(PAIRING_TTL);
                let ch = spawn_pairing(wire, hello, self.pair_events(id), ttl);
                if let Some(p) = &mut self.pairing.cur {
                    p.addr = addr; // the address that answered becomes the approved device's endpoint
                    p.peer_fp = Some(fp);
                    p.ch = Some(ch);
                }
            }
        }
        self.dirty = true;
    }

    pub(super) fn on_incoming_pairing(&mut self, wire: Wire<ServerStream>, peer: Fingerprint, addr: SocketAddr) {
        let open = self.pairing.window_until.is_some_and(|t| t > Instant::now());
        if !open || self.pairing.cur.as_ref().is_some_and(|p| p.active()) || self.trust.contains(&peer) {
            // Not invited (or busy): close without saying anything useful to a stranger.
            tracing::debug!("pairing request from {addr} ignored (window closed, busy, or already paired)");
            return;
        }
        self.pairing.window_until = None;
        let id = self.new_pairing(false, addr, Some(peer));
        let hello = self.my_pair_hello();
        let ch = spawn_pairing(wire, hello, self.pair_events(id), PAIRING_TTL);
        if let Some(p) = &mut self.pairing.cur {
            p.ch = Some(ch);
        }
    }

    pub(super) fn on_pair_event(&mut self, id: u64, ev: PairEvent) {
        if self.pairing.cur.as_ref().map(|p| p.id) != Some(id) {
            return;
        }
        self.dirty = true;
        match ev {
            PairEvent::Hello { name, platform, listen_port, .. } => {
                let resembles = self
                    .cfg
                    .peers
                    .iter()
                    .find(|p| p.announced_name.eq_ignore_ascii_case(&name) || p.label.eq_ignore_ascii_case(&name))
                    .map(|p| p.label.clone());
                if let Some(p) = &mut self.pairing.cur {
                    p.hello = Some(PeerHello { name: name.clone(), platform, listen_port });
                    if p.active() {
                        p.stage = PairStage::Verify;
                    }
                }
                if let Some(l) = resembles {
                    self.notice(
                        Level::Warn,
                        &format!("\"{name}\" has the same name as {l}, but a different identity."),
                        "Names are not proof of identity. Only approve if the fingerprints match what is shown on that computer; nothing is trusted yet.",
                        None,
                    );
                }
            }
            PairEvent::Decision(approved) => {
                if let Some(p) = &mut self.pairing.cur {
                    p.remote = Some(approved);
                }
                if approved {
                    self.maybe_complete_pairing();
                } else {
                    self.fail_pairing(PairError::RejectedByPeer, true);
                }
            }
            PairEvent::Ended(end) => {
                let Some(p) = &self.pairing.cur else { return };
                if !p.active() {
                    return;
                }
                let err = match end {
                    PairEnd::Cancelled => return,
                    PairEnd::Expired => PairError::Expired,
                    // Connected, then dropped before saying hello: the other side was not
                    // expecting a pairing request.
                    PairEnd::Closed | PairEnd::Lost if p.hello.is_none() => PairError::NotInvited,
                    PairEnd::Lost => PairError::Unreachable,
                    PairEnd::Protocol => PairError::Protocol,
                    PairEnd::Closed => PairError::RejectedByPeer,
                };
                self.fail_pairing(err, true);
            }
        }
    }

    /// This person compared the fingerprints and approved.
    pub(super) fn approve_pairing(&mut self, label: String, perms: PeerPerms) {
        let Some(p) = &mut self.pairing.cur else { return };
        if p.stage != PairStage::Verify || p.hello.is_none() {
            return;
        }
        let label = proto::clean_text(&label, MAX_NAME_BYTES);
        let label = if label.is_empty() { p.hello.as_ref().map(|h| h.name.clone()).unwrap_or_default() } else { label };
        p.chosen = Some((label, perms));
        p.local = Some(true);
        p.stage = PairStage::WaitingForPeer;
        if let Some(ch) = &p.ch {
            let _ = ch.tx.try_send(Msg::PairDecision { approved: true });
        }
        self.maybe_complete_pairing();
    }

    pub(super) fn reject_pairing(&mut self) {
        let Some(p) = &mut self.pairing.cur else { return };
        if !p.active() {
            return;
        }
        if let Some(ch) = &p.ch {
            let _ = ch.tx.try_send(Msg::PairDecision { approved: false });
            let _ = ch.tx.try_send(Msg::Bye(ReleaseReason::Manual));
        }
        self.fail_pairing(PairError::RejectedHere, false);
    }

    fn maybe_complete_pairing(&mut self) {
        let ready = self.pairing.cur.as_ref().is_some_and(|p| p.local == Some(true) && p.remote == Some(true) && p.active());
        if !ready {
            return;
        }
        let Some(p) = self.pairing.cur.as_mut() else { return };
        let (Some(fp), Some(h), Some((label, perms))) = (p.peer_fp, p.hello.as_ref(), p.chosen.clone()) else { return };
        // Where the other computer listens: its own announcement wins over the
        // ephemeral port it connected from.
        let endpoint = if p.initiator { p.addr } else { SocketAddr::new(p.addr.ip(), h.listen_port) };
        let (name, platform) = (h.name.clone(), h.platform);
        p.stage = PairStage::Done;
        p.finished_at = Some(Instant::now());
        if let Some(ch) = &p.ch {
            let _ = ch.tx.try_send(Msg::Bye(ReleaseReason::Manual));
        }

        self.trust.insert(fp);
        self.cfg.peers.retain(|x| x.fingerprint != fp);
        self.cfg.peers.push(TrustedPeer {
            fingerprint: fp,
            label: label.clone(),
            announced_name: name,
            platform,
            paired_at: now_secs(),
            last_connected: None,
            last_endpoint: Some(endpoint.to_string()),
            perms,
            displays: vec![],
        });
        self.save();
        let rt = self.peers.entry(fp).or_default();
        rt.endpoints.insert(0, endpoint);
        rt.hold = false;
        rt.next_try = None;
        rt.error = None;
        self.rebuild_desk();
        self.notice(
            Level::Info,
            &format!("Paired with {label}."),
            "Only the keyboard, clipboard and file permissions you chose are on. You can change them in Devices.",
            None,
        );
        self.dial_due(Instant::now());
        self.dirty = true;
    }

    fn fail_pairing(&mut self, err: PairError, tell_user: bool) {
        let Some(p) = &mut self.pairing.cur else { return };
        if !p.active() {
            return;
        }
        p.stage = PairStage::Failed;
        p.error = Some(err);
        p.finished_at = Some(Instant::now());
        if let Some(ch) = p.ch.take() {
            ch.cancel.cancel();
        }
        self.dirty = true;
        if !tell_user {
            return;
        }
        let (what, safe, action): (&str, &str, Option<(NoticeAction, String)>) = match err {
            PairError::Expired => (
                "The pairing request expired.",
                "Nothing was trusted. Start again when both computers are ready.",
                Some((NoticeAction::OpenPairing, "Try again".into())),
            ),
            PairError::RejectedByPeer => ("The other computer did not approve the pairing.", "Nothing was trusted and nothing was shared.", None),
            PairError::RejectedHere => return,
            PairError::Unreachable => (
                "No computer answered at that address.",
                "Nothing was sent. Check that Synkflow is open there, that you are on the same network, and that its firewall allows Synkflow.",
                Some((NoticeAction::OpenPairing, "Try again".into())),
            ),
            PairError::NotInvited => (
                "That computer is not accepting pairing requests right now.",
                "Nothing was trusted. On the other computer, open Devices → Pair a device first, then try again.",
                Some((NoticeAction::OpenPairing, "Try again".into())),
            ),
            PairError::VersionMismatch => {
                ("That computer runs an incompatible version of Synkflow.", "Nothing was exchanged. Update both computers to the same version.", None)
            }
            PairError::Protocol => ("The other computer did not answer correctly.", "Nothing was trusted.", None),
            PairError::Busy => ("A pairing is already in progress.", "Nothing else changed.", None),
            PairError::AlreadyPaired => {
                ("That computer is already paired.", "Nothing changed. Look for it under Devices.", Some((NoticeAction::OpenDevices, "Open devices".into())))
            }
        };
        self.notice(Level::Warn, what, safe, action);
    }

    pub(super) fn pair_tick(&mut self, now: Instant) {
        if self.pairing.window_until.is_some_and(|t| t <= now) {
            self.pairing.window_until = None;
            self.dirty = true;
        }
        let expired = self.pairing.cur.as_ref().is_some_and(|p| p.active() && now >= p.deadline);
        if expired {
            self.fail_pairing(PairError::Expired, true);
        }
        if let Some(p) = &mut self.pairing.cur
            && let (Some(done), true) = (p.finished_at, p.ch.is_some())
            && now.duration_since(done) > Duration::from_secs(3)
            && let Some(ch) = p.ch.take()
        {
            ch.cancel.cancel();
        }
        // A finished pairing stays on screen for the user to read, then clears itself.
        if self.pairing.cur.as_ref().is_some_and(|p| p.finished_at.is_some_and(|t| now.duration_since(t) > Duration::from_secs(60))) {
            self.pairing.cur = None;
            self.dirty = true;
        }
    }

    pub(super) fn pairing_view(&self) -> Option<PairingView> {
        let p = self.pairing.cur.as_ref()?;
        let resembles = p.hello.as_ref().and_then(|h| {
            self.cfg.peers.iter().find(|x| x.announced_name.eq_ignore_ascii_case(&h.name) || x.label.eq_ignore_ascii_case(&h.name)).map(|x| x.label.clone())
        });
        Some(PairingView {
            id: p.id,
            initiator: p.initiator,
            stage: p.stage,
            peer_name: p.hello.as_ref().map(|h| h.name.clone()),
            peer_platform: p.hello.as_ref().map(|h| h.platform),
            peer_fingerprint: p.peer_fp,
            local_fingerprint: self.local(),
            expires_in_secs: p.deadline.saturating_duration_since(Instant::now()).as_secs() as u32,
            error: p.error,
            resembles,
        })
    }
}
