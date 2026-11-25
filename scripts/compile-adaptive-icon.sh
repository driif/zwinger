#!/usr/bin/env bash
# Compiles src-tauri/icons/zwinger.icon to Assets.car before the Rust build and writes a tauri config that adds it to
# bundle.icon. tauri-bundler then uses this Assets.car and does not run actool itself: on macos-26 its own actool run at
# the end of a full `tauri build` fails (NSPlaceholderArray nil object), but the same call before the build works (#98).
# Needs actool 26 on a macOS 26 host. Usage: compile-adaptive-icon.sh, then `tauri build --config <printed path>`.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
out="$root/src-tauri/target/adaptive-icon"
rm -rf "$out"
mkdir -p "$out/work/out"
cp -R "$root/src-tauri/icons/zwinger.icon" "$out/work/Icon.icon"

# The arguments of tauri-bundler (crates/tauri-bundler/src/bundle/macos/icon.rs).
actool "$out/work/Icon.icon" --compile "$out/work/out" --output-format human-readable-text --notices --warnings \
  --output-partial-info-plist "$out/work/out/assetcatalog_generated_info.plist" --app-icon Icon \
  --include-all-app-icons --accent-color AccentColor --enable-on-demand-resources NO \
  --development-region en --target-device mac --minimum-deployment-target 26.0 --platform macosx >&2

[ -f "$out/work/out/Assets.car" ] || { echo "compile-adaptive-icon: actool made no Assets.car" >&2; exit 1; }
mv "$out/work/out/Assets.car" "$out/Assets.car"
rm -rf "$out/work"

jq --arg car "$out/Assets.car" '{bundle: {icon: (.bundle.icon + [$car])}}' "$root/src-tauri/tauri.conf.json" \
  > "$out/tauri.conf.json"
echo "$out/tauri.conf.json"
