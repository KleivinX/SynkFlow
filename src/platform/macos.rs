//! macOS backend.
//!
//! * Capture: a CoreGraphics **event tap** on a dedicated run-loop thread. An
//!   active tap (Accessibility permission) can swallow events; Input Monitoring
//!   is needed to see keyboard events at all.
//! * Injection: `CGEventPost`, every event stamped with [`INJECT_MARKER`] so the
//!   tap ignores our own output.
//! * Limits we do not paper over: secure input (password fields) and the login
//!   window are not reachable; the cursor stays visible (frozen) while another
//!   computer is controlled, because hiding it from a background app needs a
//!   private API.

use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use core_foundation::base::{CFType, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::runloop::{CFRunLoop, kCFRunLoopCommonModes};
use core_foundation::string::CFString;
use core_graphics::display::CGDisplay;
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType, CGMouseButton, CallbackResult, EventField,
    ScrollEventUnit,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;

use super::*;
use crate::control::Raw;
use crate::proto::MouseButton;

// SAFETY (all of this block): plain C functions from system frameworks that
// take and return only scalars; they have no preconditions beyond being linked.
#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
}
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightListenEventAccess() -> bool;
    fn CGRequestListenEventAccess() -> bool;
    fn CGPreflightPostEventAccess() -> bool;
    fn CGRequestPostEventAccess() -> bool;
    fn CGEventTapEnable(tap: *mut c_void, enable: bool);
    fn CGSessionCopyCurrentDictionary() -> CFDictionaryRef;
}

pub fn accessibility_trusted() -> bool {
    // SAFETY: scalar-only system call, see block comment above.
    unsafe { AXIsProcessTrusted() }
}
fn listen_access() -> bool {
    // SAFETY: as above.
    unsafe { CGPreflightListenEventAccess() }
}
fn post_access() -> bool {
    // SAFETY: as above.
    unsafe { CGPreflightPostEventAccess() }
}

const SETTINGS_ACCESSIBILITY: &str = "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility";
const SETTINGS_INPUT_MONITORING: &str = "x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent";

pub struct MacInput {
    displays: Mutex<Vec<DisplayInfo>>,
    inj: Mutex<InjectState>,
}

#[derive(Default)]
struct InjectState {
    flags: u64,
    buttons_down: HashSet<u8>,
    last_pos: Option<(f64, f64)>,
    last_click: Option<(Instant, u8, (f64, f64), i64)>,
}

impl MacInput {
    pub fn new() -> Self {
        Self { displays: Mutex::new(vec![]), inj: Mutex::new(InjectState::default()) }
    }

    fn read_displays() -> Result<Vec<DisplayInfo>, BackendError> {
        let ids = CGDisplay::active_displays().map_err(|e| BackendError::Failed(format!("display list failed ({e})")))?;
        let mut out = Vec::new();
        for (i, id) in ids.iter().enumerate() {
            let d = CGDisplay::new(*id);
            let b = d.bounds();
            let scale = d.display_mode().map(|m| if m.width() > 0 { m.pixel_width() as f32 / m.width() as f32 } else { 1.0 }).unwrap_or(1.0).clamp(0.5, 8.0);
            out.push(DisplayInfo {
                id: *id,
                name: if d.is_builtin() { "Built-in display".into() } else { format!("Display {}", i + 1) },
                x: b.origin.x.round() as i32,
                y: b.origin.y.round() as i32,
                width: b.size.width.round().max(1.0) as u32,
                height: b.size.height.round().max(1.0) as u32,
                scale,
                rotation: (d.rotation().round() as i64).rem_euclid(360) as u16,
                primary: d.is_main(),
            });
        }
        Ok(out)
    }

    fn find_display(&self, id: u32) -> Option<DisplayInfo> {
        let mut cache = self.displays.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(d) = cache.iter().find(|d| d.id == id) {
            return Some(d.clone());
        }
        // Unknown display: the configuration changed. Refresh once.
        *cache = Self::read_displays().ok()?;
        cache.iter().find(|d| d.id == id).cloned()
    }
}

