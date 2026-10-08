//! Physical key identity, hotkeys, and optional ⌘↔Ctrl shortcut translation.
//!
//! Keys travel as **USB HID usages (page 0x07)** – a position on the keyboard,
//! not a character. Each platform backend converts its native scancode to and
//! from HID with the table below, and the receiving OS applies its own layout,
//! dead keys and input method. That is why Synkflow never "types text".

use std::collections::HashMap;

use crate::proto::InputEvent;

pub const LCTRL: u16 = 0xE0;
pub const LSHIFT: u16 = 0xE1;
pub const LALT: u16 = 0xE2;
pub const LMETA: u16 = 0xE3;
pub const RCTRL: u16 = 0xE4;
pub const RSHIFT: u16 = 0xE5;
pub const RALT: u16 = 0xE6;
pub const RMETA: u16 = 0xE7;
pub const KEY_ESCAPE: u16 = 0x29;

pub struct Key {
    pub hid: u16,
    pub name: &'static str,
    /// macOS virtual key code (kVK_*).
    pub mac: Option<u16>,
    /// Linux evdev code (X11 keycode is this + 8).
    pub evdev: Option<u16>,
    /// Windows set-1 scancode; `0xE0xx` marks an extended key.
    pub win: Option<u16>,
}

const fn k(hid: u16, name: &'static str, mac: Option<u16>, evdev: Option<u16>, win: Option<u16>) -> Key {
    Key { hid, name, mac, evdev, win }
}
const fn s(v: u16) -> Option<u16> {
    Some(v)
}

