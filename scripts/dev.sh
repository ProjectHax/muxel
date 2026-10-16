#!/usr/bin/env bash
# Run muxel against an ISOLATED config/data dir so local testing never touches
# the real workspace.
#
#   scripts/dev.sh            # build + run muxel in the sandbox
#   scripts/dev.sh --release  # extra args go to cargo…
#   scripts/dev.sh -- ARGS    # …and anything after `--` goes to muxel
#   MUXEL_DEV_DIR=/tmp/x scripts/dev.sh   # override the sandbox location
#
# muxel's own `--user-data-dir` flag puts its settings and state in the sandbox
# on every OS. Environment variables can't: the `directories` crate honours
# XDG_* only on Linux and HOME only on Linux and macOS, and a pane of a muxel
# started with the flag inherits MUXEL_USER_DATA_DIR, which beats both. The flag
# beats that variable; isolated ctl refuses a foreign MUXEL_CONTROL endpoint.
# Panes keep your real profile, so agents in them stay logged in.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
dev_dir="${MUXEL_DEV_DIR:-$repo_root/.muxel-dev}"
mkdir -p "$dev_dir"
case "$(uname -s)" in
    # A Windows path for the Windows program.
    MINGW* | MSYS* | CYGWIN*) user_data_dir="$(cygpath -m "$dev_dir")" ;;
    *) user_data_dir="$dev_dir" ;;
esac

# Split cargo's args from muxel's at the first `--`, as `cargo run` would.
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

echo "muxel dev sandbox: $dev_dir" >&2
# The flag goes first, so it also covers `scripts/dev.sh -- ctl …`.
exec cargo run -p muxel ${cargo_args[@]+"${cargo_args[@]}"} -- \
    --user-data-dir "$user_data_dir" ${app_args[@]+"${app_args[@]}"}
