#!/usr/bin/env bash
# Authenticode-sign muxel's Windows builds with Azure Trusted Signing, locally.
#
# Releases are built unsigned in CI (.github/workflows/release.yml). Run this
# afterwards from a machine that's logged into Azure (`az login`) to sign
# muxel.exe inside the Windows release zips, so users stop seeing the
# SmartScreen "unknown publisher" prompt.
#
#   scripts/sign-windows.sh muxel-windows-x86_64.zip muxel-windows-arm64.zip
#       Sign muxel.exe inside each zip, in place (re-zips with the signed exe).
#       Also accepts a bare .exe, which it signs directly.
#
#   scripts/sign-windows.sh --release v0.1.0
#       Download the Windows zips AND installer .exe's from GitHub Release
#       v0.1.0, sign them, and re-upload (replacing the unsigned assets). Needs
#       the `gh` CLI.
#
#   scripts/sign-windows.sh --release v0.1.0 --no-upload
#       Same, but stop after signing — leaves the signed zips in the cwd.
#
# Mechanism: jsign is a pure-Java Authenticode signer, so it signs Windows PEs
# from Linux with no Windows SDK. It talks to Azure Trusted Signing via
# `--storetype TRUSTEDSIGNING`, using a short-lived OAuth token pulled from the
# local `az` session — no stored secret.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# --- jsign (pure-Java Authenticode signer; fetched on first use) -------------
JSIGN_VERSION="${JSIGN_VERSION:-7.4}"
JSIGN_JAR="${JSIGN_JAR:-$repo_root/target/tools/jsign-$JSIGN_VERSION.jar}"

# --- Azure Trusted Signing (eastus -> eus endpoint) --------------------------
# Defaults for the maintainer's signing account; override any of these via the
# environment (TS_ENDPOINT, TS_ACCOUNT, TS_PROFILE, TS_TSA) if they change.
TS_ENDPOINT="${TS_ENDPOINT:-https://eus.codesigning.azure.net}"
TS_ACCOUNT="${TS_ACCOUNT:-projecthax}"
TS_PROFILE="${TS_PROFILE:-ProjectHax}"
TS_TSA="${TS_TSA:-http://timestamp.acs.microsoft.com}"

# OAuth token for the Trusted Signing resource. Fetched once, lazily, only when
# we're actually about to sign. Override TS_TOKEN to inject a CI credential.
TS_TOKEN="${TS_TOKEN:-}"