#[rustfmt::skip]
pub static KEYS: &[Key] = &[
    k(0x04, "A", s(0), s(30), s(0x1E)),   k(0x05, "B", s(11), s(48), s(0x30)),
    k(0x06, "C", s(8), s(46), s(0x2E)),   k(0x07, "D", s(2), s(32), s(0x20)),
    k(0x08, "E", s(14), s(18), s(0x12)),  k(0x09, "F", s(3), s(33), s(0x21)),
    k(0x0A, "G", s(5), s(34), s(0x22)),   k(0x0B, "H", s(4), s(35), s(0x23)),
    k(0x0C, "I", s(34), s(23), s(0x17)),  k(0x0D, "J", s(38), s(36), s(0x24)),
    k(0x0E, "K", s(40), s(37), s(0x25)),  k(0x0F, "L", s(37), s(38), s(0x26)),
    k(0x10, "M", s(46), s(50), s(0x32)),  k(0x11, "N", s(45), s(49), s(0x31)),
    k(0x12, "O", s(31), s(24), s(0x18)),  k(0x13, "P", s(35), s(25), s(0x19)),
    k(0x14, "Q", s(12), s(16), s(0x10)),  k(0x15, "R", s(15), s(19), s(0x13)),
    k(0x16, "S", s(1), s(31), s(0x1F)),   k(0x17, "T", s(17), s(20), s(0x14)),
    k(0x18, "U", s(32), s(22), s(0x16)),  k(0x19, "V", s(9), s(47), s(0x2F)),
    k(0x1A, "W", s(13), s(17), s(0x11)),  k(0x1B, "X", s(7), s(45), s(0x2D)),
    k(0x1C, "Y", s(16), s(21), s(0x15)),  k(0x1D, "Z", s(6), s(44), s(0x2C)),
    k(0x1E, "1", s(18), s(2), s(0x02)),   k(0x1F, "2", s(19), s(3), s(0x03)),
    k(0x20, "3", s(20), s(4), s(0x04)),   k(0x21, "4", s(21), s(5), s(0x05)),
    k(0x22, "5", s(23), s(6), s(0x06)),   k(0x23, "6", s(22), s(7), s(0x07)),
    k(0x24, "7", s(26), s(8), s(0x08)),   k(0x25, "8", s(28), s(9), s(0x09)),
    k(0x26, "9", s(25), s(10), s(0x0A)),  k(0x27, "0", s(29), s(11), s(0x0B)),
    k(0x28, "Enter", s(36), s(28), s(0x1C)),
    k(0x29, "Escape", s(53), s(1), s(0x01)),
    k(0x2A, "Backspace", s(51), s(14), s(0x0E)),
    k(0x2B, "Tab", s(48), s(15), s(0x0F)),
    k(0x2C, "Space", s(49), s(57), s(0x39)),
    k(0x2D, "Minus", s(27), s(12), s(0x0C)),
    k(0x2E, "Equal", s(24), s(13), s(0x0D)),
    k(0x2F, "LeftBracket", s(33), s(26), s(0x1A)),
    k(0x30, "RightBracket", s(30), s(27), s(0x1B)),
    k(0x31, "Backslash", s(42), s(43), s(0x2B)),
    k(0x33, "Semicolon", s(41), s(39), s(0x27)),
    k(0x34, "Quote", s(39), s(40), s(0x28)),
    k(0x35, "Grave", s(50), s(41), s(0x29)),
    k(0x36, "Comma", s(43), s(51), s(0x33)),
    k(0x37, "Period", s(47), s(52), s(0x34)),
    k(0x38, "Slash", s(44), s(53), s(0x35)),
    k(0x39, "CapsLock", s(57), s(58), s(0x3A)),
    k(0x3A, "F1", s(122), s(59), s(0x3B)), k(0x3B, "F2", s(120), s(60), s(0x3C)),
    k(0x3C, "F3", s(99), s(61), s(0x3D)),  k(0x3D, "F4", s(118), s(62), s(0x3E)),
    k(0x3E, "F5", s(96), s(63), s(0x3F)),  k(0x3F, "F6", s(97), s(64), s(0x40)),
    k(0x40, "F7", s(98), s(65), s(0x41)),  k(0x41, "F8", s(100), s(66), s(0x42)),
    k(0x42, "F9", s(101), s(67), s(0x43)), k(0x43, "F10", s(109), s(68), s(0x44)),
    k(0x44, "F11", s(103), s(87), s(0x57)), k(0x45, "F12", s(111), s(88), s(0x58)),
    k(0x46, "PrintScreen", None, s(99), s(0xE037)),
    k(0x47, "ScrollLock", None, s(70), s(0x46)),
    k(0x49, "Insert", s(114), s(110), s(0xE052)),
    k(0x4A, "Home", s(115), s(102), s(0xE047)),
    k(0x4B, "PageUp", s(116), s(104), s(0xE049)),
    k(0x4C, "Delete", s(117), s(111), s(0xE053)),
    k(0x4D, "End", s(119), s(107), s(0xE04F)),
    k(0x4E, "PageDown", s(121), s(109), s(0xE051)),
    k(0x4F, "Right", s(124), s(106), s(0xE04D)),
    k(0x50, "Left", s(123), s(105), s(0xE04B)),
    k(0x51, "Down", s(125), s(108), s(0xE050)),
    k(0x52, "Up", s(126), s(103), s(0xE048)),
    k(0x53, "NumLock", s(71), s(69), s(0x45)),
    k(0x54, "KeypadSlash", s(75), s(98), s(0xE035)),
    k(0x55, "KeypadStar", s(67), s(55), s(0x37)),
    k(0x56, "KeypadMinus", s(78), s(74), s(0x4A)),
    k(0x57, "KeypadPlus", s(69), s(78), s(0x4E)),
    k(0x58, "KeypadEnter", s(76), s(96), s(0xE01C)),
    k(0x59, "Keypad1", s(83), s(79), s(0x4F)), k(0x5A, "Keypad2", s(84), s(80), s(0x50)),
    k(0x5B, "Keypad3", s(85), s(81), s(0x51)), k(0x5C, "Keypad4", s(86), s(75), s(0x4B)),
    k(0x5D, "Keypad5", s(87), s(76), s(0x4C)), k(0x5E, "Keypad6", s(88), s(77), s(0x4D)),
    k(0x5F, "Keypad7", s(89), s(71), s(0x47)), k(0x60, "Keypad8", s(91), s(72), s(0x48)),
    k(0x61, "Keypad9", s(92), s(73), s(0x49)), k(0x62, "Keypad0", s(82), s(82), s(0x52)),
    k(0x63, "KeypadPeriod", s(65), s(83), s(0x53)),
    k(0x64, "IsoBackslash", s(10), s(86), s(0x56)),
    k(0x65, "Menu", None, s(127), s(0xE05D)),
    k(0x67, "KeypadEqual", s(81), s(117), None),
    k(0x68, "F13", s(105), s(183), s(0x64)), k(0x69, "F14", s(107), s(184), s(0x65)),
    k(0x6A, "F15", s(113), s(185), s(0x66)), k(0x6B, "F16", s(106), s(186), s(0x67)),
    k(0x6C, "F17", s(64), s(187), s(0x68)),  k(0x6D, "F18", s(79), s(188), s(0x69)),
    k(0x6E, "F19", s(80), s(189), s(0x6A)),  k(0x6F, "F20", s(90), s(190), s(0x6B)),
    k(LCTRL, "LeftControl", s(59), s(29), s(0x1D)),
    k(LSHIFT, "LeftShift", s(56), s(42), s(0x2A)),
    k(LALT, "LeftAlt", s(58), s(56), s(0x38)),
    k(LMETA, "LeftMeta", s(55), s(125), s(0xE05B)),
    k(RCTRL, "RightControl", s(62), s(97), s(0xE01D)),
    k(RSHIFT, "RightShift", s(60), s(54), s(0x36)),
    k(RALT, "RightAlt", s(61), s(100), s(0xE038)),
    k(RMETA, "RightMeta", s(54), s(126), s(0xE05C)),
];

