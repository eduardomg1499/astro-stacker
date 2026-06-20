#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT_DIR"

MACOS_BUILD_TARGET="${MACOS_BUILD_TARGET:-aarch64-apple-darwin}"
ARCH_LABEL="${ARCH_LABEL:-aarch64}"

echo "== Zenith Astro Stacker Apple preflight =="
echo "Root: $ROOT_DIR"
echo "Target: $MACOS_BUILD_TARGET"

echo "== Frontend build =="
npm run build

echo "== Tauri JSON validation =="
node -e "for (const f of ['src-tauri/tauri.conf.json','src-tauri/tauri.macos.conf.json','src-tauri/tauri.no-updater-artifacts.conf.json']) { JSON.parse(require('fs').readFileSync(f,'utf8')); console.log(f + ': OK'); }"

echo "== Rust desktop check =="
(cd src-tauri && cargo check)

echo "== Apple plist validation =="
plutil -lint src-tauri/Entitlements.mac.plist

echo "== Apple tools =="
xcrun notarytool --version
security find-identity -v -p codesigning | grep 'Developer ID Application' || {
  echo "No Developer ID Application identity was found."
  exit 1
}

echo "== FFmpeg packaged binaries =="
case "$MACOS_BUILD_TARGET" in
  aarch64-apple-darwin)
    REQUIRED_SUFFIX="aarch64-apple-darwin"
    ;;
  x86_64-apple-darwin)
    REQUIRED_SUFFIX="x86_64-apple-darwin"
    ;;
  *)
    REQUIRED_SUFFIX="$ARCH_LABEL"
    ;;
esac

if [[ -f "src-tauri/bin/ffmpeg-$REQUIRED_SUFFIX" && -f "src-tauri/bin/ffprobe-$REQUIRED_SUFFIX" ]]; then
  echo "Found packaged FFmpeg and FFprobe for $REQUIRED_SUFFIX."
elif [[ -f "src-tauri/bin/ffmpeg" && -f "src-tauri/bin/ffprobe" ]]; then
  echo "Found generic packaged FFmpeg and FFprobe."
else
  echo "Warning: packaged macOS FFmpeg/FFprobe were not found."
  echo "The app can still build, but video features may depend on system FFmpeg."
fi

echo "Preflight OK."
