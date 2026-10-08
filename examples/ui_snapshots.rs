#![allow(clippy::field_reassign_with_default)]
//! Developer tool: renders every screen of the real UI to PNG files.
//!
//! The data is real engine state: two complete engines pair and talk over loopback TLS.
//! Only the *operating-system* layer (keyboard/mouse/clipboard) is the in-memory fake, so
//! this proves how the screens look and behave, never that OS capture works.
//!
//!   cargo run --release --example ui_snapshots --features dev-backend -- <output-dir>
//!   (add SYNKFLOW_SNAP_TITLEBAR=1 to render the macOS unified-title-bar layout)

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use slint::ComponentHandle;
use synkflow::config::{AppPaths, Config, PeerPerms};
use synkflow::engine::{Engine, EngineDeps};
use synkflow::identity::SecretStore;
use synkflow::platform::InputBackend;
use synkflow::platform::fake::{FakeClipboard, FakeInput};
use synkflow::ui::{AppState, AppWindow};
use synkflow::view::*;

struct Node {
    engine: Engine,
    port: u16,
    fp: synkflow::identity::Fingerprint,
}

async fn node(dir: &Path, name: &str, w: u32, h: u32, onboarding_done: bool) -> Node {
    std::fs::create_dir_all(dir).unwrap();
    let mut cfg = Config::default();
    cfg.device_name = name.into();
    cfg.general.start_paused = false;
    cfg.onboarding_done = onboarding_done;
    cfg.network.port = 0;
    cfg.network.discovery = false;
    cfg.files.inbox_dir = Some(dir.join("inbox"));
    let paths = AppPaths::at(dir);
    cfg.save(&paths.config_file()).unwrap();
    let file = SecretStore::File { path: paths.identity_file() };
    // Same stand-in input layer as the tests; only the display label differs, so pictures do not say "Fake".
    let input = FakeInput::new(vec![synkflow::proto::DisplayInfo {
        id: 1,
        name: "Main display".into(),
        x: 0,
        y: 0,
        width: w,
        height: h,
        scale: 1.0,
        rotation: 0,
        primary: true,
    }]);
    let engine = Engine::start(EngineDeps {
        paths,
        input: Arc::new(input) as Arc<dyn InputBackend>,
        clipboard: Some(Box::new(FakeClipboard::default())),
        stores: (file.clone(), file),
        bind: "127.0.0.1".parse().unwrap(),
        discovery: false,
        allow_loopback: true,
    })
    .await
    .unwrap();
    let s = engine.snapshot();
    Node { engine, port: s.device.listen_port, fp: s.device.fingerprint }
}

async fn until(e: &Engine, what: &str, f: impl Fn(&Snapshot) -> bool) {
    let mut rx = e.subscribe();
    let end = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if f(&rx.borrow_and_update()) {
            return;
        }
        if tokio::time::timeout_at(end, rx.changed()).await.is_err() {
            panic!("timed out waiting for {what}");
        }
    }
}

fn snap(out: PathBuf, name: &'static str) -> impl FnOnce(&AppWindow) + Send + 'static {
    move |w: &AppWindow| {
        let buf = w.window().take_snapshot().expect("snapshot");
        let file = std::fs::File::create(out.join(format!("{name}.png"))).unwrap();
        let mut enc = png::Encoder::new(std::io::BufWriter::new(file), buf.width(), buf.height());
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header().unwrap().write_image_data(slint::SharedPixelBuffer::as_bytes(&buf)).unwrap();
        println!("wrote {name}.png ({}x{})", buf.width(), buf.height());
    }
}

/// Notifications are real, but they cover the screen they land on: clear them so each picture shows the screen itself.
async fn quiet(e: &Engine) {
    for n in e.snapshot().notices.iter() {
        e.send(Command::DismissNotice(n.id));
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
}

/// Run `f` on the UI thread and wait a moment for the frame to settle.
async fn ui(f: impl FnOnce(&AppWindow) + Send + 'static, ui: &slint::Weak<AppWindow>) {
    let w = ui.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = w.upgrade() {
            f(&w);
        }
    });
    tokio::time::sleep(Duration::from_millis(450)).await;
}