pub fn from_mac(vk: u16) -> Option<u16> {
    KEYS.iter().find(|k| k.mac == Some(vk)).map(|k| k.hid)
}
pub fn to_mac(hid: u16) -> Option<u16> {
    KEYS.iter().find(|k| k.hid == hid).and_then(|k| k.mac)
}
pub fn from_evdev(code: u16) -> Option<u16> {
    // PrintScreen has two evdev codes (SYSRQ 99, PRINT 210); prefer the first row.
    KEYS.iter().find(|k| k.evdev == Some(code)).map(|k| k.hid).or(if code == 210 { Some(0x46) } else { None })
}
pub fn to_evdev(hid: u16) -> Option<u16> {
    KEYS.iter().find(|k| k.hid == hid).and_then(|k| k.evdev)
}
pub fn from_win(scan: u16) -> Option<u16> {
    KEYS.iter().find(|k| k.win == Some(scan)).map(|k| k.hid)
}
pub fn to_win(hid: u16) -> Option<u16> {
    KEYS.iter().find(|k| k.hid == hid).and_then(|k| k.win)
}
pub fn name_of(hid: u16) -> Option<&'static str> {
    KEYS.iter().find(|k| k.hid == hid).map(|k| k.name)
}
pub fn hid_of_name(name: &str) -> Option<u16> {
    KEYS.iter().find(|k| k.name.eq_ignore_ascii_case(name)).map(|k| k.hid)
}

pub fn is_modifier(hid: u16) -> bool {
    (LCTRL..=RMETA).contains(&hid)
}

// ───────────────────────────── modifiers and hotkeys ─────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Mods(pub u8);

impl Mods {
    pub const CTRL: u8 = 1;
    pub const SHIFT: u8 = 2;
    pub const ALT: u8 = 4;
    pub const META: u8 = 8;

