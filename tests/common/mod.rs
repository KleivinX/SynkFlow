//! Shared harness: whole engines talking over loopback TLS with fake OS
//! backends. Nothing here exercises a real keyboard, mouse or clipboard.
#![allow(dead_code, clippy::field_reassign_with_default)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use synkflow::config::{AppPaths, Config, PeerPerms};
use synkflow::control::Raw;
use synkflow::engine::{Engine, EngineDeps};
use synkflow::identity::{Fingerprint, SecretStore};
use synkflow::platform::InputBackend;
use synkflow::platform::fake::{FakeClipboard, FakeInput};
use synkflow::view::*;

pub struct Node {
    pub engine: Engine,
    pub input: Arc<FakeInput>,
    pub clip: FakeClipboard,
    pub dir: tempfile::TempDir,
    pub fp: Fingerprint,
    pub port: u16,
    pub name: String,
}

pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

pub fn deps_for(dir: &std::path::Path, input: Arc<FakeInput>, clip: FakeClipboard) -> EngineDeps {
    let paths = AppPaths::at(dir);
    let file = SecretStore::File { path: paths.identity_file() };
    EngineDeps {
        paths,
        input: Arc::new(input) as Arc<dyn InputBackend>,
        clipboard: Some(Box::new(clip)),
        stores: (file.clone(), file),
        bind: "127.0.0.1".parse().unwrap(),
        discovery: false,
        allow_loopback: true,
    }
}

impl Node {
    pub async fn new(name: &str, w: u32, h: u32) -> Node {
        Self::with(name, w, h, |_| {}).await
    }

    pub async fn with(name: &str, w: u32, h: u32, edit: impl FnOnce(&mut Config)) -> Node {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.device_name = name.to_string();
        cfg.general.start_paused = false;
        cfg.onboarding_done = true;
        cfg.network.port = 0;
        cfg.network.discovery = false;
        cfg.files.inbox_dir = Some(dir.path().join("inbox"));
        edit(&mut cfg);
        std::fs::create_dir_all(dir.path()).unwrap();
        cfg.save(&AppPaths::at(dir.path()).config_file()).unwrap();
        Self::start_in(name, dir, w, h).await
    }

    pub async fn start_in(name: &str, dir: tempfile::TempDir, w: u32, h: u32) -> Node {
        let input = FakeInput::standard(w, h);
        let clip = FakeClipboard::default();
        let engine = Engine::start(deps_for(dir.path(), input.clone(), clip.clone())).await.expect("engine starts");
        let snap = engine.snapshot();
        Node { engine, input, clip, fp: snap.device.fingerprint, port: snap.device.listen_port, dir, name: name.to_string() }
    }

    pub fn addr(&self) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], self.port))
    }

    pub fn snap(&self) -> Arc<Snapshot> {
        self.engine.snapshot()
    }

    /// Wait until `f` holds for a snapshot; panics with context otherwise.
    pub async fn wait(&self, what: &str, f: impl Fn(&Snapshot) -> bool) -> Arc<Snapshot> {
        let mut rx = self.engine.subscribe();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(12);
        loop {
            let s = rx.borrow_and_update().clone();
            if f(&s) {
                return s;
            }
            if tokio::time::timeout_at(deadline, rx.changed()).await.is_err() {
                let s = rx.borrow().clone();
                panic!(
                    "[{}] timed out waiting for: {what}\n state={:?}\n peers={:#?}\n pairing={:?}\n notices={:#?}\n transfers={:#?}",
                    self.name,
                    s.sharing.state,
                    s.peers.iter().map(|p| (&p.label, p.status, &p.error)).collect::<Vec<_>>(),
                    s.pairing,
                    s.notices.iter().map(|n| &n.what).collect::<Vec<_>>(),
                    s.transfers
                );
            }
        }
    }

    pub async fn wait_connected(&self, peer: &Fingerprint) -> Arc<Snapshot> {
        let p = *peer;
        self.wait("peer connected", move |s| {
            s.peers.iter().any(|x| x.fingerprint == p && matches!(x.status, PeerStatus::Connected | PeerStatus::Controlling | PeerStatus::BeingControlled))
        })
        .await
    }

    pub fn physical(&self, raw: Raw) {
        self.input.physical(raw);
    }
}

pub fn full_perms() -> PeerPerms {
    PeerPerms {
        share_input: true,
        accept_input: true,
        clipboard_send: true,
        clipboard_receive: true,
        files_receive: true,
        files_auto_accept: false,
        translate_shortcuts: None,
        invert_scroll: false,
    }
}

