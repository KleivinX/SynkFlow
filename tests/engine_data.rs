mod common;

use std::time::Duration;

use common::*;
use synkflow::clipboard::encode_png_rgba;
use synkflow::config::PeerPerms;
use synkflow::limits::*;
use synkflow::platform::ClipContent;
use synkflow::proto::{ClipKind, FileMeta, InputEvent, Msg, RejectReason};
use synkflow::view::*;

async fn connected(a_gives_b: PeerPerms, b_gives_a: PeerPerms) -> (Node, Node) {
    let a = Node::new("Alpha", 1000, 800).await;
    let b = Node::new("Beta", 1000, 800).await;
    pair(&a, &b, a_gives_b, b_gives_a).await;
    // Let the clipboard workers react to the new permissions.
    tokio::time::sleep(Duration::from_millis(700)).await;
    (a, b)
}

async fn eventually(what: &str, secs: u64, f: impl Fn() -> bool) {
    let end = tokio::time::Instant::now() + Duration::from_secs(secs);
    while !f() {
        assert!(tokio::time::Instant::now() < end, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn text(s: &str) -> ClipContent {
    ClipContent::Text(s.into())
}

// ───────────────────────────── clipboard ─────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clipboard_text_syncs_once_and_never_bounces_back() {
    let (a, b) = connected(full_perms(), full_perms()).await;
    a.clip.user_copy(text("hello from alpha"), false);
    eventually("B receives the text", 6, || b.clip.content() == Some(text("hello from alpha"))).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(b.clip.write_count(), 1, "applied exactly once");
    assert_eq!(a.clip.write_count(), 0, "B must not echo what it just received");
    // The other direction works too, and also does not ping-pong.
    b.clip.user_copy(text("reply from beta"), false);
    eventually("A receives", 6, || a.clip.content() == Some(text("reply from beta"))).await;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!((a.clip.write_count(), b.clip.write_count()), (1, 1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clipboard_images_cross_in_bounded_chunks() {
    let (a, b) = connected(full_perms(), full_perms()).await;
    // Incompressible noise → a multi-chunk PNG of roughly a megabyte.
    let mut rgba = vec![0u8; 500 * 500 * 4];
    rand::fill(&mut rgba[..]);
    let png = encode_png_rgba(500, 500, &rgba).unwrap();
    assert!(png.len() > CLIPBOARD_CHUNK * 4);
    a.clip.user_copy(ClipContent::Image(png.clone()), false);
    eventually("B receives the image", 10, || b.clip.content() == Some(ClipContent::Image(png.clone()))).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clipboard_respects_permissions_pause_and_private_markers() {
    // B does not let A send it clipboard content.
    let b_gives_a = PeerPerms { clipboard_receive: false, ..full_perms() };
    let (a, b) = connected(full_perms(), b_gives_a).await;
    a.clip.user_copy(text("should not arrive"), false);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(b.clip.write_count(), 0);

    // Re-enable, then pause clipboard sharing on A.
    b.engine.send(Command::SetPerms(a.fp, full_perms()));
    tokio::time::sleep(Duration::from_millis(600)).await;
    a.engine.send(Command::SetClipboardPaused(true));
    a.wait("paused", |s| s.sharing.clipboard_paused).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    a.clip.user_copy(text("paused copy"), false);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(b.clip.write_count(), 0, "paused clipboard never leaves this computer");

    // Resume: a normal copy flows, a password-manager copy does not.
    a.engine.send(Command::SetClipboardPaused(false));
    tokio::time::sleep(Duration::from_millis(700)).await;
    a.clip.user_copy(text("hunter2"), true);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(b.clip.write_count(), 0, "content marked private is skipped");
    assert!(a.snap().notices.iter().any(|n| n.what.contains("marked private")));
    a.clip.user_copy(text("plain"), false);
    eventually("plain text arrives", 6, || b.clip.content() == Some(text("plain"))).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clipboard_does_not_sync_while_sharing_is_paused() {
    let (a, b) = connected(full_perms(), full_perms()).await;
    a.engine.send(Command::SetPaused(true));
    a.wait("paused", |s| s.sharing.paused).await;
    tokio::time::sleep(Duration::from_millis(700)).await;
    a.clip.user_copy(text("while paused"), false);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(b.clip.write_count(), 0);
}

// ───────────────────────────── files ─────────────────────────────

fn make_file(dir: &std::path::Path, name: &str, len: usize) -> (std::path::PathBuf, Vec<u8>) {
    let mut data = vec![0u8; len];
    rand::fill(&mut data[..]);
    let p = dir.join(name);
    std::fs::write(&p, &data).unwrap();
    (p, data)
}

fn inbox_files(n: &Node) -> Vec<std::path::PathBuf> {
    let mut out = vec![];
    if let Ok(rd) = std::fs::read_dir(n.dir.path().join("inbox")) {
        for sub in rd.flatten() {
            if let Ok(inner) = std::fs::read_dir(sub.path()) {
                out.extend(inner.flatten().map(|e| e.path()));
            }
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn files_need_acceptance_then_arrive_intact_with_verification() {
    let (a, b) = connected(full_perms(), full_perms()).await;
    let src = tempfile::tempdir().unwrap();
    let (p1, d1) = make_file(src.path(), "photo.bin", 3 * 1024 * 1024 + 123);
    let (p2, d2) = make_file(src.path(), "notes.txt", 42);
    a.engine.send(Command::SendFiles { peer: b.fp, paths: vec![p1, p2] });

    // B is asked; nothing is written until it says yes.
    let sb = b.wait("offer shown", |s| s.transfers.iter().any(|t| t.status == TransferStatus::WaitingForAcceptance)).await;
    let t = &sb.transfers[0];
    assert_eq!((t.direction, t.files.len(), t.total), (Direction::Receive, 2, 3 * 1024 * 1024 + 123 + 42));
    assert!(inbox_files(&b).is_empty(), "nothing is saved before acceptance");
    b.engine.send(Command::AcceptTransfer(t.id));

    b.wait("received", |s| s.transfers.iter().all(|t| t.status == TransferStatus::Complete) && !s.transfers.is_empty()).await;
    a.wait("sent", |s| s.transfers.iter().all(|t| t.status == TransferStatus::Complete) && !s.transfers.is_empty()).await;
    let files = inbox_files(&b);
    assert_eq!(files.len(), 2, "{files:?}");
    let by = |n: &str| files.iter().find(|f| f.file_name().unwrap() == n).cloned().unwrap();
    assert_eq!(std::fs::read(by("photo.bin")).unwrap(), d1);
    assert_eq!(std::fs::read(by("notes.txt")).unwrap(), d2);
    assert!(files.iter().all(|f| !f.file_name().unwrap().to_string_lossy().contains(".part")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn declining_or_cancelling_an_offer_writes_nothing() {
    let (a, b) = connected(full_perms(), full_perms()).await;
    let src = tempfile::tempdir().unwrap();
    let (p, _) = make_file(src.path(), "x.bin", 1000);
    a.engine.send(Command::SendFiles { peer: b.fp, paths: vec![p.clone()] });
    let sb = b.wait("offer", |s| s.transfers.iter().any(|t| t.status == TransferStatus::WaitingForAcceptance)).await;
    b.engine.send(Command::DeclineTransfer(sb.transfers[0].id));
    a.wait("declined", |s| s.transfers.iter().any(|t| t.status == TransferStatus::Declined)).await;
    assert!(inbox_files(&b).is_empty());

    // The sender can also withdraw an offer; the receiver's prompt disappears.
    a.engine.send(Command::SendFiles { peer: b.fp, paths: vec![p] });
    let sa = a.wait("second offer", |s| s.transfers.iter().any(|t| t.status == TransferStatus::WaitingForAcceptance)).await;
    let id = sa.transfers.iter().find(|t| t.status == TransferStatus::WaitingForAcceptance).unwrap().id;
    a.engine.send(Command::CancelTransfer(id));
    b.wait("receiver sees cancel", |s| s.transfers.iter().filter(|t| t.id == id).all(|t| t.status == TransferStatus::Cancelled)).await;
    assert!(inbox_files(&b).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trusted_peer_auto_accept_still_never_overwrites() {
    let b_gives_a = PeerPerms { files_auto_accept: true, ..full_perms() };
    let (a, b) = connected(full_perms(), b_gives_a).await;
    let src = tempfile::tempdir().unwrap();
    let (p, d) = make_file(src.path(), "same.txt", 5000);
    for _ in 0..2 {
        a.engine.send(Command::SendFiles { peer: b.fp, paths: vec![p.clone()] });
        tokio::time::sleep(Duration::from_millis(1200)).await;
    }
    b.wait("both complete", |s| s.transfers.iter().filter(|t| t.status == TransferStatus::Complete).count() == 2).await;
    let files = inbox_files(&b);
    assert_eq!(files.len(), 2, "two separate transfer folders: {files:?}");
    assert!(files.iter().all(|f| std::fs::read(f).unwrap() == d));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sending_to_a_device_that_does_not_accept_files_is_refused_up_front() {
    let b_gives_a = PeerPerms { files_receive: false, ..full_perms() };
    let (a, b) = connected(full_perms(), b_gives_a).await;
    let src = tempfile::tempdir().unwrap();
    let (p, _) = make_file(src.path(), "x.bin", 10);
    a.engine.send(Command::SendFiles { peer: b.fp, paths: vec![p] });
    a.wait("refused", |s| s.notices.iter().any(|n| n.what.contains("does not accept files"))).await;
    assert!(a.snap().transfers.is_empty() && b.snap().transfers.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn folders_are_not_sent_and_the_user_is_told() {
    let (a, b) = connected(full_perms(), full_perms()).await;
    let src = tempfile::tempdir().unwrap();
    a.engine.send(Command::SendFiles { peer: b.fp, paths: vec![src.path().to_path_buf()] });
    a.wait("explained", |s| s.notices.iter().any(|n| n.what.contains("Folders cannot be sent yet"))).await;
    assert!(a.snap().transfers.is_empty());
}

// ───────────────────────────── hostile peers ─────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unpaired_identity_cannot_open_a_session() {
    let target = Node::new("Target", 1000, 800).await;
    let stranger = Rogue::new();
    let r = stranger.connect(&target).await;
    assert!(r.is_err(), "the pinned handshake must refuse an identity nobody approved");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(target.snap().peers.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hostile_file_offers_are_declined_without_touching_the_disk() {
    let target = Node::new("Target", 1000, 800).await;
    let rogue = Rogue::new();
    rogue.pair_with(&target, PeerPerms { files_receive: true, files_auto_accept: true, ..PeerPerms::default() }).await;
    let mut d = rogue.connect(&target).await.expect("trusted now");
    for bad in ["../../etc/passwd", "..\\..\\windows\\evil.exe", "/absolute/path", "CON", "name\u{0}.txt", "C:\\boot.ini"] {
        send_msg(&mut d.wire, &Msg::FileOffer { transfer_id: 77, files: vec![FileMeta { name: bad.into(), size: 4 }] }).await;
        match next_msg(&mut d.wire).await {
            Some(Msg::FileAnswer { accepted: false, reason: Some(RejectReason::UnsafeName), .. }) => {}
            other => panic!("{bad:?} should be declined as unsafe, got {other:?}"),
        }
    }
    assert!(inbox_files(&target).is_empty());
    assert!(target.snap().transfers.is_empty(), "declined offers do not even create transfers");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn control_without_permission_is_refused_and_nothing_is_injected() {
    let target = Node::new("Target", 1000, 800).await;
    let rogue = Rogue::new();
    rogue.pair_with(&target, PeerPerms::default()).await; // trusted, but no permissions at all
    let mut d = rogue.connect(&target).await.unwrap();
    send_msg(&mut d.wire, &Msg::Enter { seq: 1, display: 1, x: 5.0, y: 5.0, held: vec![] }).await;
    match next_msg(&mut d.wire).await {
        Some(Msg::EnterAck { accepted: false, reason: Some(RejectReason::NotAllowed), .. }) => {}
        other => panic!("expected refusal, got {other:?}"),
    }
    send_msg(&mut d.wire, &Msg::Input(InputEvent::Key { usage: 4, down: true, repeat: false })).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(target.input.injected().is_empty(), "input from a peer that is not in control is ignored");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clipboard_without_permission_is_aborted_and_oversize_offers_end_the_session() {
    let target = Node::new("Target", 1000, 800).await;
    let rogue = Rogue::new();
    rogue.pair_with(&target, PeerPerms::default()).await;
    let mut d = rogue.connect(&target).await.unwrap();
    send_msg(&mut d.wire, &Msg::ClipOffer { id: 5, origin: rogue.id.fingerprint(), kind: ClipKind::Text, size: 10 }).await;
    assert!(matches!(next_msg(&mut d.wire).await, Some(Msg::ClipAbort { id: 5 })));
    assert_eq!(target.clip.write_count(), 0);
    // A declared size above the limit is a protocol violation: the engine hangs up.
    let bad = Msg::ClipOffer { id: 6, origin: rogue.id.fingerprint(), kind: ClipKind::Text, size: (MAX_CLIPBOARD_TEXT + 1) as u32 };
    let raw = postcard::to_stdvec(&bad).unwrap();
    use futures_util::SinkExt;
    d.wire.send(bytes::Bytes::from(raw)).await.unwrap();
    assert!(closed_within(&mut d.wire, 5).await, "a violating peer is disconnected");
    assert_eq!(target.clip.write_count(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn garbage_frames_close_the_session_but_the_engine_keeps_serving() {
    let target = Node::new("Target", 1000, 800).await;
    let rogue = Rogue::new();
    rogue.pair_with(&target, PeerPerms::default()).await;
    let mut d = rogue.connect(&target).await.unwrap();
    use futures_util::SinkExt;
    d.wire.send(bytes::Bytes::from(vec![0xFFu8; 64])).await.unwrap();
    assert!(closed_within(&mut d.wire, 5).await);
    // A fresh connection from the same (trusted) identity still works.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let again = rogue.connect(&target).await;
    assert!(again.is_ok(), "{:?}", again.err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bulk_channel_cannot_attach_without_the_session_secret() {
    let target = Node::new("Target", 1000, 800).await;
    let rogue = Rogue::new();
    rogue.pair_with(&target, PeerPerms::default()).await;
    let d = rogue.connect(&target).await.unwrap();
    let good_key = [3u8; 16]; // the session id the rogue chose in its Hello
    // Wrong token.
    let r = session::dial_bulk(target.addr(), &rogue.id, target.fp, good_key, [9; 16], 1).await;
    assert!(r.is_err(), "wrong token must not attach");
    // Right token, but no such transfer was ever accepted.
    let r = session::dial_bulk(target.addr(), &rogue.id, target.fp, good_key, d.ack.bulk_token, 1).await;
    assert!(r.is_err(), "unknown transfer must not attach");
}

use synkflow::session;
