//! Linux backend.
//!
//! * **X11**: capture with XInput2 *raw* events (no grabs while idle), a pointer
//!   and keyboard grab only while another computer is controlled, and
//!   injection through the XTEST extension. Displays come from RandR.
//! * **Wayland**: not supported yet, and said so. Wayland deliberately forbids
//!   global input capture and synthetic input through X11 APIs, and Synkflow
//!   does not run privileged helpers or open `/dev/uinput`. The supported
//!   route (the `InputCapture` and `RemoteDesktop` portals, libei) is not
//!   implemented in this version, so both capabilities are reported as
//!   unavailable rather than faked.
//!
//! **This backend has been compiled for Linux but not run on Linux by the
//! author's tooling.**

use std::sync::{Arc, Mutex};
use std::time::Duration;

use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xinput::{self, ConnectionExt as _, XIEventMask};
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

use super::*;
use crate::control::Raw;
use crate::proto::MouseButton;

const WAYLAND_WHY: &str = "this is a Wayland session; Wayland does not allow global input capture or injection through X11 APIs, and the portal/libei route is not implemented in this version";

pub struct LinuxInput {
    wayland: bool,
    inj: Mutex<Option<Injector>>,
}

struct Injector {
    conn: RustConnection,
    root: xproto::Window,
    scroll_residue: (f32, f32),
}

impl LinuxInput {
    pub fn new() -> Self {
        Self { wayland: Platform::current() == Platform::LinuxWayland, inj: Mutex::new(None) }
    }

    fn connect() -> Result<(RustConnection, usize), BackendError> {
        RustConnection::connect(None).map_err(|e| BackendError::Unavailable(format!("cannot reach the X server: {e}")))
    }
}

impl Default for LinuxInput {
    fn default() -> Self {
        Self::new()
    }
}

fn button_number(b: MouseButton) -> Option<u8> {
    Some(match b {
        MouseButton::Left => 1,
        MouseButton::Middle => 2,
        MouseButton::Right => 3,
        MouseButton::Back => 8,
        MouseButton::Forward => 9,
        MouseButton::Other(_) => return None,
    })
}

fn failed<E: std::fmt::Display>(e: E) -> BackendError {
    BackendError::Failed(e.to_string())
}

impl InputBackend for LinuxInput {
    fn capabilities(&self) -> BackendCaps {
        if self.wayland {
            return BackendCaps {
                platform: Platform::LinuxWayland,
                api: "none (Wayland)",
                capture: Cap::Unavailable(WAYLAND_WHY.into()),
                inject: Cap::Unavailable(WAYLAND_WHY.into()),
                notes: vec![
                    "Clipboard and file transfer still work. Use a Wayland-capable computer as the *other* side of the pairing, or an X11 session here.".into(),
                ],
            };
        }
        let reachable = Self::connect().is_ok();
        let why = "no X server reachable (is DISPLAY set?)".to_string();
        BackendCaps {
            platform: Platform::LinuxX11,
            api: "X11: XInput2 raw events, XTEST, RandR",
            capture: if reachable { Cap::Available } else { Cap::Unavailable(why.clone()) },
            inject: if reachable { Cap::Available } else { Cap::Unavailable(why) },
            notes: vec![
                "While another computer is controlled, the pointer and keyboard are grabbed; applications may briefly re-read their key state when control returns.".into(),
                "Raw events cannot be swallowed, so the emergency shortcut's last key is also seen by the focused application while sharing is idle.".into(),
                "Display scale is assumed to be 1.0; per-monitor scaling is not read.".into(),
            ],
        }
    }