log() { printf '%s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

usage() {
    # Print the header comment (everything from line 2 up to the first non-# line).
    awk 'NR>1 && /^#/ {sub(/^# ?/, ""); print; next} NR>1 {exit}' "${BASH_SOURCE[0]}"
    exit "${1:-0}"
}

# Fetch the pinned jsign jar from Maven Central if we don't have it yet.
ensure_jsign() {
    command -v java >/dev/null 2>&1 \
        || die "java not found — jsign needs a JRE (e.g. \`sdk install java\`)."
    [[ -f "$JSIGN_JAR" ]] && return
    log "fetching jsign $JSIGN_VERSION -> $JSIGN_JAR"
    mkdir -p "$(dirname "$JSIGN_JAR")"
    curl -fsSL -o "$JSIGN_JAR" \
        "https://repo1.maven.org/maven2/net/jsign/jsign/$JSIGN_VERSION/jsign-$JSIGN_VERSION.jar" \
        || die "failed to download jsign jar"
}

# Pull an access token from the logged-in az session (once per run).
ensure_token() {
    [[ -n "$TS_TOKEN" ]] && return
    command -v az >/dev/null 2>&1 \
        || die "az (Azure CLI) not found, and TS_TOKEN is unset."
    log "requesting a Trusted Signing token from your az session…"
    TS_TOKEN="$(az account get-access-token \
        --resource https://codesigning.azure.net \
        --query accessToken -o tsv 2>/dev/null)" \
        || true
    [[ -n "$TS_TOKEN" ]] \
        || die "couldn't get a token — run \`az login\` (and ensure your account can sign with the '$TS_ACCOUNT' Trusted Signing account)."
}

# Sign a single PE file in place. The token is in argv while jsign runs; that's
# acceptable on a single-user box.
sign_pe() {
    local file="$1"
    ensure_jsign
    ensure_token
    log "signing $(basename "$file")…"
    java -jar "$JSIGN_JAR" --storetype TRUSTEDSIGNING \
        --keystore "$TS_ENDPOINT" \
        --storepass "$TS_TOKEN" \
        --alias "$TS_ACCOUNT/$TS_PROFILE" \
        --tsaurl "$TS_TSA" --tsmode RFC3161 --alg SHA-256 \
        "$file"
}

# Extract muxel.exe from a release zip, sign it, and repackage the zip with the
# signed exe (preserving README.md / LICENSE alongside it).
sign_zip() {
    local zip="$1"
    [[ -f "$zip" ]] || die "no such file: $zip"
    command -v unzip >/dev/null 2>&1 || die "unzip not found (dnf install unzip)"
    command -v zip   >/dev/null 2>&1 || die "zip not found (dnf install zip)"
    local abs work
    abs="$(cd "$(dirname "$zip")" && pwd)/$(basename "$zip")"
    work="$(mktemp -d)"
    # shellcheck disable=SC2064
    trap "rm -rf '$work'" RETURN

    unzip -q "$abs" -d "$work"
    [[ -f "$work/muxel.exe" ]] \
        || die "$zip doesn't contain muxel.exe at its root — is it a muxel Windows build?"

    sign_pe "$work/muxel.exe"

    # Rebuild from scratch so no stale entries linger; -X drops extra attrs.
    rm -f "$abs"
    ( cd "$work" && zip -q -X -r "$abs" . )
    log "re-zipped $zip with the signed exe"
}

# Dispatch one argument: .zip -> sign-in-zip, anything else -> sign as a PE.
sign_arg() {
    case "$1" in
        *.zip) sign_zip "$1" ;;
        *)     [[ -f "$1" ]] || die "no such file: $1"; sign_pe "$1" ;;
    esac
}

# --- release round-trip: download from a GH release, sign, re-upload ----------
release_mode() {
    local tag="$1" upload="$2"
    command -v gh >/dev/null 2>&1 \
        || die "gh (GitHub CLI) not found. Install it, or download the zips yourself and run: scripts/sign-windows.sh <zip>..."

    local work
    work="$(mktemp -d)"
    log "downloading Windows assets from release $tag…"
    # Both the portable zips and the installer .exe's.
    gh release download "$tag" --dir "$work" \
        --pattern 'muxel-windows-*.zip' --pattern 'muxel-windows-*-setup.exe' \
        || die "no muxel-windows-* assets on release $tag"

    local f signed=()
    for f in "$work"/muxel-windows-*.zip "$work"/muxel-windows-*-setup.exe; do
        [[ -e "$f" ]] || continue # a glob that matches nothing stays literal
        sign_arg "$f"             # .zip → sign-in-zip, .exe → sign the PE directly
        signed+=("$f")
    done
    [[ ${#signed[@]} -gt 0 ]] || die "found no Windows assets to sign on $tag"

    if [[ "$upload" == "yes" ]]; then
        log "re-uploading signed assets to release $tag (replacing the unsigned ones)…"
        gh release upload "$tag" "${signed[@]}" --clobber
        log "done — release $tag now carries signed Windows builds."
    else
        local dest
        for z in "${signed[@]}"; do
            dest="$(pwd)/$(basename "$z")"
            mv -f "$z" "$dest"
            log "signed: $dest"
        done
        log "(--no-upload) upload them yourself with: gh release upload $tag <zip>... --clobber"
    fi
    rm -rf "$work"
}

main() {
    [[ $# -eq 0 ]] && usage 1
    case "$1" in
        -h|--help) usage 0 ;;
        --release|--tag)
            local tag="${2:-}" upload="yes"
            [[ -n "$tag" ]] || die "--release needs a tag, e.g. --release v0.1.0"
            shift 2
            [[ "${1:-}" == "--no-upload" ]] && upload="no"
            release_mode "$tag" "$upload"
            ;;
        -*) die "unknown option: $1 (try --help)" ;;
        *)
            for arg in "$@"; do sign_arg "$arg"; done
            log "done. Verify on Windows with: signtool verify /pa muxel.exe"
            ;;
    esac
}

main "$@"
