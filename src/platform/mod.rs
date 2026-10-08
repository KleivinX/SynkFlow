//! OS integration boundary.
//!
//! One trait for input capture/injection, one for the clipboard. Each OS gets
//! its own implementation; unsafe code lives only in those files, in small,
//! documented FFI calls. Nothing here pretends a capability exists: every
//! backend reports what it can really do right now.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use crate::control::{Action, Raw};
use crate::keys::{self, Hotkey, Mods};
use crate::proto::{DisplayInfo, InputEvent, Platform};

pub mod clipboard;
#[cfg(feature = "dev-backend")]
pub mod fake;
#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "windows")]
pub mod windows;

#[derive(Debug, thiserror::Error, Clone)]
pub enum BackendError {
    #[error("{0}")]
    Unavailable(String),
    #[error("permission needed: {0}")]
    Permission(String),
    #[error("platform call failed: {0}")]
    Failed(String),
}

/// What one capability can do *right now*.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cap {
    Available,
    /// Works once the user grants something; the text says what and where.
    NeedsPermission(String),
    /// Cannot work here, and why.
    Unavailable(String),
}

impl Cap {
    pub fn is_available(&self) -> bool {
        matches!(self, Cap::Available)
    }
    pub fn text(&self) -> String {
        match self {
            Cap::Available => "Available".into(),
            Cap::NeedsPermission(t) => format!("Needs permission — {t}"),
            Cap::Unavailable(t) => format!("Unavailable — {t}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct BackendCaps {
    pub platform: Platform,
    /// Human description, e.g. "macOS CoreGraphics event tap".
    pub api: &'static str,
    /// Read global input (and forward it to a peer).
    pub capture: Cap,
    /// Synthesise input received from a peer.
    pub inject: Cap,
    /// Hide-the-cursor / lock-the-cursor while controlling another computer.
    pub notes: Vec<String>,
}

/// Something observed by a capture backend.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CaptureEvent {
    Input(Raw),
    Hotkey(Action),
    /// The OS stopped delivering input (permission revoked, tap killed).
    Lost,
}

/// Handle to a running capture.
pub trait Capture: Send {
    /// `true`: swallow local input and freeze the cursor; `false`: undo.
    fn set_grab(&self, grab: bool);
    /// Move the local cursor to a device-local global point.
    fn warp(&self, x: f64, y: f64);
    fn set_hotkeys(&self, panic: Hotkey, switch: Hotkey);
}

pub trait InputBackend: Send + Sync {
    fn capabilities(&self) -> BackendCaps;
    fn displays(&self) -> Result<Vec<DisplayInfo>, BackendError>;
    /// Start capturing. Events flow into `sink` until the handle is dropped.
    fn start_capture(&self, sink: CaptureSink, panic: Hotkey, switch: Hotkey) -> Result<Box<dyn Capture>, BackendError>;
    /// Apply one event received from a peer. Injected events must be marked so
    /// the capture side ignores them.
    fn inject(&self, ev: &InputEvent) -> Result<(), BackendError>;
    /// Ask the OS to show its permission prompt, where one exists.
    fn request_permissions(&self) {}
    /// Open the OS settings page where the user grants permissions.
    fn open_permission_settings(&self) {}
    /// True while the session is locked / at the login window.
    fn session_locked(&self) -> bool {
        false
    }
}

// ───────────────────────────── clipboard ─────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipContent {
    Text(String),
    /// PNG bytes.
    Image(Vec<u8>),
}

pub trait ClipboardBackend: Send {
    /// Monotonic token that changes whenever the clipboard changes, when the OS
    /// offers one cheaply (macOS change count, Windows sequence number).
    fn change_token(&mut self) -> Option<u64>;
    fn read(&mut self) -> Result<Option<ClipContent>, BackendError>;
    /// Write and return the change token *after* our own write, so the watcher
    /// can ignore it.
    fn write(&mut self, c: &ClipContent) -> Result<Option<u64>, BackendError>;
    /// True if the OS marks the current content as secret/concealed
    /// (password managers do this on macOS and Windows).
    fn is_sensitive(&mut self) -> bool {
        false
    }
}

// ───────────────────────────── shared capture plumbing ─────────────────────────────

/// Bounded channel into the engine with delta-preserving motion coalescing.
#[derive(Clone)]
pub struct CaptureSink {
    tx: mpsc::Sender<CaptureEvent>,
    carry: Arc<Mutex<(f64, f64)>>,
}

impl CaptureSink {
    pub fn new(tx: mpsc::Sender<CaptureEvent>) -> Self {
        Self { tx, carry: Arc::new(Mutex::new((0.0, 0.0))) }
    }

