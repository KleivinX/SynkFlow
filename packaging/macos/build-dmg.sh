#!/bin/bash
# Assemble Synkflow.app from target/release and wrap it in a compressed disk image.
# The app is ad-hoc signed (NOT notarized); see docs/BUILDING.md for Developer-ID signing.
set -euo pipefail
cd "$(dirname "$0")/../.."
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
BIN=${BIN:-target/release/synkflow}
OUT=${OUT:-dist}
[ -x "$BIN" ] || { echo "build first: cargo build --release" >&2; exit 1; }

rm -rf "$OUT/Synkflow.app" "$OUT/dmg" "$OUT/Synkflow-$VERSION.dmg"
mkdir -p "$OUT/Synkflow.app/Contents/MacOS" "$OUT/Synkflow.app/Contents/Resources" "$OUT/dmg"
cp "$BIN" "$OUT/Synkflow.app/Contents/MacOS/synkflow"
cp assets/Synkflow.icns "$OUT/Synkflow.app/Contents/Resources/Synkflow.icns"
sed "s/@VERSION@/$VERSION/g" packaging/macos/Info.plist.template > "$OUT/Synkflow.app/Contents/Info.plist"
cp LICENSE "$OUT/Synkflow.app/Contents/Resources/LICENSE.txt"
cp THIRD_PARTY_NOTICES.md THIRD_PARTY_LICENSES.txt "$OUT/Synkflow.app/Contents/Resources/"
plutil -lint "$OUT/Synkflow.app/Contents/Info.plist" >/dev/null
codesign --force --deep -s - "$OUT/Synkflow.app"
codesign --verify --deep --strict "$OUT/Synkflow.app"

cp -R "$OUT/Synkflow.app" "$OUT/dmg/"
ln -s /Applications "$OUT/dmg/Applications"
cp docs/TUTORIAL.txt "$OUT/dmg/Synkflow Tutorial.txt"
cp LICENSE "$OUT/dmg/License (GPL-3.0).txt"
hdiutil create -volname "Synkflow" -srcfolder "$OUT/dmg" -fs HFS+ -format UDZO -imagekey zlib-level=9 -ov "$OUT/Synkflow-$VERSION.dmg" >/dev/null
hdiutil verify "$OUT/Synkflow-$VERSION.dmg" >/dev/null
echo "$OUT/Synkflow-$VERSION.dmg"
