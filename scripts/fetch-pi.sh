#!/usr/bin/env bash
# Puts the pinned pi bundle in src-tauri/resources/pi. The app mounts it read-only
# into each shield, so a shield needs no registry access to run pi.
set -euo pipefail
VERSION=1.3.0
root=$(cd "$(dirname "$0")/.." && pwd)
dest="$root/src-tauri/resources/pi"
[ -f "$dest/VERSION" ] && [ "$(cat "$dest/VERSION")" = "$VERSION" ] && exit 0
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
npm pack --silent --pack-destination "$tmp" "@earendil-works/pi-coding-agent@$VERSION" >/dev/null
tar -xzf "$tmp"/*.tgz -C "$tmp"
rm -rf "$dest"
mkdir -p "$dest"
cp -R "$tmp/package/dist/bundle/." "$dest/"
echo '{"type":"module"}' > "$dest/package.json"
echo "$VERSION" > "$dest/VERSION"
