#!/usr/bin/env bash
# Arnés A/B planetario (F0): mide un stack candidato (métricas de limbo,
# ringing, ocupación de rango) y, si se aporta una referencia con la MISMA
# geometría (p.ej. el stack de AutoStakkert!4 del mismo vídeo y encuadre),
# añade PSNR/SSIM contra ella. Escribe el informe JSON.
#
# Uso: run-planetary-ab.sh <candidato.png|tif> [referencia.png|tif] [salida.json]
set -euo pipefail

abspath() { cd "$(dirname "$1")" >/dev/null && printf '%s/%s\n' "$PWD" "$(basename "$1")"; }

CAND="${1:?uso: run-planetary-ab.sh <candidato> [referencia] [salida.json]}"
REF="${2:-}"
OUT="${3:-planetary-ab-report.json}"

export ZAS_AB_CANDIDATE="$(abspath "$CAND")"
if [ -n "$REF" ]; then
  export ZAS_AB_REFERENCE="$(abspath "$REF")"
fi
# La salida puede no existir todavía: resolver contra el cwd actual.
case "$OUT" in
  /*) export ZAS_AB_OUT="$OUT" ;;
  *) export ZAS_AB_OUT="$PWD/$OUT" ;;
esac

cd "$(dirname "$0")/../../src-tauri"
cargo test --release --quiet f0_ab_compare -- --ignored --nocapture
echo "Informe: $ZAS_AB_OUT"