impl Default for MacInput {
    fn default() -> Self {
        Self::new()
    }
}

fn flag_for(hid: u16) -> u64 {
    match hid {
        keys::LSHIFT | keys::RSHIFT => CGEventFlags::CGEventFlagShift.bits(),
        keys::LCTRL | keys::RCTRL => CGEventFlags::CGEventFlagControl.bits(),
        keys::LALT | keys::RALT => CGEventFlags::CGEventFlagAlternate.bits(),
        keys::LMETA | keys::RMETA => CGEventFlags::CGEventFlagCommand.bits(),
        _ => 0,
    }
}

struct SendRunLoop(CFRunLoop);
// SAFETY: CFRunLoopStop is documented as thread-safe; we only ever call `stop`.
unsafe impl Send for SendRunLoop {}

struct MacCapture {
    filter: Arc<CaptureFilter>,
    run_loop: Arc<Mutex<Option<SendRunLoop>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Capture for MacCapture {
    fn set_grab(&self, grab: bool) {
        self.filter.set_grab(grab);
        // Freeze the cursor in place; deltas keep flowing in the events.
        let _ = CGDisplay::associate_mouse_and_mouse_cursor_position(!grab);
    }
    fn warp(&self, x: f64, y: f64) {
        let _ = CGDisplay::warp_mouse_cursor_position(CGPoint::new(x, y));
    }
    fn set_hotkeys(&self, panic: Hotkey, switch: Hotkey) {
        self.filter.set_hotkeys(panic, switch);
    }
}

impl Drop for MacCapture {
    fn drop(&mut self) {
        // Never leave the cursor frozen.
        let _ = CGDisplay::associate_mouse_and_mouse_cursor_position(true);
        if let Some(rl) = self.run_loop.lock().unwrap_or_else(|e| e.into_inner()).take() {
            rl.0.stop();
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl InputBackend for MacInput {
    fn capabilities(&self) -> BackendCaps {
        let ax = accessibility_trusted();
        let listen = listen_access();
        let capture = match (ax, listen) {
            (true, true) => Cap::Available,
            (false, true) => Cap::NeedsPermission("Accessibility (System Settings → Privacy & Security → Accessibility)".into()),
            (true, false) => Cap::NeedsPermission("Input Monitoring (System Settings → Privacy & Security → Input Monitoring)".into()),
            (false, false) => Cap::NeedsPermission("Accessibility and Input Monitoring (System Settings → Privacy & Security)".into()),
        };
        let inject = if ax || post_access() {
            Cap::Available
        } else {
            Cap::NeedsPermission("Accessibility (System Settings → Privacy & Security → Accessibility)".into())
        };
        BackendCaps {
            platform: Platform::MacOs,
            api: "CoreGraphics event tap and CGEventPost",
            capture,
            inject,
            notes: vec![
                "Password fields (secure input) and the login window cannot be controlled.".into(),
                "While another computer is controlled, this Mac's cursor stays visible, frozen at the screen edge.".into(),
                "The permission is tied to this app's code signature; rebuilding an ad-hoc signed app asks again.".into(),
            ],
        }
    }

    fn displays(&self) -> Result<Vec<DisplayInfo>, BackendError> {
        let d = Self::read_displays()?;
        *self.displays.lock().unwrap_or_else(|e| e.into_inner()) = d.clone();
        Ok(d)
    }

    fn start_capture(&self, sink: CaptureSink, panic: Hotkey, switch: Hotkey) -> Result<Box<dyn Capture>, BackendError> {
        if !accessibility_trusted() {
            return Err(BackendError::Permission("Accessibility".into()));
        }
        if !listen_access() {
            return Err(BackendError::Permission("Input Monitoring".into()));
        }
        let filter = Arc::new(CaptureFilter::new(panic, switch));
        let run_loop: Arc<Mutex<Option<SendRunLoop>>> = Arc::new(Mutex::new(None));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), BackendError>>();
        let thread = {
            let (filter, run_loop) = (filter.clone(), run_loop.clone());
            std::thread::Builder::new()
                .name("synkflow-capture".into())
                .spawn(move || run_tap(sink, filter, run_loop, ready_tx))
                .map_err(|e| BackendError::Failed(e.to_string()))?
        };
        match ready_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(())) => Ok(Box::new(MacCapture { filter, run_loop, thread: Some(thread) })),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => Err(BackendError::Failed("event tap did not start".into())),
        }
    }

