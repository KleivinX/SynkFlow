//! Clipboard glue: a worker thread owns the OS clipboard (watching it only
//! while sharing is active and allowed), the engine decides who may send and
//! receive. Content is never logged, stored, or kept after it is applied.

use std::collections::HashMap;
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use super::*;
use crate::clipboard::{ClipGuard, INCOMING_STALL, Incoming, decode_png_rgba, outgoing};
use crate::platform::ClipContent;
use crate::proto::ClipKind;

/// OS clipboards offer no portable change notification; macOS and Windows
/// expose a cheap change counter that this polls (a single integer read).
const POLL: Duration = Duration::from_millis(400);

pub(crate) enum ClipEvent {
    Changed(ClipContent),
    SkippedSensitive,
    WriteFailed,
}

enum Cmd {
    SetActive(bool),
    Write(ClipContent),
}

pub(super) struct ClipWorker {
    tx: std_mpsc::Sender<Cmd>,
}

pub(super) fn spawn_worker(mut backend: Box<dyn ClipboardBackend>, core: mpsc::Sender<CoreMsg>) -> ClipWorker {
    let (tx, rx) = std_mpsc::channel::<Cmd>();
    let _ = std::thread::Builder::new().name("synkflow-clipboard".into()).spawn(move || {
        let mut guard = ClipGuard::default();
        let mut active = false;
        let mut last_token: Option<u64> = None;
        let mut skipped_token: Option<u64> = None;
        let send = |ev: ClipEvent| {
            let _ = core.blocking_send(CoreMsg::Clip(ev));
        };
        loop {
            let cmd = if active { rx.recv_timeout(POLL) } else { rx.recv().map_err(|_| std_mpsc::RecvTimeoutError::Disconnected) };
            match cmd {
                Ok(Cmd::SetActive(a)) => {
                    active = a;
                    if a {
                        // What is on the clipboard right now was copied before sharing
                        // was on; never send it.
                        last_token = backend.change_token();
                        if last_token.is_none()
                            && let Ok(Some(c)) = backend.read()
                        {
                            guard.note_applied(None, &c, Instant::now());
                        }
                    } else {
                        guard.clear();
                        last_token = None;
                    }
                }
                Ok(Cmd::Write(c)) => match backend.write(&c) {
                    Ok(tok) => {
                        guard.note_applied(tok, &c, Instant::now());
                        if tok.is_some() {
                            last_token = tok;
                        }
                    }
                    Err(_) => send(ClipEvent::WriteFailed),
                },
                Err(std_mpsc::RecvTimeoutError::Timeout) => {
                    let tok = backend.change_token();
                    match tok {
                        Some(t) if Some(t) == last_token => continue,
                        Some(t) => last_token = Some(t),
                        None => {}
                    }
                    if backend.is_sensitive() {
                        if skipped_token != tok || tok.is_none() {
                            skipped_token = tok;
                            send(ClipEvent::SkippedSensitive);
                        }
                        continue;
                    }
                    let Ok(Some(content)) = backend.read() else { continue };
                    let size = match &content {
                        ClipContent::Text(t) => t.len().min(MAX_CLIPBOARD_TEXT + 1),
                        ClipContent::Image(i) => i.len(),
                    };
                    let max = if matches!(content, ClipContent::Text(_)) { MAX_CLIPBOARD_TEXT } else { MAX_CLIPBOARD_IMAGE };
                    if size == 0 || size > max {
                        continue;
                    }
                    if guard.should_send(tok, &content, Instant::now()) {
                        send(ClipEvent::Changed(content));
                    }
                }
                Err(std_mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    });
    ClipWorker { tx }
}

pub(super) struct ClipState {
    worker: Option<ClipWorker>,
    pub(super) available: bool,
    active: bool,
    /// Offers already handled, by (origin, id) only.
    seen: ClipGuard,
    incoming: Option<(Fingerprint, Incoming)>,
    send_tasks: HashMap<Fingerprint, tokio::task::JoinHandle<()>>,
    last_skip_notice: Option<Instant>,
}

impl ClipState {
    pub(super) fn new(worker: Option<ClipWorker>, available: bool) -> Self {
        Self { worker, available, active: false, seen: ClipGuard::default(), incoming: None, send_tasks: HashMap::new(), last_skip_notice: None }
    }
}

impl Core {
    fn clip_allowed_now(&self) -> bool {
        self.clip.available && !self.cfg.clipboard_paused && !self.local_paused()
    }

    /// Peers this computer may send clipboard content to.
    fn clip_targets(&self, kind: ClipKind) -> Vec<(Fingerprint, mpsc::Sender<Msg>)> {
        let mine_ok = self.clip.available;
        self.sessions
            .values()
            .filter(|s| {
                let perms = self.cfg.peer(&s.peer).map(|p| p.perms.clipboard_send).unwrap_or(false);
                let kind_ok = match kind {
                    ClipKind::Text => s.remote.caps.clipboard_text,
                    ClipKind::Image => s.remote.caps.clipboard_image,
                };
                mine_ok && perms && s.remote.grants.clipboard && kind_ok && !s.remote.paused && !s.remote.locked
            })
            .map(|s| (s.peer, s.ch.lo.clone()))
            .collect()
    }

    /// Start or stop watching the OS clipboard to match what is allowed.
    pub(super) fn clip_reconcile(&mut self) {
        let want = self.clip_allowed_now() && (!self.clip_targets(ClipKind::Text).is_empty() || !self.clip_targets(ClipKind::Image).is_empty());
        if want != self.clip.active {
            self.clip.active = want;
            if let Some(w) = &self.clip.worker {
                let _ = w.tx.send(Cmd::SetActive(want));
            }
            if !want {
                for (_, t) in self.clip.send_tasks.drain() {
                    t.abort();
                }
            }
        }
        if !self.clip_allowed_now() {
            self.clip.incoming = None;
            self.clip.seen.clear();
        }
    }

    pub(super) fn clip_housekeeping(&mut self, now: Instant) {
        if self.clip.incoming.as_ref().is_some_and(|(_, i)| now.duration_since(i.last_chunk) > INCOMING_STALL) {
            self.clip.incoming = None;
        }
        self.clip.send_tasks.retain(|_, t| !t.is_finished());
    }

    pub(super) fn clip_session_gone(&mut self, peer: &Fingerprint) {
        if self.clip.incoming.as_ref().is_some_and(|(p, _)| p == peer) {
            self.clip.incoming = None;
        }
        if let Some(t) = self.clip.send_tasks.remove(peer) {
            t.abort();
        }
        self.clip_reconcile();
    }

    pub(super) fn on_clip_event(&mut self, ev: ClipEvent) {
        match ev {
            ClipEvent::Changed(content) => {
                if !self.clip.active || !self.clip_allowed_now() {
                    return;
                }
                let kind = if matches!(content, ClipContent::Text(_)) { ClipKind::Text } else { ClipKind::Image };
                let Some((offer, chunks)) = outgoing(self.clip.seen.next_id(), self.local(), &content) else { return };
                for (peer, lo) in self.clip_targets(kind) {
                    if let Some(old) = self.clip.send_tasks.remove(&peer) {
                        old.abort(); // a newer copy supersedes one still in flight
                    }
                    let (offer, chunks) = (offer.clone(), chunks.clone());
                    let task = tokio::spawn(async move {
                        // Chunks wait for room, so a big image never starves input.
                        if lo.send(offer).await.is_err() {
                            return;
                        }
                        for c in chunks {
                            if lo.send(c).await.is_err() {
                                return;
                            }
                        }
                    });
                    self.clip.send_tasks.insert(peer, task);
                }
            }
            ClipEvent::SkippedSensitive => {
                let now = Instant::now();
                if self.clip.last_skip_notice.is_none_or(|t| now.duration_since(t) > Duration::from_secs(60)) {
                    self.clip.last_skip_notice = Some(now);
                    self.notice(
                        Level::Info,
                        "A copied item was marked private, so it was not shared.",
                        "Password managers mark secrets this way. Not every program does, so avoid copying secrets while clipboard sharing is on.",
                        None,
                    );
                }
            }
            ClipEvent::WriteFailed => self.notice(Level::Warn, "Shared clipboard content could not be applied.", "Your clipboard was not changed.", None),
        }
    }

    pub(super) fn on_clip_offer(&mut self, _sid: SessionId, peer: Fingerprint, id: u64, origin: Fingerprint, kind: ClipKind, size: u32) {
        let allowed = self.cfg.peer(&peer).is_some_and(|p| p.perms.clipboard_receive);
        let kind_ok = match kind {
            ClipKind::Text => self.clip.available,
            ClipKind::Image => self.clip.available,
        };
        if !allowed || !kind_ok || !self.clip_allowed_now() || origin == self.local() || !self.clip.seen.accept_offer(origin, id) {
            self.send_to(&peer, Msg::ClipAbort { id });
            return;
        }
        match Incoming::new(id, origin, kind, size, Instant::now()) {
            Ok(i) => self.clip.incoming = Some((peer, i)),
            Err(_) => self.send_to(&peer, Msg::ClipAbort { id }),
        }
    }

    pub(super) fn on_clip_chunk(&mut self, peer: Fingerprint, id: u64, data: Vec<u8>) {
        let Some((p, inc)) = self.clip.incoming.as_mut() else { return };
        if *p != peer || inc.id != id {
            return;
        }
        match inc.push(&data, Instant::now()) {
            Ok(None) => {}
            Ok(Some(content)) => {
                self.clip.incoming = None;
                if let ClipContent::Image(png) = &content
                    && decode_png_rgba(png).is_err()
                {
                    self.notice(Level::Warn, "A shared image was damaged and was ignored.", "Your clipboard was not changed.", None);
                    return;
                }
                if let Some(w) = &self.clip.worker {
                    let _ = w.tx.send(Cmd::Write(content));
                }
            }
            Err(_) => {
                self.clip.incoming = None;
                self.send_to(&peer, Msg::ClipAbort { id });
            }
        }
    }

    pub(super) fn on_clip_abort(&mut self, peer: Fingerprint, _id: u64) {
        if let Some(t) = self.clip.send_tasks.remove(&peer) {
            t.abort();
        }
    }
}
