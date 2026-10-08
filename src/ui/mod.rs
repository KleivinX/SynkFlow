//! The Slint desktop UI: turns engine [`Snapshot`]s into view data and view
//! events into engine [`Command`]s. No policy lives here.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use slint::{ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel};

use crate::config::{AcceptMode, Config, CrossModifier, PeerPerms, Theme as ThemePref, Tri};
use crate::engine::Engine;
use crate::format;
use crate::identity::{Fingerprint, StoreKind};
use crate::layout_editor::{Defaults, DeviceInfo, Editor};
use crate::view::*;

slint::include_modules!();

const DWELL_MS: [u32; 4] = [0, 150, 300, 500];
const ZONES: [u32; 3] = [2, 4, 8];
const LOG_LEVELS: [&str; 4] = ["error", "warn", "info", "debug"];

pub struct Options {
    /// Start with only the tray item (used by "start at login").
    pub background: bool,
}

pub struct Ui {
    pub window: AppWindow,
    pub tray: AppTray,
}

struct Models {
    peers: Rc<VecModel<PeerRow>>,
    candidates: Rc<VecModel<CandidateRow>>,
    transfers: Rc<VecModel<TransferRow>>,
    tiles: Rc<VecModel<TileRow>>,
    groups: Rc<VecModel<GroupRow>>,
    guides: Rc<VecModel<GuideRow>>,
    edges: Rc<VecModel<EdgeRow>>,
    notices: Rc<VecModel<NoticeRow>>,
    problems: Rc<VecModel<SharedString>>,
    targets: Rc<VecModel<SharedString>>,
    notes: Rc<VecModel<SharedString>>,
    caps: Rc<VecModel<CapRow>>,
    interfaces: Rc<VecModel<InterfaceRow>>,
}

struct Local {
    engine: Engine,
    editor: Editor,
    snap: Option<Arc<Snapshot>>,
    models: Models,
    target_ids: Vec<Fingerprint>,
    notice_actions: HashMap<i32, NoticeAction>,
    initialized: bool,
    last_pairing_id: Option<u64>,
    pending_drops: Vec<PathBuf>,
    drop_timer: Timer,
    target_index: usize,
}

thread_local! {
    static LOCAL: RefCell<Option<Local>> = const { RefCell::new(None) };
}

