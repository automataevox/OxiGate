#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

echo "=== OxiGate vs HAProxy Benchmark ==="
echo "Working directory: $ROOT"
echo

# ---------- helpers ----------
cleanup() {
  echo "Cleaning up..."
  pkill -f "python3 -m http.server 808" 2>/dev/null || true
  pkill -f "oxigate" 2>/dev/null || true
  pkill -f "haproxy.*benchmark/haproxy.cfg" 2>/dev/null || true
  sleep 1
}
trap cleanup EXIT

start_backends() {
  echo "Starting 3 backend servers..."
  python3 -m http.server 8081 >/dev/null 2>&1 &
  python3 -m http.server 8082 >/dev/null 2>&1 &
  python3 -m http.server 8083 >/dev/null 2>&1 &
  sleep 1
}

measure_rss() {
  local pid=$1
  if [[ -n "$pid" ]]; then
    ps -o rss= -p "$pid" 2>/dev/null | awk '{printf "%.1f MB", $1/1024}' || echo "n/a"
  else
    echo "n/a"
  fi
}

run_k6() {
  local name=$1
  local out="benchmark/${name}-results.json"
  echo "→ Running k6 against $name ..."
  if command -v k6 >/dev/null 2>&1; then
    k6 run --quiet --out json="$out" benchmark/k6-test.js || true
  else
    echo "  k6 not found – falling back to hey (if available)"
    if command -v hey >/dev/null 2>&1; then
      hey -z 30s -c 150 -q 0 http://127.0.0.1:8080/ | tee "benchmark/${name}-hey.txt"
    else
      echo "  Neither k6 nor hey found. Install one of them."
      return 1
    fi
  fi
}

# ---------- prepare ----------
cleanup
start_backends

# Ensure release binary exists
if [[ ! -x target/release/oxigate ]]; then
  echo "Building OxiGate (release)..."
  cargo build --release
fi

# ---------- OxiGate ----------
echo
echo ">>> Testing OxiGate"
./target/release/oxigate --config config.yaml &
OX_PID=$!
sleep 2

echo "  PID: $OX_PID  RSS: $(measure_rss $OX_PID)"
run_k6 "oxigate"
echo "  RSS after load: $(measure_rss $OX_PID)"

kill -TERM $OX_PID 2>/dev/null || true
wait $OX_PID 2>/dev/null || true
sleep 1

# ---------- HAProxy ----------
echo
echo ">>> Testing HAProxy"
if ! command -v haproxy >/dev/null 2>&1; then
  echo "  HAProxy not installed – skipping comparison."
  echo "  Install with: sudo apt install haproxy   or   brew install haproxy"
  exit 0
fi

haproxy -f benchmark/haproxy.cfg -D
sleep 1
HAP_PID=$(pgrep -f "haproxy.*benchmark/haproxy.cfg" | head -1 || true)
echo "  PID: $HAP_PID  RSS: $(measure_rss $HAP_PID)"

run_k6 "haproxy"
echo "  RSS after load: $(measure_rss $HAP_PID)"

# ---------- summary ----------
echo
echo "=== Benchmark finished ==="
echo "Results saved in benchmark/*-results.json (or *-hey.txt)"
echo
echo "Quick comparison tips:"
echo "  - Look at Requests/s, p95, p99 and RSS"
echo "  - OxiGate should show very low memory usage"
echo "  - For production numbers use a faster backend than python http.server"
echo
