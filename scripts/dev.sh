#!/usr/bin/env bash
# Run muxel against an ISOLATED config/data dir so local testing never touches
# the real ~/.config/muxel or ~/.local/share/muxel. The `directories` crate
# honours XDG_CONFIG_HOME / XDG_DATA_HOME on Linux.
#
#   scripts/dev.sh            # cargo run -p muxel
#   scripts/dev.sh --release  # passes extra args through to cargo run
#   MUXEL_DEV_DIR=/tmp/x scripts/dev.sh   # override the sandbox location
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
dev_dir="${MUXEL_DEV_DIR:-$repo_root/.muxel-dev}"

export XDG_CONFIG_HOME="$dev_dir/config"
export XDG_DATA_HOME="$dev_dir/data"
mkdir -p "$XDG_CONFIG_HOME" "$XDG_DATA_HOME"

echo "muxel dev sandbox: $dev_dir" >&2
exec cargo run -p muxel "$@"
