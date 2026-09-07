#!/usr/bin/env bash
# 24–48h soak test harness for OxiGate
# Usage: ./benchmark/soak.sh [hours]
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
HOURS="${1:-24}"
SECONDS=$((HOURS * 3600))
OUT_DIR="benchmark/soak-$(date +%Y%m%d-%H%M%S)"
mkdir -p "$OUT_DIR"

echo "=== OxiGate soak test: ${HOURS}h ==="
echo "Output: $OUT_DIR"

cleanup() {
  pkill -f "python3 -m http.server 808" 2>/dev/null || true
  pkill -f "target/release/oxigate" 2>/dev/null || true
}
trap cleanup EXIT

python3 -m http.server 8081 >/dev/null 2>&1 &
python3 -m http.server 8082 >/dev/null 2>&1 &
python3 -m http.server 8083 >/dev/null 2>&1 &
sleep 1

if [[ ! -x target/release/oxigate ]]; then
  cargo build --release
fi

./target/release/oxigate --config config.yaml >"$OUT_DIR/oxigate.log" 2>&1 &
OX_PID=$!
sleep 2

# Continuous moderate load; prefer k6, fallback to hey, then curl loop
END=$(( $(date +%s) + SECONDS ))
ROUND=0
while [[ $(date +%s) -lt $END ]]; do
  ROUND=$((ROUND + 1))
  TS=$(date -Iseconds)
  RSS=$(ps -o rss= -p "$OX_PID" 2>/dev/null | awk '{printf "%.1f", $1/1024}' || echo "0")
  echo "$TS round=$ROUND rss_mb=$RSS" | tee -a "$OUT_DIR/rss.log"

  if command -v k6 >/dev/null; then
    k6 run --quiet --duration 60s --vus 50 \
      -e TARGET=http://127.0.0.1:8080 \
      benchmark/k6-test.js >>"$OUT_DIR/k6.log" 2>&1 || true
  elif command -v hey >/dev/null; then
    hey -z 60s -c 50 http://127.0.0.1:8080/ >>"$OUT_DIR/hey.log" 2>&1 || true
  else
    for i in $(seq 1 500); do
      curl -sf -o /dev/null http://127.0.0.1:8080/ || echo "fail $i" >>"$OUT_DIR/curl_fail.log"
    done
    sleep 30
  fi

  # Health probe
  curl -sf http://127.0.0.1:9090/healthz >/dev/null || echo "$TS healthz_fail" >>"$OUT_DIR/errors.log"
  curl -sf http://127.0.0.1:9090/metrics >"$OUT_DIR/metrics-latest.txt" || true
done

echo "Soak finished. Review $OUT_DIR"