    fn displays(&self) -> Result<Vec<DisplayInfo>, BackendError> {
        if self.wayland {
            return Err(BackendError::Unavailable(WAYLAND_WHY.into()));
        }
        let (conn, screen) = Self::connect()?;
        let root = conn.setup().roots[screen].root;
        let monitors = conn.randr_get_monitors(root, true).map_err(failed)?.reply().map_err(failed)?;
        let mut out = Vec::new();
        for (i, m) in monitors.monitors.iter().enumerate() {
            let name = conn
                .get_atom_name(m.name)
                .ok()
                .and_then(|c| c.reply().ok())
                .map(|r| String::from_utf8_lossy(&r.name).to_string())
                .unwrap_or_else(|| format!("Display {}", i + 1));
            out.push(DisplayInfo {
                id: i as u32 + 1,
                name,
                x: m.x as i32,
                y: m.y as i32,
                width: m.width.max(1) as u32,
                height: m.height.max(1) as u32,
                scale: 1.0,
                rotation: 0,
                primary: m.primary,
            });
        }
        if out.is_empty() {
            let s = &conn.setup().roots[screen];
            out.push(DisplayInfo {
                id: 1,
                name: "Screen".into(),
                x: 0,
                y: 0,
                width: s.width_in_pixels.max(1) as u32,
                height: s.height_in_pixels.max(1) as u32,
                scale: 1.0,
                rotation: 0,
                primary: true,
            });
        }
        Ok(out)
    }

    fn start_capture(&self, sink: CaptureSink, panic: Hotkey, switch: Hotkey) -> Result<Box<dyn Capture>, BackendError> {
        if self.wayland {
            return Err(BackendError::Unavailable(WAYLAND_WHY.into()));
        }
        let (conn, screen) = Self::connect()?;
        let root = conn.setup().roots[screen].root;
        conn.xinput_xi_query_version(2, 2).map_err(failed)?.reply().map_err(|_| BackendError::Unavailable("the X server has no XInput 2".into()))?;
        let mask = XIEventMask::RAW_MOTION
            | XIEventMask::RAW_BUTTON_PRESS
            | XIEventMask::RAW_BUTTON_RELEASE
            | XIEventMask::RAW_KEY_PRESS
            | XIEventMask::RAW_KEY_RELEASE;
        conn.xinput_xi_select_events(root, &[xinput::EventMask { deviceid: 1, mask: vec![mask] }]).map_err(failed)?.check().map_err(failed)?;
        // Our own XTEST devices must not feed back into capture.
        let own: Vec<u16> = conn
            .xinput_xi_query_device(0)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| r.infos.iter().filter(|d| String::from_utf8_lossy(&d.name).contains("XTEST")).map(|d| d.deviceid).collect())
            .unwrap_or_default();
        conn.flush().map_err(failed)?;

