#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT_DIR"

# === CONFIGURACION APPLE ZENITH ASTRO STACKER ===
# La identidad no es secreta; ya existe en el llavero de esta Mac.
SAVED_APPLE_SIGNING_IDENTITY="Developer ID Application: Eduardo Martinez Gomez (434G7RN9PF)"
SAVED_APPLE_TEAM_ID="434G7RN9PF"

# Para notarizar puedes usar un perfil guardado con notarytool o llenar una
# de las parejas de credenciales de abajo antes de ejecutar este script.
SAVED_APPLE_NOTARY_PROFILE="ZenithAstroNotary"
SAVED_APPLE_API_ISSUER=""
SAVED_APPLE_API_KEY=""
SAVED_APPLE_API_KEY_PATH=""
SAVED_APPLE_ID=""
SAVED_APPLE_PASSWORD=""

export APPLE_SIGNING_IDENTITY="${APPLE_SIGNING_IDENTITY:-$SAVED_APPLE_SIGNING_IDENTITY}"
export APPLE_TEAM_ID="${APPLE_TEAM_ID:-$SAVED_APPLE_TEAM_ID}"
export APPLE_NOTARY_PROFILE="${APPLE_NOTARY_PROFILE:-$SAVED_APPLE_NOTARY_PROFILE}"
export APPLE_API_ISSUER="${APPLE_API_ISSUER:-$SAVED_APPLE_API_ISSUER}"
export APPLE_API_KEY="${APPLE_API_KEY:-$SAVED_APPLE_API_KEY}"
export APPLE_API_KEY_PATH="${APPLE_API_KEY_PATH:-$SAVED_APPLE_API_KEY_PATH}"
export APPLE_ID="${APPLE_ID:-$SAVED_APPLE_ID}"
export APPLE_PASSWORD="${APPLE_PASSWORD:-$SAVED_APPLE_PASSWORD}"

MACOS_BUILD_TARGET="${MACOS_BUILD_TARGET:-aarch64-apple-darwin}"
ARCH_LABEL="${ARCH_LABEL:-aarch64}"
TARGET_DIR="src-tauri/target/$MACOS_BUILD_TARGET/release"

# --- Verificación de binarios FFmpeg (deben empaquetarse en el .app) ---
# Están ignorados por Git; si faltan, el bundle queda sin FFmpeg y la app
# dará "No se encontró FFprobe". Abortamos antes de compilar.
case "$ARCH_LABEL" in
  aarch64) need_bins=("ffmpeg-aarch64-apple-darwin" "ffprobe-aarch64-apple-darwin") ;;
  x86_64)  need_bins=("ffmpeg-x86_64-apple-darwin" "ffprobe-x86_64-apple-darwin") ;;
  *)       need_bins=() ;;
esac
missing_bins=()
for b in "${need_bins[@]}"; do
  f="src-tauri/bin/$b"
  if [[ ! -f "$f" ]] || [[ "$(stat -f%z "$f" 2>/dev/null || echo 0)" -lt 1000000 ]]; then
    missing_bins+=("$b")
  fi
done
if [[ ${#missing_bins[@]} -gt 0 ]]; then
  echo "ERROR: faltan binarios FFmpeg en src-tauri/bin para $ARCH_LABEL: ${missing_bins[*]}" >&2
  echo "Están ignorados por Git. Colócalos en src-tauri/bin antes de compilar para que el .app los empaquete." >&2
  exit 1
fi
echo "-> Binarios FFmpeg ($ARCH_LABEL) presentes; se empaquetarán en el .app."

# Una release Hybrid v2 no puede publicarse con los kernels GPU sin ejecutar.
# El gate guarda los logs y aborta ante cualquier fallo de paridad física.
scripts/release/verify-hybrid-v2-gate.sh

scripts/apple/build-macos-updater-artifact.sh
scripts/apple/create-custom-macos-dmg.sh
npm run release:latest-json

echo ""
echo "macOS release files:"
find "$TARGET_DIR/bundle/macos" \
  \( -name '*.app.tar.gz' -o -name '*.app.tar.gz.sig' \) \
  -print
find "$TARGET_DIR/bundle/dmg" \
  -name "*_${ARCH_LABEL}_styled.dmg" \
  -print
echo "latest.json"
