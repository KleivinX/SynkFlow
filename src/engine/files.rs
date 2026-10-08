//! File-transfer orchestration: offers, acceptance, bulk-channel binding,
//! progress, history. The byte streaming itself lives in `transfer.rs`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use cap_std::fs::Dir;
use subtle::ConstantTimeEq;

use super::*;
use crate::config::AcceptMode;
use crate::proto::FileMeta;
use crate::transfer::{FailKind, Inbox, Outcome, ReceiveJob, SendFile, Shared, run_receive, run_send, safe_name};

const BULK_WAIT: Duration = Duration::from_secs(15);
const PROGRESS_EVERY: Duration = Duration::from_millis(250);
const HISTORY: usize = 50;
/// Keep some headroom free beyond the transfer itself.
const SPACE_MARGIN: u64 = 16 * 1024 * 1024;

pub(super) struct Xfer {
    id: u64,
    dir: Direction,
    peer: Fingerprint,
    files: Vec<(String, u64)>,
    /// Send side: where the files are. Held in memory only, never persisted.
    paths: Vec<PathBuf>,
    status: TransferStatus,
    shared: Shared,
    offered_at: Instant,
    started: Option<Instant>,
    awaiting_bulk: Option<Instant>,
    last_sample: (Instant, u64),
    rate: f64,
    error: Option<String>,
    folder: Option<String>,
    inbox_dir: Option<Dir>,
}

impl Xfer {
    fn total(&self) -> u64 {
        self.files.iter().map(|f| f.1).sum()
    }
}

#[derive(Default)]
pub(super) struct Transfers {
    map: HashMap<u64, Xfer>,
    order: Vec<u64>,
    progress_next: Option<Instant>,
}

impl Transfers {
    pub(super) fn next_progress(&self) -> Option<Instant> {
        self.progress_next
    }

    pub(super) fn clear_finished(&mut self) {
        let done: Vec<u64> = self.map.values().filter(|x| x.status.is_finished()).map(|x| x.id).collect();
        for id in done {
            self.map.remove(&id);
        }
        self.order.retain(|id| self.map.contains_key(id));
    }

    fn insert(&mut self, x: Xfer) {
        self.order.push(x.id);
        self.map.insert(x.id, x);
        // Bounded history of finished transfers.
        let finished: Vec<u64> = self.order.iter().copied().filter(|id| self.map.get(id).is_some_and(|x| x.status.is_finished())).collect();
        if finished.len() > HISTORY {
            for id in &finished[..finished.len() - HISTORY] {
                self.map.remove(id);
            }
            self.order.retain(|id| self.map.contains_key(id));
        }
    }
}

fn fail_text(k: FailKind) -> (&'static str, &'static str) {
    match k {
        FailKind::DiskFull => ("The disk is full.", "The partial file was removed; nothing was left half-written."),
        FailKind::UnsafeName => ("A file name was not safe to accept.", "Nothing was written."),
        FailKind::Io => ("A file could not be read or written.", "Partial files were removed."),
        FailKind::Integrity => ("A file arrived damaged and was discarded.", "Nothing damaged was kept."),
        FailKind::PeerLost => ("The connection was lost during the transfer.", "Partial files were removed. You can retry."),
        FailKind::FileChanged => ("A file changed while it was being sent.", "Nothing incomplete was kept."),
        FailKind::Protocol => ("The other computer did not follow the transfer protocol.", "Nothing was kept."),
        FailKind::Timeout => ("The transfer stalled and was stopped.", "Partial files were removed."),
    }
}

