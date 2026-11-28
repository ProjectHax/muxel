#!/usr/bin/env bash
# Fail if any whisper.cpp build under target/ was configured with ggml's
# GGML_NATIVE (`-march=native`). A native build bakes in the build machine's CPU,
# so a release built on an AVX-512 CI runner dies with SIGILL the first time a
# user without AVX-512 dictates. .cargo/config.toml turns native off, but
# whisper-rs-sys's build script doesn't rerun when that changes, so a restored
# rust-cache can still carry native ggml objects. Run this after the release
# build, before anything is packaged.
#
#   scripts/check-portable-whisper.sh [TARGET_DIR]   (default: target)
set -euo pipefail

dir="${1:-target}"
found=0
while IFS= read -r cache; do
  found=1
  # tr: CMake on Windows may write the cache with CRLF line endings.
  if tr -d '\r' <"$cache" | grep -q '^GGML_NATIVE:BOOL=OFF$'; then
    echo "ok: $cache"
  else
    echo "::error::whisper.cpp was built with GGML_NATIVE ($cache): see .cargo/config.toml, then bump the rust-cache key in release.yml"
    exit 1
  fi
done < <(find "$dir" -path '*/build/whisper-rs-sys-*/out/build/CMakeCache.txt')

if [ "$found" = 0 ]; then
  echo "::error::no whisper.cpp build found under $dir/"
  exit 1
fi
