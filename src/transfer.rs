//! Streamed, accepted file transfer.
//!
//! * Received names are untrusted: [`safe_name`] rejects traversal, separators,
//!   control and bidi characters, Windows device names, and fixes characters
//!   that are illegal on some platforms.
//! * Received files land in a unique, freshly created sub-folder of the user's
//!   inbox, opened through a **capability handle** (`cap-std`), so no name can
//!   escape it even through symlinks or reparse points. Files are created
//!   exclusively as hidden `.part` files and renamed into place only after the
//!   SHA-256 matches. Nothing is ever overwritten, opened or executed.
//! * Memory is bounded: one 256 KiB chunk in flight on each side.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bytes::{BufMut, BytesMut};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};
use futures_util::{SinkExt, StreamExt};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::limits::*;
use crate::proto::{self, BULK_CTL, BULK_DATA, BulkCtl, RejectReason, Wire};

/// A receiver that hears nothing for this long gives up.
const STALL: Duration = Duration::from_secs(30);

// ───────────────────────────── names ─────────────────────────────

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum NameError {
    Empty,
    TooLong,
    Separator,
    Traversal,
    Control,
    Reserved,
}

const RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "COM¹", "COM²", "COM³", "LPT1",
    "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9", "LPT¹", "LPT²", "LPT³",
];

fn is_bidi_or_format(c: char) -> bool {
    matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}')
}

