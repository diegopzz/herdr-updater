#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
"$ROOT/bin/herdr-updater" version >/dev/null

if [ -x "$ROOT/target/release/herdr-updater" ]; then
    source_binary="$ROOT/target/release/herdr-updater"
else
    source_binary="$ROOT/bin/.cache/0.4.0/herdr-updater"
fi
destination_dir="${XDG_BIN_HOME:-$HOME/.local/bin}"
mkdir -p "$destination_dir"
install -m 0755 "$source_binary" "$destination_dir/herdr-updater.tmp"
mv -f "$destination_dir/herdr-updater.tmp" "$destination_dir/herdr-updater"
printf 'installed %s\n' "$destination_dir/herdr-updater"