    pub fn bit_of(hid: u16) -> u8 {
        match hid {
            LCTRL | RCTRL => Self::CTRL,
            LSHIFT | RSHIFT => Self::SHIFT,
            LALT | RALT => Self::ALT,
            LMETA | RMETA => Self::META,
            _ => 0,
        }
    }
    pub fn contains(self, m: Mods) -> bool {
        self.0 & m.0 == m.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Hotkey {
    pub mods: Mods,
    pub key: u16,
}

#[derive(Debug, thiserror::Error)]
#[error("not a valid shortcut: {0}")]
pub struct HotkeyParse(pub String);

impl Hotkey {
    pub const PANIC: Hotkey = Hotkey { mods: Mods(Mods::CTRL | Mods::ALT | Mods::SHIFT), key: KEY_ESCAPE };
    pub const SWITCH: Hotkey = Hotkey { mods: Mods(Mods::CTRL | Mods::ALT | Mods::SHIFT), key: 0x16 }; // S

    pub fn parse(text: &str) -> Result<Self, HotkeyParse> {
        let err = || HotkeyParse(text.to_string());
        let mut mods = 0u8;
        let mut key = None;
        for part in text.split('+').map(str::trim).filter(|p| !p.is_empty()) {
            match part.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => mods |= Mods::CTRL,
                "shift" => mods |= Mods::SHIFT,
                "alt" | "option" => mods |= Mods::ALT,
                "meta" | "cmd" | "command" | "win" | "super" => mods |= Mods::META,
                _ if key.is_none() => key = hid_of_name(part),
                _ => return Err(err()),
            }
        }
        match key {
            Some(key) if mods != 0 && !is_modifier(key) => Ok(Self { mods: Mods(mods), key }),
            _ => Err(err()),
        }
    }

    pub fn display(&self) -> String {
        let mut parts = Vec::new();
        for (bit, name) in [(Mods::CTRL, "Ctrl"), (Mods::ALT, "Alt"), (Mods::SHIFT, "Shift"), (Mods::META, "Meta")] {
            if self.mods.0 & bit != 0 {
                parts.push(name.to_string());
            }
        }
        parts.push(name_of(self.key).unwrap_or("?").to_string());
        parts.join("+")
    }
}

// ───────────────────────────── shortcut translation ─────────────────────────────

/// Which platform family is typing and which is receiving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mapping {
    /// Sender is macOS (⌘ is the shortcut key), receiver is Windows/Linux.
    MacToPc,
    /// Sender is Windows/Linux (Ctrl is the shortcut key), receiver is macOS.
    PcToMac,
}

/// Chord replacement: modifiers to hold, then the key to press.
type Chord = (&'static [u16], u16);

fn is_text_key(hid: u16) -> bool {
    matches!(hid, 0x04..=0x31 | 0x33..=0x38 if !matches!(hid, 0x29 | 0x2B | 0x2C))
}

fn map_chord(mapping: Mapping, key: u16) -> Option<Chord> {
    match mapping {
        Mapping::MacToPc => match key {
            0x14 => Some((&[LALT], 0x3D)),  // Cmd+Q → Alt+F4
            0x2B => Some((&[LALT], 0x2B)),  // Cmd+Tab → Alt+Tab
            0x50 => Some((&[], 0x4A)),      // Cmd+Left → Home
            0x4F => Some((&[], 0x4D)),      // Cmd+Right → End
            0x52 => Some((&[LCTRL], 0x4A)), // Cmd+Up → Ctrl+Home
            0x51 => Some((&[LCTRL], 0x4D)), // Cmd+Down → Ctrl+End
            0x28 | 0x2A | 0x4C | 0x4A | 0x4D | 0x4B | 0x4E => Some((&[LCTRL], key)),
            k if is_text_key(k) => Some((&[LCTRL], k)),
            _ => None, // Cmd+Space, F-keys, …: the Windows/Super key is delivered unchanged
        },
        Mapping::PcToMac => match key {
            0x50 | 0x4F | 0x2A | 0x4C => Some((&[LALT], key)), // word-wise navigation/deletion
            0x4A => Some((&[LMETA], 0x52)),                    // Ctrl+Home → Cmd+Up
            0x4D => Some((&[LMETA], 0x51)),                    // Ctrl+End → Cmd+Down
            k if is_text_key(k) => Some((&[LMETA], k)),
            _ => None, // Ctrl+Tab, Ctrl+Space, F-keys, …: Control is delivered unchanged
        },
    }
}

/// Pointer actions while the shortcut key is held (⌘+click ↔ Ctrl+click …).
fn pointer_prefix(mapping: Mapping) -> &'static [u16] {
    match mapping {
        Mapping::MacToPc => &[LCTRL],
        Mapping::PcToMac => &[LMETA],
    }
}

/// Stateful ⌘↔Ctrl translator. It only rewrites chords where the shortcut key
/// is held together with another key or a pointer action; a lone modifier tap
/// and every other key pass through unchanged. Every key-down it emits is
/// matched by a key-up, even if the physical release comes late or never.
#[derive(Debug)]
pub struct Translator {
    mapping: Mapping,
    /// Physical shortcut keys currently down: [left, right].
    primary: [bool; 2],
    /// Whether the original (untranslated) modifier has been delivered.
    delivered: [bool; 2],
    used: bool,
    /// Modifiers held on the primary's behalf.
    prefix: Vec<u16>,
    /// physical key → key actually emitted, for balanced releases.
    active: HashMap<u16, u16>,
}