        let filter = Arc::new(CaptureFilter::new(panic, switch));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (f2, s2) = (filter.clone(), stop.clone());
        let conn = Arc::new(conn);
        let c2 = conn.clone();
        let thread = std::thread::Builder::new().name("synkflow-capture".into()).spawn(move || event_loop(c2, root, sink, f2, own, s2)).map_err(failed)?;
        Ok(Box::new(X11Capture { conn, root, filter, stop, thread: Some(thread), grabbed: Mutex::new(None) }))
    }

    fn inject(&self, ev: &InputEvent) -> Result<(), BackendError> {
        if self.wayland {
            return Err(BackendError::Unavailable(WAYLAND_WHY.into()));
        }
        let mut guard = self.inj.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_none() {
            let (conn, screen) = Self::connect()?;
            let root = conn.setup().roots[screen].root;
            *guard = Some(Injector { conn, root, scroll_residue: (0.0, 0.0) });
        }
        let inj = guard.as_mut().expect("just created");
        let fake = |ty: u8, detail: u8, x: i16, y: i16| inj.conn.xtest_fake_input(ty, detail, 0, inj.root, x, y, 0).map(|_| ()).map_err(failed);
        const KEY_PRESS: u8 = 2;
        const KEY_RELEASE: u8 = 3;
        const BUTTON_PRESS: u8 = 4;
        const BUTTON_RELEASE: u8 = 5;
        const MOTION: u8 = 6;
        match *ev {
            InputEvent::PointerAbs { display, x, y } => {
                let d = self.displays()?.into_iter().find(|d| d.id == display).ok_or_else(|| BackendError::Failed("unknown display".into()))?;
                fake(MOTION, 0, (d.x as f32 + x).round() as i16, (d.y as f32 + y).round() as i16)?;
            }
            InputEvent::Button { button, down } => {
                let n = button_number(button).ok_or_else(|| BackendError::Failed("unsupported mouse button".into()))?;
                fake(if down { BUTTON_PRESS } else { BUTTON_RELEASE }, n, 0, 0)?;
            }
            InputEvent::Scroll { dx, dy, pixels } => {
                let per_notch = if pixels { 60.0 } else { 1.0 };
                inj.scroll_residue.0 += dx / per_notch;
                inj.scroll_residue.1 += dy / per_notch;
                let (nx, ny) = (inj.scroll_residue.0.trunc() as i32, inj.scroll_residue.1.trunc() as i32);
                inj.scroll_residue.0 -= nx as f32;
                inj.scroll_residue.1 -= ny as f32;
                // Buttons 4/5 are wheel up/down, 6/7 left/right; positive dy scrolls up.
                for (n, count) in [(if ny > 0 { 4u8 } else { 5 }, ny.abs()), (if nx > 0 { 7u8 } else { 6 }, nx.abs())] {
                    for _ in 0..count {
                        fake(BUTTON_PRESS, n, 0, 0)?;
                        fake(BUTTON_RELEASE, n, 0, 0)?;
                    }
                }
            }
            InputEvent::Key { usage, down, .. } => {
                let evdev = keys::to_evdev(usage).ok_or_else(|| BackendError::Failed(format!("key {usage:#x} has no Linux equivalent")))?;
                fake(if down { KEY_PRESS } else { KEY_RELEASE }, (evdev + 8) as u8, 0, 0)?;
            }
        }
        inj.conn.flush().map_err(failed)
    }
}

struct X11Capture {
    conn: Arc<RustConnection>,
    root: xproto::Window,
    filter: Arc<CaptureFilter>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// Window the pointer is confined to while grabbed.
    grabbed: Mutex<Option<xproto::Window>>,
}

impl Capture for X11Capture {
    fn set_grab(&self, grab: bool) {
        self.filter.set_grab(grab);
        let mut g = self.grabbed.lock().unwrap_or_else(|e| e.into_inner());
        let c = &*self.conn;
        if grab && g.is_none() {
            // Freeze the pointer by confining it to a 1×1 window under it, and take the keyboard.
            let (px, py) = c.query_pointer(self.root).ok().and_then(|q| q.reply().ok()).map(|r| (r.root_x, r.root_y)).unwrap_or((0, 0));
            if let Ok(win) = c.generate_id() {
                let _ = c.create_window(
                    0,
                    win,
                    self.root,
                    px,
                    py,
                    1,
                    1,
                    0,
                    xproto::WindowClass::INPUT_ONLY,
                    0,
                    &xproto::CreateWindowAux::new().override_redirect(1),
                );
                let _ = c.map_window(win);
                let ev = xproto::EventMask::BUTTON_PRESS | xproto::EventMask::BUTTON_RELEASE | xproto::EventMask::POINTER_MOTION;
                let _ = c.grab_pointer(false, self.root, ev, xproto::GrabMode::ASYNC, xproto::GrabMode::ASYNC, win, 0u32, x11rb::CURRENT_TIME);
                let _ = c.grab_keyboard(false, self.root, x11rb::CURRENT_TIME, xproto::GrabMode::ASYNC, xproto::GrabMode::ASYNC);
                *g = Some(win);
            }
        } else if !grab {
            if let Some(win) = g.take() {
                let _ = c.ungrab_pointer(x11rb::CURRENT_TIME);
                let _ = c.ungrab_keyboard(x11rb::CURRENT_TIME);
                let _ = c.destroy_window(win);
            }
        }
        let _ = c.flush();
    }

    fn warp(&self, x: f64, y: f64) {
        let c = &*self.conn;
        let _ = c.warp_pointer(0u32, self.root, 0, 0, 0, 0, x.round() as i16, y.round() as i16);
        let _ = c.flush();
    }

