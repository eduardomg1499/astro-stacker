#!/usr/bin/env bash
# Benchmark E2E planetario (macOS/Linux): corre la APP REAL con perf_trace
# activado y agrega las trazas (mediana + IC95 por fase).
#
#   planetary_e2e.sh run [etiqueta]     — abre la app con ZAS_PERF_TRACE_DIR
#       apuntando a benchmarks/telemetry/<etiqueta>/. Ejecuta en la UI el
#       análisis y el apilado sobre el MISMO vídeo N veces (recomendado 5)
#       y cierra la app al terminar.
#   planetary_e2e.sh report <etiqueta> [--baseline <json>] [--save-baseline <json>]
#       — imprime mediana/IC95 del total y mediana por fase; compara/guarda
#       baseline (benchmarks/baselines/planetary-timing-<host>.json).
set -euo pipefail
cd "$(dirname "$0")/../.."
ROOT="$PWD"
CMD="${1:?uso: planetary_e2e.sh run|report [etiqueta] ...}"
LABEL="${2:-session}"
TRACE_DIR="$ROOT/benchmarks/telemetry/$LABEL"

case "$CMD" in
  run)
    mkdir -p "$TRACE_DIR"
    echo "Trazas en: $TRACE_DIR"
    echo "1) Se abrirá la app con el tracing activo."
    echo "2) Ejecuta Análisis + Apilado sobre tu vídeo 5 veces (mismos ajustes)."
    echo "   Para medir cold-cache, borra la caché de decode entre runs si aplica."
    echo "3) Cierra la app y corre: $0 report $LABEL"
    ZAS_PERF_TRACE_DIR="$TRACE_DIR" npm run tauri dev
    ;;
  report)
    shift 2 || true
    node "$ROOT/scripts/benchmark/planetary-e2e-report.mjs" "$TRACE_DIR" \
      --out "$TRACE_DIR/report.json" "$@"
    ;;
  *)
    echo "subcomando desconocido: $CMD (usa run|report)" >&2
    exit 2
    ;;
esac
