#!/usr/bin/env bash
# Run muxel against an ISOLATED config/data dir so local testing never touches
# the real workspace.
#
#   scripts/dev.sh            # build + run muxel in the sandbox
#   scripts/dev.sh --release  # extra args go to cargo…
#   scripts/dev.sh -- ARGS    # …and anything after `--` goes to muxel
#   MUXEL_DEV_DIR=/tmp/x scripts/dev.sh   # override the sandbox location
#
# How the sandbox is made depends on the OS, because the `directories` crate
# finds muxel's dirs differently on each:
#
# - Linux honours XDG_CONFIG_HOME / XDG_DATA_HOME, so those are enough, and panes
#   keep your real HOME (agents in them stay logged in).
# - macOS ignores XDG and uses ~/Library/Application Support — the real
#   workspace — so only a sandbox HOME isolates it. Cargo needs the real HOME to
#   find ~/.cargo and ~/.rustup, so muxel is built first and only the binary gets
#   the sandbox HOME. Its panes inherit it: agents there start without your
#   login/config.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
dev_dir="${MUXEL_DEV_DIR:-$repo_root/.muxel-dev}"

export XDG_CONFIG_HOME="$dev_dir/config"
export XDG_DATA_HOME="$dev_dir/data"
mkdir -p "$XDG_CONFIG_HOME" "$XDG_DATA_HOME"

echo "muxel dev sandbox: $dev_dir" >&2
if [ "$(uname -s)" != "Darwin" ]; then
    exec cargo run -p muxel "$@"
fi

# macOS: split cargo's args from muxel's at the first `--`, as `cargo run` would.
# (The `${a[@]+…}` expansions keep bash 3.2 — macOS's /bin/bash — happy with an
# empty array under `set -u`.)
cargo_args=()
app_args=()
past_separator=false
for arg in "$@"; do
    if ! $past_separator && [ "$arg" = "--" ]; then
        past_separator=true
    elif $past_separator; then
        app_args+=("$arg")
    else
        cargo_args+=("$arg")
    fi
done

# Build with the real environment, reading the binary's path from cargo's own
# report (so `--release` and friends land on the right one). Compiler output
# still goes to the terminal.
bin="$(cd "$repo_root" &&
    cargo build -p muxel ${cargo_args[@]+"${cargo_args[@]}"} \
        --message-format=json-render-diagnostics |
    sed -n 's/.*"executable":"\([^"]*\)".*/\1/p' | tail -n 1)"
if [ -z "$bin" ]; then
    echo "dev.sh: cargo built no muxel binary" >&2
    exit 1
fi

export HOME="$dev_dir/home"
mkdir -p "$HOME"
exec "$bin" ${app_args[@]+"${app_args[@]}"}
