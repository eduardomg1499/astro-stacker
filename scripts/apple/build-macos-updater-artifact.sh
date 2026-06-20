#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT_DIR"

read_ps1_value() {
  node -e '
    const fs = require("fs");
    const name = process.argv[1];
    const text = fs.readFileSync("build_installer.ps1", "utf8");
    const match = text.match(new RegExp("\\$" + name + "\\s*=\\s*\"([\\s\\S]*?)\""));
    if (match) process.stdout.write(match[1]);
  ' "$1"
}

export APPLE_SIGNING_IDENTITY="${APPLE_SIGNING_IDENTITY:-Developer ID Application: Eduardo Martinez Gomez (434G7RN9PF)}"
export APPLE_TEAM_ID="${APPLE_TEAM_ID:-434G7RN9PF}"
export TAURI_SIGNING_PRIVATE_KEY="${TAURI_SIGNING_PRIVATE_KEY:-$(read_ps1_value SavedKey)}"
export TAURI_SIGNING_PRIVATE_KEY_PASSWORD="${TAURI_SIGNING_PRIVATE_KEY_PASSWORD:-$(read_ps1_value SavedPassword)}"

if [[ -z "${APPLE_SIGNING_IDENTITY:-}" ]]; then
  echo "Missing APPLE_SIGNING_IDENTITY."
  echo 'Example: export APPLE_SIGNING_IDENTITY="Developer ID Application: Your Name (TEAMID)"'
  exit 1
fi

if [[ -z "${TAURI_SIGNING_PRIVATE_KEY:-}" || -z "${TAURI_SIGNING_PRIVATE_KEY_PASSWORD:-}" ]]; then
  echo "Missing Tauri updater signing key."
  echo "Keep SavedKey and SavedPassword in build_installer.ps1 or set TAURI_SIGNING_PRIVATE_KEY and TAURI_SIGNING_PRIVATE_KEY_PASSWORD."
  exit 1
fi

MACOS_BUILD_TARGET="${MACOS_BUILD_TARGET:-aarch64-apple-darwin}"
if [[ -z "${ARCH_LABEL:-}" ]]; then
  case "$MACOS_BUILD_TARGET" in
    aarch64-apple-darwin) ARCH_LABEL="aarch64" ;;
    x86_64-apple-darwin) ARCH_LABEL="x86_64" ;;
    *) ARCH_LABEL="${MACOS_BUILD_TARGET%%-*}" ;;
  esac
fi

PRODUCT_NAME="$(node -e "const fs=require('fs'); const c=JSON.parse(fs.readFileSync('src-tauri/tauri.conf.json','utf8')); console.log(c.productName)")"
VERSION="$(node -e "const fs=require('fs'); const c=JSON.parse(fs.readFileSync('src-tauri/tauri.conf.json','utf8')); console.log(c.version)")"
SAFE_PRODUCT_NAME="${PRODUCT_NAME// /.}"
BUNDLE_DIR="src-tauri/target/$MACOS_BUILD_TARGET/release/bundle/macos"
GENERIC_UPDATER="$BUNDLE_DIR/$PRODUCT_NAME.app.tar.gz"
RELEASE_UPDATER="$BUNDLE_DIR/${SAFE_PRODUCT_NAME}_${VERSION}_${ARCH_LABEL}.app.tar.gz"

echo "== Building signed macOS updater artifact =="
echo "Target: $MACOS_BUILD_TARGET"
env \
  -u APPLE_ID \
  -u APPLE_PASSWORD \
  -u APPLE_API_ISSUER \
  -u APPLE_API_KEY \
  -u APPLE_API_KEY_PATH \
  -u APPLE_NOTARY_PROFILE \
  npm run tauri -- build --bundles app --target "$MACOS_BUILD_TARGET"

if [[ ! -f "$GENERIC_UPDATER" || ! -f "$GENERIC_UPDATER.sig" ]]; then
  echo "Missing generated updater artifacts for $MACOS_BUILD_TARGET."
  exit 1
fi

cp "$GENERIC_UPDATER" "$RELEASE_UPDATER"
cp "$GENERIC_UPDATER.sig" "$RELEASE_UPDATER.sig"

echo "macOS updater artifacts:"
find "$BUNDLE_DIR" \
  \( -name '*.app.tar.gz' -o -name '*.app.tar.gz.sig' \) \
  -print

npm run release:latest-json