/// `YYYY-MM-DD HH-MM-SS` in UTC, safe in file names on every platform.
pub fn utc_stamp(secs: u64) -> String {
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Howard Hinnant's civil-from-days.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02} {:02}-{:02}-{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

impl Core {
    fn new_xfer_id(&self) -> u64 {
        loop {
            let id = rand::random::<u64>() >> 1;
            if id != 0 && !self.xfers.map.contains_key(&id) {
                return id;
            }
        }
    }

    // ── sending ────────────────────────────────────────────────────────────

    pub(super) fn send_files(&mut self, peer: Fingerprint, paths: Vec<PathBuf>) {
        let label = self.label_of(&peer);
        let Some(sid) = self.peers.get(&peer).and_then(|p| p.session) else {
            self.notice(Level::Warn, &format!("{label} is not connected."), "Nothing was sent.", Some((NoticeAction::OpenDevices, "Open devices".into())));
            return;
        };
        let (grants_files, caps_files) = self.sessions.get(&sid).map(|s| (s.remote.grants.files, s.remote.caps.files)).unwrap_or((false, false));
        if !caps_files {
            self.notice(Level::Warn, &format!("{label} cannot receive files."), "Nothing was sent.", None);
            return;
        }
        if !grants_files {
            self.notice(Level::Warn, &format!("{label} does not accept files from this computer."), "Nothing was sent. Ask them to allow it in Devices.", None);
            return;
        }
        let (mut files, mut sources) = (Vec::new(), Vec::new());
        let mut skipped_dirs = false;
        for p in paths {
            match std::fs::metadata(&p) {
                Ok(m) if m.is_file() => {
                    let Some(name) = p.file_name().map(|n| n.to_string_lossy().to_string()) else { continue };
                    if name.len() > MAX_FILE_NAME_BYTES {
                        self.notice(Level::Warn, "A file name is too long to send.", "That file was skipped.", None);
                        continue;
                    }
                    files.push((name, m.len()));
                    sources.push(p);
                }
                Ok(m) if m.is_dir() => skipped_dirs = true,
                _ => {}
            }
        }
        if skipped_dirs {
            self.notice(Level::Info, "Folders cannot be sent yet.", "Send the files inside instead. Everything else in your selection continues.", None);
        }
        if files.is_empty() || files.len() > MAX_FILES_PER_OFFER {
            if files.len() > MAX_FILES_PER_OFFER {
                self.notice(Level::Warn, "Too many files at once.", &format!("Send at most {MAX_FILES_PER_OFFER} files per transfer."), None);
            }
            return;
        }
        self.queue_send(peer, files, sources);
    }

    fn queue_send(&mut self, peer: Fingerprint, files: Vec<(String, u64)>, sources: Vec<PathBuf>) {
        let id = self.new_xfer_id();
        let metas: Vec<FileMeta> = files.iter().map(|(n, s)| FileMeta { name: n.clone(), size: *s }).collect();
        let now = Instant::now();
        self.xfers.insert(Xfer {
            id,
            dir: Direction::Send,
            peer,
            files,
            paths: sources,
            status: TransferStatus::WaitingForAcceptance,
            shared: Shared::new(),
            offered_at: now,
            started: None,
            awaiting_bulk: None,
            last_sample: (now, 0),
            rate: 0.0,
            error: None,
            folder: None,
            inbox_dir: None,
        });
        self.send_to(&peer, Msg::FileOffer { transfer_id: id, files: metas });
        self.dirty = true;
    }

    pub(super) fn retry_transfer(&mut self, id: u64) {
        let Some(x) = self.xfers.map.get(&id) else { return };
        if x.dir != Direction::Send || !matches!(x.status, TransferStatus::Failed | TransferStatus::Cancelled | TransferStatus::Declined) {
            return;
        }
        let (peer, paths) = (x.peer, x.paths.clone());
        self.send_files(peer, paths);
    }

    pub(super) fn on_file_answer(&mut self, peer: Fingerprint, id: u64, accepted: bool, reason: Option<RejectReason>) {
        let label = self.label_of(&peer);
        let Some(x) = self.xfers.map.get_mut(&id) else { return };
        if x.peer != peer || x.dir != Direction::Send || x.status != TransferStatus::WaitingForAcceptance {
            return;
        }
        if !accepted {
            x.status = TransferStatus::Declined;
            let (level, what, safe): (Level, String, &str) = match reason {
                Some(RejectReason::NoSpace) => (Level::Warn, format!("{label} does not have enough free space."), "Nothing was sent."),
                Some(RejectReason::UnsafeName) => (Level::Warn, format!("{label} refused a file name as unsafe."), "Nothing was sent."),
                _ => (Level::Info, format!("{label} declined the files."), "Nothing was sent."),
            };
            self.notice(level, &what, safe, None);
            return;
        }
        x.status = TransferStatus::Preparing;
        x.awaiting_bulk = Some(Instant::now());
        self.maybe_dial_bulk(id);
    }

    // ── receiving ──────────────────────────────────────────────────────────

    pub(super) fn on_file_offer(&mut self, _sid: SessionId, peer: Fingerprint, id: u64, files: Vec<FileMeta>) {
        let reply = |core: &mut Core, accepted: bool, reason: Option<RejectReason>| core.send_to(&peer, Msg::FileAnswer { transfer_id: id, accepted, reason });
        let label = self.label_of(&peer);
        let perms = self.cfg.peer(&peer).map(|p| p.perms.clone());
        let Some(perms) = perms else { return };
        if self.xfers.map.contains_key(&id) {
            return;
        }
        if !perms.files_receive || self.cfg.files.accept_mode == AcceptMode::Never {
            return reply(self, false, Some(RejectReason::NotAllowed));
        }
        if self.local_paused() {
            return reply(self, false, Some(RejectReason::Paused));
        }
        let mut names = Vec::new();
        for f in &files {
            match safe_name(&f.name) {
                Ok(n) => names.push((n, f.size)),
                Err(_) => {
                    self.notice(
                        Level::Warn,
                        &format!("{label} offered a file with an unsafe name."),
                        "The whole offer was declined and nothing was written.",
                        None,
                    );
                    return reply(self, false, Some(RejectReason::UnsafeName));
                }
            }
        }
        let now = Instant::now();
        self.xfers.insert(Xfer {
            id,
            dir: Direction::Receive,
            peer,
            files: names,
            paths: vec![],
            status: TransferStatus::WaitingForAcceptance,
            shared: Shared::new(),
            offered_at: now,
            started: None,
            awaiting_bulk: None,
            last_sample: (now, 0),
            rate: 0.0,
            error: None,
            folder: None,
            inbox_dir: None,
        });
        if perms.files_auto_accept {
            self.accept_transfer(id);
        } else {
            let n = files.len();
            self.notice(
                Level::Info,
                &format!("{label} wants to send you {n} file{}.", if n == 1 { "" } else { "s" }),
                "Nothing is saved unless you accept it in Transfers.",
                None,
            );
        }
        self.dirty = true;
    }

    pub(super) fn accept_transfer(&mut self, id: u64) {
        let Some(x) = self.xfers.map.get(&id) else { return };
        if x.dir != Direction::Receive || x.status != TransferStatus::WaitingForAcceptance {
            return;
        }
        let (peer, total) = (x.peer, x.total());
        let label = self.label_of(&peer);
        let decline = |core: &mut Core, why: RejectReason, text: &str, safe: &str| {
            if let Some(x) = core.xfers.map.get_mut(&id) {
                x.status = TransferStatus::Failed;
                x.error = Some(text.to_string());
            }
            core.send_to(&peer, Msg::FileAnswer { transfer_id: id, accepted: false, reason: Some(why) });
            core.notice(Level::Warn, text, safe, None);
        };
        let inbox = match Inbox::open(&self.cfg.inbox_dir()) {
            Ok(i) => i,
            Err(_) => {
                return decline(
                    self,
                    RejectReason::NoSpace,
                    "The receive folder could not be opened.",
                    "Nothing was received. Choose another folder in Settings → Files.",
                );
            }
        };
        match inbox.available_space() {
            Ok(free) if free < total.saturating_add(SPACE_MARGIN) => {
                return decline(
                    self,
                    RejectReason::NoSpace,
                    "There is not enough free disk space for this transfer.",
                    "Nothing was received. Free some space or choose another folder in Settings → Files.",
                );
            }
            _ => {}
        }
        let (dir, folder) = match inbox.new_transfer_dir(&label, &utc_stamp(now_secs())) {
            Ok(d) => d,
            Err(_) => return decline(self, RejectReason::NoSpace, "A folder for the files could not be created.", "Nothing was received."),
        };
        let Some(x) = self.xfers.map.get_mut(&id) else { return };
        x.status = TransferStatus::Preparing;
        x.inbox_dir = Some(dir);
        x.folder = Some(folder);
        x.awaiting_bulk = Some(Instant::now());
        self.send_to(&peer, Msg::FileAnswer { transfer_id: id, accepted: true, reason: None });
        self.maybe_dial_bulk(id);
    }

    pub(super) fn decline_transfer(&mut self, id: u64) {
        let Some(x) = self.xfers.map.get_mut(&id) else { return };
        if x.dir != Direction::Receive || x.status != TransferStatus::WaitingForAcceptance {
            return;
        }
        x.status = TransferStatus::Declined;
        let peer = x.peer;
        self.send_to(&peer, Msg::FileAnswer { transfer_id: id, accepted: false, reason: Some(RejectReason::Declined) });
    }

    // ── bulk channel ───────────────────────────────────────────────────────

    /// The side that opened the control connection also opens bulk channels, so
    /// the path that already works through any firewall is the one reused.
    fn maybe_dial_bulk(&mut self, id: u64) {
        let Some(x) = self.xfers.map.get(&id) else { return };
        let peer = x.peer;
        let Some(sid) = self.peers.get(&peer).and_then(|p| p.session) else { return };
        let Some(s) = self.sessions.get(&sid) else { return };
        if !s.we_connected {
            return; // the other side will dial us
        }
        let (addr, key, token) = (s.addr, s.session_key, s.bulk_token);
        let (identity, tx) = (self.identity.clone(), self.tx.clone());
        tokio::spawn(async move {
            let r = session::dial_bulk(addr, &identity, peer, key, token, id).await;
            let _ = tx.send(CoreMsg::BulkDialDone { transfer_id: id, result: Box::new(r) }).await;
        });
    }

    pub(super) fn on_bulk_dial_done(&mut self, id: u64, result: Result<Wire<ClientStream>, DialError>) {
        match result {
            Ok(wire) => self.run_transfer(id, wire),
            Err(_) => self.finish_transfer(id, Outcome::Failed(FailKind::PeerLost)),
        }
    }

    pub(super) fn on_incoming_bulk(&mut self, mut wire: Wire<ServerStream>, session_id: [u8; 16], token: [u8; 16], transfer_id: u64, peer: Fingerprint) {
        let ok = (|| {
            let x = self.xfers.map.get(&transfer_id)?;
            if x.peer != peer || x.status != TransferStatus::Preparing || x.awaiting_bulk.is_none() {
                return None;
            }
            let s = self.peers.get(&peer).and_then(|p| p.session).and_then(|s| self.sessions.get(&s))?;
            // Both the session id and the secret token must match the control session.
            let good = s.session_key.ct_eq(&session_id).unwrap_u8() & s.bulk_token.ct_eq(&token).unwrap_u8();
            (good == 1 && !s.we_connected).then_some(())
        })()
        .is_some();
        if !ok {
            tracing::warn!("bulk attach from {} rejected", peer.short());
            return; // dropping the wire closes the connection
        }
        // Reply AttachOk, then stream; both inside the transfer task.
        let tx = self.tx.clone();
        let Some(x) = self.xfers.map.get_mut(&transfer_id) else { return };
        x.awaiting_bulk = None;
        let work = self.take_job(transfer_id);
        let Some(work) = work else { return };
        tokio::spawn(async move {
            let outcome = match session::accept_bulk_ok(&mut wire).await {
                Ok(()) => work.run(wire).await,
                Err(_) => Outcome::Failed(FailKind::PeerLost),
            };
            let _ = tx.send(CoreMsg::TransferDone { id: transfer_id, outcome }).await;
        });
        self.mark_started(transfer_id);
    }

    fn run_transfer<S>(&mut self, id: u64, wire: Wire<S>)
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let Some(work) = self.take_job(id) else { return };
        let tx = self.tx.clone();
        if let Some(x) = self.xfers.map.get_mut(&id) {
            x.awaiting_bulk = None;
        }
        tokio::spawn(async move {
            let outcome = work.run(wire).await;
            let _ = tx.send(CoreMsg::TransferDone { id, outcome }).await;
        });
        self.mark_started(id);
    }

    fn take_job(&mut self, id: u64) -> Option<Job> {
        let x = self.xfers.map.get_mut(&id)?;
        let shared = x.shared.clone();
        match x.dir {
            Direction::Send => {
                let files = x.paths.iter().zip(&x.files).map(|(p, (_, size))| SendFile { path: p.clone(), size: *size }).collect();
                Some(Job::Send { files, shared })
            }
            Direction::Receive => {
                let dir = x.inbox_dir.as_ref()?.try_clone().ok()?;
                Some(Job::Receive { job: ReceiveJob { files: x.files.clone(), dir }, shared })
            }
        }
    }

    fn mark_started(&mut self, id: u64) {
        let now = Instant::now();
        if let Some(x) = self.xfers.map.get_mut(&id) {
            x.status = TransferStatus::Transferring;
            x.started = Some(now);
            x.last_sample = (now, 0);
        }
        self.xfers.progress_next = Some(now + PROGRESS_EVERY);
        self.dirty = true;
    }

    pub(super) fn on_transfer_done(&mut self, id: u64, outcome: Outcome) {
        self.finish_transfer(id, outcome);
    }

    fn finish_transfer(&mut self, id: u64, outcome: Outcome) {
        let (dir, peer);
        {
            let Some(x) = self.xfers.map.get_mut(&id) else { return };
            if x.status.is_finished() {
                return;
            }
            (dir, peer) = (x.dir, x.peer);
            match outcome {
                Outcome::Complete => {
                    x.status = TransferStatus::Complete;
                    x.shared.done.store(x.files.iter().map(|f| f.1).sum(), std::sync::atomic::Ordering::Relaxed);
                }
                Outcome::Cancelled => x.status = TransferStatus::Cancelled,
                Outcome::Failed(k) => {
                    x.status = TransferStatus::Failed;
                    x.error = Some(fail_text(k).0.to_string());
                }
            }
            x.inbox_dir = None;
            x.awaiting_bulk = None;
        }
        let name = self.label_of(&peer);
        match outcome {
            Outcome::Complete if dir == Direction::Receive => {
                let n = self.xfers.map.get(&id).map(|x| x.files.len()).unwrap_or(0);
                self.notice(
                    Level::Info,
                    &format!("Received {n} file{} from {name}.", if n == 1 { "" } else { "s" }),
                    "Files are in your Synkflow folder. Nothing was opened or run.",
                    None,
                );
            }
            Outcome::Failed(k) => {
                let (what, safe) = fail_text(k);
                self.notice(Level::Warn, what, safe, None);
            }
            _ => {}
        }
        self.dirty = true;
    }

    pub(super) fn cancel_transfer(&mut self, id: u64) {
        let Some(x) = self.xfers.map.get_mut(&id) else { return };
        if x.status.is_finished() {
            return;
        }
        let peer = x.peer;
        x.shared.cancel.cancel();
        let running = matches!(x.status, TransferStatus::Transferring | TransferStatus::Verifying);
        if !running {
            x.status = TransferStatus::Cancelled;
            x.inbox_dir = None;
        }
        self.send_to(&peer, Msg::FileCancel { transfer_id: id });
        self.dirty = true;
    }

    pub(super) fn on_file_cancel(&mut self, peer: Fingerprint, id: u64) {
        let Some(x) = self.xfers.map.get_mut(&id) else { return };
        if x.peer != peer || x.status.is_finished() {
            return;
        }
        x.shared.cancel.cancel();
        if !matches!(x.status, TransferStatus::Transferring | TransferStatus::Verifying) {
            x.status = TransferStatus::Cancelled;
            x.inbox_dir = None;
        }
        self.dirty = true;
    }

    pub(super) fn fail_transfers_with(&mut self, peer: &Fingerprint) {
        let ids: Vec<u64> = self.xfers.map.values().filter(|x| &x.peer == peer && !x.status.is_finished()).map(|x| x.id).collect();
        for id in ids {
            if let Some(x) = self.xfers.map.get(&id) {
                x.shared.cancel.cancel();
            }
            self.finish_transfer(id, Outcome::Failed(FailKind::PeerLost));
        }
    }

    pub(super) fn cancel_all_transfers(&mut self) {
        for x in self.xfers.map.values() {
            x.shared.cancel.cancel();
        }
    }

    pub(super) fn refresh_progress(&mut self, now: Instant) {
        let mut any = false;
        for x in self.xfers.map.values_mut() {
            if x.status != TransferStatus::Transferring {
                continue;
            }
            any = true;
            let done = x.shared.bytes();
            let dt = now.duration_since(x.last_sample.0).as_secs_f64();
            if dt > 0.0 {
                let inst = (done.saturating_sub(x.last_sample.1)) as f64 / dt;
                x.rate = if x.rate == 0.0 { inst } else { x.rate * 0.7 + inst * 0.3 };
                x.last_sample = (now, done);
            }
        }
        self.xfers.progress_next = any.then_some(now + PROGRESS_EVERY);
        self.dirty = true;
    }

    pub(super) fn files_housekeeping(&mut self, now: Instant) {
        let mut timeouts = Vec::new();
        let mut expired = Vec::new();
        for x in self.xfers.map.values() {
            if x.awaiting_bulk.is_some_and(|t| now.duration_since(t) > BULK_WAIT) {
                timeouts.push(x.id);
            }
            if x.dir == Direction::Receive && x.status == TransferStatus::WaitingForAcceptance && now.duration_since(x.offered_at) > OFFER_TTL {
                expired.push(x.id);
            }
        }
        for id in timeouts {
            self.finish_transfer(id, Outcome::Failed(FailKind::Timeout));
        }
        for id in expired {
            self.decline_transfer(id);
        }
    }

    pub(super) fn transfer_views(&self) -> Vec<TransferView> {
        self.xfers
            .order
            .iter()
            .filter_map(|id| self.xfers.map.get(id))
            .map(|x| {
                let total = x.total();
                let done = if x.status == TransferStatus::Complete { total } else { x.shared.bytes().min(total) };
                let status = if x.status == TransferStatus::Transferring && done >= total && total > 0 { TransferStatus::Verifying } else { x.status };
                let eta = (x.status == TransferStatus::Transferring && x.rate > 1.0 && total > done)
                    .then(|| ((total - done) as f64 / x.rate).ceil().min(86_400.0) as u32);
                TransferView {
                    id: x.id,
                    direction: x.dir,
                    peer: x.peer,
                    peer_label: self.label_of(&x.peer),
                    status,
                    files: x.files.clone(),
                    total,
                    done,
                    rate_bps: if x.status == TransferStatus::Transferring { x.rate } else { 0.0 },
                    eta_secs: eta,
                    error: x.error.clone(),
                    folder: x.folder.clone(),
                    can_retry: x.dir == Direction::Send && matches!(x.status, TransferStatus::Failed | TransferStatus::Cancelled | TransferStatus::Declined),
                }
            })
            .collect()
    }
}

enum Job {
    Send { files: Vec<SendFile>, shared: Shared },
    Receive { job: ReceiveJob, shared: Shared },
}

impl Job {
    async fn run<S>(self, wire: Wire<S>) -> Outcome
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send,
    {
        match self {
            Job::Send { files, shared } => run_send(wire, files, shared).await,
            Job::Receive { job, shared } => run_receive(wire, job, shared).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::utc_stamp;

    #[test]
    fn utc_stamp_matches_known_dates() {
        assert_eq!(utc_stamp(0), "1970-01-01 00-00-00");
        assert_eq!(utc_stamp(951_782_400), "2000-02-29 00-00-00");
        assert_eq!(utc_stamp(1_700_000_000), "2023-11-14 22-13-20");
        assert!(!utc_stamp(86_399).contains(':'), "no characters that are illegal in Windows file names");
    }
}
