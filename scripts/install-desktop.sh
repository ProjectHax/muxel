#!/usr/bin/env bash
# Install muxel's icon + .desktop entry for the current user, so it shows up in
# the app launcher and its desktop notifications carry the muxel name + icon.
#
#   scripts/install-desktop.sh            # build release + install
#   scripts/install-desktop.sh --no-build # install using the existing binary
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
bin="$repo_root/target/release/muxel"

if [[ "${1:-}" != "--no-build" ]]; then
    echo "building release binary…" >&2
    (cd "$repo_root" && cargo build --release -p muxel)
fi
if [[ ! -x "$bin" ]]; then
    echo "error: $bin not found (build first, or drop --no-build)" >&2
    exit 1
fi

data_home="${XDG_DATA_HOME:-$HOME/.local/share}"
icon_dir="$data_home/icons/hicolor/scalable/apps"
apps_dir="$data_home/applications"
mkdir -p "$icon_dir" "$apps_dir"

install -m644 "$repo_root/crates/muxel/assets/muxel.svg" "$icon_dir/muxel.svg"

# Point Exec at the built binary's absolute path so it runs without PATH setup.
sed "s|^Exec=muxel\$|Exec=$bin|" "$repo_root/packaging/muxel.desktop" \
    > "$apps_dir/muxel.desktop"
chmod 644 "$apps_dir/muxel.desktop"

# Refresh the caches (best-effort; harmless if the tools are absent).
command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$apps_dir" || true
command -v gtk-update-icon-cache >/dev/null 2>&1 \
    && gtk-update-icon-cache -f -t "$data_home/icons/hicolor" >/dev/null 2>&1 || true

echo "installed:" >&2
echo "  $icon_dir/muxel.svg" >&2
echo "  $apps_dir/muxel.desktop (Exec=$bin)" >&2
echo "muxel should now appear in your launcher; notifications will show its icon." >&2
