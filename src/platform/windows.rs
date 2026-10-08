//! Windows backend.
//!
//! * Capture: `WH_KEYBOARD_LL` / `WH_MOUSE_LL` hooks (they can swallow input)
//!   plus a message-only window receiving **Raw Input** for relative pointer
//!   motion, so pushing against a screen edge is visible even though the
//!   cursor cannot move further.
//! * Injection: `SendInput` using *scancodes*, so the receiving layout decides
//!   what a key types.
//! * Coordinates: all displays share one logical unit, `pixels / primary
//!   scale`, which keeps monitors with different DPI exactly adjacent.
//!
//! Honest limits (see docs/CAPABILITIES.md): elevated windows, the lock screen
//! and UAC prompts do not accept injected input (User Interface Privilege
//! Isolation); no Administrator rights are requested and none are bypassed.
//! **This backend has been compiled for Windows but not run on Windows by the
//! author's tooling.**

use std::collections::HashSet;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFOEXW};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::StationsAndDesktops::{CloseDesktop, DESKTOP_SWITCHDESKTOP, OpenInputDesktop};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor, MDT_EFFECTIVE_DPI, SetProcessDpiAwarenessContext};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEEVENTF_ABSOLUTE,
    MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, SendInput,
};
use windows_sys::Win32::UI::Input::{
    GetRawInputData, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER, RID_INPUT, RIDEV_INPUTSINK, RIM_TYPEMOUSE, RegisterRawInputDevices,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW, GetSystemMetrics, HC_ACTION, HHOOK,
    HWND_MESSAGE, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_UP, MSG, MSLLHOOKSTRUCT, PostThreadMessageW, RegisterClassW, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SetCursorPos, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_INPUT,
    WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_QUIT, WM_RBUTTONDOWN, WM_RBUTTONUP,
    WM_SYSKEYDOWN, WM_XBUTTONDOWN, WM_XBUTTONUP, WNDCLASSW,
};

use super::*;
use crate::control::Raw;
use crate::proto::MouseButton;

const MARKER: usize = INJECT_MARKER as usize;
const WHEEL_DELTA: f32 = 120.0;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[derive(Default)]
pub struct WinInput {
    scale: AtomicU32,
    cache: Mutex<Vec<DisplayInfo>>,
}