impl Translator {
    pub fn new(mapping: Mapping) -> Self {
        Self { mapping, primary: [false; 2], delivered: [false; 2], used: false, prefix: vec![], active: HashMap::new() }
    }

    fn primary_keys(&self) -> [u16; 2] {
        match self.mapping {
            Mapping::MacToPc => [LMETA, RMETA],
            Mapping::PcToMac => [LCTRL, RCTRL],
        }
    }

    fn primary_idx(&self, hid: u16) -> Option<usize> {
        self.primary_keys().iter().position(|k| *k == hid)
    }

    fn key(usage: u16, down: bool, repeat: bool) -> InputEvent {
        InputEvent::Key { usage, down, repeat }
    }

    /// Make the held translated modifiers equal to `want`.
    fn reconcile(&mut self, want: &[u16], out: &mut Vec<InputEvent>) {
        for m in self.prefix.clone() {
            if !want.contains(&m) {
                out.push(Self::key(m, false, false));
                self.prefix.retain(|x| *x != m);
            }
        }
        for m in want {
            if !self.prefix.contains(m) {
                out.push(Self::key(*m, true, false));
                self.prefix.push(*m);
            }
        }
    }

    fn deliver_original(&mut self, out: &mut Vec<InputEvent>) {
        self.reconcile(&[], out);
        for i in 0..2 {
            if self.primary[i] && !self.delivered[i] {
                out.push(Self::key(self.primary_keys()[i], true, false));
                self.delivered[i] = true;
            }
        }
    }

    pub fn feed(&mut self, ev: InputEvent, out: &mut Vec<InputEvent>) {
        let primary_down = self.primary.iter().any(|d| *d);
        match ev {
            InputEvent::Key { usage, down, repeat } => {
                if let Some(i) = self.primary_idx(usage) {
                    if down {
                        if !repeat && !self.primary[i] {
                            if !primary_down {
                                self.used = false;
                            }
                            self.primary[i] = true;
                        }
                        // Held back until we know whether this is a chord.
                    } else if self.primary[i] {
                        self.primary[i] = false;
                        if self.delivered[i] {
                            out.push(Self::key(usage, false, false));
                            self.delivered[i] = false;
                        }
                        if !self.primary.iter().any(|d| *d) {
                            self.reconcile(&[], out);
                            if !self.used && !self.delivered.iter().any(|d| *d) {
                                // A lone tap of the shortcut key: pass it on unchanged.
                                out.push(Self::key(usage, true, false));
                                out.push(Self::key(usage, false, false));
                            }
                        }
                    }
                    return;
                }
                if is_modifier(usage) {
                    out.push(ev); // Shift / Alt / the other family's Meta: untouched.
                    return;
                }
                if !down {
                    let emitted = self.active.remove(&usage).unwrap_or(usage);
                    out.push(Self::key(emitted, false, false));
                    return;
                }
                if primary_down {
                    self.used = true;
                    match map_chord(self.mapping, usage) {
                        Some((prefix, mapped)) => {
                            self.reconcile(prefix, out);
                            self.active.insert(usage, mapped);
                            out.push(Self::key(mapped, true, repeat));
                        }
                        None => {
                            self.deliver_original(out);
                            self.active.insert(usage, usage);
                            out.push(ev);
                        }
                    }
                } else {
                    self.active.insert(usage, usage);
                    out.push(ev);
                }
            }
            InputEvent::Button { down: true, .. } | InputEvent::Scroll { .. } if primary_down => {
                self.used = true;
                self.reconcile(pointer_prefix(self.mapping), out);
                out.push(ev);
            }
            other => out.push(other),
        }
    }

