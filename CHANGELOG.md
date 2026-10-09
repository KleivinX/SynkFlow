# Changelog

## 0.1.0 — early preview (2026-10-08)

First public release of Synkflow. It is a tested vertical slice, not a finished product; what is and is not verified is listed in
[`docs/LEDGER.md`](docs/LEDGER.md).

### What is in it
- **One mouse and keyboard across computers**: push the pointer past a screen edge and it continues on the next computer; the keyboard follows.
  Drag-to-arrange layout with per-edge rules, snapping and mixed-DPI handling.
- **Shared clipboard** (text and images, opt-in per device) and **file sending** (approved by the receiver, SHA-256 verified, never overwrites).
- **Security by default**: TLS 1.3 with mutual authentication and pinned device identities, explicit pairing by comparing full fingerprints on both
  computers, per-device permissions, emergency stop. No account, no cloud, no relay, no telemetry.
- **Backends** for macOS (CoreGraphics), Windows (low-level hooks and `SendInput`) and Linux X11 (XInput2 and XTEST). Linux Wayland is reported as unsupported.
- **Native Slint interface**: onboarding, overview, screen layout, devices, transfers, settings, tray / menu-bar item, light and dark themes,
  reduced motion and transparency, large text. On macOS the window uses a unified title bar.
- **Connecting a Mac and a Windows laptop**: all of a computer's network addresses are ranked and tried together, a connection log
  (`synkflow.log`) with "Copy diagnostics", this computer's address shown in the pairing dialog, and Windows Firewall steps in the tutorial.
- **Packaging**: macOS `.dmg` (ad-hoc signed) and a per-user Windows installer (no administrator rights).

### Known limitations
- Installers are **unsigned**: macOS Gatekeeper and Windows SmartScreen will warn on first launch.
- The macOS build is Intel (x86_64).
- Real keyboard and mouse capture and injection are not covered by the automated tests (they are checked by hand). Windows has been tested by the author on a real laptop paired with a Mac and works. Linux (X11) is work in progress and not yet tested.
- No Wayland input sharing, no folder transfers, no transfer resume, no game-grade raw input.