impl WinInput {
    pub fn new() -> Self {
        // SAFETY: plain call; failure (already set by the toolkit) is harmless.
        unsafe {
            SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
        Self::default()
    }

    fn read_displays() -> Vec<DisplayInfo> {
        struct Acc(Vec<(RECT, bool, u32)>);
        unsafe extern "system" fn cb(h: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> i32 {
            // SAFETY: `data` is the `&mut Acc` passed to EnumDisplayMonitors below and is
            // valid for the whole enumeration; `info` is a correctly sized, zeroed struct.
            unsafe {
                let acc = &mut *(data as *mut Acc);
                let mut info: MONITORINFOEXW = zeroed();
                info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
                if GetMonitorInfoW(h, &mut info as *mut MONITORINFOEXW as *mut _) != 0 {
                    let (mut dx, mut dy) = (96u32, 96u32);
                    GetDpiForMonitor(h, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
                    acc.0.push((info.monitorInfo.rcMonitor, info.monitorInfo.dwFlags & 1 != 0, dx.max(48)));
                }
            }
            1
        }
        let mut acc = Acc(vec![]);
        // SAFETY: the callback only touches `acc`, which outlives the call.
        unsafe {
            EnumDisplayMonitors(null_mut(), null(), Some(cb), &mut acc as *mut Acc as LPARAM);
        }
        // One uniform logical unit: pixels divided by the primary monitor's scale.
        let scale = acc.0.iter().find(|m| m.1).or(acc.0.first()).map(|m| m.2 as f32 / 96.0).unwrap_or(1.0).clamp(0.5, 8.0);
        acc.0
            .iter()
            .enumerate()
            .map(|(i, (r, primary, dpi))| DisplayInfo {
                id: i as u32 + 1,
                name: format!("Display {} ({}%)", i + 1, (*dpi as f32 / 96.0 * 100.0).round() as u32),
                x: (r.left as f32 / scale).round() as i32,
                y: (r.top as f32 / scale).round() as i32,
                width: (((r.right - r.left) as f32 / scale).round() as u32).max(1),
                height: (((r.bottom - r.top) as f32 / scale).round() as u32).max(1),
                scale,
                rotation: 0,
                primary: *primary,
            })
            .collect()
    }

    fn scale(&self) -> f32 {
        let s = f32::from_bits(self.scale.load(Ordering::Relaxed));
        if s > 0.0 { s } else { 1.0 }
    }

    fn refresh(&self) -> Vec<DisplayInfo> {
        let d = Self::read_displays();
        if let Some(p) = d.iter().find(|x| x.primary).or(d.first()) {
            self.scale.store(p.scale.to_bits(), Ordering::Relaxed);
        }
        *self.cache.lock().unwrap_or_else(|e| e.into_inner()) = d.clone();
        d
    }
}

fn mouse_input(flags: u32, dx: i32, dy: i32, data: u32) -> INPUT {
    INPUT { r#type: INPUT_MOUSE, Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy, mouseData: data, dwFlags: flags, time: 0, dwExtraInfo: MARKER } } }
}

fn key_input(win_scan: u16, down: bool) -> INPUT {
    let mut flags = KEYEVENTF_SCANCODE;
    if win_scan & 0xFF00 == 0xE000 {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if !down {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: 0, wScan: win_scan & 0x00FF, dwFlags: flags, time: 0, dwExtraInfo: MARKER } } }
}

fn send(inputs: &[INPUT]) -> Result<(), BackendError> {
    // SAFETY: `inputs` is a valid slice of fully initialised INPUT structs.
    let n = unsafe { SendInput(inputs.len() as u32, inputs.as_ptr(), size_of::<INPUT>() as i32) };
    if n as usize == inputs.len() {
        Ok(())
    } else {
        Err(BackendError::Failed("Windows refused the input (the target window may be elevated or on a secure desktop)".into()))
    }
}

impl InputBackend for WinInput {
    fn capabilities(&self) -> BackendCaps {
        BackendCaps {
            platform: Platform::Windows,
            api: "Windows low-level hooks, Raw Input and SendInput",
            capture: Cap::Available,
            inject: Cap::Available,
            notes: vec![
                "Elevated (Administrator) windows, UAC prompts and the lock screen do not accept injected input; Synkflow does not run elevated and does not bypass this.".into(),
                "Pointer speed follows raw mouse counts; adjust it in Settings → Input if it feels different from the local mouse.".into(),
            ],
        }
    }

    fn displays(&self) -> Result<Vec<DisplayInfo>, BackendError> {
        let d = self.refresh();
        if d.is_empty() { Err(BackendError::Failed("no displays reported".into())) } else { Ok(d) }
    }

    fn start_capture(&self, sink: CaptureSink, panic: Hotkey, switch: Hotkey) -> Result<Box<dyn Capture>, BackendError> {
        let _ = self.refresh();
        let filter = Arc::new(CaptureFilter::new(panic, switch));
        let st = Arc::new(HookState { sink, filter: filter.clone(), scale: self.scale(), keys_down: Mutex::new(HashSet::new()) });
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<u32, BackendError>>();
        let thread = std::thread::Builder::new()
            .name("synkflow-capture".into())
            .spawn(move || run_hooks(st, ready_tx))
            .map_err(|e| BackendError::Failed(e.to_string()))?;
        match ready_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(tid)) => Ok(Box::new(WinCapture { filter, scale: self.scale(), thread_id: tid, thread: Some(thread) })),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => Err(BackendError::Failed("input hooks did not start".into())),
        }
    }

    fn inject(&self, ev: &InputEvent) -> Result<(), BackendError> {
        match *ev {
            InputEvent::PointerAbs { display, x, y } => {
                let d = {
                    let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
                    cache.iter().find(|d| d.id == display).cloned()
                };
                let d = match d {
                    Some(d) => d,
                    None => self.refresh().into_iter().find(|d| d.id == display).ok_or_else(|| BackendError::Failed("unknown display".into()))?,
                };
                let s = d.scale;
                let px = ((d.x as f32 + x.clamp(0.0, d.width as f32 - 1.0)) * s).round() as i32;
                let py = ((d.y as f32 + y.clamp(0.0, d.height as f32 - 1.0)) * s).round() as i32;
                // SAFETY: GetSystemMetrics has no preconditions.
                let (vx, vy, vw, vh) = unsafe {
                    (
                        GetSystemMetrics(SM_XVIRTUALSCREEN),
                        GetSystemMetrics(SM_YVIRTUALSCREEN),
                        GetSystemMetrics(SM_CXVIRTUALSCREEN).max(2),
                        GetSystemMetrics(SM_CYVIRTUALSCREEN).max(2),
                    )
                };
                let nx = ((px - vx) as i64 * 65535 / (vw as i64 - 1)).clamp(0, 65535) as i32;
                let ny = ((py - vy) as i64 * 65535 / (vh as i64 - 1)).clamp(0, 65535) as i32;
                send(&[mouse_input(MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK, nx, ny, 0)])
            }
            InputEvent::Button { button, down } => {
                let (flag, data) = match (button, down) {
                    (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
                    (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
                    (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
                    (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
                    (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
                    (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
                    (MouseButton::Back, d) => (if d { MOUSEEVENTF_XDOWN } else { MOUSEEVENTF_XUP }, 1),
                    (MouseButton::Forward, d) => (if d { MOUSEEVENTF_XDOWN } else { MOUSEEVENTF_XUP }, 2),
                    (MouseButton::Other(_), _) => return Ok(()),
                };
                send(&[mouse_input(flag, 0, 0, data)])
            }
            InputEvent::Scroll { dx, dy, pixels } => {
                // Wheel units: 120 per notch. Smooth deltas arrive in pixels (~60 px ≈ one notch).
                let k = if pixels { WHEEL_DELTA / 60.0 } else { WHEEL_DELTA };
                let mut v = vec![];
                if dy != 0.0 {
                    v.push(mouse_input(MOUSEEVENTF_WHEEL, 0, 0, (dy * k).round() as i32 as u32));
                }
                if dx != 0.0 {
                    v.push(mouse_input(MOUSEEVENTF_HWHEEL, 0, 0, (dx * k).round() as i32 as u32));
                }
                if v.is_empty() { Ok(()) } else { send(&v) }
            }
            InputEvent::Key { usage, down, .. } => {
                let scan = keys::to_win(usage).ok_or_else(|| BackendError::Failed(format!("key {usage:#x} has no Windows equivalent")))?;
                send(&[key_input(scan, down)])
            }
        }
    }

    fn session_locked(&self) -> bool {
        // SAFETY: OpenInputDesktop returns a handle or null; a handle is closed immediately.
        // On the secure (lock / UAC) desktop a normal process cannot open it.
        unsafe {
            let h = OpenInputDesktop(0, 0, DESKTOP_SWITCHDESKTOP);
            if h.is_null() {
                return true;
            }
            CloseDesktop(h);
        }
        false
    }
}

// ───────────────────────────── capture ─────────────────────────────

struct HookState {
    sink: CaptureSink,
    filter: Arc<CaptureFilter>,
    scale: f32,
    keys_down: Mutex<HashSet<u16>>,
}

static STATE: OnceLock<Mutex<Option<Arc<HookState>>>> = OnceLock::new();

fn slot() -> &'static Mutex<Option<Arc<HookState>>> {
    STATE.get_or_init(|| Mutex::new(None))
}

fn current() -> Option<Arc<HookState>> {
    slot().lock().ok().and_then(|g| g.clone())
}

struct WinCapture {
    filter: Arc<CaptureFilter>,
    scale: f32,
    thread_id: u32,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Capture for WinCapture {
    fn set_grab(&self, grab: bool) {
        self.filter.set_grab(grab);
    }
    fn warp(&self, x: f64, y: f64) {
        // SAFETY: plain call with scalar arguments.
        unsafe {
            SetCursorPos((x as f32 * self.scale).round() as i32, (y as f32 * self.scale).round() as i32);
        }
    }
    fn set_hotkeys(&self, panic: Hotkey, switch: Hotkey) {
        self.filter.set_hotkeys(panic, switch);
    }
}

impl Drop for WinCapture {
    fn drop(&mut self) {
        // SAFETY: posting WM_QUIT to the capture thread's queue is always valid.
        unsafe {
            PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0);
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        if let Ok(mut g) = slot().lock() {
            *g = None;
        }
    }
}

unsafe extern "system" fn ll_keyboard(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        // SAFETY: for HC_ACTION, lparam points to a valid KBDLLHOOKSTRUCT for this call.
        let info = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
        if info.dwExtraInfo != MARKER {
            if let Some(st) = current() {
                if st.on_key(wparam as u32, info) {
                    return 1;
                }
            }
        }
    }
    // SAFETY: forwarding the unchanged arguments to the next hook.
    unsafe { CallNextHookEx(null_mut(), code, wparam, lparam) }
}

unsafe extern "system" fn ll_mouse(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        // SAFETY: for HC_ACTION, lparam points to a valid MSLLHOOKSTRUCT for this call.
        let info = unsafe { &*(lparam as *const MSLLHOOKSTRUCT) };
        if info.dwExtraInfo != MARKER {
            if let Some(st) = current() {
                if st.on_mouse(wparam as u32, info) {
                    return 1;
                }
            }
        }
    }
    // SAFETY: forwarding the unchanged arguments to the next hook.
    unsafe { CallNextHookEx(null_mut(), code, wparam, lparam) }
}

impl HookState {
    fn on_key(&self, msg: u32, info: &KBDLLHOOKSTRUCT) -> bool {
        let up = info.flags & LLKHF_UP != 0;
        let _ = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
        let scan = info.scanCode as u16;
        let win = if info.flags & LLKHF_EXTENDED != 0 { 0xE000 | scan } else { scan };
        let Some(hid) = keys::from_win(win) else { return self.filter.grabbed() };
        let down = !up;
        let repeat = {
            let mut held = self.keys_down.lock().unwrap_or_else(|e| e.into_inner());
            if down {
                !held.insert(hid)
            } else {
                held.remove(&hid);
                false
            }
        };
        let (verdict, action) = self.filter.key(hid, down);
        match action {
            Some(a) => self.sink.reliable(CaptureEvent::Hotkey(a)),
            None => self.sink.reliable(CaptureEvent::Input(Raw::Key { usage: hid, down, repeat })),
        }
        verdict == Verdict::Swallow
    }

    fn on_mouse(&self, msg: u32, info: &MSLLHOOKSTRUCT) -> bool {
        let button = |b: MouseButton, down: bool| {
            let v = self.filter.button(crate::control::button_index_pub(b), down);
            self.sink.reliable(CaptureEvent::Input(Raw::Button { button: b, down }));
            v == Verdict::Swallow
        };
        let xbutton = || if (info.mouseData >> 16) & 0xFFFF == 1 { MouseButton::Back } else { MouseButton::Forward };
        match msg {
            WM_MOUSEMOVE => self.filter.motion() == Verdict::Swallow, // position events come from Raw Input
            WM_LBUTTONDOWN => button(MouseButton::Left, true),
            WM_LBUTTONUP => button(MouseButton::Left, false),
            WM_RBUTTONDOWN => button(MouseButton::Right, true),
            WM_RBUTTONUP => button(MouseButton::Right, false),
            WM_MBUTTONDOWN => button(MouseButton::Middle, true),
            WM_MBUTTONUP => button(MouseButton::Middle, false),
            WM_XBUTTONDOWN => button(xbutton(), true),
            WM_XBUTTONUP => button(xbutton(), false),
            WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
                let delta = ((info.mouseData >> 16) as u16 as i16) as f32 / WHEEL_DELTA;
                let (dx, dy) = if msg == WM_MOUSEWHEEL { (0.0, delta as f64) } else { (delta as f64, 0.0) };
                self.sink.reliable(CaptureEvent::Input(Raw::Scroll { dx, dy, pixels: false }));
                self.filter.motion() == Verdict::Swallow
            }
            _ => false,
        }
    }

    fn on_raw_motion(&self, dx: i32, dy: i32) {
        let mut p = POINT { x: 0, y: 0 };
        // SAFETY: `p` is a valid out-pointer.
        unsafe {
            GetCursorPos(&mut p);
        }
        let s = self.scale as f64;
        self.sink.pointer(p.x as f64 / s, p.y as f64 / s, dx as f64, dy as f64);
    }
}

unsafe extern "system" fn raw_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_INPUT {
        let mut size: u32 = 0;
        let header = size_of::<RAWINPUTHEADER>() as u32;
        // SAFETY: first call asks for the required size; second fills a buffer of that size.
        unsafe {
            GetRawInputData(lparam as HRAWINPUT, RID_INPUT, null_mut(), &mut size, header);
            if size as usize >= size_of::<RAWINPUT>() && size < 4096 {
                let mut buf = vec![0u8; size as usize];
                let got = GetRawInputData(lparam as HRAWINPUT, RID_INPUT, buf.as_mut_ptr() as *mut _, &mut size, header);
                if got == size {
                    // The buffer is at least size_of::<RAWINPUT>() and was written by the OS.
                    let raw = &*(buf.as_ptr() as *const RAWINPUT);
                    if raw.header.dwType == RIM_TYPEMOUSE {
                        let m = raw.data.mouse;
                        // usFlags bit 0 clear ⇒ relative motion.
                        if m.usFlags & 1 == 0 && (m.lLastX != 0 || m.lLastY != 0) {
                            if let Some(st) = current() {
                                st.on_raw_motion(m.lLastX, m.lLastY);
                            }
                        }
                    }
                }
            }
        }
    }
    // SAFETY: default handling for everything else.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

fn run_hooks(st: Arc<HookState>, ready: std::sync::mpsc::Sender<Result<u32, BackendError>>) {
    if let Ok(mut g) = slot().lock() {
        *g = Some(st);
    }
    // SAFETY: Win32 setup on this thread; every created resource is released before returning.
    unsafe {
        let hinst = GetModuleHandleW(null());
        let class_name = wide("SynkflowRawInput");
        let wc = WNDCLASSW { lpfnWndProc: Some(raw_wndproc), hInstance: hinst, lpszClassName: class_name.as_ptr(), ..zeroed() };
        RegisterClassW(&wc); // already registered on a restart: fine
        let hwnd = CreateWindowExW(0, class_name.as_ptr(), class_name.as_ptr(), 0, 0, 0, 0, 0, HWND_MESSAGE, null_mut(), hinst, null());
        let rid = RAWINPUTDEVICE { usUsagePage: 0x01, usUsage: 0x02, dwFlags: RIDEV_INPUTSINK, hwndTarget: hwnd };
        let raw_ok = !hwnd.is_null() && RegisterRawInputDevices(&rid, 1, size_of::<RAWINPUTDEVICE>() as u32) != 0;
        let kb: HHOOK = SetWindowsHookExW(WH_KEYBOARD_LL, Some(ll_keyboard), hinst, 0);
        let ms: HHOOK = SetWindowsHookExW(WH_MOUSE_LL, Some(ll_mouse), hinst, 0);
        if kb.is_null() || ms.is_null() || !raw_ok {
            for h in [kb, ms] {
                if !h.is_null() {
                    UnhookWindowsHookEx(h);
                }
            }
            if !hwnd.is_null() {
                DestroyWindow(hwnd);
            }
            let _ = ready.send(Err(BackendError::Failed("Windows would not install the input hooks".into())));
            return;
        }
        let _ = ready.send(Ok(GetCurrentThreadId()));
        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        UnhookWindowsHookEx(kb);
        UnhookWindowsHookEx(ms);
        DestroyWindow(hwnd);
    }
}
