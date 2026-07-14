#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT_DIR"

EVIDENCE_DIR="${HYBRID_V2_EVIDENCE_DIR:-src-tauri/target/hybrid-v2-verification}"
mkdir -p "$EVIDENCE_DIR"

echo "== Hybrid v2 release gate =="
echo "Evidence: $EVIDENCE_DIR"

echo "== Frontend and Rust checks =="
npm run check 2>&1 | tee "$EVIDENCE_DIR/frontend-rust-check.log"

echo "== CPU and synthetic suite =="
cargo test --manifest-path src-tauri/Cargo.toml --no-fail-fast 2>&1 \
  | tee "$EVIDENCE_DIR/cpu-synthetic-tests.log"

echo "== Physical GPU parity suite =="
cargo test --manifest-path src-tauri/Cargo.toml --no-fail-fast -- --ignored --nocapture 2>&1 \
  | tee "$EVIDENCE_DIR/gpu-physical-tests.log"

if [[ -n "$(git status --porcelain 2>/dev/null)" ]]; then
  HYBRID_GATE_DIRTY=true
else
  HYBRID_GATE_DIRTY=false
fi
HYBRID_GATE_COMMIT="$(git rev-parse HEAD 2>/dev/null || echo unknown)" \
HYBRID_GATE_OS="$(uname -s)" \
HYBRID_GATE_ARCH="$(uname -m)" \
HYBRID_GATE_DIRTY="$HYBRID_GATE_DIRTY" \
node scripts/release/verify-hybrid-v2-evidence.mjs "$EVIDENCE_DIR"

echo "Hybrid v2 release gate: PASS"
