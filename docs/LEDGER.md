# Implementation ledger

What was built, what was verified (and how), what is incomplete, blocked, or unverified. Written for honesty, not marketing:
nothing here is "production-ready" — this is a tested vertical slice with known gaps.

Build/test host: **macOS 13, Intel (x86_64), Rust 1.96.1**. Windows was cross-compiled here and has since been **tested by the author on a real Windows laptop paired with a Mac (works)**. Linux (X11) is work in progress and was not built here.

## Completed and verified by automated tests (171 tests; `cargo test`)

| Area | Evidence |
|---|---|
| Device identity (Ed25519 cert, SHA-256 fingerprint, grouped display, parse), key storage (keyring + 0600 file fallback) | `identity` tests |
| TLS 1.3 mutual auth with pinned identities; strict vs pairing-only modes; untrusted client refused; wrong server refused; revocation at next handshake; no resumption | `tls` tests (6) |
| Bounded protocol: preamble/version check, frame limits before allocation, structural validation, trailing bytes, name sanitising | `proto` tests (9) |
| Reciprocal verified pairing (one-sided approval trusts nothing; decline; closed window; expiry; rate limiting at accept) | `tests/engine.rs` |
| Control state machine incl. pending/ack/timeout, panic, pause, lock, loss, permission withdrawal, dwell/modifier/corner/edge rules, multi-hop, translator, held-input release | `control` tests (27) + engine tests |
| Layout geometry (adjacency, spans, gaps, pinch points, mixed DPI conversion, snapping, reachability) and the editor draft | `geometry` (13), `layout_editor` (9) |
| Shortcut translation (⌘↔Ctrl) incl. property test that every emitted key-down is released | `keys` tests |
| Capture filter (grab, hotkeys, never-stuck-key rule), motion coalescing without losing deltas | `platform` tests |
| Clipboard: loop prevention, de-duplication without history, size limits, PNG-bomb rejection, permissions, pause, private items | `clipboard` tests + `tests/engine_data.rs` |
| File transfer: safe names, capability inbox (symlinks, no overwrite), atomic finalise, SHA-256, cancel, overrun, disconnect cleanup, offers/accept/decline, folders refused, 96 MiB streaming | `transfer` tests (14) + engine tests |
| Hostile peer handling (unsafe names, control without permission, oversize/garbage frames, bulk attach without secret) | `tests/engine_data.rs` |
| Session: heartbeat timeout, protocol errors, distinct dial errors, pairing channel isolation | `session` tests |
| Settings persistence, corrupt-file recovery, clamping, forward compatibility; start-at-login entries | `config`, `autostart` tests |
| Automatic reconnect after a restart | `tests/engine.rs` |
| Design tokens meet WCAG targets in both themes; status never colour-only | `tests/design.rs` |
| Discovery: two mDNS instances find each other | `discovery` test (ignored by default; passed when run with `--ignored`) |

## Verified by running the real thing on macOS 13 (the build machine)

* The GUI (all screens, dark and light, large text, reduced motion/transparency, onboarding, pairing, verification, layout, devices,
  transfers, settings) rendered from **real engine state** (two engines paired over loopback) — `examples/ui_snapshots.rs`.
  The fake OS layer was used there; screenshots show behaviour of the UI, not of OS capture.