fn with<R>(f: impl FnOnce(&mut Local) -> R) -> Option<R> {
    LOCAL.with(|l| l.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

fn ss(s: impl AsRef<str>) -> SharedString {
    SharedString::from(s.as_ref())
}

fn sync_model<T: Clone + PartialEq + 'static>(m: &VecModel<T>, new: Vec<T>) {
    if m.row_count() == new.len() {
        for (i, r) in new.into_iter().enumerate() {
            if m.row_data(i).as_ref() != Some(&r) {
                m.set_row_data(i, r);
            }
        }
    } else {
        m.set_vec(new);
    }
}

fn editor_defaults(cfg: &Config) -> Defaults {
    Defaults { dwell_ms: cfg.input.edge_dwell_ms, require_modifier: cfg.input.require_modifier, block_corners: cfg.input.block_corners }
}

#[cfg(target_os = "macos")]
fn os_reduce_flags() -> (bool, bool) {
    let ws = objc2_app_kit::NSWorkspace::sharedWorkspace();
    (ws.accessibilityDisplayShouldReduceMotion(), ws.accessibilityDisplayShouldReduceTransparency())
}

#[cfg(not(target_os = "macos"))]
fn os_reduce_flags() -> (bool, bool) {
    (false, false)
}

fn open_path(p: &std::path::Path) {
    let _ = std::fs::create_dir_all(p);
    #[cfg(target_os = "macos")]
    let prog = "open";
    #[cfg(target_os = "windows")]
    let prog = "explorer";
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    let prog = "xdg-open";
    let _ = std::process::Command::new(prog).arg(p).spawn();
}

/// The last `n` lines of the log file, for "Copy diagnostics".
fn log_tail(n: usize) -> String {
    let path = crate::config::AppPaths::discover().dir.join("synkflow.log");
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

fn copy_to_clipboard(text: &str) {
    if let Ok(mut c) = arboard::Clipboard::new() {
        let _ = c.set_text(text.to_string());
    }
}

// ───────────────────────────── building the UI ─────────────────────────────

pub fn build(engine: Engine, rt: tokio::runtime::Handle) -> Result<Ui, slint::PlatformError> {
    let window = AppWindow::new()?;
    let tray = AppTray::new()?;
    let snap0 = engine.snapshot();

    let models = Models {
        peers: Rc::new(VecModel::default()),
        candidates: Rc::new(VecModel::default()),
        transfers: Rc::new(VecModel::default()),
        tiles: Rc::new(VecModel::default()),
        groups: Rc::new(VecModel::default()),
        guides: Rc::new(VecModel::default()),
        edges: Rc::new(VecModel::default()),
        notices: Rc::new(VecModel::default()),
        problems: Rc::new(VecModel::default()),
        targets: Rc::new(VecModel::default()),
        notes: Rc::new(VecModel::default()),
        caps: Rc::new(VecModel::default()),
        interfaces: Rc::new(VecModel::default()),
    };
    {
        let st = window.global::<AppState>();
        st.set_peers(ModelRc::from(models.peers.clone()));
        st.set_candidates(ModelRc::from(models.candidates.clone()));
        st.set_transfers(ModelRc::from(models.transfers.clone()));
        st.set_tiles(ModelRc::from(models.tiles.clone()));
        st.set_groups(ModelRc::from(models.groups.clone()));
        st.set_guides(ModelRc::from(models.guides.clone()));
        st.set_edges(ModelRc::from(models.edges.clone()));
        st.set_notices(ModelRc::from(models.notices.clone()));
        st.set_layout_problems(ModelRc::from(models.problems.clone()));
        st.set_target_names(ModelRc::from(models.targets.clone()));
        st.set_backend_notes(ModelRc::from(models.notes.clone()));
        st.set_capability_matrix(ModelRc::from(models.caps.clone()));
        st.set_interfaces(ModelRc::from(models.interfaces.clone()));
        st.set_version(ss(env!("CARGO_PKG_VERSION")));
    }
    LOCAL.with(|l| {
        *l.borrow_mut() = Some(Local {
            engine: engine.clone(),
            editor: Editor::new(editor_defaults(&snap0.config)),
            snap: None,
            models,
            target_ids: vec![],
            notice_actions: HashMap::new(),
            initialized: false,
            last_pairing_id: None,
            pending_drops: vec![],
            drop_timer: Timer::default(),
            target_index: 0,
        });
    });

    wire_callbacks(&window, engine.clone(), rt.clone());
    wire_window(&window, &tray, engine.clone(), rt.clone());
    apply(&window, &tray, snap0);

    // Push every new snapshot to the UI thread.
    let (wk, tk) = (window.as_weak(), tray.as_weak());
    let mut rx = engine.subscribe();
    rt.spawn(async move {
        loop {
            let snap = rx.borrow_and_update().clone();
            let (wk, tk) = (wk.clone(), tk.clone());
            let _ = slint::invoke_from_event_loop(move || {
                if let (Some(w), Some(t)) = (wk.upgrade(), tk.upgrade()) {
                    apply(&w, &t, snap);
                }
            });
            if rx.changed().await.is_err() {
                break;
            }
        }
    });
    Ok(Ui { window, tray })
}

/// Show the window (unless starting in the background) and run until quit.
pub fn run(engine: Engine, rt: tokio::runtime::Handle, opts: Options) -> Result<(), slint::PlatformError> {
    #[cfg(target_os = "macos")]
    unified_title_bar()?;
    let ui = build(engine, rt)?;
    #[cfg(target_os = "macos")]
    ui.window.global::<AppState>().set_titlebar_inset(TITLEBAR_INSET);
    if !opts.background {
        ui.window.show()?;
    }
    slint::run_event_loop_until_quit()
}

/// Height (points) of a standard macOS title bar: the strip the UI leaves clear for the traffic lights.
#[cfg(target_os = "macos")]
const TITLEBAR_INSET: f32 = 28.0;

/// macOS: let the dark UI run under a transparent title bar, with the traffic lights floating over the
/// sidebar, instead of a flat grey system bar on top of it. Must run before the first window exists.
#[cfg(target_os = "macos")]
fn unified_title_bar() -> Result<(), slint::PlatformError> {
    use slint::winit_030::winit::platform::macos::WindowAttributesExtMacOS;
    slint::BackendSelector::new()
        .with_winit_window_attributes_hook(|a| a.with_titlebar_transparent(true).with_title_hidden(true).with_fullsize_content_view(true))
        .select()
}

fn quit_app(engine: Engine, rt: &tokio::runtime::Handle) {
    rt.spawn(async move {
        engine.shutdown().await;
        let _ = slint::invoke_from_event_loop(|| {
            let _ = slint::quit_event_loop();
        });
    });
}

fn wire_window(window: &AppWindow, tray: &AppTray, engine: Engine, rt: tokio::runtime::Handle) {
    // Closing hides to the tray when asked to; otherwise it quits cleanly.
    let (e, r) = (engine.clone(), rt.clone());
    window.window().on_close_requested(move || {
        let to_tray = with(|l| l.snap.as_ref().map(|s| s.config.general.close_to_tray).unwrap_or(false)).unwrap_or(false);
        if !to_tray {
            quit_app(e.clone(), &r);
        }
        slint::CloseRequestResponse::HideWindow
    });

    let wk = window.as_weak();
    tray.on_open(move || {
        if let Some(w) = wk.upgrade() {
            let _ = w.show();
        }
    });
    let e = engine.clone();
    tray.on_toggle_pause(move || {
        let paused = with(|l| l.snap.as_ref().map(|s| s.sharing.paused).unwrap_or(true)).unwrap_or(true);
        e.send(Command::SetPaused(!paused));
    });
    let e = engine.clone();
    tray.on_toggle_clipboard(move || {
        let p = with(|l| l.snap.as_ref().map(|s| s.sharing.clipboard_paused).unwrap_or(false)).unwrap_or(false);
        e.send(Command::SetClipboardPaused(!p));
    });
    let e = engine.clone();
    tray.on_return_local(move || e.send(Command::ReturnLocal));
    let (e, r) = (engine, rt);
    tray.on_quit(move || quit_app(e.clone(), &r));

    // Double-click on the title-bar strip zooms the window, as a native title bar does.
    let wk = window.as_weak();
    window.global::<AppState>().on_toggle_zoom(move || {
        if let Some(w) = wk.upgrade() {
            w.window().set_maximized(!w.window().is_maximized());
        }
    });

    // Files dropped on the window go to the chosen destination (real OS drop events).
    use slint::winit_030::{EventResult, WinitWindowAccessor, winit::event::WindowEvent};
    let wk = window.as_weak();
    window.window().on_winit_window_event(move |_, ev| {
        let Some(w) = wk.upgrade() else { return EventResult::Propagate };
        match ev {
            WindowEvent::HoveredFile(_) => w.global::<AppState>().set_drop_hover(true),
            WindowEvent::HoveredFileCancelled => w.global::<AppState>().set_drop_hover(false),
            WindowEvent::DroppedFile(p) => {
                w.global::<AppState>().set_drop_hover(false);
                let p = p.clone();
                let target = w.global::<AppState>().get_target_index().max(0) as usize;
                with(|l| {
                    l.pending_drops.push(p);
                    l.target_index = target;
                    // Several files arrive as several events; send them as one offer.
                    l.drop_timer.start(TimerMode::SingleShot, Duration::from_millis(150), || {
                        with(|l| {
                            let paths = std::mem::take(&mut l.pending_drops);
                            let peer = l.target_ids.get(l.target_index).copied().unwrap_or(Fingerprint([0; 32]));
                            l.engine.send(Command::SendFiles { peer, paths });
                        });
                    });
                });
            }
            _ => {}
        }
        EventResult::Propagate
    });
}

// ───────────────────────────── view → engine ─────────────────────────────

fn find_peer(snap: &Snapshot, id: &str) -> Option<PeerView> {
    snap.peers.iter().find(|p| p.fingerprint.hex() == id).cloned()
}

fn wire_callbacks(window: &AppWindow, engine: Engine, rt: tokio::runtime::Handle) {
    let st = window.global::<AppState>();
    let wk = window.as_weak();

    // sharing
    let e = engine.clone();
    st.on_set_paused(move |p| e.send(Command::SetPaused(p)));
    let e = engine.clone();
    st.on_set_clipboard_paused(move |p| e.send(Command::SetClipboardPaused(p)));
    let e = engine.clone();
    st.on_return_local(move || e.send(Command::ReturnLocal));
    let e = engine.clone();
    st.on_request_permissions(move || e.send(Command::RequestPermissions));
    let e = engine.clone();
    st.on_open_permission_settings(move || e.send(Command::OpenPermissionSettings));
    let e = engine.clone();
    st.on_recheck_permissions(move || e.send(Command::RecheckPermissions));

    // pairing
    let e = engine.clone();
    st.on_open_pairing(move || e.send(Command::OpenPairing));
    let e = engine.clone();
    st.on_close_pairing(move || e.send(Command::ClosePairing));
    let e = engine.clone();
    st.on_pair_with(move |key| {
        let addr = with(|l| l.snap.as_ref().and_then(|s| s.candidates.iter().find(|c| c.key == key.as_str()).map(|c| c.addr))).flatten();
        if let Some(a) = addr {
            e.send(Command::PairWith(a));
        }
    });
    let e = engine.clone();
    st.on_pair_manual(move |t| e.send(Command::PairManual(t.to_string())));
    let (e, w) = (engine.clone(), wk.clone());
    st.on_approve_pairing(move || {
        let Some(w) = w.upgrade() else { return };
        let s = w.global::<AppState>();
        let perms = PeerPerms {
            share_input: s.get_new_share_input(),
            accept_input: s.get_new_accept_input(),
            clipboard_send: s.get_new_clipboard(),
            clipboard_receive: s.get_new_clipboard(),
            files_receive: s.get_new_files(),
            ..PeerPerms::default()
        };
        e.send(Command::ApprovePairing { label: s.get_new_label().to_string(), perms });
    });
    let e = engine.clone();
    st.on_reject_pairing(move || e.send(Command::RejectPairing));

    // devices
    let e = engine.clone();
    st.on_peer_perm(move |id, key, on| {
        let Some(snap) = with(|l| l.snap.clone()).flatten() else { return };
        let Some(p) = find_peer(&snap, id.as_str()) else { return };
        let mut perms = p.perms;
        match key.as_str() {
            "share_input" => perms.share_input = on,
            "accept_input" => perms.accept_input = on,
            "clipboard_send" => perms.clipboard_send = on,
            "clipboard_receive" => perms.clipboard_receive = on,
            "files_receive" => {
                perms.files_receive = on;
                if !on {
                    perms.files_auto_accept = false;
                }
            }
            "files_auto_accept" => perms.files_auto_accept = on,
            "invert_scroll" => perms.invert_scroll = on,
            _ => return,
        }
        e.send(Command::SetPerms(p.fingerprint, perms));
    });
    let e = engine.clone();
    st.on_peer_translate(move |id, mode| {
        let Some(snap) = with(|l| l.snap.clone()).flatten() else { return };
        let Some(p) = find_peer(&snap, id.as_str()) else { return };
        let mut perms = p.perms;
        perms.translate_shortcuts = match mode {
            1 => Some(true),
            2 => Some(false),
            _ => None,
        };
        e.send(Command::SetPerms(p.fingerprint, perms));
    });
    let e = engine.clone();
    st.on_peer_rename(move |id, name| {
        if let Some(p) = with(|l| l.snap.as_ref().and_then(|s| find_peer(s, id.as_str()))).flatten() {
            e.send(Command::RenamePeer(p.fingerprint, name.to_string()));
        }
    });
    let e = engine.clone();
    st.on_peer_connect(move |id| {
        if let Some(p) = with(|l| l.snap.as_ref().and_then(|s| find_peer(s, id.as_str()))).flatten() {
            e.send(Command::Connect(p.fingerprint));
        }
    });
    let e = engine.clone();
    st.on_peer_disconnect(move |id| {
        if let Some(p) = with(|l| l.snap.as_ref().and_then(|s| find_peer(s, id.as_str()))).flatten() {
            e.send(Command::Disconnect(p.fingerprint));
        }
    });
    let e = engine.clone();
    st.on_peer_revoke(move |id| {
        if let Some(p) = with(|l| l.snap.as_ref().and_then(|s| find_peer(s, id.as_str()))).flatten() {
            e.send(Command::Revoke(p.fingerprint));
        }
    });
    st.on_copy_text(|t| copy_to_clipboard(t.as_str()));

    // layout
    let w = wk.clone();
    st.on_layout_select(move |tile| {
        with(|l| l.editor.select(usize::try_from(tile).ok()));
        if let Some(w) = w.upgrade() {
            refresh_layout(&w);
        }
    });
    st.on_layout_drag(move |device, dx, dy| {
        with(|l| {
            let guides = l.editor.drag_preview(device as usize, dx as f64, dy as f64);
            sync_model(
                &l.models.guides,
                guides.into_iter().map(|g| GuideRow { vertical: g.vertical, pos: g.pos as f32, from: g.from as f32, to: g.to as f32 }).collect(),
            );
        });
    });
    let w = wk.clone();
    st.on_layout_drop(move |device, dx, dy| {
        with(|l| {
            l.editor.drop_device(device as usize, dx as f64, dy as f64);
            l.models.guides.set_vec(vec![]);
        });
        if let Some(w) = w.upgrade() {
            refresh_layout(&w);
        }
    });
    let w = wk.clone();
    st.on_layout_nudge(move |tile, dx, dy| {
        with(|l| l.editor.nudge(tile as usize, dx as f64, dy as f64));
        if let Some(w) = w.upgrade() {
            refresh_layout(&w);
        }
    });
    let w = wk.clone();
    st.on_layout_toggle_tile(move |tile, on| {
        with(|l| l.editor.toggle_tile(tile as usize, on));
        if let Some(w) = w.upgrade() {
            refresh_layout(&w);
        }
    });
    let e = engine.clone();
    st.on_layout_apply(move || {
        if let Some(doc) = with(|l| (!l.editor.blocking()).then(|| l.editor.draft())).flatten() {
            e.send(Command::ApplyLayout(doc));
        }
    });
    let w = wk.clone();
    st.on_layout_revert(move || {
        with(|l| l.editor.revert());
        if let Some(w) = w.upgrade() {
            refresh_layout(&w);
        }
    });
    let w = wk.clone();
    st.on_layout_reset(move || {
        with(|l| l.editor.reset_auto());
        if let Some(w) = w.upgrade() {
            refresh_layout(&w);
        }
    });
    let w = wk.clone();
    st.on_edge_set(move |edge, enabled, dwell, modifier, corners| {
        with(|l| {
            if let Some(tile) = l.editor.selected() {
                l.editor.set_edge(tile, edge as usize, enabled, dwell.max(0) as u32, modifier, corners);
            }
        });
        if let Some(w) = w.upgrade() {
            refresh_layout(&w);
        }
    });

    // transfers
    let (e, w) = (engine.clone(), wk.clone());
    st.on_pick_files(move || {
        let idx = w.upgrade().map(|w| w.global::<AppState>().get_target_index().max(0) as usize).unwrap_or(0);
        let Some(peer) = with(|l| l.target_ids.get(idx).copied()).flatten() else { return };
        if let Some(paths) = rfd::FileDialog::new().set_title("Choose files to send").pick_files() {
            e.send(Command::SendFiles { peer, paths });
        }
    });
    let e = engine.clone();
    st.on_accept_transfer(move |id| {
        if let Ok(id) = id.parse() {
            e.send(Command::AcceptTransfer(id));
        }
    });
    let e = engine.clone();
    st.on_decline_transfer(move |id| {
        if let Ok(id) = id.parse() {
            e.send(Command::DeclineTransfer(id));
        }
    });
    let e = engine.clone();
    st.on_cancel_transfer(move |id| {
        if let Ok(id) = id.parse() {
            e.send(Command::CancelTransfer(id));
        }
    });
    let e = engine.clone();
    st.on_retry_transfer(move |id| {
        if let Ok(id) = id.parse() {
            e.send(Command::RetryTransfer(id));
        }
    });
    let e = engine.clone();
    st.on_clear_history(move || e.send(Command::ClearTransferHistory));
    st.on_open_settings_folder(|| open_path(&crate::config::AppPaths::discover().dir));
    let diag_window = window.as_weak();
    st.on_copy_diagnostics(move || {
        if let Some(w) = diag_window.upgrade() {
            let summary = w.global::<AppState>().get_diagnostics_text();
            copy_to_clipboard(&format!("{summary}\n\nRecent log:\n{}", log_tail(40)));
        }
    });
    st.on_open_inbox(|| {
        if let Some(dir) = with(|l| l.snap.as_ref().map(|s| s.config.inbox_dir())).flatten() {
            open_path(&dir);
        }
    });
    let e = engine.clone();
    st.on_choose_inbox(move || {
        if let Some(dir) = rfd::FileDialog::new().set_title("Choose where received files go").pick_folder() {
            e.send(Command::UpdateConfig(Box::new(move |c| c.files.inbox_dir = Some(dir))));
        }
    });

    // settings
    let e = engine.clone();
    st.on_set_bool(move |key, on| {
        let key = key.to_string();
        if key == "start_at_login" && crate::autostart::set_enabled(on).is_err() {
            return;
        }
        e.send(Command::UpdateConfig(Box::new(move |c| match key.as_str() {
            "start_paused" => c.general.start_paused = on,
            "close_to_tray" => c.general.close_to_tray = on,
            "start_at_login" => c.general.start_at_login = on,
            "block_corners" => c.input.block_corners = on,
            "require_modifier" => c.input.require_modifier = on,
            "shortcut_translation" => c.input.shortcut_translation = on,
            "resume_after_lock" => c.input.resume_after_lock = on,
            "stay_local" => c.input.stay_local = on,
            "accept_never" => c.files.accept_mode = if on { AcceptMode::Never } else { AcceptMode::Ask },
            "discovery" => c.network.discovery = on,
            "include_tunnels" => c.network.include_tunnels = on,
            k if k.starts_with("iface:") => {
                let name = k["iface:".len()..].to_string();
                c.network.interfaces.retain(|n| *n != name);
                if on {
                    c.network.interfaces.push(name);
                }
            }
            _ => {}
        })));
    });
    let e = engine.clone();
    st.on_set_int(move |key, v| {
        let (key, v) = (key.to_string(), v.max(0) as usize);
        e.send(Command::UpdateConfig(Box::new(move |c| match key.as_str() {
            "edge_dwell" => c.input.edge_dwell_ms = DWELL_MS[v.min(3)],
            "edge_zone" => c.input.edge_zone = ZONES[v.min(2)],
            "cross_modifier" => c.input.cross_modifier = [CrossModifier::Shift, CrossModifier::Ctrl, CrossModifier::Alt, CrossModifier::Meta][v.min(3)],
            "theme" => c.appearance.theme = [ThemePref::System, ThemePref::Dark, ThemePref::Light][v.min(2)],
            "motion" => c.appearance.reduced_motion = [Tri::System, Tri::On, Tri::Off][v.min(2)],
            "transparency" => c.appearance.reduced_transparency = [Tri::System, Tri::On, Tri::Off][v.min(2)],
            "text_scale" => c.appearance.text_scale = [100, 115, 130][v.min(2)],
            "log_level" => c.diagnostics.log_level = LOG_LEVELS[v.min(3)].to_string(),
            _ => {}
        })));
    });
    let e = engine.clone();
    st.on_set_float(move |key, v| {
        let key = key.to_string();
        e.send(Command::UpdateConfig(Box::new(move |c| {
            if key == "pointer_sensitivity" {
                c.input.pointer_sensitivity = v;
            }
        })));
    });
    let e = engine.clone();
    st.on_set_text(move |key, t| {
        let (key, t) = (key.to_string(), t.to_string());
        e.send(Command::UpdateConfig(Box::new(move |c| match key.as_str() {
            "device_name" => {
                let n = crate::proto::clean_text(&t, crate::limits::MAX_NAME_BYTES);
                if !n.is_empty() {
                    c.device_name = n;
                }
            }
            "panic_hotkey" if crate::keys::Hotkey::parse(&t).is_ok() => c.input.panic_hotkey = t,
            "switch_hotkey" if crate::keys::Hotkey::parse(&t).is_ok() => c.input.switch_hotkey = t,
            "port" => {
                if let Ok(p) = t.trim().parse::<u16>() {
                    c.network.port = p;
                }
            }
            _ => {}
        })));
    });
    let e = engine.clone();
    st.on_dismiss_notice(move |id| e.send(Command::DismissNotice(id as u64)));
    let (e, w) = (engine.clone(), wk.clone());
    st.on_notice_action(move |id| {
        let action = with(|l| l.notice_actions.get(&id).copied()).flatten();
        if let Some(w) = w.upgrade() {
            let s = w.global::<AppState>();
            match action {
                Some(NoticeAction::OpenPermissionSettings) => e.send(Command::OpenPermissionSettings),
                Some(NoticeAction::OpenPairing) => {
                    e.send(Command::OpenPairing);
                    s.set_pair_sheet(true);
                }
                Some(NoticeAction::OpenDevices) => s.set_page(2),
                Some(NoticeAction::OpenSettings) => s.set_page(4),
                Some(NoticeAction::Retry) | None => {}
            }
        }
        e.send(Command::DismissNotice(id as u64));
    });
    let (e, w) = (engine.clone(), wk);
    st.on_finish_onboarding(move |start| {
        e.send(Command::UpdateConfig(Box::new(|c| c.onboarding_done = true)));
        e.send(Command::SetPaused(!start));
        if let Some(w) = w.upgrade() {
            w.global::<AppState>().set_onboarding(false);
        }
    });
    let e = engine.clone();
    st.on_delete_all_data(move || e.send(Command::DeleteAllLocalData));
    let (e, r) = (engine, rt);
    st.on_quit(move || quit_app(e.clone(), &r));
}

// ───────────────────────────── engine → view ─────────────────────────────

fn status_text(s: PeerStatus) -> (&'static str, &'static str) {
    match s {
        PeerStatus::Offline => ("Offline", "idle"),
        PeerStatus::Connecting => ("Connecting", "info"),
        PeerStatus::Connected => ("Connected", "ok"),
        PeerStatus::RemotePaused => ("Paused there", "warn"),
        PeerStatus::RemoteLocked => ("Locked", "warn"),
        PeerStatus::Controlling => ("In use", "info"),
        PeerStatus::BeingControlled => ("Controlling you", "info"),
        PeerStatus::NotTrustedByPeer => ("Not recognised", "err"),
        PeerStatus::Blocked => ("Blocked", "err"),
    }
}

fn is_online(s: PeerStatus) -> bool {
    matches!(s, PeerStatus::Connected | PeerStatus::Controlling | PeerStatus::BeingControlled | PeerStatus::RemotePaused | PeerStatus::RemoteLocked)
}

fn state_text(s: StateLabel) -> (&'static str, &'static str) {
    match s {
        StateLabel::Disconnected => ("Not connected", "idle"),
        StateLabel::Local => ("Sharing", "ok"),
        StateLabel::Transitioning => ("Switching", "info"),
        StateLabel::Controlling => ("Controlling", "info"),
        StateLabel::BeingControlled => ("Being controlled", "info"),
        StateLabel::Paused => ("Paused", "warn"),
        StateLabel::Panic => ("Stopped", "err"),
        StateLabel::Locked => ("Locked", "warn"),
        StateLabel::NeedsPermission => ("Needs permission", "err"),
    }
}

fn pair_error_text(e: PairError) -> &'static str {
    match e {
        PairError::Expired => "The pairing request expired.",
        PairError::RejectedByPeer => "The other computer did not approve the pairing.",
        PairError::RejectedHere => "You declined the pairing.",
        PairError::Unreachable => {
            "No computer answered. Check that both are on the same network and that Synkflow is allowed through the other computer's firewall (on Windows: allow it on private networks). You can also start the pairing from the other computer, using this computer's address."
        }
        PairError::NotInvited => "That computer is not accepting pairing requests right now. Open Pair a device on it first.",
        PairError::VersionMismatch => "That computer runs an incompatible version of Synkflow.",
        PairError::Protocol => "The other computer did not answer correctly.",
        PairError::Busy => "A pairing is already in progress.",
        PairError::AlreadyPaired => "That computer is already paired.",
    }
}

fn caps_summary(c: &crate::proto::Capabilities) -> String {
    let mut v = vec![];
    if c.input_source || c.input_inject {
        v.push("keyboard and mouse");
    }
    if c.clipboard_text {
        v.push("clipboard text");
    }
    if c.clipboard_image {
        v.push("clipboard images");
    }
    if c.files {
        v.push("files");
    }
    v.join(", ")
}

fn tri_index(t: Tri) -> i32 {
    match t {
        Tri::System => 0,
        Tri::On => 1,
        Tri::Off => 2,
    }
}

fn theme_index(t: ThemePref) -> i32 {
    match t {
        ThemePref::System => 0,
        ThemePref::Dark => 1,
        ThemePref::Light => 2,
    }
}

fn apply(win: &AppWindow, tray: &AppTray, snap: Arc<Snapshot>) {
    let st = win.global::<AppState>();
    let cfg = &snap.config;
    let now = crate::config::now_secs();
    let (os_motion, os_transparency) = os_reduce_flags();

    // Theme and accessibility preferences.
    st.set_theme_pref(theme_index(cfg.appearance.theme));
    st.set_motion_pref(tri_index(cfg.appearance.reduced_motion));
    st.set_transparency_pref(tri_index(cfg.appearance.reduced_transparency));
    st.set_os_reduce_motion(os_motion);
    st.set_os_reduce_transparency(os_transparency);
    st.set_text_scale(cfg.appearance.text_scale as f32 / 100.0);

    // This computer.
    st.set_device_name(ss(&snap.device.name));
    st.set_platform_label(ss(snap.device.platform.label()));
    st.set_local_fingerprint(ss(snap.device.fingerprint.grouped_lines()));
    st.set_local_fp_short(ss(snap.device.fingerprint.short()));
    st.set_store_label(ss(match snap.device.store {
        StoreKind::OsCredentialStore => "the operating system's secure credential store",
        StoreKind::File => "a private file in your settings folder (less protected than the system keychain)",
    }));
    st.set_port_label(ss(snap.device.listen_port.to_string()));

    // Sharing.
    let (label, kind) = state_text(snap.sharing.state);
    st.set_state_label(ss(label));
    st.set_state_kind(ss(kind));
    st.set_summary(ss(&snap.sharing.summary));
    st.set_paused(snap.sharing.paused);
    st.set_clipboard_paused(snap.sharing.clipboard_paused);
    st.set_stay_local(snap.sharing.stay_local);
    st.set_panic_hotkey(ss(&snap.sharing.panic_hotkey));
    st.set_panic_live(snap.sharing.panic_hotkey_live);
    st.set_capture_text(ss(&snap.backend.capture));
    st.set_inject_text(ss(&snap.backend.inject));
    st.set_capture_ok(snap.backend.capture_ok);
    st.set_inject_ok(snap.backend.inject_ok);
    st.set_clipboard_ok(snap.backend.clipboard_ok);
    st.set_backend_api(ss(&snap.backend.api));
    let connected = snap.peers.iter().filter(|p| is_online(p.status)).count();
    st.set_connected_count(connected as i32);
    st.set_trusted_count(snap.peers.len() as i32);
    st.set_sharing_ready(snap.backend.capture_ok && !snap.peers.is_empty());
    st.set_controlling_name(ss(snap
        .sharing
        .active_peer
        .and_then(|fp| snap.peers.iter().find(|p| p.fingerprint == fp))
        .map(|p| p.label.clone())
        .unwrap_or_default()));
    st.set_no_lan(!snap.network.has_lan);
    st.set_discovery_off(!cfg.network.discovery);
    st.set_discovery_error(ss(snap.network.discovery_error.clone().unwrap_or_default()));
    st.set_cross_modifier_name(ss(match cfg.input.cross_modifier {
        CrossModifier::Shift => "Shift",
        CrossModifier::Ctrl => "Ctrl",
        CrossModifier::Alt => "Alt",
        CrossModifier::Meta => "Meta",
    }));
    let any_clip = snap.peers.iter().any(|p| p.perms.clipboard_send || p.perms.clipboard_receive);
    st.set_clipboard_state(ss(if snap.sharing.clipboard_paused {
        "Paused"
    } else if any_clip {
        "On"
    } else {
        "Off"
    }));
    st.set_input_state(ss(if snap.sharing.paused {
        "Paused"
    } else if snap.peers.iter().any(|p| p.perms.share_input || p.perms.accept_input) {
        "On"
    } else {
        "Off"
    }));

    with(|l| {
        // Devices.
        let rows: Vec<PeerRow> = snap
            .peers
            .iter()
            .map(|p| {
                let (stext, skind) = status_text(p.status);
                PeerRow {
                    id: ss(p.fingerprint.hex()),
                    label: ss(&p.label),
                    platform: ss(p.platform.label()),
                    status: ss(stext),
                    kind: ss(skind),
                    endpoint: ss(p.endpoint.clone().unwrap_or_default()),
                    fingerprint: ss(p.fingerprint.grouped_lines()),
                    last_seen: ss(format::ago(now, p.last_connected)),
                    error: ss(p.error.clone().unwrap_or_default()),
                    caps: ss(p.caps.map(|c| caps_summary(&c)).unwrap_or_default()),
                    online: is_online(p.status),
                    share_input: p.perms.share_input,
                    accept_input: p.perms.accept_input,
                    clip_send: p.perms.clipboard_send,
                    clip_receive: p.perms.clipboard_receive,
                    files_receive: p.perms.files_receive,
                    files_auto: p.perms.files_auto_accept,
                    translate: match p.perms.translate_shortcuts {
                        None => 0,
                        Some(true) => 1,
                        Some(false) => 2,
                    },
                    invert_scroll: p.perms.invert_scroll,
                }
            })
            .collect();
        sync_model(&l.models.peers, rows);
        if st.get_selected_peer() >= snap.peers.len() as i32 {
            st.set_selected_peer(-1);
        }
        sync_model(
            &l.models.candidates,
            snap.candidates
                .iter()
                .map(|c| CandidateRow {
                    key: ss(&c.key),
                    name: ss(&c.name),
                    platform: ss(c.platform.label()),
                    addr: ss(c.addr.to_string()),
                    hint: ss(&c.fp_hint),
                    paired: c.already_paired,
                    warning: ss(c
                        .resembles
                        .as_ref()
                        .map(|l| format!("Same name as {l}, but this identity is not paired. Treat with care."))
                        .unwrap_or_default()),
                })
                .collect(),
        );

        // Transfers.
        let targets: Vec<&PeerView> = snap.peers.iter().filter(|p| is_online(p.status)).collect();
        l.target_ids = targets.iter().map(|p| p.fingerprint).collect();
        sync_model(&l.models.targets, targets.iter().map(|p| ss(&p.label)).collect());
        if st.get_target_index() as usize >= targets.len().max(1) {
            st.set_target_index(0);
        }
        let rows: Vec<TransferRow> = snap
            .transfers
            .iter()
            .rev()
            .map(|t| {
                let (status, kind) = match t.status {
                    TransferStatus::WaitingForAcceptance => ("Waiting for acceptance", "warn"),
                    TransferStatus::Preparing => ("Preparing", "info"),
                    TransferStatus::Transferring => ("Transferring", "info"),
                    TransferStatus::Verifying => ("Verifying", "info"),
                    TransferStatus::Complete => ("Complete", "ok"),
                    TransferStatus::Cancelled => ("Cancelled", "idle"),
                    TransferStatus::Declined => ("Declined", "warn"),
                    TransferStatus::Failed => ("Failed", "err"),
                };
                let first = t.files.first().map(|f| f.0.clone()).unwrap_or_default();
                let more = t.files.len().saturating_sub(1);
                let title = if more == 0 { first } else { format!("{first} and {}", format::plural(more, "other file", "other files")) };
                let dir = if t.direction == Direction::Send { "To" } else { "From" };
                let running = matches!(t.status, TransferStatus::Preparing | TransferStatus::Transferring | TransferStatus::Verifying);
                TransferRow {
                    id: ss(t.id.to_string()),
                    incoming: t.direction == Direction::Receive,
                    peer: ss(&t.peer_label),
                    title: ss(title),
                    subtitle: ss(format!("{dir} {} · {}", t.peer_label, format::bytes(t.total))),
                    status: ss(status),
                    kind: ss(kind),
                    progress: if t.total == 0 { if t.status == TransferStatus::Complete { 1.0 } else { 0.0 } } else { t.done as f32 / t.total as f32 },
                    bytes: ss(format!("{} of {}", format::bytes(t.done), format::bytes(t.total))),
                    speed: ss(format::rate(t.rate_bps)),
                    eta: ss(t.eta_secs.map(format::eta).unwrap_or_default()),
                    error: ss(t.error.clone().unwrap_or_default()),
                    folder: ss(t.folder.clone().unwrap_or_default()),
                    can_accept: t.direction == Direction::Receive && t.status == TransferStatus::WaitingForAcceptance,
                    can_cancel: running || (t.direction == Direction::Send && t.status == TransferStatus::WaitingForAcceptance),
                    can_retry: t.can_retry,
                    finished: t.status.is_finished(),
                }
            })
            .collect();
        sync_model(&l.models.transfers, rows);
        st.set_active_transfers(
            snap.transfers.iter().filter(|t| matches!(t.status, TransferStatus::Transferring | TransferStatus::Verifying | TransferStatus::Preparing)).count()
                as i32,
        );
        st.set_waiting_offers(
            snap.transfers.iter().filter(|t| t.direction == Direction::Receive && t.status == TransferStatus::WaitingForAcceptance).count() as i32
        );

        // Notices.
        l.notice_actions.clear();
        let rows: Vec<NoticeRow> = snap
            .notices
            .iter()
            .map(|n| {
                if let Some((a, _)) = &n.action {
                    l.notice_actions.insert(n.id as i32, *a);
                }
                NoticeRow {
                    id: n.id as i32,
                    level: ss(match n.level {
                        Level::Info => "info",
                        Level::Warn => "warn",
                        Level::Error => "err",
                    }),
                    what: ss(&n.what),
                    safe: ss(&n.safe),
                    action: ss(n.action.as_ref().map(|(_, t)| t.clone()).unwrap_or_default()),
                    action_id: 0,
                }
            })
            .collect();
        sync_model(&l.models.notices, rows);

        // Backend notes and the capability matrix.
        sync_model(&l.models.notes, snap.backend.notes.iter().map(ss).collect());
        let discovery = if snap.network.discovery_on {
            "On".to_string()
        } else if let Some(e) = &snap.network.discovery_error {
            format!("Unavailable — {e}")
        } else {
            "Off (turned off in Settings)".to_string()
        };
        let panic_status = if snap.sharing.panic_hotkey_live {
            "Active now".to_string()
        } else {
            format!("{} becomes active when sharing is on; the Pause button and menu-bar item always work", snap.sharing.panic_hotkey)
        };
        let caps = vec![
            CapRow { name: ss("Read keyboard and mouse"), status: ss(&snap.backend.capture), ok: snap.backend.capture_ok },
            CapRow { name: ss("Be controlled by another computer"), status: ss(&snap.backend.inject), ok: snap.backend.inject_ok },
            CapRow {
                name: ss("Clipboard (text and images)"),
                status: ss(if snap.backend.clipboard_ok { "Available" } else { "Unavailable on this system" }),
                ok: snap.backend.clipboard_ok,
            },
            CapRow { name: ss("File transfer"), status: ss("Available (files only; no folders, no resume)"), ok: true },
            CapRow { name: ss("Discovery (mDNS)"), status: ss(discovery), ok: snap.network.discovery_on },
            CapRow { name: ss("Emergency shortcut"), status: ss(panic_status), ok: snap.sharing.panic_hotkey_live },
            CapRow {
                name: ss("Identity key storage"),
                status: ss(match snap.device.store {
                    StoreKind::OsCredentialStore => "System credential store",
                    StoreKind::File => "Private file (less protected)",
                }),
                ok: snap.device.store == StoreKind::OsCredentialStore,
            },
            CapRow { name: ss("Display server"), status: ss(snap.device.platform.label()), ok: true },
        ];
        sync_model(&l.models.caps, caps);
        let sel = &cfg.network.interfaces;
        sync_model(
            &l.models.interfaces,
            snap.network.interfaces.iter().map(|(n, t)| InterfaceRow { name: ss(n), tunnel: *t, selected: sel.contains(n) }).collect(),
        );

        st.set_local_addrs(ss(snap.network.addresses.join("  ·  ")));
        st.set_diagnostics_text(ss(format!(
            "Synkflow {}\nPlatform: {}\nInput API: {}\nCapture: {}\nInjection: {}\nListening on TCP port {}\nAddresses: {}\nDiscovery: {}\nTrusted devices: {} ({} connected)\nIdentity: {}…\nProtocol: v{}.{}",
            env!("CARGO_PKG_VERSION"),
            snap.device.platform.label(),
            snap.backend.api,
            snap.backend.capture,
            snap.backend.inject,
            snap.device.listen_port,
            if snap.network.addresses.is_empty() { "none on a local network".to_string() } else { snap.network.addresses.join(", ") },
            if snap.network.discovery_on { "on" } else { "off" },
            snap.peers.len(),
            connected,
            snap.device.fingerprint.short(),
            crate::limits::PROTOCOL_MAJOR,
            crate::limits::PROTOCOL_MINOR,
        )));

        // Pairing.
        match &snap.pairing {
            Some(p) => {
                st.set_pairing_active(true);
                st.set_pair_initiator(p.initiator);
                st.set_pair_stage(ss(match p.stage {
                    PairStage::Connecting => "connecting",
                    PairStage::Verify => "verify",
                    PairStage::WaitingForPeer => "waiting",
                    PairStage::Done => "done",
                    PairStage::Failed => "failed",
                }));
                st.set_pair_peer_name(ss(p.peer_name.clone().unwrap_or_default()));
                st.set_pair_peer_platform(ss(p.peer_platform.map(|x| x.label()).unwrap_or("")));
                st.set_pair_peer_fp(ss(p.peer_fingerprint.map(|f| f.grouped_lines()).unwrap_or_default()));
                st.set_pair_error(ss(p.error.map(pair_error_text).unwrap_or("")));
                st.set_pair_warning(ss(p
                    .resembles
                    .as_ref()
                    .map(|l| format!("This device has the same name as {l}, which you already trust, but its identity is different. A name proves nothing: only approve if the fingerprints match."))
                    .unwrap_or_default()));
                st.set_pair_seconds(p.expires_in_secs as i32);
                if l.last_pairing_id != Some(p.id) && p.stage == PairStage::Verify {
                    l.last_pairing_id = Some(p.id);
                    st.set_new_label(ss(p.peer_name.clone().unwrap_or_default()));
                    st.set_new_share_input(true);
                    st.set_new_accept_input(true);
                    st.set_new_clipboard(false);
                    st.set_new_files(true);
                    // A request from another computer must be seen: bring up the sheet.
                    if !p.initiator && !st.get_onboarding() {
                        st.set_pair_sheet(true);
                    }
                }
                if st.get_onboarding() && st.get_onboard_step() == 2 {
                    st.set_onboard_step(3);
                }
            }
            None => {
                st.set_pairing_active(false);
                l.last_pairing_id = None;
            }
        }
        st.set_window_secs(snap.pairing_window_secs.unwrap_or(0) as i32);

        // Settings struct.
        st.set_settings(Settings {
            device_name: ss(&cfg.device_name),
            start_paused: cfg.general.start_paused,
            close_to_tray: cfg.general.close_to_tray,
            start_at_login: cfg.general.start_at_login,
            edge_dwell: DWELL_MS.iter().position(|d| *d == cfg.input.edge_dwell_ms).unwrap_or(0) as i32,
            edge_zone: ZONES.iter().position(|z| *z == cfg.input.edge_zone).unwrap_or(0) as i32,
            block_corners: cfg.input.block_corners,
            require_modifier: cfg.input.require_modifier,
            cross_modifier: match cfg.input.cross_modifier {
                CrossModifier::Shift => 0,
                CrossModifier::Ctrl => 1,
                CrossModifier::Alt => 2,
                CrossModifier::Meta => 3,
            },
            sensitivity: cfg.input.pointer_sensitivity,
            shortcut_translation: cfg.input.shortcut_translation,
            panic_hotkey: ss(&cfg.input.panic_hotkey),
            switch_hotkey: ss(&cfg.input.switch_hotkey),
            resume_after_lock: cfg.input.resume_after_lock,
            stay_local: cfg.input.stay_local,
            clipboard_paused: cfg.clipboard_paused,
            accept_never: cfg.files.accept_mode == AcceptMode::Never,
            inbox: ss(cfg.inbox_dir().to_string_lossy()),
            discovery: cfg.network.discovery,
            include_tunnels: cfg.network.include_tunnels,
            port: ss(cfg.network.port.to_string()),
            theme: theme_index(cfg.appearance.theme),
            motion: tri_index(cfg.appearance.reduced_motion),
            transparency: tri_index(cfg.appearance.reduced_transparency),
            text_scale: match cfg.appearance.text_scale {
                130 => 2,
                115 => 1,
                _ => 0,
            },
            log_level: LOG_LEVELS.iter().position(|l| *l == cfg.diagnostics.log_level).unwrap_or(1) as i32,
        });

        if !l.initialized {
            l.initialized = true;
            st.set_onboarding(!cfg.onboarding_done);
            if !cfg.onboarding_done {
                st.set_onboard_step(0);
            }
        }

        // Layout editor.
        let devices: Vec<DeviceInfo> = snap
            .layout
            .devices
            .iter()
            .map(|d| DeviceInfo { fp: d.fingerprint, label: d.label.clone(), local: d.is_local, online: d.online, displays: d.displays.clone() })
            .collect();
        l.editor.sync(devices, &snap.layout.doc, snap.sharing.active_peer, editor_defaults(cfg));
        l.snap = Some(snap.clone());
    });
    refresh_layout(win);

    // Tray: state in the shape of the mark and in words.
    let mode = if snap.sharing.paused {
        "paused"
    } else if connected > 0 {
        "active"
    } else {
        "idle"
    };
    tray.set_mode(ss(mode));
    tray.set_status_text(ss(&snap.sharing.summary));
    tray.set_devices_text(ss(match connected {
        0 => "No devices connected".to_string(),
        1 => "1 device connected".to_string(),
        n => format!("{n} devices connected"),
    }));
    tray.set_clipboard_paused(snap.sharing.clipboard_paused);
    tray.set_controlling(matches!(snap.sharing.state, StateLabel::Controlling | StateLabel::BeingControlled | StateLabel::Transitioning));
    tray.set_dark_icon(win.global::<AppState>().get_system_dark());
}

fn refresh_layout(win: &AppWindow) {
    let st = win.global::<AppState>();
    with(|l| {
        let tiles: Vec<TileRow> = l
            .editor
            .tiles()
            .into_iter()
            .map(|t| TileRow {
                index: t.index as i32,
                device: t.device as i32,
                label: ss(&t.label),
                device_label: ss(&t.device_label),
                detail: ss(&t.detail),
                x: t.rect.x as f32,
                y: t.rect.y as f32,
                w: t.rect.w as f32,
                h: t.rect.h as f32,
                local: t.local,
                enabled: t.enabled,
                online: t.online,
                invalid: t.invalid,
            })
            .collect();
        sync_model(&l.models.tiles, tiles);
        sync_model(
            &l.models.groups,
            l.editor
                .groups()
                .into_iter()
                .map(|g| GroupRow {
                    index: g.index as i32,
                    label: ss(&g.label),
                    x: g.rect.x as f32,
                    y: g.rect.y as f32,
                    w: g.rect.w as f32,
                    h: g.rect.h as f32,
                    local: g.local,
                    online: g.online,
                    invalid: g.invalid,
                    unreachable: g.unreachable,
                    active: g.active,
                })
                .collect(),
        );
        let b = l.editor.bounds();
        st.set_bounds_x(b.x as f32);
        st.set_bounds_y(b.y as f32);
        st.set_bounds_w(b.w.max(1.0) as f32);
        st.set_bounds_h(b.h.max(1.0) as f32);
        st.set_layout_dirty(l.editor.dirty());
        sync_model(&l.models.problems, l.editor.problems().into_iter().map(|p| ss(p.text)).collect());
        match l.editor.selected() {
            Some(t) => {
                st.set_selected_tile(t as i32);
                if let Some((title, detail)) = l.editor.tile_title(t) {
                    st.set_inspector_title(ss(title));
                    st.set_inspector_detail(ss(detail));
                }
                let tiles = l.editor.tiles();
                st.set_inspector_enabled(tiles.get(t).map(|x| x.enabled).unwrap_or(true));
                st.set_inspector_can_toggle(true);
                sync_model(
                    &l.models.edges,
                    l.editor
                        .edges_of(t)
                        .into_iter()
                        .map(|e| EdgeRow {
                            side: ss(match e.side {
                                crate::geometry::Side::Left => "Left",
                                crate::geometry::Side::Right => "Right",
                                crate::geometry::Side::Top => "Top",
                                crate::geometry::Side::Bottom => "Bottom",
                            }),
                            neighbor: ss(e.neighbor),
                            span: ss(format!("{:.0}–{:.0} pt", e.span.0, e.span.1)),
                            enabled: e.enabled,
                            dwell: e.dwell_ms as i32,
                            modifier: e.modifier,
                            corners: e.corners,
                        })
                        .collect(),
                );
            }
            None => {
                st.set_selected_tile(-1);
                st.set_inspector_title(ss(""));
                st.set_inspector_detail(ss(""));
                st.set_inspector_can_toggle(false);
                l.models.edges.set_vec(vec![]);
            }
        }
    });
}
