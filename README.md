<div align="center">

<img src="assets/icon-256.png" width="128" alt="Synkflow logo">

# Synkflow

### Your desk. In sync.

One mouse and keyboard across your own computers, with a shared clipboard and file sending.<br>
Open source · 100 % local · no account · no cloud · no relay · no telemetry.

<p>
  <a href="LICENSE"><img alt="License: GPL-3.0-only" src="https://img.shields.io/badge/license-GPL--3.0--only-blue?style=flat-square"></a>
  <img alt="Rust" src="https://img.shields.io/badge/Rust-1.96-orange?style=flat-square&logo=rust&logoColor=white">
  <img alt="UI: Slint" src="https://img.shields.io/badge/UI-Slint-2379F4?style=flat-square">
  <img alt="Platforms: macOS, Windows, Linux X11" src="https://img.shields.io/badge/macOS_·_Windows_·_Linux_(X11)-lightgrey?style=flat-square">
  <img alt="171 tests passing" src="https://img.shields.io/badge/tests-171_passing-brightgreen?style=flat-square">
  <img alt="Telemetry: none" src="https://img.shields.io/badge/telemetry-none-success?style=flat-square">
  <img alt="Status: v0.1 early preview" src="https://img.shields.io/badge/status-v0.1_early_preview-F5A524?style=flat-square">
</p>

<p>
  <a href="#see-it-in-action"><b>See it in action</b></a> ·
  <a href="#download"><b>Download</b></a> ·
  <a href="#quick-start">Quick start</a> ·
  <a href="#how-it-works">How it works</a> ·
  <a href="#security-in-one-paragraph">Security</a> ·
  <a href="#documentation">Docs</a>
</p>

<img src="docs/media/hero.jpg" width="900" alt="The Synkflow Overview screen on macOS: a MacBook Pro and a Mac mini arranged side by side, sharing one mouse and keyboard">

</div>

<br>

Synkflow is a **software KVM**. Push the pointer past the edge of one computer's screen and it carries on at the next computer, with
the keyboard following. Copy on one, paste on the other. Drop a file on the window to send it. It all happens on **your own network**,
between computers **you** approved by comparing their fingerprints. Nothing goes to the internet, ever.

It is written in **Rust** (Tokio, rustls / TLS 1.3, mDNS) with a native **Slint** interface: no browser, no Electron, no web server.

## See it in action

<table>
<tr>
<td width="300" valign="top">
<img src="docs/media/showcase-preview.gif" width="280" alt="Short preview of the Synkflow film: the pointer leaving a MacBook and arriving on a Windows monitor, then text copied across">
</td>
<td valign="top">

**The 40-second film** (the clip on the left is a taste of it)