/// A pairs with B: B opens its window, A connects, both compare and approve.
/// `a_gives_b` is what A lets B do; `b_gives_a` is what B lets A do.
pub async fn pair(a: &Node, b: &Node, a_gives_b: PeerPerms, b_gives_a: PeerPerms) {
    b.engine.send(Command::OpenPairing);
    b.wait("pairing window open", |s| s.pairing_window_secs.is_some()).await;
    a.engine.send(Command::PairWith(b.addr()));
    let (af, bf) = (a.fp, b.fp);
    let sa = a.wait("A sees verification", |s| s.pairing.as_ref().is_some_and(|p| p.stage == PairStage::Verify)).await;
    let sb = b.wait("B sees verification", |s| s.pairing.as_ref().is_some_and(|p| p.stage == PairStage::Verify)).await;
    // Each side shows the other's full fingerprint.
    assert_eq!(sa.pairing.as_ref().unwrap().peer_fingerprint, Some(bf));
    assert_eq!(sb.pairing.as_ref().unwrap().peer_fingerprint, Some(af));
    a.engine.send(Command::ApprovePairing { label: b.name.clone(), perms: a_gives_b });
    a.wait("A waits for B", |s| s.pairing.as_ref().is_some_and(|p| p.stage == PairStage::WaitingForPeer)).await;
    b.engine.send(Command::ApprovePairing { label: a.name.clone(), perms: b_gives_a });
    a.wait_connected(&bf).await;
    b.wait_connected(&af).await;
}

pub fn layout_side_by_side(a: &Node, b: &Node, a_w: i32) -> synkflow::geometry::LayoutDoc {
    let mut doc = synkflow::geometry::LayoutDoc::default();
    doc.set_placement(a.fp, 0, 0);
    doc.set_placement(b.fp, a_w, 0);
    doc
}

pub async fn settle() {
    tokio::time::sleep(Duration::from_millis(150)).await;
}

// ───────────────────────────── a hand-driven peer ─────────────────────────────

use futures_util::{SinkExt, StreamExt};
use synkflow::identity::Identity;
use synkflow::proto::{self, Capabilities, Grants, Hello, Msg, Platform};
use synkflow::session::{self, Dialed};

/// A peer whose messages the test writes by hand, to check how the engine
/// treats hostile or malformed traffic.
pub struct Rogue {
    pub id: Arc<Identity>,
}

pub fn hello(name: &str) -> Hello {
    Hello {
        name: name.into(),
        platform: Platform::Other,
        app_version: "rogue".into(),
        caps: Capabilities { input_source: true, input_inject: true, clipboard_text: true, clipboard_image: true, files: true },
        grants: Grants { control: true, clipboard: true, files: true },
        session_id: [3; 16],
        displays: vec![proto::DisplayInfo { id: 1, name: "R".into(), x: 0, y: 0, width: 800, height: 600, scale: 1.0, rotation: 0, primary: true }],
        paused: false,
    }
}

impl Rogue {
    pub fn new() -> Rogue {
        Rogue { id: Arc::new(Identity::generate().unwrap()) }
    }

    /// Complete a *legitimate* pairing so the engine trusts this identity.
    pub async fn pair_with(&self, target: &Node, perms: PeerPerms) {
        target.engine.send(Command::OpenPairing);
        target.wait("window", |s| s.pairing_window_secs.is_some()).await;
        let (mut stream, seen) = synkflow::tls::connect_pairing(target.addr(), &self.id).await.unwrap();
        assert_eq!(seen, target.fp);
        proto::exchange_preamble(&mut stream).await.unwrap();
        let mut wire = proto::control_wire(stream);
        wire.send(proto::encode(&Msg::PairHello { name: "Rogue".into(), platform: Platform::Other, app_version: "r".into(), listen_port: 1 }).unwrap())
            .await
            .unwrap();
        target.wait("verify", |s| s.pairing.as_ref().is_some_and(|p| p.stage == PairStage::Verify)).await;
        assert_eq!(target.snap().pairing.as_ref().unwrap().peer_fingerprint, Some(self.id.fingerprint()));
        target.engine.send(Command::ApprovePairing { label: "Rogue".into(), perms });
        wire.send(proto::encode(&Msg::PairDecision { approved: true }).unwrap()).await.unwrap();
        target.wait("trusted", |s| s.peers.iter().any(|p| p.fingerprint == self.id.fingerprint())).await;
    }

    pub async fn connect(&self, target: &Node) -> Result<Dialed, session::DialError> {
        session::dial(target.addr(), &self.id, target.fp, hello("Rogue")).await
    }
}

pub async fn next_msg(wire: &mut proto::Wire<synkflow::tls::ClientStream>) -> Option<Msg> {
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(5), wire.next()).await.ok()??.ok()?;
        match proto::decode(&frame) {
            Ok(Msg::Ping(_)) | Ok(Msg::Pong(_)) => continue,
            Ok(m) => return Some(m),
            Err(_) => return None,
        }
    }
}

pub async fn send_msg(wire: &mut proto::Wire<synkflow::tls::ClientStream>, m: &Msg) {
    wire.send(proto::encode(m).unwrap()).await.unwrap();
}

/// Wait until the engine closes the connection (true) or `secs` pass (false).
pub async fn closed_within(wire: &mut proto::Wire<synkflow::tls::ClientStream>, secs: u64) -> bool {
    let end = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        match tokio::time::timeout_at(end, wire.next()).await {
            Err(_) => return false,
            Ok(None) | Ok(Some(Err(_))) => return true,
            Ok(Some(Ok(_))) => continue,
        }
    }
}