    fn inject(&self, ev: &InputEvent) -> Result<(), BackendError> {
        let src = CGEventSource::new(CGEventSourceStateID::HIDSystemState).map_err(|_| BackendError::Failed("event source".into()))?;
        let mut st = self.inj.lock().unwrap_or_else(|e| e.into_inner());
        let post = |e: CGEvent, flags: u64| {
            e.set_integer_value_field(EventField::EVENT_SOURCE_USER_DATA, INJECT_MARKER);
            e.set_flags(CGEventFlags::from_bits_truncate(flags));
            e.post(CGEventTapLocation::HID);
        };
        match *ev {
            InputEvent::PointerAbs { display, x, y } => {
                let d = self.find_display(display).ok_or_else(|| BackendError::Failed("unknown display".into()))?;
                let p = (d.x as f64 + (x as f64).clamp(0.0, d.width as f64 - 1.0), d.y as f64 + (y as f64).clamp(0.0, d.height as f64 - 1.0));
                let (ty, button) = if st.buttons_down.contains(&0) {
                    (CGEventType::LeftMouseDragged, CGMouseButton::Left)
                } else if st.buttons_down.contains(&1) {
                    (CGEventType::RightMouseDragged, CGMouseButton::Right)
                } else if st.buttons_down.iter().any(|b| *b >= 2) {
                    (CGEventType::OtherMouseDragged, CGMouseButton::Center)
                } else {
                    (CGEventType::MouseMoved, CGMouseButton::Left)
                };
                let e = CGEvent::new_mouse_event(src, ty, CGPoint::new(p.0, p.1), button).map_err(|_| BackendError::Failed("mouse event".into()))?;
                if let Some(last) = st.last_pos {
                    e.set_integer_value_field(EventField::MOUSE_EVENT_DELTA_X, (p.0 - last.0).round() as i64);
                    e.set_integer_value_field(EventField::MOUSE_EVENT_DELTA_Y, (p.1 - last.1).round() as i64);
                }
                st.last_pos = Some(p);
                post(e, st.flags);
            }
            InputEvent::Button { button, down } => {
                let (idx, down_ty, up_ty, cg_button, number) = match button {
                    MouseButton::Left => (0u8, CGEventType::LeftMouseDown, CGEventType::LeftMouseUp, CGMouseButton::Left, 0),
                    MouseButton::Right => (1, CGEventType::RightMouseDown, CGEventType::RightMouseUp, CGMouseButton::Right, 1),
                    MouseButton::Middle => (2, CGEventType::OtherMouseDown, CGEventType::OtherMouseUp, CGMouseButton::Center, 2),
                    MouseButton::Back => (3, CGEventType::OtherMouseDown, CGEventType::OtherMouseUp, CGMouseButton::Center, 3),
                    MouseButton::Forward => (4, CGEventType::OtherMouseDown, CGEventType::OtherMouseUp, CGMouseButton::Center, 4),
                    MouseButton::Other(n) => {
                        (5u8.saturating_add(n), CGEventType::OtherMouseDown, CGEventType::OtherMouseUp, CGMouseButton::Center, 5 + n as i64)
                    }
                };
                let pos = match st.last_pos {
                    Some(p) => p,
                    None => {
                        let l =
                            CGEvent::new(CGEventSource::new(CGEventSourceStateID::HIDSystemState).map_err(|_| BackendError::Failed("event source".into()))?)
                                .map_err(|_| BackendError::Failed("event".into()))?
                                .location();
                        (l.x, l.y)
                    }
                };
                let click_state = if down {
                    st.buttons_down.insert(idx);
                    let n = match st.last_click {
                        Some((t, b, p, n))
                            if b == idx && t.elapsed() < Duration::from_millis(500) && (p.0 - pos.0).abs() < 5.0 && (p.1 - pos.1).abs() < 5.0 =>
                        {
                            n + 1
                        }
                        _ => 1,
                    };
                    st.last_click = Some((Instant::now(), idx, pos, n));
                    n
                } else {
                    st.buttons_down.remove(&idx);
                    st.last_click.map_or(1, |c| c.3)
                };
                let e = CGEvent::new_mouse_event(src, if down { down_ty } else { up_ty }, CGPoint::new(pos.0, pos.1), cg_button)
                    .map_err(|_| BackendError::Failed("mouse event".into()))?;
                e.set_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER, number);
                e.set_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE, click_state);
                post(e, st.flags);
            }
            InputEvent::Scroll { dx, dy, pixels } => {
                let unit = if pixels { ScrollEventUnit::PIXEL } else { ScrollEventUnit::LINE };
                let v = dy.round() as i32;
                let h = dx.round() as i32;
                let (v, h) = if !pixels {
                    (if v == 0 && dy != 0.0 { dy.signum() as i32 } else { v }, if h == 0 && dx != 0.0 { dx.signum() as i32 } else { h })
                } else {
                    (v, h)
                };
                let e = CGEvent::new_scroll_event(src, unit, 2, v, h, 0).map_err(|_| BackendError::Failed("scroll event".into()))?;
                post(e, st.flags);
            }
            InputEvent::Key { usage, down, repeat } => {
                let vk = keys::to_mac(usage).ok_or_else(|| BackendError::Failed(format!("key {usage:#x} has no macOS equivalent")))?;
                let bit = flag_for(usage);
                if bit != 0 {
                    if down {
                        st.flags |= bit;
                    } else {
                        st.flags &= !bit;
                    }
                }
                let e = CGEvent::new_keyboard_event(src, vk, down).map_err(|_| BackendError::Failed("key event".into()))?;
                if repeat {
                    e.set_integer_value_field(EventField::KEYBOARD_EVENT_AUTOREPEAT, 1);
                }
                post(e, st.flags);
            }
        }
        Ok(())
    }

    fn request_permissions(&self) {
        // SAFETY: scalar-only calls that make the OS show its own permission prompt.
        unsafe {
            CGRequestListenEventAccess();
            CGRequestPostEventAccess();
        }
    }

    fn open_permission_settings(&self) {
        let url = if !accessibility_trusted() { SETTINGS_ACCESSIBILITY } else { SETTINGS_INPUT_MONITORING };
        let _ = std::process::Command::new("open").arg(url).spawn();
    }

    fn session_locked(&self) -> bool {
        // SAFETY: returns a +1 retained dictionary or null; we adopt it under
        // the create rule and CoreFoundation releases it on drop.
        let dict = unsafe {
            let p = CGSessionCopyCurrentDictionary();
            if p.is_null() {
                return false;
            }
            CFDictionary::<CFString, CFType>::wrap_under_create_rule(p)
        };
        let flag = |key: &str| dict.find(CFString::new(key)).and_then(|v| v.downcast::<CFBoolean>()).map(bool::from);
        flag("CGSSessionScreenIsLocked").unwrap_or(false) || flag("kCGSSessionOnConsoleKey") == Some(false)
    }
}