⬇ [`synkflow-showcase.mp4`](https://github.com/KleivinX/SynkFlow/raw/main/docs/media/synkflow-showcase.mp4) · 1080×1920 · 12 MB<br>
⬇ [`synkflow-teaser.mp4`](https://github.com/KleivinX/SynkFlow/raw/main/docs/media/synkflow-teaser.mp4) · the 23-second teaser · 7 MB

Pair once. Arrange your screens by dragging. Push past the edge. Copy here, paste there. Send a file. Hit emergency stop.

<sub>The film is a concept animation with simulated Mac and Windows screens, not a screen recording. The real interface is shown below.</sub>

</td>
</tr>
</table>

<p align="center">
  <img src="docs/media/showcase-strip.png" width="900" alt="Five frames from the film: Pair once, Arrange your screens, One mouse and one keyboard, Copy here and paste there, Emergency stop">
</p>

## The interface

<table>
<tr>
<td width="50%"><img src="docs/media/shot-layout.jpg" alt="Screen layout: drag computers to where they sit on your desk"><br><sub><b>Arrange by dragging.</b> Computers, with all their monitors, snap into place.</sub></td>
<td width="50%"><img src="docs/media/shot-pairing.jpg" alt="Pairing: compare the full fingerprints on both computers before approving"><br><sub><b>Pair by comparing fingerprints.</b> Nothing is trusted until both people approve.</sub></td>
</tr>
<tr>
<td width="50%"><img src="docs/media/shot-devices.jpg" alt="Devices: per-computer permissions for input, clipboard and files"><br><sub><b>Per-device permissions</b> for input, clipboard and files.</sub></td>
<td width="50%"><img src="docs/media/shot-transfers.jpg" alt="Transfers: the receiver approves every file, and each is SHA-256 verified"><br><sub><b>Files the other side approves,</b> verified with SHA-256.</sub></td>
</tr>
<tr>
<td width="50%"><img src="docs/media/shot-light.jpg" alt="The light theme of the Overview screen"><br><sub><b>Light and dark,</b> plus reduced motion, reduced transparency and large text.</sub></td>
<td width="50%"><img src="docs/media/shot-onboarding.jpg" alt="Onboarding welcome screen: no account, no cloud relay, you approve every device"><br><sub><b>Five-screen onboarding</b> that says what it does and what it never does.</sub></td>
</tr>
</table>

<sub>These screenshots are rendered by the real interface from real engine state (two engines paired over loopback TLS); only the operating-system keyboard and mouse layer is a stand-in. Regenerate them with `examples/ui_snapshots.rs`.</sub>

## What it does

| | |
|---|---|
| **Cross the edge** | Push the pointer past the edge of one computer's screen and it continues on the next. Keyboard focus follows. |
| **Arrange by dragging** | Drag computers (each with all its monitors) to where they sit on your desk. Snapping, validity checks, per-edge rules. |
| **Mac ↔ PC shortcuts** | ⌘C on a Mac keyboard is Ctrl+C on the Windows computer, and the other way round. Only between a Mac and a PC. |
| **Clipboard** | Text and images, opt-in per device, never stored or logged, loop-proof, skips items password managers mark private. |
| **Send files** | Choose files or drop them on the window. The other side approves; SHA-256-verified; never overwrites; never opens anything. |
| **Discovery** | mDNS on your LAN, or type an address. Discovery only introduces a device; **you** approve it by comparing fingerprints. |
| **Emergency stop** | `Ctrl+Alt+Shift+Esc` (configurable) and a menu-bar item return control and release every held key. |

## Download

> **Early preview (v0.1).** The installers are **unsigned**, so macOS Gatekeeper and Windows SmartScreen will warn the first time.
> That is expected for a project without paid signing certificates. Read [the status](#status-what-is-and-is-not-verified) first.

Installers are attached to the [**v0.1.0 release**](https://github.com/KleivinX/SynkFlow/releases).

| Platform | File | Notes |
|---|---|---|
| **macOS 11+** | `Synkflow-0.1.0.dmg` | Intel (x86_64) build. Open it, drag Synkflow to Applications, then right-click → Open the first time. Grant Accessibility and Input Monitoring when asked. |
| **Windows 10 / 11** (64-bit) | `Synkflow-Setup-0.1.0.exe` | Per-user install, no administrator rights. When Windows Firewall asks, allow Synkflow on **private** networks. |
| **Linux (X11)** | build from source | Needs an X11 session. Wayland cannot capture or inject input and is reported as unsupported. |

A plain-text walkthrough with troubleshooting is in [`docs/TUTORIAL.txt`](docs/TUTORIAL.txt).

## Quick start

1. Install Synkflow on both computers and open it. A five-screen onboarding walks you through permissions.
2. On one computer open **Pair a device**. The other computer appears under *Nearby computers* (or type its address).
3. Both screens show **the same two fingerprints**. Check they match, then approve on **both** computers.
4. Drag the computers into the order they sit on your desk, then **Apply**.
5. Push the pointer past the shared edge. You are now using one mouse and keyboard.

<details>
<summary><b>Run or build from source</b></summary>

```bash
cargo run --release                   # the desktop app
cargo run --release -- --selftest     # headless engine check, prints capabilities
cargo test --no-default-features      # all tests without compiling the UI
cargo fmt --check && cargo clippy --all-targets
```

Packaging (macOS `.app`/`.dmg`, Windows installer, Linux desktop entry), signing and notarization notes, and **offline / vendored builds** are in
[`docs/BUILDING.md`](docs/BUILDING.md). Environment variables for testing, such as two instances on one computer:
`SYNKFLOW_CONFIG_DIR=<dir>`, `SYNKFLOW_SECRET_STORE=file`, `SYNKFLOW_LOG=debug`, `SYNKFLOW_DEVICE_NAME=<name>`.

</details>

## How it works

```mermaid
flowchart LR
    subgraph A[Computer A]
        capA[Capture: event tap, hooks, XInput2] --> engA[Engine]
        uiA[Slint UI] <--> engA
        engA --> injA[Inject: CGEventPost, SendInput, XTEST]
    end
    subgraph B[Computer B]
        capB[Capture] --> engB[Engine]
        uiB[Slint UI] <--> engB
        engB --> injB[Inject]
    end
    engA <-->|TLS 1.3, mutual auth, pinned identities| engB
    mdns((mDNS)) -.->|introduces only| engA
    mdns -.-> engB
```

- **One engine per computer.** A single actor owns all state; a pure state machine (events in, effects out) decides who controls whom, so
  the logic is testable without a keyboard, a mouse or a network.
- **Two engines in the tests.** The automated tests pair two complete engines over loopback TLS, including a hand-driven hostile peer.
- **Operating-system layers are small and separate.** Capture and injection sit behind one trait with macOS, Windows and X11 implementations.

Deeper reading: [Architecture & protocol](docs/ARCHITECTURE.md) · [Design system](docs/DESIGN.md).

## Security in one paragraph

Every byte between your computers (input, clipboard, files, control messages) travels over **TLS 1.3 with mutual authentication and pinned
device identities**; there is no plaintext fallback and no "accept any certificate" path to a session. A device becomes trusted only after both
people compare the **full SHA-256 fingerprints** and both approve. Revoking a device ends its sessions immediately. Transport encryption does
**not** protect against malware already running on a participating computer. See [`SECURITY.md`](SECURITY.md) and
[`docs/THREAT_MODEL.md`](docs/THREAT_MODEL.md).

## Status: what is and is not verified

This project says plainly what has been checked. The full list is in [`docs/LEDGER.md`](docs/LEDGER.md) and [`docs/CAPABILITIES.md`](docs/CAPABILITIES.md).

| | |
|---|---|
| ✅ **Automated tests (171)** | Protocol, pairing, trust, the control state machine, clipboard rules, file transfer and layout logic, with two complete engines over loopback TLS. |
| ✅ **macOS app** | Builds, passes its self-test, launches, packages into an ad-hoc-signed `.dmg`. |
| ⚠️ **Real input capture and injection** | Not exercised by the automated tests on any operating system. Use the manual checklist in [`docs/TESTING.md`](docs/TESTING.md). |
| ⚠️ **Windows** | Cross-built from macOS and checked structurally. Real-hardware testing is only starting; expect rough edges and please report them. |
| ◻️ **Linux X11** | Written but not yet built or run by the author. |
| ❌ **Linux Wayland** | Cannot capture or inject input. Clipboard and files only, and the app says so. |

## Roadmap

- Native Apple Silicon build, and code signing / notarization for macOS and Windows
- Verification on real Windows and Linux hardware, and continuous integration on all three systems
- QR-assisted fingerprint verification
- Wayland support through portals and libei
- Folder transfers and transfer resume, fuzz targets for the protocol decoder

## Documentation

[Architecture & protocol](docs/ARCHITECTURE.md) · [Threat model](docs/THREAT_MODEL.md) · [Capability matrix](docs/CAPABILITIES.md) ·
[Permissions](docs/PERMISSIONS.md) · [Building](docs/BUILDING.md) · [Testing & manual checklist](docs/TESTING.md) ·
[Benchmarks](docs/BENCHMARKS.md) · [Design system](docs/DESIGN.md) · [Implementation ledger](docs/LEDGER.md) ·
[Changelog](CHANGELOG.md) · [Contributing](CONTRIBUTING.md)

## Contributing

Bug reports, ideas and pull requests are welcome. If two computers will not connect, open an issue and paste **Settings → Diagnostics → Copy diagnostics**
from both of them. Read [`CONTRIBUTING.md`](CONTRIBUTING.md) first; security reports go through [`SECURITY.md`](SECURITY.md).

If Synkflow is useful to you, a ⭐ on the repository helps other people find it.

## Made by

**Kleivin Gjuzi** builds open-source tools for fun and uses every one of them in daily life. Say hello or follow along:

<p>
  <a href="https://www.linkedin.com/in/kleivin-gjuzi-7a7w/"><img alt="LinkedIn: Kleivin Gjuzi" src="https://img.shields.io/badge/LinkedIn-Kleivin_Gjuzi-0A66C2?style=for-the-badge&logo=linkedin&logoColor=white"></a>
  <a href="https://www.instagram.com/kleivingjuzi/"><img alt="Instagram: @kleivingjuzi" src="https://img.shields.io/badge/Instagram-@kleivingjuzi-E4405F?style=for-the-badge&logo=instagram&logoColor=white"></a>
</p>
<p>
  <a href="https://www.instagram.com/blocksandbrew/"><img alt="Instagram: @blocksandbrew" src="https://img.shields.io/badge/Instagram-@blocksandbrew-E4405F?style=for-the-badge&logo=instagram&logoColor=white"></a>
  <a href="https://www.tiktok.com/@blocksandbrew"><img alt="TikTok: @blocksandbrew" src="https://img.shields.io/badge/TikTok-@blocksandbrew-000000?style=for-the-badge&logo=tiktok&logoColor=white"></a>
  <a href="https://blocksandbrew.com"><img alt="Website: blocksandbrew.com" src="https://img.shields.io/badge/Web-blocksandbrew.com-F5A524?style=for-the-badge&logo=safari&logoColor=white"></a>
</p>

## License

[GPL-3.0-only](LICENSE). Third-party notices are in [`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md).
Synkflow is an independent implementation. It is inspired by the publicly described behaviour of similar tools, shares no code, branding or
assets with them, and is not affiliated with any of them. The application identifier is the provisional `example.synkflow.Synkflow`
(the `.example` top-level domain is reserved and cannot be registered); replace it when the project owns a real one.