* Release binary: `--version`, `--selftest` (real CoreGraphics display list and permission checks), 10-second GUI launch without a crash.
* macOS `.dmg`: checksum valid; bundle structure; Info.plist; ad-hoc signature valid; real `.icns`; app runs from the mounted image.
* Real CoreGraphics display enumeration and permission-state reporting (`platform::macos` tests).
* **Unified title bar (macOS)**: the real window was launched and screen-captured: no grey system bar, the dark UI runs to the top edge and the
  traffic lights float over it (onboarding screen, dark theme). Every other screen was checked as a rendered snapshot with the 28 pt inset
  (`SYNKFLOW_SNAP_TITLEBAR=1`), which does not draw the native bar. **Not exercised**: dragging by the strip, double-click zoom (both rely on
  Slint's `WindowMoveArea` and a `set_maximized` call), full-screen mode, the light theme in a real window, and the post-onboarding screens in a real window.

## Mac ↔ Windows connection fixes (2026-10-08)

Reported: "the Mac app doesn't sync with a Windows laptop". **Not reproduced** (no Windows machine here); these are defects found by reading
the code, each of which can make exactly that happen. All have automated tests; none has been run against a real Windows laptop.

| Found | Fix |
|---|---|
| A peer's addresses were sorted by raw IP number and only 4 kept, so on a laptop with WSL/Hyper-V/VPN adapters the `169.254.x`/`172.x` ones came first and the real Wi-Fi address could be cut off. | `discovery::rank_addrs`: the address on a LAN we share first, link-local last, 8 kept. |
| Pairing from the list used only the first address; session dials tried addresses one by one (5 s each). | `session::race`: all addresses tried together (250 ms apart), first success wins; the winner is remembered. |
| Re-announcements put the *worst* address first (inserted one by one at the front). | `merge_endpoints` keeps the announcement's best-first order. |
| Windows virtual adapters (vEthernet, VMware, VirtualBox) were advertised over mDNS. | Treated like tunnels: not advertised. |
| Synkflow **started paused on every launch** and ⌘/Ctrl translation was off, so a paired Mac and PC looked dead after any restart. | New defaults (active, translation on between Mac and PC) and a one-time migration of old settings files (version 2). |
| If the macOS Keychain refused access, a **new identity** was silently created in a file, so Windows would no longer recognise the Mac. | An identity that exists anywhere always wins; a denied Keychain stops the app with an explanation. |
| No way to see why a connection failed (a Windows GUI program has no console). | `synkflow.log` in the settings folder, a startup line, failures logged, "Copy diagnostics" includes the log tail. |
| When the other computer's firewall blocks pairing, nothing said what to do. | The pairing dialog shows this computer's address (so pairing can start from the other side) and the error explains the firewall; `docs/TUTORIAL.txt` has the Windows steps. |

Windows Firewall remains the most likely cause on a real laptop and cannot be fixed from inside an unsigned per-user app without administrator
rights; the tutorial lists the steps. Verified here with the real macOS binary: the log is written with this Mac's real LAN address, an old
settings file starts with sharing active, the GUI launches.

## Windows artifacts: cross-built and checked here, then tested by the author on real hardware

* **Real-hardware test (2026-10-09):** the author ran the Windows app on a real Windows laptop paired with a Mac and reports that it works. What was
  exercised in that test was not recorded in detail, so the structural checks below remain the evidence this repository can show itself.

* `synkflow.exe` and `Synkflow-Setup-0.1.0.exe` were cross-built for `x86_64-pc-windows-gnu` with `cargo zigbuild` (zig as C compiler/linker).
* Checked by parsing the PE headers: 64-bit, **GUI subsystem**, imports only Windows system DLLs (no MinGW runtime DLL is needed).
* Installer: all six payload files are embedded byte-for-byte; its 4 logic tests pass natively on macOS (`cargo test` in the installer crate).
* Found by doing this: the installer crate had never been compiled for Windows and was missing a `windows-sys` feature, and the first
  `synkflow.exe` was a console-subsystem program (it would have opened a black window). Both fixed, then rebuilt.
* The macOS `.dmg` was **rebuilt from the final sources** after those fixes (release build, `--selftest` OK, `clippy -D warnings` clean with and
  without the UI, `cargo fmt --check` clean, tests passed, signature and contents re-verified from the mounted image).
* Unsigned: Windows SmartScreen will warn. Because the author's test was not recorded in detail, uninstall, the Start-menu shortcut and the Apps & features entry are not independently verified.

## Implemented, with parts **not independently verified** (needs the manual checklist)

* **Real keyboard/mouse capture and injection on macOS** (event tap / `CGEventPost`). The permission checks report "available" on
  the build machine, but no input was captured or injected (doing so would have moved the author's real pointer).
* The **Windows** backend (low-level hooks, Raw Input, `SendInput`, monitors, lock detection), Windows clipboard markers, the Windows
  GUI (software renderer) and the **Windows installer** — cross-compiled for `x86_64-pc-windows-gnu`; tested by the author on a real laptop (see above),
  but not covered by automated tests.
* The **Linux/X11** backend (XInput2 raw events, XTEST, RandR) — **work in progress**: written, never compiled with the Linux target in this
  environment (see below) and never run.
* mDNS on Windows/Linux; start-at-login on Windows; tray on every OS other than macOS; OS file-drop events (compiled, not exercised).
* Screen-reader behaviour of the UI (AccessKit): not tested.

## Incomplete / not implemented (by choice, labelled in the UI/docs)

* **Wayland** keyboard/mouse sharing (portals + libei). Reported as unavailable with reasons.
* OS-level cross-computer drag-and-drop (use "Send files" or drop on the window); folders in transfers; transfer resume.
* Game-grade raw/relative input; media keys, Fn, IME, JIS-only keys.
* QR-assisted fingerprint verification (scanning would need camera access); fingerprints are compared as text.
* Screen-lock detection on Linux; per-monitor scale on X11; hiding the cursor while it is frozen on macOS.
* Fuzz targets, `cargo audit`/`cargo deny` runs, packet-capture run, two-machine acceptance run, real-hardware/real-network benchmarks (a loopback latency run, transfer memory and idle usage of the macOS app *were* measured: `docs/BENCHMARKS.md`).
* Code signing/notarization (documented, not performed) — installers are unsigned.

## Blocked by the environment

* The build machine had very little free disk for the whole session. Builds were done strictly one at a time and the large
  test was run at 32 MiB on the final pass (96 MiB passed earlier, peak RSS ≈ 10 MB). Consequently Linux cross-compilation was not
  attempted, and benchmark coverage is limited (see `docs/BENCHMARKS.md`).
* No Windows or Linux machine was available to the build session itself; the Windows test above was done separately by the author, and Linux has not been run.

## Deliberate security decisions worth re-reading

1. Pairing channel accepts any presented certificate **but grants nothing** and is gated by a user-opened, expiring window.
2. TLS session resumption is disabled so revocation cannot be bypassed.
3. Clipboard is opt-in per device, never stored, skipped for password-manager markers (best effort, documented as incomplete).
4. Files go to a fresh folder through a capability handle; names are flat and sanitised; nothing is opened or overwritten.
5. Identity key falls back to a `0600` file when no credential store exists, and the app says so.

## Ponytail review (smallest complete implementation)

Findings acted on: removed dead fields and unused abstractions, fake backends compiled only with `dev-backend`, one Cargo package, Slint
behind a feature so the engine tests need no UI, platform trait kept (it has four real implementations), no database, no wrapper crates.
Items consciously left simple with named ceilings: global 1 Hz housekeeping tick (clipboard 400 ms change-counter poll while active),
single-controller model, linear key-table scans (≈110 entries).

## Apple-design review

Acted on: response on pointer-down (0 ms) with calmer release; tiles follow the pointer 1:1 and glide to snapped places (160 ms);
critically-damped motion only (no overshoot; nothing is flicked); symmetric enter/exit of pages by cross-fade; materials restrained
(solid surfaces, two static washes); size-specific tracking; reduced motion/transparency/contrast respected; hierarchy by weight and size.
Not done: spring/velocity handoff (no flick gestures exist in this UI), blur materials (Slint has no backdrop blur), hover tooltips.
