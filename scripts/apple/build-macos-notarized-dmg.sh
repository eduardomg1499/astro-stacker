#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT_DIR"

export APPLE_SIGNING_IDENTITY="${APPLE_SIGNING_IDENTITY:-Developer ID Application: Eduardo Martinez Gomez (434G7RN9PF)}"
export APPLE_TEAM_ID="${APPLE_TEAM_ID:-434G7RN9PF}"
export APPLE_NOTARY_PROFILE="${APPLE_NOTARY_PROFILE:-ZenithAstroNotary}"

if [[ -z "${APPLE_SIGNING_IDENTITY:-}" ]]; then
  echo "Missing APPLE_SIGNING_IDENTITY."
  exit 1
fi

NOTARY_ARGS=()
if [[ -n "${APPLE_NOTARY_PROFILE:-}" ]]; then
  NOTARY_ARGS=(--keychain-profile "$APPLE_NOTARY_PROFILE")
elif [[ -n "${APPLE_API_ISSUER:-}" && -n "${APPLE_API_KEY:-}" && -n "${APPLE_API_KEY_PATH:-}" ]]; then
  NOTARY_ARGS=(--key "$APPLE_API_KEY_PATH" --key-id "$APPLE_API_KEY" --issuer "$APPLE_API_ISSUER")
elif [[ -n "${APPLE_ID:-}" && -n "${APPLE_PASSWORD:-}" && -n "${APPLE_TEAM_ID:-}" ]]; then
  NOTARY_ARGS=(--apple-id "$APPLE_ID" --password "$APPLE_PASSWORD" --team-id "$APPLE_TEAM_ID")
fi

if [[ "${#NOTARY_ARGS[@]}" -eq 0 ]]; then
  echo "Missing Apple notarization credentials."
  echo 'Use APPLE_NOTARY_PROFILE, or APPLE_API_ISSUER + APPLE_API_KEY + APPLE_API_KEY_PATH, or APPLE_ID + APPLE_PASSWORD + APPLE_TEAM_ID.'
  exit 1
fi

MACOS_TARGET_ARGS=()
if [[ -n "${MACOS_BUILD_TARGET:-}" ]]; then
  MACOS_TARGET_ARGS=(--target "$MACOS_BUILD_TARGET")
fi

echo "== Building Tauri DMG =="
env \
  -u APPLE_ID \
  -u APPLE_PASSWORD \
  -u APPLE_API_ISSUER \
  -u APPLE_API_KEY \
  -u APPLE_API_KEY_PATH \
  -u APPLE_NOTARY_PROFILE \
  npm run tauri -- build --bundles dmg "${MACOS_TARGET_ARGS[@]}"

DMG_PATH="$(find src-tauri/target -name '*.dmg' -type f -print | sort | tail -n 1)"
if [[ -z "$DMG_PATH" ]]; then
  echo "No DMG was generated."
  exit 1
fi

codesign --force --timestamp --sign "$APPLE_SIGNING_IDENTITY" "$DMG_PATH"
xcrun notarytool submit "$DMG_PATH" --wait "${NOTARY_ARGS[@]}"
xcrun stapler staple "$DMG_PATH"
xcrun stapler validate "$DMG_PATH"

echo "$DMG_PATH"
