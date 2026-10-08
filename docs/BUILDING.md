# Building, packaging, offline builds

Requirements: current stable Rust (the tested toolchain is **1.96.1**, edition 2024) and a C compiler (`ring` compiles a little C).
No Node, no browser, no web stack. Slint's GPL-3.0 option is used (see `THIRD_PARTY_NOTICES.md`).

```bash
cargo build --release                       # target/release/synkflow
cargo build --release --no-default-features # engine only (headless; `--selftest` works)
cargo test --no-default-features            # all suites (no Slint needed, much faster to build)
```

Dependency versions are pinned by the committed `Cargo.lock`; build with `--locked` for reproducibility.

## Offline / vendored builds

Runtime needs no internet at all. Only *building* downloads crates. To build with no network:

```bash
cargo vendor --locked vendor > .cargo-vendor-config.toml   # once, while online
mkdir -p .cargo && cat .cargo-vendor-config.toml >> .cargo/config.toml
cargo build --release --locked --offline
```

Archive `vendor/` with the source for an air-gapped build. (`vendor/` is large; it is not committed.)

## macOS

```bash
cargo build --release
packaging/macos/build-dmg.sh            # -> dist/Synkflow-<version>.dmg
```

The script assembles `Synkflow.app` (binary, `Info.plist` with bundle id `example.synkflow.Synkflow`, `Synkflow.icns`), applies an
**ad-hoc signature** (`codesign -s -`), and creates a compressed read-only disk image with an `Applications` shortcut using
`hdiutil`. An ad-hoc-signed app is **not notarized**: Gatekeeper will warn on first launch (right-click → Open). For
distribution:

```bash
codesign --force --deep --options runtime --timestamp -s "Developer ID Application: YOUR NAME (TEAMID)" Synkflow.app
xcrun notarytool submit Synkflow-<version>.dmg --keychain-profile "AC_PROFILE" --wait
xcrun stapler staple Synkflow-<version>.dmg
```

Do not claim a build is signed unless these steps were run with your certificate.

## Windows

The installer is a small Rust program (`packaging/windows/installer`) that embeds `synkflow.exe`, installs **per user** (no
Administrator rights) to `%LOCALAPPDATA%\Programs\Synkflow`, creates a Start-menu shortcut and an Apps & features entry, and
installs an uninstaller (`uninstall.exe`, also reachable from Settings → Apps). `SynkflowSetup.exe /S` installs silently. It never touches
the firewall and never enables start-at-login. Both `.exe` files are GUI-subsystem programs (no console window); `synkflow.exe --version`,
`--help` and `--selftest` re-attach the parent terminal when run from cmd/PowerShell (not verified on a real Windows machine).

Cross-compiling from macOS/Linux (what the author's environment did) uses `zig cc` as the C compiler/linker:

```bash
rustup target add x86_64-pc-windows-gnu
packaging/windows/build.sh              # -> dist/Synkflow-Setup-<version>.exe  (needs `pip install cargo-zigbuild ziglang`, see the script header)
```

Native Windows: `cargo build --release` with the MSVC toolchain, then build the installer crate. The cross-built `.exe` files were
structurally checked (PE header, GUI subsystem, imports only system DLLs, payload embedded byte-for-byte) but **never executed**. The installer is **unsigned**;
SmartScreen will warn. Sign with `signtool sign /fd SHA256 /tr <timestamp-url> /td SHA256 /a SynkflowSetup.exe`.

## Linux

```bash
cargo build --release
install -Dm755 target/release/synkflow ~/.local/bin/synkflow
install -Dm644 packaging/linux/synkflow.desktop ~/.local/share/applications/synkflow.desktop
for s in 16 32 48 64 128 256 512; do install -Dm644 assets/linux/icons/hicolor/${s}x${s}/apps/synkflow.png ~/.local/share/icons/hicolor/${s}x${s}/apps/synkflow.png; done
```

The Linux build needs the usual X11/GL development libraries for Slint's winit backend. Not built or run by the author.

## Icons

`assets/` holds generated icons (`Synkflow.icns` made by `iconutil`, `synkflow.ico` multi-resolution, hicolor PNGs, tray glyphs).
They are derived from the supplied logo by `tools/make_assets.py` (a developer tool, not part of the app). Each is a real
container; none is a renamed PNG.
