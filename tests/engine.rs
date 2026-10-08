mod common;

use std::time::Duration;

use common::*;
use synkflow::config::PeerPerms;
use synkflow::control::Raw;
use synkflow::keys;
use synkflow::proto::{InputEvent, MouseButton};
use synkflow::view::*;

fn key(usage: u16, down: bool) -> Raw {
    Raw::Key { usage, down, repeat: false }
}

async fn two_connected() -> (Node, Node) {
    let a = Node::new("Alpha", 1000, 800).await;
    let b = Node::new("Beta", 1000, 800).await;
    pair(&a, &b, full_perms(), full_perms()).await;
    a.engine.send(Command::ApplyLayout(layout_side_by_side(&a, &b, 1000)));
    // The layout reaches B through the session, not by magic.
    b.wait("layout synced", |s| s.config.layout.revision > 0).await;
    (a, b)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pairing_needs_both_approvals_and_shows_each_others_full_fingerprints() {
    let a = Node::new("Alpha", 1000, 800).await;
    let b = Node::new("Beta", 1000, 800).await;
    b.engine.send(Command::OpenPairing);
    b.wait("window", |s| s.pairing_window_secs.is_some()).await;
    a.engine.send(Command::PairWith(b.addr()));
    let sa = a.wait("verify", |s| s.pairing.as_ref().is_some_and(|p| p.stage == PairStage::Verify)).await;
    let sb = b.wait("verify", |s| s.pairing.as_ref().is_some_and(|p| p.stage == PairStage::Verify)).await;
    assert_eq!(sa.pairing.as_ref().unwrap().peer_fingerprint, Some(b.fp));
    assert_eq!(sb.pairing.as_ref().unwrap().peer_fingerprint, Some(a.fp));
    assert_eq!(sa.pairing.as_ref().unwrap().peer_name.as_deref(), Some("Beta"));
    // Nothing is trusted yet, on either side.
    assert!(sa.peers.is_empty() && sb.peers.is_empty());

    // Only A approves: still nothing is trusted anywhere.
    a.engine.send(Command::ApprovePairing { label: "Beta".into(), perms: full_perms() });
    a.wait("waiting", |s| s.pairing.as_ref().is_some_and(|p| p.stage == PairStage::WaitingForPeer)).await;
    settle().await;
    assert!(a.snap().peers.is_empty() && b.snap().peers.is_empty(), "one-sided approval must not create trust");

    // B approves: now both record the other.
    b.engine.send(Command::ApprovePairing { label: "Alpha".into(), perms: full_perms() });
    let sa = a.wait("A trusts B", |s| s.peers.len() == 1).await;
    let sb = b.wait("B trusts A", |s| s.peers.len() == 1).await;
    assert_eq!(sa.peers[0].fingerprint, b.fp);
    assert_eq!(sb.peers[0].fingerprint, a.fp);
    // Trust survives a restart because it was persisted.
    let cfg = synkflow::config::Config::load(&synkflow::config::AppPaths::at(a.dir.path()).config_file()).0;
    assert_eq!(cfg.peers.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn declining_on_either_side_leaves_no_trust() {
    let a = Node::new("Alpha", 1000, 800).await;
    let b = Node::new("Beta", 1000, 800).await;
    b.engine.send(Command::OpenPairing);
    b.wait("window", |s| s.pairing_window_secs.is_some()).await;
    a.engine.send(Command::PairWith(b.addr()));
    a.wait("verify", |s| s.pairing.as_ref().is_some_and(|p| p.stage == PairStage::Verify)).await;
    b.wait("verify", |s| s.pairing.as_ref().is_some_and(|p| p.stage == PairStage::Verify)).await;
    b.engine.send(Command::RejectPairing);
    let sa = a.wait("A sees rejection", |s| s.pairing.as_ref().is_some_and(|p| p.stage == PairStage::Failed)).await;
    assert!(matches!(sa.pairing.as_ref().unwrap().error, Some(PairError::RejectedByPeer)));
    assert!(sa.peers.is_empty() && b.snap().peers.is_empty());
    assert!(sa.notices.iter().any(|n| n.what.contains("did not approve")), "the failure is explained");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pairing_requests_are_ignored_unless_the_other_side_opened_a_window() {
    let a = Node::new("Alpha", 1000, 800).await;
    let b = Node::new("Beta", 1000, 800).await;
    a.engine.send(Command::PairWith(b.addr())); // B never opened pairing
    let sa = a.wait("fails", |s| s.pairing.as_ref().is_some_and(|p| p.stage == PairStage::Failed)).await;
    assert_eq!(sa.pairing.as_ref().unwrap().error, Some(PairError::NotInvited));
    assert!(b.snap().pairing.is_none() && b.snap().peers.is_empty());
    assert!(sa.notices.iter().any(|n| n.what.contains("not accepting pairing")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pointer_crosses_to_the_other_computer_and_back_with_clean_release() {
    let (a, b) = two_connected().await;
    settle().await;

    // Push against A's right edge.
    a.physical(Raw::Pointer { x: 999.0, y: 400.0, dx: 6.0, dy: 0.0 });
    b.wait("B is controlled", |s| s.sharing.state == StateLabel::BeingControlled).await;
    a.wait("A is controlling", |s| s.sharing.state == StateLabel::Controlling).await;
    assert!(a.input.grabbed(), "local input is captured while controlling");
    let inj = b.input.injected();
    assert!(inj.contains(&InputEvent::PointerAbs { display: 1, x: 2.0, y: 400.0 }), "{inj:?}");

    // Typing and clicking reach B in order.
    a.physical(key(0x04, true));
    a.physical(key(0x04, false));
    a.physical(Raw::Button { button: MouseButton::Left, down: true });
    a.physical(Raw::Button { button: MouseButton::Left, down: false });
    a.physical(Raw::Scroll { dx: 0.0, dy: 3.0, pixels: false });
    tokio::time::sleep(Duration::from_millis(250)).await;
    let inj = b.input.injected();
    let tail: Vec<_> = inj.iter().rev().take(5).rev().copied().collect();
    assert_eq!(
        tail,
        vec![
            InputEvent::Key { usage: 0x04, down: true, repeat: false },
            InputEvent::Key { usage: 0x04, down: false, repeat: false },
            InputEvent::Button { button: MouseButton::Left, down: true },
            InputEvent::Button { button: MouseButton::Left, down: false },
            InputEvent::Scroll { dx: 0.0, dy: 3.0, pixels: false },
        ]
    );

    // Hold a key and a button, then walk back over the boundary.
    a.physical(key(0x06, true));
    a.physical(Raw::Button { button: MouseButton::Right, down: true });
    a.physical(Raw::Pointer { x: 999.0, y: 400.0, dx: -30.0, dy: 0.0 });
    a.wait("A back local", |s| s.sharing.state == StateLabel::Local).await;
    b.wait("B local", |s| s.sharing.state == StateLabel::Local).await;
    assert!(!a.input.grabbed());
    tokio::time::sleep(Duration::from_millis(150)).await;
    let inj = b.input.injected();
    assert!(inj.contains(&InputEvent::Key { usage: 0x06, down: false, repeat: false }), "held key released on B: {inj:?}");
    assert!(inj.contains(&InputEvent::Button { button: MouseButton::Right, down: false }), "held button released on B");
    assert!(!a.input.warps().is_empty(), "A's cursor is placed back on its own screen");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn emergency_shortcut_returns_control_and_blocks_reentry() {
    let (a, b) = two_connected().await;
    settle().await;
    a.physical(Raw::Pointer { x: 999.0, y: 400.0, dx: 6.0, dy: 0.0 });
    b.wait("controlled", |s| s.sharing.state == StateLabel::BeingControlled).await;
    a.wait("controlling", |s| s.sharing.state == StateLabel::Controlling).await;
    a.physical(key(0x07, true)); // a key is held when panic happens
    tokio::time::sleep(Duration::from_millis(100)).await;
    for m in [keys::LCTRL, keys::LALT, keys::LSHIFT] {
        a.physical(key(m, true));
    }
    a.physical(key(keys::KEY_ESCAPE, true));
    a.wait("A paused by panic", |s| s.sharing.state == StateLabel::Panic).await;
    b.wait("B released", |s| s.sharing.state == StateLabel::Local).await;
    assert!(!a.input.grabbed());
    let inj = b.input.injected();
    assert!(inj.contains(&InputEvent::Key { usage: 0x07, down: false, repeat: false }), "no key left held on B: {inj:?}");
    assert!(a.snap().notices.iter().any(|n| n.what.contains("Emergency stop")));
    // Pushing the edge again does nothing until sharing is turned back on.
    a.physical(Raw::Pointer { x: 999.0, y: 400.0, dx: 6.0, dy: 0.0 });
    settle().await;
    assert_eq!(a.snap().sharing.state, StateLabel::Panic);
    a.engine.send(Command::SetPaused(false));
    a.wait("resumed", |s| s.sharing.state == StateLabel::Local).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_all_sharing_returns_control_immediately() {
    let (a, b) = two_connected().await;
    settle().await;
    a.physical(Raw::Pointer { x: 999.0, y: 400.0, dx: 6.0, dy: 0.0 });
    b.wait("controlled", |s| s.sharing.state == StateLabel::BeingControlled).await;
    a.engine.send(Command::SetPaused(true));
    a.wait("A paused", |s| s.sharing.state == StateLabel::Paused).await;
    b.wait("B local", |s| s.sharing.state == StateLabel::Local).await;
    assert!(!a.input.grabbed());
    assert!(a.snap().sharing.paused);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_other_computer_going_away_returns_control_and_releases_input() {
    let (a, b) = two_connected().await;
    settle().await;
    a.physical(Raw::Pointer { x: 999.0, y: 400.0, dx: 6.0, dy: 0.0 });
    a.wait("controlling", |s| s.sharing.state == StateLabel::Controlling).await;
    b.engine.shutdown().await;
    let s = a.wait("A recovers", |s| s.sharing.state != StateLabel::Controlling).await;
    assert!(!a.input.grabbed(), "local input must come back");
    assert!(s.notices.iter().any(|n| n.what.contains("returned to this computer")), "{:?}", s.notices);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revoking_a_device_ends_its_control_at_once_and_it_cannot_return() {
    let (a, b) = two_connected().await;
    settle().await;
    a.physical(Raw::Pointer { x: 999.0, y: 400.0, dx: 6.0, dy: 0.0 });
    b.wait("controlled", |s| s.sharing.state == StateLabel::BeingControlled).await;
    a.wait("controlling", |s| s.sharing.state == StateLabel::Controlling).await;
    a.physical(key(0x08, true));
    tokio::time::sleep(Duration::from_millis(100)).await;

    b.engine.send(Command::Revoke(a.fp));
    b.wait("B local and A removed", |s| s.sharing.state != StateLabel::BeingControlled && s.peers.is_empty()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(b.input.injected().contains(&InputEvent::Key { usage: 0x08, down: false, repeat: false }), "held key released on revoke");
    a.wait("A is no longer controlling", |s| s.sharing.state != StateLabel::Controlling).await;
    // A keeps retrying but the pinned handshake is refused: it never connects again.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let sa = a.snap();
    assert!(
        sa.peers.iter().all(|p| !matches!(p.status, PeerStatus::Connected | PeerStatus::Controlling)),
        "{:?}",
        sa.peers.iter().map(|p| p.status).collect::<Vec<_>>()
    );
    assert!(b.snap().peers.is_empty() && b.snap().sharing.state != StateLabel::BeingControlled);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn withdrawing_permission_mid_session_ends_control() {
    let (a, b) = two_connected().await;
    settle().await;
    a.physical(Raw::Pointer { x: 999.0, y: 400.0, dx: 6.0, dy: 0.0 });
    b.wait("controlled", |s| s.sharing.state == StateLabel::BeingControlled).await;
    // B stops allowing A to control it.
    let mut p = full_perms();
    p.accept_input = false;
    b.engine.send(Command::SetPerms(a.fp, p));
    b.wait("B local", |s| s.sharing.state == StateLabel::Local).await;
    a.wait("A local", |s| s.sharing.state == StateLabel::Local || s.sharing.state == StateLabel::Disconnected).await;
    assert!(!a.input.grabbed());
    // And A's edge no longer leads anywhere.
    a.physical(Raw::Pointer { x: 999.0, y: 400.0, dx: 6.0, dy: 0.0 });
    settle().await;
    assert_ne!(a.snap().sharing.state, StateLabel::Controlling);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_locked_receiver_refuses_entry_and_says_so() {
    let (a, b) = two_connected().await;
    settle().await;
    b.input.set_locked(true);
    b.wait("B locked", |s| s.sharing.state == StateLabel::Locked).await;
    a.wait("A sees B locked", |s| s.peers.iter().any(|p| p.status == PeerStatus::RemoteLocked)).await;
    a.physical(Raw::Pointer { x: 999.0, y: 400.0, dx: 6.0, dy: 0.0 });
    settle().await;
    assert_ne!(a.snap().sharing.state, StateLabel::Controlling);
    assert!(b.input.injected().is_empty(), "nothing may be injected into a locked machine");
    // After unlocking, sharing stays paused unless the user opted in.
    b.input.set_locked(false);
    b.wait("B stays paused", |s| s.sharing.state == StateLabel::Paused).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_the_permitted_direction_works() {
    // A may control B, but B may not control A.
    let a = Node::new("Alpha", 1000, 800).await;
    let b = Node::new("Beta", 1000, 800).await;
    let a_gives_b = PeerPerms { share_input: true, accept_input: false, ..PeerPerms::default() }; // A drives B; A refuses to be driven
    let b_gives_a = PeerPerms { share_input: false, accept_input: true, ..PeerPerms::default() }; // B accepts being driven
    pair(&a, &b, a_gives_b, b_gives_a).await;
    a.engine.send(Command::ApplyLayout(layout_side_by_side(&a, &b, 1000)));
    b.wait("layout", |s| s.config.layout.revision > 0).await;
    settle().await;
    // B → A is not allowed: B's edge has no target.
    b.physical(Raw::Pointer { x: 0.0, y: 400.0, dx: -6.0, dy: 0.0 });
    settle().await;
    assert_ne!(b.snap().sharing.state, StateLabel::Controlling);
    // A → B works.
    a.physical(Raw::Pointer { x: 999.0, y: 400.0, dx: 6.0, dy: 0.0 });
    a.wait("controlling", |s| s.sharing.state == StateLabel::Controlling).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restarted_computer_is_reconnected_automatically_without_pairing_again() {
    let a = Node::new("Alpha", 1000, 800).await;
    let b = Node::new("Beta", 1000, 800).await;
    pair(&a, &b, full_perms(), full_perms()).await;
    let (b_fp, dir) = (b.fp, b.dir);
    // Beta quits (a graceful shutdown) …
    b.engine.shutdown().await;
    a.wait("A sees B go away", |s| s.peers.iter().all(|p| !matches!(p.status, PeerStatus::Connected))).await;
    // … and starts again later from the same settings folder, on a new port.
    let b2 = Node::start_in("Beta", dir, 1000, 800).await;
    assert_eq!(b2.fp, b_fp, "the identity survives a restart");
    assert_eq!(b2.snap().peers.len(), 1, "the trust survives a restart");
    a.wait_connected(&b_fp).await;
    b2.wait_connected(&a.fp).await;
}