    /// Pointer motion may be coalesced when the engine is behind; the deltas
    /// that were not delivered are added to the next event so none are lost.
    pub fn pointer(&self, x: f64, y: f64, dx: f64, dy: f64) {
        let mut carry = self.carry.lock().unwrap_or_else(|e| e.into_inner());
        let (cx, cy) = *carry;
        let ev = CaptureEvent::Input(Raw::Pointer { x, y, dx: dx + cx, dy: dy + cy });
        match self.tx.try_send(ev) {
            Ok(()) => *carry = (0.0, 0.0),
            Err(_) => *carry = (cx + dx, cy + dy),
        }
    }

    /// Key/button/scroll/hotkey events are never dropped: this blocks the OS
    /// capture thread (not the engine) until there is room.
    pub fn reliable(&self, ev: CaptureEvent) {
        if let Err(mpsc::error::TrySendError::Full(ev)) = self.tx.try_send(ev) {
            let _ = self.tx.blocking_send(ev);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Swallow,
}

/// Decides, per event, whether the OS should still deliver it locally, and
/// spots the panic / switch hotkeys. Shared by all backends so the rules
/// (never leave a key stuck locally, never hide the panic chord from us) are
/// implemented and tested once.
pub struct CaptureFilter {
    grabbed: AtomicBool,
    st: Mutex<FilterState>,
}

struct FilterState {
    /// Keys/buttons whose *press* the local OS has already seen.
    local_down: std::collections::HashSet<u16>,
    local_buttons: std::collections::HashSet<u8>,
    /// Every physical key currently down (for the modifier mask).
    held: std::collections::HashSet<u16>,
    /// Hotkey trigger keys we swallowed; their release is swallowed too.
    hot_swallowed: std::collections::HashSet<u16>,
    panic: Hotkey,
    switch: Hotkey,
}

impl CaptureFilter {
    pub fn new(panic: Hotkey, switch: Hotkey) -> Self {
        Self {
            grabbed: AtomicBool::new(false),
            st: Mutex::new(FilterState {
                local_down: Default::default(),
                local_buttons: Default::default(),
                held: Default::default(),
                hot_swallowed: Default::default(),
                panic,
                switch,
            }),
        }
    }

    pub fn set_hotkeys(&self, panic: Hotkey, switch: Hotkey) {
        let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
        st.panic = panic;
        st.switch = switch;
    }

    pub fn grabbed(&self) -> bool {
        self.grabbed.load(Ordering::SeqCst)
    }

    pub fn set_grab(&self, g: bool) {
        self.grabbed.store(g, Ordering::SeqCst);
    }

    pub fn key(&self, usage: u16, down: bool) -> (Verdict, Option<Action>) {
        let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
        if down {
            st.held.insert(usage);
        } else {
            st.held.remove(&usage);
        }
        let mods = Mods(st.held.iter().fold(0u8, |m, k| m | Mods::bit_of(*k)));
        if down && !keys::is_modifier(usage) {
            if st.hot_swallowed.contains(&usage) {
                return (Verdict::Swallow, None); // auto-repeat of a chord already handled
            }
            let action = if usage == st.panic.key && mods == st.panic.mods {
                Some(Action::Panic)
            } else if usage == st.switch.key && mods == st.switch.mods {
                Some(Action::Switch)
            } else {
                None
            };
            if let Some(a) = action {
                st.hot_swallowed.insert(usage);
                return (Verdict::Swallow, Some(a));
            }
        }
        if !down && st.hot_swallowed.remove(&usage) {
            return (Verdict::Swallow, None);
        }
        if !self.grabbed() {
            if down {
                st.local_down.insert(usage);
            } else {
                st.local_down.remove(&usage);
            }
            return (Verdict::Pass, None);
        }
        if down {
            (Verdict::Swallow, None)
        } else if st.local_down.remove(&usage) {
            // The local OS saw this key go down before the grab: let it see
            // the release or the key would stay stuck here.
            (Verdict::Pass, None)
        } else {
            (Verdict::Swallow, None)
        }
    }

    pub fn button(&self, index: u8, down: bool) -> Verdict {
        let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
        if !self.grabbed() {
            if down {
                st.local_buttons.insert(index);
            } else {
                st.local_buttons.remove(&index);
            }
            return Verdict::Pass;
        }
        if down {
            Verdict::Swallow
        } else if st.local_buttons.remove(&index) {
            Verdict::Pass
        } else {
            Verdict::Swallow
        }
    }

    /// Pointer motion and scroll: swallowed exactly while grabbed.
    pub fn motion(&self) -> Verdict {
        if self.grabbed() { Verdict::Swallow } else { Verdict::Pass }
    }
}

/// The input backend for the operating system this binary runs on.
pub fn native() -> Arc<dyn InputBackend> {
    #[cfg(target_os = "macos")]
    return Arc::new(macos::MacInput::new());
    #[cfg(target_os = "windows")]
    return Arc::new(windows::WinInput::new());
    #[cfg(target_os = "linux")]
    return Arc::new(linux::LinuxInput::new());
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    compile_error!("Synkflow has no input backend for this operating system");
}

/// Marker stamped on every event we inject so capture can ignore it.
pub const INJECT_MARKER: i64 = 0x53594E4B; // "SYNK"

pub fn current_platform() -> Platform {
    Platform::current()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter() -> CaptureFilter {
        CaptureFilter::new(Hotkey::PANIC, Hotkey::SWITCH)
    }

    #[test]
    fn everything_passes_when_not_grabbed() {
        let f = filter();
        assert_eq!(f.key(0x04, true), (Verdict::Pass, None));
        assert_eq!(f.key(0x04, false), (Verdict::Pass, None));
        assert_eq!(f.motion(), Verdict::Pass);
        assert_eq!(f.button(0, true), Verdict::Pass);
    }

    #[test]
    fn grabbed_input_is_swallowed_but_releases_of_pre_grab_keys_still_reach_the_local_os() {
        let f = filter();
        f.key(keys::LSHIFT, true); // held *before* the grab
        f.set_grab(true);
        assert_eq!(f.key(0x04, true), (Verdict::Swallow, None));
        assert_eq!(f.key(0x04, false), (Verdict::Swallow, None), "never saw it go down locally");
        assert_eq!(f.key(keys::LSHIFT, false), (Verdict::Pass, None), "else Shift sticks locally");
        assert_eq!(f.motion(), Verdict::Swallow);
    }

    #[test]
    fn mouse_button_release_after_grab_follows_the_same_rule() {
        let f = filter();
        f.button(0, true);
        f.set_grab(true);
        assert_eq!(f.button(0, false), Verdict::Pass);
        assert_eq!(f.button(1, true), Verdict::Swallow);
        assert_eq!(f.button(1, false), Verdict::Swallow);
    }

    #[test]
    fn panic_chord_is_detected_in_both_states_and_swallowed_with_its_repeats_and_release() {
        for grabbed in [false, true] {
            let f = filter();
            f.set_grab(grabbed);
            for m in [keys::LCTRL, keys::LALT, keys::LSHIFT] {
                f.key(m, true);
            }
            assert_eq!(f.key(keys::KEY_ESCAPE, true), (Verdict::Swallow, Some(Action::Panic)));
            assert_eq!(f.key(keys::KEY_ESCAPE, true), (Verdict::Swallow, None));
            assert_eq!(f.key(keys::KEY_ESCAPE, false), (Verdict::Swallow, None));
        }
    }

    #[test]
    fn hotkeys_require_the_exact_modifier_set_and_can_be_changed() {
        let f = filter();
        f.key(keys::LCTRL, true);
        f.key(keys::LALT, true);
        assert_eq!(f.key(keys::KEY_ESCAPE, true), (Verdict::Pass, None), "missing Shift");
        f.key(keys::KEY_ESCAPE, false);
        f.key(keys::LSHIFT, true);
        f.key(keys::LMETA, true);
        assert_eq!(f.key(keys::KEY_ESCAPE, true), (Verdict::Pass, None), "extra Meta");
        f.key(keys::KEY_ESCAPE, false);
        f.key(keys::LMETA, false);
        f.set_hotkeys(Hotkey::parse("Ctrl+Alt+Shift+Q").unwrap(), Hotkey::SWITCH);
        assert_eq!(f.key(0x14, true), (Verdict::Swallow, Some(Action::Panic)));
    }

    #[test]
    fn sink_coalesces_motion_without_losing_deltas_and_never_drops_keys() {
        let (tx, mut rx) = mpsc::channel(1);
        let sink = CaptureSink::new(tx);
        sink.pointer(10.0, 10.0, 1.0, 2.0); // fits
        sink.pointer(20.0, 20.0, 3.0, 4.0); // queue full → carried
        sink.pointer(30.0, 30.0, 5.0, 6.0); // still full → carried
        assert_eq!(rx.try_recv().unwrap(), CaptureEvent::Input(Raw::Pointer { x: 10.0, y: 10.0, dx: 1.0, dy: 2.0 }));
        sink.pointer(40.0, 40.0, 7.0, 8.0);
        assert_eq!(rx.try_recv().unwrap(), CaptureEvent::Input(Raw::Pointer { x: 40.0, y: 40.0, dx: 15.0, dy: 18.0 }));
        let key = CaptureEvent::Input(Raw::Key { usage: 4, down: false, repeat: false });
        let s2 = sink.clone();
        sink.reliable(key); // fills the single slot
        let h = std::thread::spawn(move || s2.reliable(key)); // blocks until drained
        std::thread::sleep(std::time::Duration::from_millis(30));
        assert_eq!(rx.try_recv().unwrap(), key);
        h.join().unwrap();
        assert_eq!(rx.try_recv().unwrap(), key);
    }
}
