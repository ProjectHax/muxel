#!/usr/bin/env bash
# Sign a built muxel.app and package it into a .dmg + .zip.
#
#   sign-macos.sh <muxel.app> <output-basename>
#       e.g. sign-macos.sh muxel.app muxel-macos-aarch64
#
# Two modes, chosen by whether MACOS_CERTIFICATE is set:
#
#   * Developer ID (proper distribution): imports the cert, codesigns with the
#     hardened runtime, then — if a notary key is also set — notarizes via Apple
#     and staples the ticket. The result opens with no Gatekeeper warning.
#
#   * No cert (default): ad-hoc signs (`codesign -s -`). The app launches (Apple
#     Silicon requires at least an ad-hoc signature), but downloaded copies still
#     hit Gatekeeper's "unidentified developer" prompt.
#
# macOS signing REQUIRES a paid Apple Developer Program membership ($99/yr) — there
# is no managed/free alternative like Windows' Azure Trusted Signing. To enable
# the Developer ID path, set these GitHub Actions secrets:
#
#   MACOS_CERTIFICATE       base64 of a "Developer ID Application" .p12
#                           (Apple Developer portal → Certificates → Developer ID
#                            Application; export from Keychain as .p12, then
#                            `base64 -i cert.p12 | pbcopy`)
#   MACOS_CERTIFICATE_PWD   the .p12 export password
#   MACOS_SIGN_IDENTITY     e.g. "Developer ID Application: Your Name (TEAMID)"
#   MACOS_NOTARY_KEY        base64 of an App Store Connect API key (.p8)
#                           (App Store Connect → Users and Access → Integrations →
#                            Keys; role "Developer"; download the .p8 once)
#   MACOS_NOTARY_KEY_ID     the key's ID
#   MACOS_NOTARY_ISSUER_ID  the issuer ID shown on that Keys page
#
set -euo pipefail

app="${1:?usage: sign-macos.sh <app> <output-basename>}"
out="${2:?usage: sign-macos.sh <app> <output-basename>}"
tmp="${RUNNER_TEMP:-/tmp}"

if [ -n "${MACOS_CERTIFICATE:-}" ]; then
  echo "==> Signing with Developer ID: ${MACOS_SIGN_IDENTITY:-?}"
  keychain="$tmp/muxel-signing.keychain-db"
  kc_pw="$(openssl rand -base64 24)"
  security create-keychain -p "$kc_pw" "$keychain"
  security set-keychain-settings -lut 21600 "$keychain"
  security unlock-keychain -p "$kc_pw" "$keychain"
  printf '%s' "$MACOS_CERTIFICATE" | openssl base64 -d -A -out "$tmp/cert.p12"
  security import "$tmp/cert.p12" -k "$keychain" -P "${MACOS_CERTIFICATE_PWD:-}" \
    -T /usr/bin/codesign
  security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$kc_pw" "$keychain" >/dev/null
  # Make the temp keychain searchable alongside the login keychain.
  security list-keychains -d user -s "$keychain" \
    "$(security default-keychain -d user | tr -d ' "')"
  codesign --force --deep --options runtime --timestamp \
    --sign "$MACOS_SIGN_IDENTITY" "$app"
  codesign --verify --strict --verbose=2 "$app"
else
  echo "==> No MACOS_CERTIFICATE — ad-hoc signing (Gatekeeper will warn on download)."
  codesign --force --deep --sign - "$app"
fi

# Notarization is available only with both a cert AND a notary key.
notarize_enabled=""
if [ -n "${MACOS_CERTIFICATE:-}" ] && [ -n "${MACOS_NOTARY_KEY:-}" ]; then
  notarize_enabled=1
  printf '%s' "$MACOS_NOTARY_KEY" | openssl base64 -d -A -out "$tmp/notary.p8"
fi

# Submit one artifact (a .zip or .dmg) to Apple's notary service and block until
# it's Accepted. Each distributed artifact must be notarized in its own right for
# `stapler staple` to find a ticket — stapling the DMG previously worked only by
# CloudKit chance, since the DMG itself was never submitted ("Record not found").
notarize() {
  xcrun notarytool submit "$1" \
    --key "$tmp/notary.p8" \
    --key-id "${MACOS_NOTARY_KEY_ID:?MACOS_NOTARY_KEY_ID required}" \
    --issuer "${MACOS_NOTARY_ISSUER_ID:?MACOS_NOTARY_ISSUER_ID required}" \
    --wait
}

# Notarize + staple the app (so the .zip artifact carries a valid ticket offline).
if [ -n "$notarize_enabled" ]; then
  echo "==> Notarizing muxel.app…"
  ditto -c -k --keepParent "$app" "$tmp/notarize.zip"
  notarize "$tmp/notarize.zip"
  xcrun stapler staple "$app"
fi

# Final artifacts from the (signed, possibly stapled) app.
#
# DMG: stage the app next to an /Applications symlink so the mounted volume
# shows the familiar "drag muxel onto Applications" layout. `ditto` (not `cp`)
# copies the bundle so its code signature and stapled notarization ticket carry
# over intact.
dmg_stage="$tmp/muxel-dmg-stage"
rm -rf "$dmg_stage"
mkdir -p "$dmg_stage"
ditto "$app" "$dmg_stage/$(basename "$app")"
ln -s /Applications "$dmg_stage/Applications"
hdiutil create -volname muxel -srcfolder "$dmg_stage" -ov -format UDZO "$out.dmg"
rm -rf "$dmg_stage"

ditto -c -k --keepParent "$app" "$out.zip"

# Sign, notarize + staple the DMG itself so it passes Gatekeeper offline (and so
# stapling actually has a ticket to find). The .zip carries the already-stapled
# app, so it needs no separate ticket.
if [ -n "$notarize_enabled" ]; then
  echo "==> Signing + notarizing the DMG…"
  codesign --force --timestamp --sign "$MACOS_SIGN_IDENTITY" "$out.dmg"
  notarize "$out.dmg"
  xcrun stapler staple "$out.dmg"
fi
echo "==> Wrote $out.dmg and $out.zip"
