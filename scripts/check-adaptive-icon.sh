#!/usr/bin/env bash
# Checks that a built zwinger.app has the adaptive icon (macOS 26) and the static ICNS fallback.
# tauri-bundler only logs an error when actool is older than 26, so a green build can lack Assets.car.
# Usage: scripts/check-adaptive-icon.sh path/to/zwinger.app
set -euo pipefail
app=${1:?usage: $0 path/to/zwinger.app}
plist="$app/Contents/Info.plist"
res="$app/Contents/Resources"

fail() { echo "check-adaptive-icon: $*" >&2; exit 1; }
key() { /usr/libexec/PlistBuddy -c "Print :$1" "$plist" 2>/dev/null || true; }

[ -f "$res/Assets.car" ] || fail "no Assets.car in $res (is DEVELOPER_DIR set to Xcode 26?)"
assetutil --info "$res/Assets.car" >/dev/null || fail "assetutil cannot read $res/Assets.car"

name=$(key CFBundleIconName)
[ -n "$name" ] || fail "no CFBundleIconName in $plist"
assetutil --info "$res/Assets.car" | grep -q "\"Name\" : \"$name\"" || fail "Assets.car has no icon named $name"

file=$(key CFBundleIconFile)
[ -n "$file" ] || fail "no CFBundleIconFile in $plist"
[ -f "$res/$file" ] || [ -f "$res/$file.icns" ] || fail "no ICNS fallback $file in $res"

[ "$(key LSMinimumSystemVersion)" = "14.0" ] || fail "LSMinimumSystemVersion is not 14.0"

echo "check-adaptive-icon: ok ($name, $file)"
