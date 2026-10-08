#!/bin/bash
# Cross-build synkflow.exe for Windows and wrap it in the per-user installer.
# Needs: `rustup target add x86_64-pc-windows-gnu` and `pip install cargo-zigbuild ziglang`
# (zig supplies the C compiler/linker for ring and the MinGW runtime). On Windows itself, build
# with the MSVC toolchain instead and run `cargo build --release` in both crates.
set -euo pipefail
cd "$(dirname "$0")/../.."
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
TARGET=x86_64-pc-windows-gnu
export CARGO_INCREMENTAL=0

cargo zigbuild --release --locked --target "$TARGET"

rm -rf dist/payload && mkdir -p dist/payload
cp "target/$TARGET/release/synkflow.exe" dist/payload/synkflow.exe
cp assets/synkflow.ico dist/payload/synkflow.ico
cp LICENSE dist/payload/LICENSE.txt
cp THIRD_PARTY_NOTICES.md THIRD_PARTY_LICENSES.txt dist/payload/
cp docs/TUTORIAL.txt dist/payload/TUTORIAL.txt

SYNKFLOW_PAYLOAD_DIR="$PWD/dist/payload" CARGO_TARGET_DIR=target/installer \
  cargo zigbuild --release --target "$TARGET" --manifest-path packaging/windows/installer/Cargo.toml

mkdir -p dist
cp "target/installer/$TARGET/release/synkflow-setup.exe" "dist/Synkflow-Setup-$VERSION.exe"
echo "dist/Synkflow-Setup-$VERSION.exe"
