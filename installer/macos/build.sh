#!/bin/bash
# Builds bebo for Apple Silicon and Intel Macs and makes dist/bebo-<version>-macos.dmg with bebo.app.
# Run it on a Mac with Rust and the Xcode command line tools (xcode-select --install).
set -euo pipefail
cd "$(dirname "$0")/../.."

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
rustup target add aarch64-apple-darwin x86_64-apple-darwin
cargo build --release --target aarch64-apple-darwin
cargo build --release --target x86_64-apple-darwin

out=target/macos
app=$out/bebo.app
rm -rf "$out"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
# One program file for both processor types.
lipo -create -output "$app/Contents/MacOS/bebo" \
  target/aarch64-apple-darwin/release/bebo target/x86_64-apple-darwin/release/bebo
sed "s/VERSION/$version/g" installer/macos/Info.plist > "$app/Contents/Info.plist"

# The app icon in all sizes that macOS uses.
# ponytail: the largest image is 256 px, so 512 and 1024 px are scaled up. Add a 1024 px image for a sharp icon in the Dock.
iconset=$out/bebo.iconset
mkdir -p "$iconset"
for size in 16 32 128 256 512; do
  sips -z $size $size assets/app/bebo-dark-256.png --out "$iconset/icon_${size}x${size}.png" > /dev/null
  sips -z $((size * 2)) $((size * 2)) assets/app/bebo-dark-256.png --out "$iconset/icon_${size}x${size}@2x.png" > /dev/null
done
iconutil -c icns "$iconset" -o "$app/Contents/Resources/bebo.icns"

# An ad-hoc signature: a Mac with Apple Silicon runs only signed code.
# It is not a Developer ID signature, so macOS asks the user before the first start.
codesign --force --deep --sign - "$app"

# The disk image has the app and a link to the Applications folder, so the user can drag the app there.
staging=$out/dmg
mkdir -p "$staging"
cp -R "$app" "$staging/"
ln -s /Applications "$staging/Applications"
mkdir -p dist
dmg=dist/bebo-$version-macos.dmg
rm -f "$dmg"
hdiutil create -volname bebo -srcfolder "$staging" -ov -format UDZO "$dmg"

echo "App: $app"
echo "Disk image: $dmg"
