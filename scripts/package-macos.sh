#!/usr/bin/env bash
set -euo pipefail

target=${1:?Pass a Rust macOS target triple}
package=${2:?Pass a package name for the output ZIP}
cd "$(dirname "$0")/.."

binary="target/$target/release/arcane-portal"
stage="dist/$package"
app="$stage/Arcane Portal.app"
contents="$app/Contents"
iconset="$stage/ArcanePortal.iconset"
archive="dist/$package.zip"
version=$(sed -n 's/^version = "\([0-9.]*\)"$/\1/p' Cargo.toml)

test -x "$binary"
test -n "$version"
mkdir -p "$contents/MacOS" "$contents/Resources" "$iconset"
install -m 755 "$binary" "$contents/MacOS/arcane-portal"
sed "s/@VERSION@/$version/g" assets/macos/Info.plist > "$contents/Info.plist"
plutil -lint "$contents/Info.plist"

for size in 16 32 128 256 512; do
  doubled=$((size * 2))
  sips -z "$size" "$size" assets/arcane-portal.png --out "$iconset/icon_${size}x${size}.png" >/dev/null
  sips -z "$doubled" "$doubled" assets/arcane-portal.png --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil --convert icns --output "$contents/Resources/ArcanePortal.icns" "$iconset"
codesign --force --sign - "$app"
codesign --verify --strict "$app"
ditto -c -k --norsrc --noextattr --keepParent "$app" "$archive"
unzip -tqq "$archive"
mkdir -p "$stage/verify"
ditto -x -k "$archive" "$stage/verify"
codesign --verify --strict "$stage/verify/Arcane Portal.app"
printf '%s\n' "$archive"
