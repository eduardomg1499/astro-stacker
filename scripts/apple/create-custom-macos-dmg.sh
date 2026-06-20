#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT_DIR"

export APPLE_SIGNING_IDENTITY="${APPLE_SIGNING_IDENTITY:-Developer ID Application: Eduardo Martinez Gomez (434G7RN9PF)}"
export APPLE_TEAM_ID="${APPLE_TEAM_ID:-434G7RN9PF}"
export APPLE_NOTARY_PROFILE="${APPLE_NOTARY_PROFILE:-ZenithAstroNotary}"

MACOS_BUILD_TARGET="${MACOS_BUILD_TARGET:-aarch64-apple-darwin}"
ARCH_LABEL="${ARCH_LABEL:-aarch64}"
TARGET_DIR="src-tauri/target/$MACOS_BUILD_TARGET/release"

PRODUCT_NAME="$(node -e "const fs=require('fs'); const c=JSON.parse(fs.readFileSync('src-tauri/tauri.conf.json','utf8')); console.log(c.productName)")"
VERSION="${VERSION:-$(node -e "const fs=require('fs'); const c=JSON.parse(fs.readFileSync('src-tauri/tauri.conf.json','utf8')); console.log(c.version)")}"
APP_PATH="${APP_PATH:-$TARGET_DIR/bundle/macos/$PRODUCT_NAME.app}"
VOLUME_NAME="${VOLUME_NAME:-Zenith Astro Stacker}"
OUTPUT_DIR="$TARGET_DIR/bundle/dmg"
FINAL_DMG="$OUTPUT_DIR/${PRODUCT_NAME}_${VERSION}_${ARCH_LABEL}_styled.dmg"
STAGING_DIR="$TARGET_DIR/dmg-styled-stage"
BACKGROUND_DIR="$STAGING_DIR/.background"
BACKGROUND_PNG="$BACKGROUND_DIR/background.png"
RW_DMG="$TARGET_DIR/${PRODUCT_NAME}_styled_rw.dmg"

if [[ ! -d "$APP_PATH" ]]; then
  echo "Missing app bundle: $APP_PATH"
  echo "Run scripts/apple/build-macos-updater-artifact.sh first."
  exit 1
fi

rm -rf "$STAGING_DIR" "$RW_DMG" "$FINAL_DMG"
mkdir -p "$BACKGROUND_DIR" "$OUTPUT_DIR"

cp -R "$APP_PATH" "$STAGING_DIR/"
ln -s /Applications "$STAGING_DIR/Applications"
swift scripts/apple/create-dmg-background.swift "$BACKGROUND_PNG" "src-tauri/icons/icon.png"
cp "src-tauri/icons/icon.icns" "$STAGING_DIR/.VolumeIcon.icns"

hdiutil create \
  -volname "$VOLUME_NAME" \
  -srcfolder "$STAGING_DIR" \
  -ov \
  -format UDRW \
  -fs HFS+ \
  "$RW_DMG"

MOUNT_OUTPUT="$(hdiutil attach "$RW_DMG" -readwrite -noverify -noautoopen)"
echo "$MOUNT_OUTPUT"
MOUNT_POINT="$(echo "$MOUNT_OUTPUT" | awk -F '\t' '/\/Volumes\// {print $NF; exit}')"

if [[ -z "$MOUNT_POINT" || ! -d "$MOUNT_POINT" ]]; then
  echo "Could not resolve DMG mount point."
  exit 1
fi

MOUNT_NAME="$(basename "$MOUNT_POINT")"

cleanup() {
  hdiutil detach "$MOUNT_POINT" >/dev/null 2>&1 || true
}
trap cleanup EXIT

cp "src-tauri/icons/icon.icns" "$MOUNT_POINT/.VolumeIcon.icns"
SetFile -a C "$MOUNT_POINT" || true
SetFile -a V "$MOUNT_POINT/.background" "$MOUNT_POINT/.VolumeIcon.icns" || true

osascript <<APPLESCRIPT
tell application "Finder"
  tell disk "$MOUNT_NAME"
    open
    set current view of container window to icon view
    set toolbar visible of container window to false
    set statusbar visible of container window to false
    set bounds of container window to {120, 120, 880, 560}
    set opts to icon view options of container window
    set arrangement of opts to not arranged
    set icon size of opts to 112
    set background picture of opts to (POSIX file "$MOUNT_POINT/.background/background.png")
    set position of item "$PRODUCT_NAME.app" of container window to {185, 226}
    set position of item "Applications" of container window to {575, 226}
    update without registering applications
    delay 1
    close
  end tell
end tell
APPLESCRIPT

sync
hdiutil detach "$MOUNT_POINT"
trap - EXIT

hdiutil convert "$RW_DMG" -format UDZO -imagekey zlib-level=9 -o "$FINAL_DMG"
rm -f "$RW_DMG"

if [[ -n "${APPLE_SIGNING_IDENTITY:-}" ]]; then
  codesign --force --timestamp --sign "$APPLE_SIGNING_IDENTITY" "$FINAL_DMG"
fi

if [[ "${NOTARIZE_DMG:-1}" == "1" ]]; then
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

  xcrun notarytool submit "$FINAL_DMG" --wait "${NOTARY_ARGS[@]}"
  xcrun stapler staple "$FINAL_DMG"
  xcrun stapler validate "$FINAL_DMG"
fi

echo "$FINAL_DMG"