    /// Release everything this translator is holding down (teardown).
    pub fn release_all(&mut self) -> Vec<InputEvent> {
        let mut out = Vec::new();
        for m in std::mem::take(&mut self.prefix) {
            out.push(Self::key(m, false, false));
        }
        for i in 0..2 {
            if self.delivered[i] {
                out.push(Self::key(self.primary_keys()[i], false, false));
            }
        }
        for (_, emitted) in self.active.drain() {
            out.push(Self::key(emitted, false, false));
        }
        self.primary = [false; 2];
        self.delivered = [false; 2];
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn down(u: u16) -> InputEvent {
        InputEvent::Key { usage: u, down: true, repeat: false }
    }
    fn up(u: u16) -> InputEvent {
        InputEvent::Key { usage: u, down: false, repeat: false }
    }
    fn run(t: &mut Translator, evs: &[InputEvent]) -> Vec<InputEvent> {
        let mut out = vec![];
        for e in evs {
            t.feed(*e, &mut out);
        }
        out
    }

    #[test]
    fn table_is_unique_and_round_trips() {
        for (i, a) in KEYS.iter().enumerate() {
            for b in &KEYS[i + 1..] {
                assert_ne!(a.hid, b.hid, "duplicate hid {}", a.name);
                if a.mac.is_some() {
                    assert_ne!(a.mac, b.mac, "duplicate mac code {} / {}", a.name, b.name);
                }
                if a.evdev.is_some() {
                    assert_ne!(a.evdev, b.evdev, "duplicate evdev {} / {}", a.name, b.name);
                }
                if a.win.is_some() {
                    assert_ne!(a.win, b.win, "duplicate win scancode {} / {}", a.name, b.name);
                }
            }
            if let Some(m) = a.mac {
                assert_eq!(from_mac(m), Some(a.hid));
            }
            if let Some(e) = a.evdev {
                assert_eq!(from_evdev(e), Some(a.hid));
            }
            if let Some(w) = a.win {
                assert_eq!(from_win(w), Some(a.hid));
            }
        }
        // Spot checks against well-known values.
        assert_eq!(to_mac(0x04), Some(0)); // A
        assert_eq!(to_evdev(0x29), Some(1)); // Escape
        assert_eq!(to_win(0x4F), Some(0xE04D)); // Right arrow is extended
        assert_eq!(to_win(0x1E), Some(0x02)); // 1
    }

    #[test]
    fn hotkeys_parse_and_display() {
        let h = Hotkey::parse("ctrl+alt+shift+escape").unwrap();
        assert_eq!(h, Hotkey::PANIC);
        assert_eq!(h.display(), "Ctrl+Alt+Shift+Escape");
        assert!(Hotkey::parse("Escape").is_err(), "a bare key would hijack typing");
        assert!(Hotkey::parse("Ctrl+Shift").is_err());
        assert!(Hotkey::parse("Ctrl+A+B").is_err());
        assert_eq!(Hotkey::parse("Cmd+Q").unwrap().mods.0, Mods::META);
    }

    #[test]
    fn cmd_c_becomes_ctrl_c_with_balanced_releases() {
        let mut t = Translator::new(Mapping::MacToPc);
        let out = run(&mut t, &[down(LMETA), down(0x06), up(0x06), up(LMETA)]);
        assert_eq!(out, vec![down(LCTRL), down(0x06), up(0x06), up(LCTRL)]);
    }

    #[test]
    fn lone_modifier_tap_passes_through() {
        let mut t = Translator::new(Mapping::MacToPc);
        assert_eq!(run(&mut t, &[down(LMETA), up(LMETA)]), vec![down(LMETA), up(LMETA)]);
    }

    #[test]
    fn modifier_stays_down_across_repeated_chords_and_switches_prefix() {
        let mut t = Translator::new(Mapping::MacToPc);
        let out = run(&mut t, &[down(LMETA), down(0x06), up(0x06), down(0x19), up(0x19)]);
        assert_eq!(out, vec![down(LCTRL), down(0x06), up(0x06), down(0x19), up(0x19)]);
        // Cmd+Tab → Alt+Tab with Alt held for the whole switcher session.
        let out = run(&mut t, &[down(0x2B), up(0x2B), down(0x2B), up(0x2B)]);
        assert_eq!(out, vec![up(LCTRL), down(LALT), down(0x2B), up(0x2B), down(0x2B), up(0x2B)]);
        assert_eq!(run(&mut t, &[up(LMETA)]), vec![up(LALT)]);
    }

    #[test]
    fn unmapped_chord_delivers_the_original_modifier() {
        let mut t = Translator::new(Mapping::MacToPc);
        let out = run(&mut t, &[down(LMETA), down(0x3A), up(0x3A), up(LMETA)]); // Cmd+F1
        assert_eq!(out, vec![down(LMETA), down(0x3A), up(0x3A), up(LMETA)]);
    }

    #[test]
    fn cmd_q_maps_to_alt_f4_and_arrows_to_home_end() {
        let mut t = Translator::new(Mapping::MacToPc);
        assert_eq!(run(&mut t, &[down(LMETA), down(0x14), up(0x14), up(LMETA)]), vec![down(LALT), down(0x3D), up(0x3D), up(LALT)]);
        let mut t = Translator::new(Mapping::MacToPc);
        assert_eq!(run(&mut t, &[down(LMETA), down(0x50), up(0x50), up(LMETA)]), vec![down(0x4A), up(0x4A)]);
    }

    #[test]
    fn pc_to_mac_maps_ctrl_to_cmd_but_leaves_ctrl_tab() {
        let mut t = Translator::new(Mapping::PcToMac);
        assert_eq!(run(&mut t, &[down(LCTRL), down(0x19), up(0x19), up(LCTRL)]), vec![down(LMETA), down(0x19), up(0x19), up(LMETA)]);
        let mut t = Translator::new(Mapping::PcToMac);
        assert_eq!(run(&mut t, &[down(LCTRL), down(0x2B), up(0x2B), up(LCTRL)]), vec![down(LCTRL), down(0x2B), up(0x2B), up(LCTRL)]);
        let mut t = Translator::new(Mapping::PcToMac);
        assert_eq!(run(&mut t, &[down(LCTRL), down(0x50), up(0x50), up(LCTRL)]), vec![down(LALT), down(0x50), up(0x50), up(LALT)]);
    }

    #[test]
    fn pointer_actions_with_the_shortcut_key_are_translated() {
        let mut t = Translator::new(Mapping::MacToPc);
        let btn = |d| InputEvent::Button { button: crate::proto::MouseButton::Left, down: d };
        let out = run(&mut t, &[down(LMETA), btn(true), btn(false), up(LMETA)]);
        assert_eq!(out, vec![down(LCTRL), btn(true), btn(false), up(LCTRL)]);
    }

    #[test]
    fn release_all_emits_matching_ups_for_a_cut_short_chord() {
        let mut t = Translator::new(Mapping::MacToPc);
        let out = run(&mut t, &[down(LMETA), down(0x06)]);
        assert_eq!(out, vec![down(LCTRL), down(0x06)]);
        let rel = t.release_all();
        assert!(rel.contains(&up(LCTRL)) && rel.contains(&up(0x06)));
    }

    #[test]
    fn independent_modifiers_and_other_family_meta_are_untouched() {
        let mut t = Translator::new(Mapping::MacToPc);
        assert_eq!(run(&mut t, &[down(LSHIFT), down(0x04), up(0x04), up(LSHIFT)]), vec![down(LSHIFT), down(0x04), up(0x04), up(LSHIFT)]);
        let mut t = Translator::new(Mapping::PcToMac);
        assert_eq!(run(&mut t, &[down(LMETA), up(LMETA)]), vec![down(LMETA), up(LMETA)]);
    }

    proptest! {
        /// Whatever the physical sequence, once every physical key is released
        /// the translator has released everything it pressed.
        #[test]
        fn translator_never_leaves_a_key_down(seq in proptest::collection::vec((0usize..8, any::<bool>(), 0usize..2), 0..60), pc in any::<bool>()) {
            let keys = [LMETA, RMETA, LCTRL, LSHIFT, 0x06, 0x2B, 0x50, 0x3A];
            let mut t = Translator::new(if pc { Mapping::PcToMac } else { Mapping::MacToPc });
            let mut held = std::collections::HashSet::new();
            let mut out = vec![];
            for (k, dn, _) in seq {
                let key = keys[k];
                if dn && held.insert(key) { t.feed(down(key), &mut out); }
                else if !dn && held.remove(&key) { t.feed(up(key), &mut out); }
            }
            for key in held.drain() { t.feed(up(key), &mut out); }
            let mut net: HashMap<u16, i32> = HashMap::new();
            for e in &out {
                if let InputEvent::Key { usage, down, .. } = e {
                    *net.entry(*usage).or_default() += if *down { 1 } else { -1 };
                }
            }
            for (k, v) in net {
                prop_assert!(v == 0, "key {k:#x} unbalanced by {v}: {out:?}");
            }
        }
    }
}