/// Body of the capture thread: create the tap, run the loop until stopped.
fn run_tap(sink: CaptureSink, filter: Arc<CaptureFilter>, run_loop: Arc<Mutex<Option<SendRunLoop>>>, ready: std::sync::mpsc::Sender<Result<(), BackendError>>) {
    let port: Arc<AtomicPtr<c_void>> = Arc::new(AtomicPtr::new(std::ptr::null_mut()));
    let mods_down: Arc<Mutex<HashSet<u16>>> = Arc::new(Mutex::new(HashSet::new()));
    let (cb_sink, cb_filter, cb_port) = (sink.clone(), filter.clone(), port.clone());

    let callback = move |_proxy, ty: CGEventType, ev: &CGEvent| -> CallbackResult {
        use CGEventType::*;
        if matches!(ty, TapDisabledByTimeout | TapDisabledByUserInput) {
            // The OS turned the tap off (callback too slow, or secure input). Turn it
            // back on and tell the engine so held keys can be reconciled.
            let p = cb_port.load(Ordering::SeqCst);
            if !p.is_null() {
                // SAFETY: `p` is the live mach port of this very tap, stored after creation
                // and kept alive by `CGEventTap` for the life of the run loop.
                unsafe { CGEventTapEnable(p, true) };
            }
            cb_sink.reliable(CaptureEvent::Lost);
            return CallbackResult::Keep;
        }
        if ev.get_integer_value_field(EventField::EVENT_SOURCE_USER_DATA) == INJECT_MARKER {
            return CallbackResult::Keep; // our own injected event
        }
        let verdict = match ty {
            MouseMoved | LeftMouseDragged | RightMouseDragged | OtherMouseDragged => {
                let l = ev.location();
                let dx = ev.get_integer_value_field(EventField::MOUSE_EVENT_DELTA_X) as f64;
                let dy = ev.get_integer_value_field(EventField::MOUSE_EVENT_DELTA_Y) as f64;
                cb_sink.pointer(l.x, l.y, dx, dy);
                cb_filter.motion()
            }
            LeftMouseDown | LeftMouseUp | RightMouseDown | RightMouseUp | OtherMouseDown | OtherMouseUp => {
                let down = matches!(ty, LeftMouseDown | RightMouseDown | OtherMouseDown);
                let button = match ty {
                    LeftMouseDown | LeftMouseUp => MouseButton::Left,
                    RightMouseDown | RightMouseUp => MouseButton::Right,
                    _ => match ev.get_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER) {
                        2 => MouseButton::Middle,
                        3 => MouseButton::Back,
                        4 => MouseButton::Forward,
                        n => MouseButton::Other(n.clamp(5, 200) as u8 - 5),
                    },
                };
                let v = cb_filter.button(crate::control::button_index_pub(button), down);
                cb_sink.reliable(CaptureEvent::Input(Raw::Button { button, down }));
                v
            }
            ScrollWheel => {
                let continuous = ev.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_IS_CONTINUOUS) != 0;
                let (a, b) = if continuous {
                    (EventField::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_1, EventField::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_2)
                } else {
                    (EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1, EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2)
                };
                let (dy, dx) = (ev.get_integer_value_field(a) as f64, ev.get_integer_value_field(b) as f64);
                if dx != 0.0 || dy != 0.0 {
                    cb_sink.reliable(CaptureEvent::Input(Raw::Scroll { dx, dy, pixels: continuous }));
                }
                cb_filter.motion()
            }
            KeyDown | KeyUp | FlagsChanged => {
                let vk = ev.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
                let Some(hid) = keys::from_mac(vk) else {
                    // No HID equivalent (Fn, JIS keys): cannot be forwarded.
                    return if cb_filter.grabbed() { CallbackResult::Drop } else { CallbackResult::Keep };
                };
                let (down, repeat) = if matches!(ty, FlagsChanged) {
                    let mut held = mods_down.lock().unwrap_or_else(|e| e.into_inner());
                    let bit = flag_for(hid);
                    let flags = ev.get_flags().bits();
                    let down = if hid == 0x39 {
                        true // Caps Lock reports a toggle, not a press and release
                    } else if bit != 0 && flags & bit == 0 {
                        // The whole modifier family is up: drop both sides.
                        held.retain(|k| flag_for(*k) != bit);
                        false
                    } else {
                        !held.contains(&hid)
                    };
                    if down {
                        held.insert(hid);
                    } else {
                        held.remove(&hid);
                    }
                    (down, false)
                } else {
                    (matches!(ty, KeyDown), ev.get_integer_value_field(EventField::KEYBOARD_EVENT_AUTOREPEAT) != 0)
                };
                let (v, action) = cb_filter.key(hid, down);
                match action {
                    Some(a) => cb_sink.reliable(CaptureEvent::Hotkey(a)),
                    None => cb_sink.reliable(CaptureEvent::Input(Raw::Key { usage: hid, down, repeat })),
                }
                if hid == 0x39 && down {
                    // Caps Lock: synthesise the release the OS never sends.
                    let _ = cb_filter.key(hid, false);
                    cb_sink.reliable(CaptureEvent::Input(Raw::Key { usage: hid, down: false, repeat: false }));
                    mods_down.lock().unwrap_or_else(|e| e.into_inner()).remove(&hid);
                }
                v
            }
            _ => Verdict::Pass,
        };
        if verdict == Verdict::Swallow { CallbackResult::Drop } else { CallbackResult::Keep }
    };

    let types = vec![
        CGEventType::MouseMoved,
        CGEventType::LeftMouseDown,
        CGEventType::LeftMouseUp,
        CGEventType::LeftMouseDragged,
        CGEventType::RightMouseDown,
        CGEventType::RightMouseUp,
        CGEventType::RightMouseDragged,
        CGEventType::OtherMouseDown,
        CGEventType::OtherMouseUp,
        CGEventType::OtherMouseDragged,
        CGEventType::ScrollWheel,
        CGEventType::KeyDown,
        CGEventType::KeyUp,
        CGEventType::FlagsChanged,
    ];
    let tap = match CGEventTap::new(CGEventTapLocation::Session, CGEventTapPlacement::HeadInsertEventTap, CGEventTapOptions::Default, types, callback) {
        Ok(t) => t,
        Err(()) => {
            let _ = ready.send(Err(BackendError::Permission("the system refused to create the event tap — check Accessibility and Input Monitoring".into())));
            return;
        }
    };
    port.store(tap.mach_port().as_concrete_TypeRef() as *mut c_void, Ordering::SeqCst);
    let Ok(source) = tap.mach_port().create_runloop_source(0) else {
        let _ = ready.send(Err(BackendError::Failed("run loop source".into())));
        return;
    };
    let rl = CFRunLoop::get_current();
    // SAFETY: `kCFRunLoopCommonModes` is a constant exported by CoreFoundation.
    rl.add_source(&source, unsafe { kCFRunLoopCommonModes });
    tap.enable();
    *run_loop.lock().unwrap_or_else(|e| e.into_inner()) = Some(SendRunLoop(rl));
    let _ = ready.send(Ok(()));
    CFRunLoop::run_current();
    drop(tap);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_enumeration_returns_sane_geometry() {
        let d = MacInput::read_displays().expect("CoreGraphics display list");
        assert!(!d.is_empty());
        assert_eq!(d.iter().filter(|d| d.primary).count(), 1);
        for x in &d {
            assert!(x.width > 0 && x.height > 0);
            assert!((0.5..=8.0).contains(&x.scale));
        }
    }

    #[test]
    fn capability_report_matches_the_permission_state() {
        let caps = MacInput::new().capabilities();
        assert_eq!(caps.platform, Platform::MacOs);
        let trusted = accessibility_trusted() && listen_access();
        assert_eq!(caps.capture.is_available(), trusted);
        if !trusted {
            assert!(matches!(caps.capture, Cap::NeedsPermission(_)));
        }
    }

    #[test]
    fn session_lock_query_does_not_crash() {
        let _ = MacInput::new().session_locked();
    }

    #[test]
    fn capture_refuses_to_start_without_permission_instead_of_pretending() {
        if accessibility_trusted() && listen_access() {
            return; // permission already granted to the test runner; nothing to refuse
        }
        let (tx, _rx) = mpsc::channel(8);
        let r = MacInput::new().start_capture(CaptureSink::new(tx), Hotkey::PANIC, Hotkey::SWITCH);
        assert!(matches!(r, Err(BackendError::Permission(_))));
    }
}
