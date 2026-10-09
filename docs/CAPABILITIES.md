# Capability and support matrix

Legend — **Verified**: exercised by automated tests in this repository's environment. **Compiled**: builds for that target but was
not executed there. **Unverified**: written to the OS API contract; needs the manual checklist. **No**: not supported.

The build/test host was **macOS 13 (Intel)**. Cross-compilation proves only that the code builds.

**Update, 2026-10-09:** the author has tested Windows on a real laptop paired with a Mac and reports that it works. The Windows column below was
written before that test and keeps its original marks until each row is re-checked against the manual checklist. Linux (X11) is work in progress.

| Capability | macOS | Windows | Linux X11 | Linux Wayland |
|---|---|---|---|---|
| Protocol, TLS pinning, pairing, trust, revocation | Verified (loopback tests) | Compiled | Compiled | Compiled |
| Control state machine, layout, shortcut translation | Verified | Compiled | Compiled | Compiled |
| Clipboard rules (loop, dedupe, limits, images) | Verified (fake backend) | Compiled | Compiled | Compiled |
| File transfer (accept, hash, cancel, no overwrite) | Verified | Compiled | Compiled | Compiled |
| mDNS discovery | Verified (two instances) | Unverified | Unverified | Unverified |
| Display enumeration | Verified (real CoreGraphics) | Unverified | Unverified | No |
| Permission/capability reporting | Verified (real API calls) | n/a (none needed) | Unverified | Reports "unavailable" with reason |
| **Capture** keyboard/mouse (event tap / hooks / XI2) | **Unverified** (needs Accessibility + Input Monitoring) | Unverified | Unverified | **No** |
| **Inject** keyboard/mouse/scroll | **Unverified** | Unverified | Unverified | **No** |
| System clipboard read/write, change token | Compiled | Compiled | Compiled | Compiled (X11 path only) |
| Sensitive-clipboard marker respected | Compiled (`org.nspasteboard.*`) | Compiled (`ExcludeClipboardContentFromMonitorProcessing`) | No | No |
| Screen-lock pause | Compiled (CGSession dictionary, polled 1 Hz) | Compiled (`OpenInputDesktop`, polled 1 Hz) | No | No |
| Menu-bar / tray item | Compiled (Slint `SystemTrayIcon`); run on macOS | Compiled | Compiled | Compiled |
| Window UI | **Verified** (run on macOS) | Compiled | Compiled | Compiled |
| OS file drop into the window | Compiled | Compiled | Compiled | Compiled |
| Start at login (opt-in) | Tested (file written/removed in a temp HOME) | Compiled (`reg add`) | Tested (desktop entry) | Tested |
| Cross-computer *OS-level* drag-and-drop of files | **No** — use "Send files" or drop onto the Synkflow window | No | No | No |
| Raw/relative "game-lock" input | **No** | No | No | No |
| Login window / secure desktop / UAC control | **No** (by design) | **No** (by design) | No | No |

## Wayland

Wayland forbids global input capture and synthetic input through X11 APIs, and Synkflow neither runs a privileged helper nor
opens `/dev/uinput`. The supported route is the `InputCapture` / `RemoteDesktop` XDG portals with libei/EIS, which depends on
compositor support. **That integration is not implemented.** On Wayland the app starts, pairs, shares clipboard text/images and
sends files, and shows exactly why keyboard/mouse sharing is unavailable. It never pretends otherwise.

## Gaming and fullscreen

"Stay on this computer" is a reliable switch that stops all edge crossing. Synkflow makes **no** claim of compatibility with
competitive games or anti-cheat software, which commonly reject synthetic input.

## Keyboard limits

Keys are forwarded as physical positions (USB HID usages) and the receiving OS applies its own layout. Not handled: IME
composition, dead-key state across machines, JIS/ISO-specific extra keys (a few have no HID mapping and are not forwarded),
media keys, the Fn key, and OS-reserved shortcuts that never reach applications (e.g. `Ctrl+Alt+Del`, macOS secure-input fields).
Cmd↔Ctrl translation is optional, documented in `docs/ARCHITECTURE.md`, and off by default.