/// Validate and normalise one received file name. Names are *flat*: anything
/// that looks like a path is refused, never "flattened".
pub fn safe_name(raw: &str) -> Result<String, NameError> {
    if raw.len() > MAX_FILE_NAME_BYTES {
        return Err(NameError::TooLong);
    }
    if raw.chars().any(|c| c == '/' || c == '\\') {
        return Err(NameError::Separator);
    }
    if raw.chars().any(|c| c.is_control() || is_bidi_or_format(c)) {
        return Err(NameError::Control);
    }
    let trimmed = raw.trim_matches(|c| c == ' ' || c == '.');
    if raw.trim().is_empty() || trimmed.is_empty() {
        return Err(if raw.trim_matches(' ').chars().all(|c| c == '.') && !raw.trim().is_empty() { NameError::Traversal } else { NameError::Empty });
    }
    if raw.trim() == "." || raw.trim() == ".." {
        return Err(NameError::Traversal);
    }
    let cleaned: String = raw.trim_start().chars().map(|c| if matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') { '_' } else { c }).collect();
    let cleaned = cleaned.trim_end_matches([' ', '.']).to_string();
    if cleaned.is_empty() {
        return Err(NameError::Empty);
    }
    let stem = cleaned.split('.').next().unwrap_or("").trim_end().to_uppercase();
    if RESERVED.contains(&stem.as_str()) {
        return Err(NameError::Reserved);
    }
    Ok(cleaned)
}

/// `report.pdf` → `report (2).pdf`, case-insensitively unique.
pub fn dedupe(name: &str, taken: &dyn Fn(&str) -> bool) -> String {
    if !taken(name) {
        return name.to_string();
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    for n in 2..10_000 {
        let candidate = format!("{stem} ({n}){ext}");
        if !taken(&candidate) {
            return candidate;
        }
    }
    format!("{stem}-{:x}{ext}", rand::random::<u32>())
}

// ───────────────────────────── inbox ─────────────────────────────

/// The user's receive folder, as a capability handle.
pub struct Inbox {
    root: Dir,
    root_path: PathBuf,
}

impl Inbox {
    pub fn open(path: &Path) -> io::Result<Self> {
        Dir::create_ambient_dir_all(path, ambient_authority())?;
        Ok(Self { root: Dir::open_ambient_dir(path, ambient_authority())?, root_path: path.to_path_buf() })
    }

    pub fn path(&self) -> &Path {
        &self.root_path
    }

    pub fn available_space(&self) -> io::Result<u64> {
        fs4::available_space(&self.root_path)
    }

    /// Create a brand-new sub-folder for one transfer. Never reuses a name.
    pub fn new_transfer_dir(&self, label: &str, stamp: &str) -> io::Result<(Dir, String)> {
        let base = safe_name(&format!("{label} {stamp}")).unwrap_or_else(|_| format!("Transfer {stamp}"));
        for n in 1..1000 {
            let name = if n == 1 { base.clone() } else { format!("{base} ({n})") };
            match self.root.create_dir(&name) {
                Ok(()) => return Ok((self.root.open_dir(&name)?, name)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(io::ErrorKind::AlreadyExists, "no free folder name"))
    }
}

struct PartFile {
    file: tokio::fs::File,
    part_name: String,
    final_name: String,
}

fn part_name(final_name: &str) -> String {
    let short: String = final_name.chars().take(60).collect();
    format!(".{short}.part-{:08x}", rand::random::<u32>())
}

fn create_new() -> OpenOptions {
    let mut o = OpenOptions::new();
    o.write(true).create_new(true);
    o
}

fn open_part(dir: &Dir, name: &str) -> io::Result<PartFile> {
    let part = part_name(name);
    let f = dir.open_with(&part, &create_new())?;
    Ok(PartFile { file: tokio::fs::File::from_std(f.into_std()), part_name: part, final_name: name.to_string() })
}

/// Move a verified `.part` file to a name that did not exist, atomically.
fn finalize(dir: &Dir, p: &PartFile, taken_in_transfer: &dyn Fn(&str) -> bool) -> io::Result<String> {
    for attempt in 0..50 {
        let candidate = if attempt == 0 { dedupe(&p.final_name, taken_in_transfer) } else { dedupe(&format!("{}-{attempt}", p.final_name), taken_in_transfer) };
        // Reserve the name exclusively, then replace our own placeholder: a
        // plain rename could silently overwrite a file that appeared meanwhile.
        match dir.open_with(&candidate, &create_new()) {
            Ok(_) => {
                dir.rename(&p.part_name, dir, &candidate)?;
                return Ok(candidate);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "no free file name"))
}

// ───────────────────────────── outcomes ─────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailKind {
    DiskFull,
    UnsafeName,
    Io,
    Integrity,
    PeerLost,
    FileChanged,
    Protocol,
    Timeout,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Complete,
    /// Cancelled here or by the other side.
    Cancelled,
    Failed(FailKind),
}

pub fn classify_io(e: &io::Error) -> FailKind {
    if e.kind() == io::ErrorKind::StorageFull || matches!(e.raw_os_error(), Some(28) | Some(112)) { FailKind::DiskFull } else { FailKind::Io }
}

/// Shared with the engine: lock-free progress, and the cancel switch.
#[derive(Clone)]
pub struct Shared {
    pub done: Arc<AtomicU64>,
    pub cancel: CancellationToken,
}

impl Shared {
    pub fn new() -> Self {
        Self { done: Arc::new(AtomicU64::new(0)), cancel: CancellationToken::new() }
    }
    pub fn bytes(&self) -> u64 {
        self.done.load(Ordering::Relaxed)
    }
}

impl Default for Shared {
    fn default() -> Self {
        Self::new()
    }
}

// ───────────────────────────── receive ─────────────────────────────

pub struct ReceiveJob {
    /// Safe names and declared sizes, from the accepted offer.
    pub files: Vec<(String, u64)>,
    pub dir: Dir,
}

fn ctl(c: &BulkCtl) -> bytes::Bytes {
    proto::encode_bulk_ctl(c).expect("small control frame")
}

/// Receive every file of an accepted offer over an attached bulk channel.
pub async fn run_receive<S>(wire: Wire<S>, job: ReceiveJob, shared: Shared) -> Outcome
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let (mut sink, mut stream) = wire.split();
    let mut part: Option<(usize, PartFile, Sha256, u64)> = None;
    let mut finished: Vec<String> = Vec::new();
    let mut next_index = 0usize;
    let mut completed = 0usize;

    let outcome = loop {
        let frame = tokio::select! {
            biased;
            _ = shared.cancel.cancelled() => {
                let _ = sink.send(ctl(&BulkCtl::Cancel)).await;
                break Outcome::Cancelled;
            }
            f = timeout(STALL, stream.next()) => match f {
                Err(_) => break Outcome::Failed(FailKind::Timeout),
                Ok(None) | Ok(Some(Err(_))) => break Outcome::Failed(FailKind::PeerLost),
                Ok(Some(Ok(frame))) => frame,
            },
        };
        match frame.first() {
            Some(&BULK_DATA) => {
                let Some((_, p, hasher, written)) = part.as_mut() else { break Outcome::Failed(FailKind::Protocol) };
                let data = &frame[1..];
                let declared = job.files[next_index - 1].1;
                if *written + data.len() as u64 > declared {
                    break Outcome::Failed(FailKind::Integrity); // more bytes than the offer said
                }
                if let Err(e) = p.file.write_all(data).await {
                    break Outcome::Failed(classify_io(&e));
                }
                hasher.update(data);
                *written += data.len() as u64;
                shared.done.fetch_add(data.len() as u64, Ordering::Relaxed);
            }
            Some(&BULK_CTL) => match proto::decode_bulk_ctl(&frame) {
                Ok(BulkCtl::FileStart { index, size }) => {
                    if part.is_some() || index as usize != next_index || next_index >= job.files.len() || job.files[next_index].1 != size {
                        break Outcome::Failed(FailKind::Protocol);
                    }
                    match open_part(&job.dir, &job.files[next_index].0) {
                        Ok(p) => part = Some((next_index, p, Sha256::new(), 0)),
                        Err(e) => break Outcome::Failed(classify_io(&e)),
                    }
                    next_index += 1;
                }
                Ok(BulkCtl::FileEnd { index, sha256 }) => {
                    let Some((i, mut p, hasher, written)) = part.take() else { break Outcome::Failed(FailKind::Protocol) };
                    if index as usize != i {
                        discard(&job.dir, &p);
                        break Outcome::Failed(FailKind::Protocol);
                    }
                    let expected = job.files[i].1;
                    let actual: [u8; 32] = hasher.finalize().into();
                    if written != expected || actual != sha256 {
                        discard(&job.dir, &p);
                        let _ = sink.send(ctl(&BulkCtl::FileResult { index, ok: false, reason: None })).await;
                        break Outcome::Failed(FailKind::Integrity);
                    }
                    if let Err(e) = p.file.flush().await {
                        discard(&job.dir, &p);
                        break Outcome::Failed(classify_io(&e));
                    }
                    let _ = p.file.sync_all().await;
                    let taken: &dyn Fn(&str) -> bool = &|n: &str| finished.iter().any(|f| f.eq_ignore_ascii_case(n)) || job.dir.try_exists(n).unwrap_or(true);
                    match finalize(&job.dir, &p, taken) {
                        Ok(name) => {
                            finished.push(name);
                            completed += 1;
                            let _ = sink.send(ctl(&BulkCtl::FileResult { index, ok: true, reason: None })).await;
                        }
                        Err(e) => {
                            discard(&job.dir, &p);
                            break Outcome::Failed(classify_io(&e));
                        }
                    }
                }
                Ok(BulkCtl::Done) => {
                    break if part.is_none() && completed == job.files.len() { Outcome::Complete } else { Outcome::Failed(FailKind::Protocol) };
                }
                Ok(BulkCtl::Cancel) => break Outcome::Cancelled,
                _ => break Outcome::Failed(FailKind::Protocol),
            },
            _ => break Outcome::Failed(FailKind::Protocol),
        }
    };
    if let Some((_, p, _, _)) = part.take() {
        discard(&job.dir, &p);
    }
    let _ = sink.close().await;
    outcome
}

fn discard(dir: &Dir, p: &PartFile) {
    let _ = dir.remove_file(&p.part_name);
}

// ───────────────────────────── send ─────────────────────────────

pub struct SendFile {
    pub path: PathBuf,
    pub size: u64,
}

/// Stream files to the receiver. Reads each file once, hashing as it goes.
pub async fn run_send<S>(wire: Wire<S>, files: Vec<SendFile>, shared: Shared) -> Outcome
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let (mut sink, mut stream) = wire.split();
    let outcome = send_all(&mut sink, &mut stream, &files, &shared).await;
    if outcome == Outcome::Cancelled || matches!(outcome, Outcome::Failed(k) if k != FailKind::PeerLost) {
        let _ = sink.send(ctl(&BulkCtl::Cancel)).await;
    }
    let _ = sink.close().await;
    outcome
}

async fn send_all<Si, St>(sink: &mut Si, stream: &mut St, files: &[SendFile], shared: &Shared) -> Outcome
where
    Si: futures_util::Sink<bytes::Bytes, Error = io::Error> + Unpin,
    St: futures_util::Stream<Item = Result<BytesMut, io::Error>> + Unpin,
{
    for (i, f) in files.iter().enumerate() {
        let index = i as u32;
        let mut file = match tokio::fs::File::open(&f.path).await {
            Ok(file) => file,
            Err(_) => return Outcome::Failed(FailKind::Io),
        };
        match file.metadata().await {
            Ok(m) if m.is_file() && m.len() == f.size => {}
            Ok(_) => return Outcome::Failed(FailKind::FileChanged),
            Err(_) => return Outcome::Failed(FailKind::Io),
        }
        if sink.send(ctl(&BulkCtl::FileStart { index, size: f.size })).await.is_err() {
            return Outcome::Failed(FailKind::PeerLost);
        }
        let mut hasher = Sha256::new();
        let mut sent = 0u64;
        loop {
            let mut buf = BytesMut::with_capacity(MAX_FILE_CHUNK + 1);
            buf.put_u8(BULK_DATA);
            let n = match (&mut file).take(MAX_FILE_CHUNK as u64).read_buf(&mut buf).await {
                Ok(n) => n,
                Err(_) => return Outcome::Failed(FailKind::Io),
            };
            if n == 0 {
                break;
            }
            hasher.update(&buf[1..]);
            sent += n as u64;
            if sent > f.size {
                return Outcome::Failed(FailKind::FileChanged); // grew while sending
            }
            tokio::select! {
                biased;
                _ = shared.cancel.cancelled() => return Outcome::Cancelled,
                incoming = stream.next() => return match incoming {
                    // Anything arriving mid-file means the receiver stopped us.
                    Some(Ok(frame)) => match proto::decode_bulk_ctl(&frame) {
                        Ok(BulkCtl::Cancel) => Outcome::Cancelled,
                        Ok(BulkCtl::FileResult { ok: false, .. }) => Outcome::Failed(FailKind::Integrity),
                        _ => Outcome::Failed(FailKind::Protocol),
                    },
                    _ => Outcome::Failed(FailKind::PeerLost),
                },
                r = sink.send(buf.freeze()) => if r.is_err() { return Outcome::Failed(FailKind::PeerLost) },
            }
            shared.done.fetch_add(n as u64, Ordering::Relaxed);
        }
        if sent != f.size {
            return Outcome::Failed(FailKind::FileChanged); // shrank while sending
        }
        let end = ctl(&BulkCtl::FileEnd { index, sha256: hasher.finalize().into() });
        if sink.send(end).await.is_err() {
            return Outcome::Failed(FailKind::PeerLost);
        }
        let reply = tokio::select! {
            _ = shared.cancel.cancelled() => return Outcome::Cancelled,
            r = timeout(STALL, stream.next()) => r,
        };
        match reply {
            Ok(Some(Ok(frame))) => match proto::decode_bulk_ctl(&frame) {
                Ok(BulkCtl::FileResult { index: ri, ok: true, .. }) if ri == index => {}
                Ok(BulkCtl::Cancel) => return Outcome::Cancelled,
                Ok(BulkCtl::FileResult { ok: false, reason, .. }) => {
                    return Outcome::Failed(match reason {
                        Some(RejectReason::NoSpace) => FailKind::DiskFull,
                        Some(RejectReason::UnsafeName) => FailKind::UnsafeName,
                        _ => FailKind::Integrity,
                    });
                }
                _ => return Outcome::Failed(FailKind::Protocol),
            },
            Ok(_) => return Outcome::Failed(FailKind::PeerLost),
            Err(_) => return Outcome::Failed(FailKind::Timeout),
        }
    }
    if sink.send(ctl(&BulkCtl::Done)).await.is_err() {
        return Outcome::Failed(FailKind::PeerLost);
    }
    Outcome::Complete
}

#[cfg(test)]
mod tests {
    use super::*;
    use proto::bulk_wire;

    fn tmp_inbox() -> (tempfile::TempDir, Inbox) {
        let d = tempfile::tempdir().unwrap();
        let inbox = Inbox::open(&d.path().join("inbox")).unwrap();
        (d, inbox)
    }

    fn write_random(path: &Path, len: usize) -> Vec<u8> {
        let mut data = vec![0u8; len];
        rand::fill(&mut data[..]);
        std::fs::write(path, &data).unwrap();
        data
    }

    #[test]
    fn hostile_names_are_refused_and_awkward_ones_are_repaired() {
        for (bad, why) in [
            ("../etc/passwd", NameError::Separator),
            ("..\\..\\windows\\system32", NameError::Separator),
            ("/etc/passwd", NameError::Separator),
            ("C:\\boot.ini", NameError::Separator),
            ("a/b", NameError::Separator),
            ("..", NameError::Traversal),
            (".", NameError::Traversal),
            ("...", NameError::Traversal),
            ("", NameError::Empty),
            ("   ", NameError::Empty),
            ("evil\0.txt", NameError::Control),
            ("line\nbreak", NameError::Control),
            ("invoice\u{202E}fdp.exe", NameError::Control),
            ("CON", NameError::Reserved),
            ("nul.txt", NameError::Reserved),
            ("Com1.tar.gz", NameError::Reserved),
            ("LPT9", NameError::Reserved),
            ("COM¹", NameError::Reserved),
        ] {
            assert_eq!(safe_name(bad), Err(why), "{bad:?}");
        }
        assert_eq!(safe_name(&"x".repeat(MAX_FILE_NAME_BYTES + 1)), Err(NameError::TooLong));
        assert_eq!(safe_name("report.pdf").unwrap(), "report.pdf");
        assert_eq!(safe_name("a<b>c:d\"e|f?g*h.txt").unwrap(), "a_b_c_d_e_f_g_h.txt");
        assert_eq!(safe_name("trailing. .").unwrap(), "trailing");
        assert_eq!(safe_name("  lead.txt").unwrap(), "lead.txt");
        assert_eq!(safe_name(".bashrc").unwrap(), ".bashrc");
        assert_eq!(safe_name("日本語のファイル.txt").unwrap(), "日本語のファイル.txt");
        assert_eq!(safe_name("console.txt").unwrap(), "console.txt", "only exact device names are reserved");
    }

    #[test]
    fn dedupe_is_case_insensitive_and_keeps_extensions() {
        let taken = |n: &str| ["a.txt", "a (2).txt", "noext"].iter().any(|t| t.eq_ignore_ascii_case(n));
        assert_eq!(dedupe("A.TXT", &taken), "A (3).TXT");
        assert_eq!(dedupe("b.txt", &taken), "b.txt");
        assert_eq!(dedupe("noext", &taken), "noext (2)");
    }

    #[test]
    fn transfer_dirs_are_always_new_and_never_reused() {
        let (_d, inbox) = tmp_inbox();
        let (_a, n1) = inbox.new_transfer_dir("Studio PC", "2026-10-06").unwrap();
        let (_b, n2) = inbox.new_transfer_dir("Studio PC", "2026-10-06").unwrap();
        assert_ne!(n1, n2);
        // A hostile label cannot climb out.
        let (_c, n3) = inbox.new_transfer_dir("../../evil", "x").unwrap();
        assert!(!n3.contains('/') && !n3.contains(".."), "{n3}");
    }

    async fn transfer(files: Vec<(&'static str, Vec<u8>)>) -> (Outcome, Outcome, tempfile::TempDir, Dir, String) {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("src");
        std::fs::create_dir(&src).unwrap();
        let mut sends = vec![];
        for (name, data) in &files {
            std::fs::write(src.join(name), data).unwrap();
            sends.push(SendFile { path: src.join(name), size: data.len() as u64 });
        }
        let inbox = Inbox::open(&d.path().join("inbox")).unwrap();
        let (dir, dir_name) = inbox.new_transfer_dir("peer", "t").unwrap();
        let (a, b) = tokio::io::duplex(1 << 20);
        let job = ReceiveJob { files: files.iter().map(|(n, c)| (n.to_string(), c.len() as u64)).collect(), dir: dir.try_clone().unwrap() };
        let rx = tokio::spawn(run_receive(bulk_wire(b), job, Shared::new()));
        let tx = run_send(bulk_wire(a), sends, Shared::new()).await;
        (tx, rx.await.unwrap(), d, dir, dir_name)
    }

    #[tokio::test]
    async fn files_arrive_intact_including_empty_and_multi_chunk() {
        let mut big = vec![0u8; MAX_FILE_CHUNK * 3 + 17];
        rand::fill(&mut big[..]);
        let (tx, rx, d, _dir, name) = transfer(vec![("empty.bin", vec![]), ("small.txt", b"hello".to_vec()), ("big.bin", big.clone())]).await;
        assert_eq!((tx, rx), (Outcome::Complete, Outcome::Complete));
        let out = d.path().join("inbox").join(name);
        assert_eq!(std::fs::read(out.join("empty.bin")).unwrap(), Vec::<u8>::new());
        assert_eq!(std::fs::read(out.join("small.txt")).unwrap(), b"hello");
        assert_eq!(std::fs::read(out.join("big.bin")).unwrap(), big);
        let leftovers: Vec<_> =
            std::fs::read_dir(&out).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().to_string()).filter(|n| n.contains(".part-")).collect();
        assert!(leftovers.is_empty(), "no partial files after success: {leftovers:?}");
    }

    #[tokio::test]
    async fn existing_files_and_symlinks_are_never_overwritten_or_followed() {
        let d = tempfile::tempdir().unwrap();
        let inbox = Inbox::open(&d.path().join("inbox")).unwrap();
        let (dir, dir_name) = inbox.new_transfer_dir("peer", "t").unwrap();
        let outside = d.path().join("outside.txt");
        std::fs::write(&outside, b"PRECIOUS").unwrap();
        let tdir = d.path().join("inbox").join(&dir_name);
        std::fs::write(tdir.join("a.txt"), b"already here").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, tdir.join("evil.txt")).unwrap();
        let src = d.path().join("s.txt");
        std::fs::write(&src, b"new data").unwrap();
        for target in ["a.txt", "evil.txt"] {
            let (a, b) = tokio::io::duplex(1 << 16);
            let job = ReceiveJob { files: vec![(target.to_string(), 8)], dir: dir.try_clone().unwrap() };
            let rx = tokio::spawn(run_receive(bulk_wire(b), job, Shared::new()));
            let tx = run_send(bulk_wire(a), vec![SendFile { path: src.clone(), size: 8 }], Shared::new()).await;
            assert_eq!((tx, rx.await.unwrap()), (Outcome::Complete, Outcome::Complete));
        }
        assert_eq!(std::fs::read(tdir.join("a.txt")).unwrap(), b"already here");
        assert_eq!(std::fs::read(tdir.join("a (2).txt")).unwrap(), b"new data");
        assert_eq!(std::fs::read(&outside).unwrap(), b"PRECIOUS", "a symlink in the folder must not redirect writes");
        #[cfg(unix)]
        assert_eq!(std::fs::read(tdir.join("evil (2).txt")).unwrap(), b"new data");
    }

    /// A hand-rolled sender that misbehaves on purpose.
    async fn rogue(frames: Vec<bytes::Bytes>, files: Vec<(String, u64)>) -> (Outcome, tempfile::TempDir, String) {
        let d = tempfile::tempdir().unwrap();
        let inbox = Inbox::open(&d.path().join("inbox")).unwrap();
        let (dir, name) = inbox.new_transfer_dir("peer", "t").unwrap();
        let (a, b) = tokio::io::duplex(1 << 20);
        let job = ReceiveJob { files, dir };
        let rx = tokio::spawn(run_receive(bulk_wire(b), job, Shared::new()));
        let mut w = bulk_wire(a);
        for f in frames {
            let _ = w.send(f).await;
        }
        let _ = w.close().await;
        drop(w);
        (rx.await.unwrap(), d, name)
    }

    fn data(b: &[u8]) -> bytes::Bytes {
        proto::encode_bulk_data(b).unwrap()
    }

    fn files_in(d: &tempfile::TempDir, name: &str) -> Vec<String> {
        std::fs::read_dir(d.path().join("inbox").join(name)).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().to_string()).collect()
    }

    #[tokio::test]
    async fn wrong_hash_is_rejected_and_nothing_is_left_behind() {
        let (out, d, name) = rogue(
            vec![ctl(&BulkCtl::FileStart { index: 0, size: 3 }), data(b"abc"), ctl(&BulkCtl::FileEnd { index: 0, sha256: [0; 32] })],
            vec![("f.txt".into(), 3)],
        )
        .await;
        assert_eq!(out, Outcome::Failed(FailKind::Integrity));
        assert!(files_in(&d, &name).is_empty(), "corrupt data must not survive");
    }

    #[tokio::test]
    async fn sender_cannot_exceed_the_declared_size() {
        let (out, d, name) = rogue(vec![ctl(&BulkCtl::FileStart { index: 0, size: 3 }), data(b"abcdef")], vec![("f.txt".into(), 3)]).await;
        assert_eq!(out, Outcome::Failed(FailKind::Integrity));
        assert!(files_in(&d, &name).is_empty());
    }

    #[tokio::test]
    async fn size_or_order_that_differs_from_the_accepted_offer_is_a_protocol_error() {
        let (out, ..) = rogue(vec![ctl(&BulkCtl::FileStart { index: 0, size: 999 })], vec![("f.txt".into(), 3)]).await;
        assert_eq!(out, Outcome::Failed(FailKind::Protocol));
        let (out, ..) = rogue(vec![ctl(&BulkCtl::FileStart { index: 1, size: 3 })], vec![("f.txt".into(), 3), ("g.txt".into(), 3)]).await;
        assert_eq!(out, Outcome::Failed(FailKind::Protocol));
        let (out, ..) = rogue(vec![data(b"data before any start")], vec![("f.txt".into(), 3)]).await;
        assert_eq!(out, Outcome::Failed(FailKind::Protocol));
        let (out, ..) = rogue(vec![ctl(&BulkCtl::Done)], vec![("f.txt".into(), 3)]).await;
        assert_eq!(out, Outcome::Failed(FailKind::Protocol), "Done before every file arrived");
    }

    #[tokio::test]
    async fn disconnect_mid_file_cleans_up_the_partial_file() {
        let (out, d, name) = rogue(vec![ctl(&BulkCtl::FileStart { index: 0, size: 100 }), data(b"only a bit")], vec![("f.txt".into(), 100)]).await;
        assert_eq!(out, Outcome::Failed(FailKind::PeerLost));
        assert!(files_in(&d, &name).is_empty());
    }

    #[tokio::test]
    async fn cancelling_stops_both_sides_and_leaves_no_files() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("big.bin");
        write_random(&src, MAX_FILE_CHUNK * 40);
        let inbox = Inbox::open(&d.path().join("inbox")).unwrap();
        let (dir, name) = inbox.new_transfer_dir("peer", "t").unwrap();
        // A tiny pipe makes the sender wait on the receiver, so we can cancel mid-flight.
        let (a, b) = tokio::io::duplex(MAX_FILE_CHUNK * 2);
        let shared = Shared::new();
        let rx = tokio::spawn(run_receive(bulk_wire(b), ReceiveJob { files: vec![("big.bin".into(), (MAX_FILE_CHUNK * 40) as u64)], dir }, Shared::new()));
        let sh2 = shared.clone();
        let canceller = tokio::spawn(async move {
            while sh2.bytes() < (MAX_FILE_CHUNK * 4) as u64 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            sh2.cancel.cancel();
        });
        let tx = run_send(bulk_wire(a), vec![SendFile { path: src, size: (MAX_FILE_CHUNK * 40) as u64 }], shared).await;
        canceller.await.unwrap();
        assert_eq!(tx, Outcome::Cancelled);
        assert_eq!(rx.await.unwrap(), Outcome::Cancelled);
        assert!(files_in(&d, &name).is_empty());
    }

    #[tokio::test]
    async fn a_file_that_changes_while_sending_is_detected() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("f.bin");
        std::fs::write(&src, b"12345").unwrap();
        let (a, b) = tokio::io::duplex(1 << 16);
        let _hold = b;
        let out = run_send(bulk_wire(a), vec![SendFile { path: src, size: 99 }], Shared::new()).await;
        assert_eq!(out, Outcome::Failed(FailKind::FileChanged));
    }

    #[test]
    fn disk_full_is_recognised() {
        assert_eq!(classify_io(&io::Error::from(io::ErrorKind::StorageFull)), FailKind::DiskFull);
        assert_eq!(classify_io(&io::Error::from_raw_os_error(28)), FailKind::DiskFull);
        assert_eq!(classify_io(&io::Error::from(io::ErrorKind::PermissionDenied)), FailKind::Io);
    }

    #[test]
    fn free_space_is_reported() {
        let (_d, inbox) = tmp_inbox();
        assert!(inbox.available_space().unwrap() > 0);
    }

    #[tokio::test]
    async fn large_file_streams_with_one_chunk_in_flight() {
        // 96 MiB by default: far larger than any buffer on either side. `SYNKFLOW_LARGE_MIB` lowers it
        // on machines with little free disk (the test writes the file and a received copy).
        let mib: u64 = std::env::var("SYNKFLOW_LARGE_MIB").ok().and_then(|v| v.parse().ok()).unwrap_or(96);
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("large.bin");
        let size: u64 = mib * 1024 * 1024;
        {
            let mut f = std::fs::File::create(&src).unwrap();
            let block: Vec<u8> = (0..1024 * 1024).map(|i| (i % 251) as u8).collect();
            for _ in 0..mib {
                std::io::Write::write_all(&mut f, &block).unwrap();
            }
        }
        let inbox = Inbox::open(&d.path().join("inbox")).unwrap();
        let (dir, name) = inbox.new_transfer_dir("peer", "t").unwrap();
        let (a, b) = tokio::io::duplex(MAX_FILE_CHUNK * 2);
        let rx = tokio::spawn(run_receive(bulk_wire(b), ReceiveJob { files: vec![("large.bin".into(), size)], dir }, Shared::new()));
        let started = std::time::Instant::now();
        let tx = run_send(bulk_wire(a), vec![SendFile { path: src.clone(), size }], Shared::new()).await;
        assert_eq!((tx, rx.await.unwrap()), (Outcome::Complete, Outcome::Complete));
        eprintln!("{mib} MiB loopback transfer took {:?}", started.elapsed());
        let out = d.path().join("inbox").join(name).join("large.bin");
        assert_eq!(std::fs::metadata(&out).unwrap().len(), size);
        let stream_hash = |p: &Path| {
            let mut f = std::fs::File::open(p).unwrap();
            let (mut h, mut buf) = (Sha256::new(), vec![0u8; 64 * 1024]);
            loop {
                let n = std::io::Read::read(&mut f, &mut buf).unwrap();
                if n == 0 {
                    break;
                }
                h.update(&buf[..n]);
            }
            h.finalize()
        };
        assert_eq!(stream_hash(&src), stream_hash(&out));
    }
}