fn main() {
    let out: PathBuf = std::env::args().nth(1).expect("output directory").into();
    std::fs::create_dir_all(&out).unwrap();
    let base = std::env::temp_dir().join(format!("synkflow-shots-{}", std::process::id()));
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let h = rt.handle().clone();
    let (a, b) =
        rt.block_on(async { (node(&base.join("a"), "MacBook Pro", 1440, 900, false).await, node(&base.join("b"), "Mac mini", 1920, 1080, true).await) });
    let ui_handle = synkflow::ui::build(a.engine.clone(), h.clone()).expect("ui");
    let weak = ui_handle.window.as_weak();
    // SYNKFLOW_SNAP_TITLEBAR=1 renders the macOS layout (UI under a transparent title bar; the bar itself is not drawn here).
    if std::env::var_os("SYNKFLOW_SNAP_TITLEBAR").is_some() {
        ui_handle.window.global::<AppState>().set_titlebar_inset(28.0);
    }
    ui_handle.window.show().unwrap();

    let script_out = out.clone();
    let (ea, eb, b_port, a_fp, b_fp) = (a.engine.clone(), b.engine.clone(), b.port, a.fp, b.fp);
    rt.spawn(async move {
        let o = script_out;
        tokio::time::sleep(Duration::from_millis(800)).await;
        // Onboarding.
        let names = ["01-welcome", "02-setup", "03-discover", "04-verify", "05-arrange"];
        for (i, n) in names.iter().enumerate() {
            let (o2, n) = (o.clone(), *n);
            ui(move |w| w.global::<AppState>().set_onboard_step(i as i32), &weak).await;
            if i == 3 {
                continue; // verify needs a live pairing; captured below
            }
            quiet(&ea).await;
            ui(snap(o2, n), &weak).await;
        }
        // Pair for real: B opens its window, A connects, both verify.
        eb.send(Command::OpenPairing);
        until(&eb, "window", |s| s.pairing_window_secs.is_some()).await;
        ea.send(Command::PairWith(format!("127.0.0.1:{b_port}").parse().unwrap()));
        until(&ea, "verify", |s| s.pairing.as_ref().is_some_and(|p| p.stage == PairStage::Verify)).await;
        until(&eb, "verify", |s| s.pairing.as_ref().is_some_and(|p| p.stage == PairStage::Verify)).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        ui(|w| w.global::<AppState>().set_onboard_step(3), &weak).await;
        quiet(&ea).await;
        ui(snap(o.clone(), "04-verify"), &weak).await;
        let full =
            PeerPerms { share_input: true, accept_input: true, clipboard_send: false, clipboard_receive: false, files_receive: true, ..PeerPerms::default() };
        ea.send(Command::ApprovePairing { label: "Mac mini".into(), perms: full.clone() });
        until(&ea, "waiting", |s| s.pairing.as_ref().is_some_and(|p| p.stage == PairStage::WaitingForPeer)).await;
        quiet(&ea).await;
        ui(snap(o.clone(), "04b-waiting"), &weak).await;
        eb.send(Command::ApprovePairing { label: "MacBook Pro".into(), perms: PeerPerms { files_auto_accept: false, ..full } });
        until(&ea, "connected", |s| s.peers.iter().any(|p| p.fingerprint == b_fp && matches!(p.status, PeerStatus::Connected))).await;
        until(&eb, "connected", |s| s.peers.iter().any(|p| p.fingerprint == a_fp && matches!(p.status, PeerStatus::Connected))).await;
        // Arrange side by side, then leave onboarding.
        let mut doc = synkflow::geometry::LayoutDoc::default();
        doc.set_placement(a_fp, 0, 0);
        doc.set_placement(b_fp, 1440, 0);
        ea.send(Command::ApplyLayout(doc));
        tokio::time::sleep(Duration::from_millis(600)).await;
        ui(|w| w.global::<AppState>().set_onboard_step(4), &weak).await;
        quiet(&ea).await;
        ui(snap(o.clone(), "05-arrange-ready"), &weak).await;
        ui(|w| w.global::<AppState>().invoke_finish_onboarding(true), &weak).await;
        // Main screens.
        for (page, n) in [(0, "10-overview"), (1, "11-layout"), (2, "12-devices"), (3, "13-transfers")] {
            ui(move |w| w.global::<AppState>().set_page(page), &weak).await;
            if page == 2 {
                ui(|w| w.global::<AppState>().set_selected_peer(0), &weak).await;
            }
            if page == 1 {
                ui(|w| w.global::<AppState>().invoke_layout_select(1), &weak).await;
            }
            quiet(&ea).await;
            ui(snap(o.clone(), n), &weak).await;
        }
        // A file offer from B waiting for A, and a finished transfer.
        let dir = std::env::temp_dir().join(format!("synkflow-shot-files-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Quarterly-report.pdf"), vec![7u8; 2_400_000]).unwrap();
        std::fs::write(dir.join("notes.txt"), b"hello").unwrap();
        eb.send(Command::SendFiles { peer: a_fp, paths: vec![dir.join("Quarterly-report.pdf"), dir.join("notes.txt")] });
        until(&ea, "offer", |s| s.transfers.iter().any(|t| t.status == TransferStatus::WaitingForAcceptance)).await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        quiet(&ea).await;
        ui(snap(o.clone(), "14-transfers-offer"), &weak).await;
        let id = ea.snapshot().transfers[0].id;
        ea.send(Command::AcceptTransfer(id));
        until(&ea, "done", |s| s.transfers.iter().all(|t| t.status == TransferStatus::Complete)).await;
        quiet(&ea).await;
        ui(snap(o.clone(), "15-transfers-done"), &weak).await;
        // Settings sections.
        ui(|w| w.global::<AppState>().set_page(4), &weak).await;
        for (i, n) in [
            "20-settings-general",
            "21-settings-input",
            "22-settings-clipboard",
            "23-settings-files",
            "24-settings-network",
            "25-settings-privacy",
            "26-settings-appearance",
            "27-settings-diagnostics",
        ]
        .iter()
        .enumerate()
        {
            ui(move |w| w.global::<AppState>().set_settings_section(i as i32), &weak).await;
            quiet(&ea).await;
            ui(snap(o.clone(), n), &weak).await;
        }
        // Pair sheet, light theme, paused, reduced transparency, large text.
        ui(
            |w| {
                w.global::<AppState>().set_page(0);
                w.global::<AppState>().set_pair_sheet(true)
            },
            &weak,
        )
        .await;
        quiet(&ea).await;
        ui(snap(o.clone(), "30-pair-sheet"), &weak).await;
        ui(|w| w.global::<AppState>().set_pair_sheet(false), &weak).await;
        ea.send(Command::UpdateConfig(Box::new(|c| c.appearance.theme = synkflow::config::Theme::Light)));
        tokio::time::sleep(Duration::from_millis(700)).await;
        for (page, n) in [(0, "40-light-overview"), (1, "41-light-layout"), (2, "42-light-devices")] {
            ui(move |w| w.global::<AppState>().set_page(page), &weak).await;
            quiet(&ea).await;
            ui(snap(o.clone(), n), &weak).await;
        }
        ea.send(Command::UpdateConfig(Box::new(|c| {
            c.appearance.theme = synkflow::config::Theme::Dark;
            c.appearance.text_scale = 130;
            c.appearance.reduced_transparency = synkflow::config::Tri::On;
            c.appearance.reduced_motion = synkflow::config::Tri::On;
        })));
        ea.send(Command::SetPaused(true));
        tokio::time::sleep(Duration::from_millis(700)).await;
        ui(|w| w.global::<AppState>().set_page(0), &weak).await;
        quiet(&ea).await;
        ui(snap(o.clone(), "50-large-text-reduced-paused"), &weak).await;
        let _ = slint::invoke_from_event_loop(|| {
            let _ = slint::quit_event_loop();
        });
    });
    slint::run_event_loop_until_quit().unwrap();
    rt.block_on(async {
        a.engine.shutdown().await;
        b.engine.shutdown().await
    });
    let _ = std::fs::remove_dir_all(&base);
}
