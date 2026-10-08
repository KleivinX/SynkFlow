//! In-memory backends for tests and the explicitly labeled developer mode
//! (`--features dev-backend`). They exercise the engine, never the OS: nothing
//! here may be cited as evidence that real capture or injection works.

use std::sync::{Arc, Mutex};

use super::*;

#[derive(Default)]
struct FakeState {
    sink: Option<CaptureSink>,
    filter: Option<Arc<CaptureFilter>>,
    injected: Vec<InputEvent>,
    injected_at: Vec<std::time::Instant>,
    warps: Vec<(f64, f64)>,
    locked: bool,
}

pub struct FakeInput {
    pub displays: Vec<DisplayInfo>,
    st: Mutex<FakeState>,
}

impl FakeInput {
    pub fn new(displays: Vec<DisplayInfo>) -> Arc<Self> {
        Arc::new(Self { displays, st: Mutex::new(FakeState::default()) })
    }

    pub fn standard(w: u32, h: u32) -> Arc<Self> {
        Self::new(vec![DisplayInfo { id: 1, name: "Fake display".into(), x: 0, y: 0, width: w, height: h, scale: 1.0, rotation: 0, primary: true }])
    }

    /// Feed a physical event as the OS capture would, honouring the grab filter.
    /// Returns whether the local OS would still have received it.
    pub fn physical(&self, raw: Raw) -> Verdict {
        let (sink, filter) = {
            let st = self.st.lock().unwrap();
            (st.sink.clone(), st.filter.clone())
        };
        let (Some(sink), Some(filter)) = (sink, filter) else { return Verdict::Pass };
        match raw {
            Raw::Pointer { x, y, dx, dy } => {
                sink.pointer(x, y, dx, dy);
                filter.motion()
            }
            Raw::Key { usage, down, repeat } => {
                let (v, action) = filter.key(usage, down);
                match action {
                    Some(a) => sink.reliable(CaptureEvent::Hotkey(a)),
                    None if v == Verdict::Pass || filter.grabbed() => sink.reliable(CaptureEvent::Input(Raw::Key { usage, down, repeat })),
                    None => sink.reliable(CaptureEvent::Input(Raw::Key { usage, down, repeat })),
                }
                v
            }
            Raw::Button { button, down } => {
                let v = filter.button(crate::control::button_index_pub(button), down);
                sink.reliable(CaptureEvent::Input(raw));
                v
            }
            Raw::Scroll { .. } => {
                sink.reliable(CaptureEvent::Input(raw));
                filter.motion()
            }
        }
    }

    pub fn lose_capture(&self) {
        if let Some(s) = self.st.lock().unwrap().sink.clone() {
            s.reliable(CaptureEvent::Lost);
        }
    }

    pub fn set_locked(&self, l: bool) {
        self.st.lock().unwrap().locked = l;
    }
    pub fn injected(&self) -> Vec<InputEvent> {
        self.st.lock().unwrap().injected.clone()
    }
    pub fn clear_injected(&self) {
        let mut st = self.st.lock().unwrap();
        st.injected.clear();
        st.injected_at.clear();
    }
    /// When each event in `injected()` was applied (for latency measurements).
    pub fn injected_times(&self) -> Vec<std::time::Instant> {
        self.st.lock().unwrap().injected_at.clone()
    }
    pub fn warps(&self) -> Vec<(f64, f64)> {
        self.st.lock().unwrap().warps.clone()
    }
    pub fn grabbed(&self) -> bool {
        self.st.lock().unwrap().filter.as_ref().is_some_and(|f| f.grabbed())
    }
}

struct FakeCapture {
    owner: Arc<FakeInput>,
    filter: Arc<CaptureFilter>,
}

impl Capture for FakeCapture {
    fn set_grab(&self, grab: bool) {
        self.filter.set_grab(grab);
    }
    fn warp(&self, x: f64, y: f64) {
        self.owner.st.lock().unwrap().warps.push((x, y));
    }
    fn set_hotkeys(&self, panic: Hotkey, switch: Hotkey) {
        self.filter.set_hotkeys(panic, switch);
    }
}

impl Drop for FakeCapture {
    fn drop(&mut self) {
        let mut st = self.owner.st.lock().unwrap();
        st.sink = None;
        st.filter = None;
    }
}

impl InputBackend for Arc<FakeInput> {
    fn capabilities(&self) -> BackendCaps {
        BackendCaps {
            platform: Platform::Other,
            api: "In-memory fake (developer/test only)",
            capture: Cap::Available,
            inject: Cap::Available,
            notes: vec!["Fake backend: does not touch the operating system.".into()],
        }
    }
    fn displays(&self) -> Result<Vec<DisplayInfo>, BackendError> {
        Ok(self.displays.clone())
    }
    fn start_capture(&self, sink: CaptureSink, panic: Hotkey, switch: Hotkey) -> Result<Box<dyn Capture>, BackendError> {
        let filter = Arc::new(CaptureFilter::new(panic, switch));
        {
            let mut st = self.st.lock().unwrap();
            st.sink = Some(sink);
            st.filter = Some(filter.clone());
        }
        Ok(Box::new(FakeCapture { owner: self.clone(), filter }))
    }
    fn inject(&self, ev: &InputEvent) -> Result<(), BackendError> {
        let mut st = self.st.lock().unwrap();
        st.injected.push(*ev);
        st.injected_at.push(std::time::Instant::now());
        Ok(())
    }
    fn session_locked(&self) -> bool {
        self.st.lock().unwrap().locked
    }
}

/// Clipboard held in memory.
#[derive(Default, Clone)]
pub struct FakeClipboard {
    inner: Arc<Mutex<(Option<ClipContent>, u64, bool)>>,
    writes: Arc<std::sync::atomic::AtomicU32>,
}

impl FakeClipboard {
    /// Simulate the user copying something.
    pub fn user_copy(&self, c: ClipContent, sensitive: bool) {
        let mut g = self.inner.lock().unwrap();
        g.0 = Some(c);
        g.1 += 1;
        g.2 = sensitive;
    }
    pub fn content(&self) -> Option<ClipContent> {
        self.inner.lock().unwrap().0.clone()
    }
    /// How many times the engine wrote to this clipboard.
    pub fn write_count(&self) -> u32 {
        self.writes.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl ClipboardBackend for FakeClipboard {
    fn change_token(&mut self) -> Option<u64> {
        Some(self.inner.lock().unwrap().1)
    }
    fn read(&mut self) -> Result<Option<ClipContent>, BackendError> {
        Ok(self.inner.lock().unwrap().0.clone())
    }
    fn write(&mut self, c: &ClipContent) -> Result<Option<u64>, BackendError> {
        self.writes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut g = self.inner.lock().unwrap();
        g.0 = Some(c.clone());
        g.1 += 1;
        g.2 = false;
        Ok(Some(g.1))
    }
    fn is_sensitive(&mut self) -> bool {
        self.inner.lock().unwrap().2
    }
}