    fn set_hotkeys(&self, panic: Hotkey, switch: Hotkey) {
        self.filter.set_hotkeys(panic, switch);
    }
}

impl Drop for X11Capture {
    fn drop(&mut self) {
        self.set_grab(false);
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        // Wake the blocked event loop with a harmless request/reply cycle.
        let _ = self.conn.get_input_focus().map(|c| c.reply());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Valuator values for axes 0 (x) and 1 (y) from an XInput2 raw motion event.
fn motion_delta(mask: &[u32], values: &[xinput::Fp3232]) -> (f64, f64) {
    let (mut dx, mut dy, mut idx) = (0.0, 0.0, 0usize);
    for (word, bits) in mask.iter().enumerate() {
        for bit in 0..32 {
            if bits & (1 << bit) != 0 {
                let axis = word * 32 + bit;
                if let Some(v) = values.get(idx) {
                    let f = v.integral as f64 + v.frac as f64 / 4_294_967_296.0;
                    if axis == 0 {
                        dx = f;
                    } else if axis == 1 {
                        dy = f;
                    }
                }
                idx += 1;
            }
        }
    }
    (dx, dy)
}

fn event_loop(
    conn: Arc<RustConnection>,
    root: xproto::Window,
    sink: CaptureSink,
    filter: Arc<CaptureFilter>,
    own: Vec<u16>,
    stop: Arc<std::sync::atomic::AtomicBool>,
) {
    let mut keys_down = std::collections::HashSet::<u16>::new();
    while !stop.load(std::sync::atomic::Ordering::SeqCst) {
        let ev = match conn.wait_for_event() {
            Ok(e) => e,
            Err(_) => {
                sink.reliable(CaptureEvent::Lost);
                return;
            }
        };
        match ev {
            Event::XinputRawMotion(e) if !own.contains(&e.sourceid) => {
                let (dx, dy) = motion_delta(&e.valuator_mask, &e.axisvalues);
                let (x, y) = conn.query_pointer(root).ok().and_then(|q| q.reply().ok()).map(|r| (r.root_x as f64, r.root_y as f64)).unwrap_or((0.0, 0.0));
                sink.pointer(x, y, dx, dy);
            }
            Event::XinputRawButtonPress(e) | Event::XinputRawButtonRelease(e) if !own.contains(&e.sourceid) => {
                let down = matches!(ev, Event::XinputRawButtonPress(_));
                let b = match e.detail {
                    1 => Some(MouseButton::Left),
                    2 => Some(MouseButton::Middle),
                    3 => Some(MouseButton::Right),
                    8 => Some(MouseButton::Back),
                    9 => Some(MouseButton::Forward),
                    4..=7 if down => {
                        let (dx, dy) = match e.detail {
                            4 => (0.0, 1.0),
                            5 => (0.0, -1.0),
                            6 => (-1.0, 0.0),
                            _ => (1.0, 0.0),
                        };
                        sink.reliable(CaptureEvent::Input(Raw::Scroll { dx, dy, pixels: false }));
                        None
                    }
                    _ => None,
                };
                if let Some(b) = b {
                    filter.button(crate::control::button_index_pub(b), down);
                    sink.reliable(CaptureEvent::Input(Raw::Button { button: b, down }));
                }
            }
            Event::XinputRawKeyPress(e) | Event::XinputRawKeyRelease(e) if !own.contains(&e.sourceid) => {
                let down = matches!(ev, Event::XinputRawKeyPress(_));
                let Some(hid) = (e.detail as u16).checked_sub(8).and_then(keys::from_evdev) else { continue };
                let repeat = if down {
                    !keys_down.insert(hid)
                } else {
                    keys_down.remove(&hid);
                    false
                };
                let (_, action) = filter.key(hid, down);
                match action {
                    Some(a) => sink.reliable(CaptureEvent::Hotkey(a)),
                    None => sink.reliable(CaptureEvent::Input(Raw::Key { usage: hid, down, repeat })),
                }
            }
            _ => {}
        }
    }
    let _ = Duration::ZERO;
}
