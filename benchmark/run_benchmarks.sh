#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

echo "=== OxiGate vs HAProxy Benchmark ==="
echo "Working directory: $ROOT"
echo

BACKEND_PIDS=()
OX_PID=""
HAP_PID=""
OX_RSS_BEFORE="n/a"
OX_RSS_AFTER="n/a"
HAP_RSS_BEFORE="n/a"
HAP_RSS_AFTER="n/a"

# ---------- helpers ----------
cleanup() {
  echo "Cleaning up..."
  if [[ -n "$OX_PID" ]]; then kill "$OX_PID" 2>/dev/null || true; fi
  if [[ -n "$HAP_PID" ]]; then kill "$HAP_PID" 2>/dev/null || true; fi
  for pid in "${BACKEND_PIDS[@]}"; do kill "$pid" 2>/dev/null || true; done
  sleep 1
}
trap cleanup EXIT

start_backends() {
  echo "Building benchmark backend..."
  cargo build --manifest-path benchmark/backend/Cargo.toml --release --target-dir target/benchmark-backend
  echo "Starting 3 low-latency backend servers..."
  for port in 8081 8082 8083; do
    target/benchmark-backend/release/oxigate-benchmark-backend "$port" \
      >"benchmark/results/backend-${port}.log" 2>&1 &
    BACKEND_PIDS+=("$!")
  done

  for port in 8081 8082 8083; do
    for _ in {1..50}; do
      if curl -fsS --max-time 1 "http://127.0.0.1:${port}/" >/dev/null; then
        break
      fi
      sleep 0.1
    done
    if ! curl -fsS --max-time 1 "http://127.0.0.1:${port}/" >/dev/null; then
      echo "Benchmark backend failed to start on port ${port}"
      exit 1
    fi
  done
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
  local out="benchmark/results/${name}-summary.json"
  echo "→ Running k6 against $name ..."
  if command -v k6 >/dev/null 2>&1; then
    k6 run --quiet --summary-export "$out" benchmark/k6-test.js | tee "benchmark/results/${name}-k6.txt"
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
./target/release/oxigate --config benchmark/oxigate-benchmark.yaml >benchmark/results/oxigate-run.log 2>&1 &
OX_PID=$!
sleep 2

if ! kill -0 "$OX_PID" 2>/dev/null; then
  echo "OxiGate failed to start"
  cat benchmark/results/oxigate-run.log
  exit 1
fi

OX_RSS_BEFORE=$(measure_rss "$OX_PID")
echo "  PID: $OX_PID  RSS: $OX_RSS_BEFORE"
run_k6 "oxigate"
OX_RSS_AFTER=$(measure_rss "$OX_PID")
echo "  RSS after load: $OX_RSS_AFTER"
printf '%s\n' "rss_before=$OX_RSS_BEFORE" "rss_after=$OX_RSS_AFTER" > benchmark/results/oxigate-rss.txt

if ! kill -0 "$OX_PID" 2>/dev/null; then
  echo "OxiGate exited during load"
  cat benchmark/results/oxigate-run.log
  exit 1
fi

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

rm -f benchmark/results/haproxy.pid
haproxy -f benchmark/haproxy.cfg -p benchmark/results/haproxy.pid -D
sleep 1
HAP_PID=""
if [[ -f benchmark/results/haproxy.pid ]]; then
  HAP_PID=$(cat benchmark/results/haproxy.pid)
fi
HAP_RSS_BEFORE=$(measure_rss "$HAP_PID")
echo "  PID: $HAP_PID  RSS: $HAP_RSS_BEFORE"

run_k6 "haproxy"
HAP_RSS_AFTER=$(measure_rss "$HAP_PID")
echo "  RSS after load: $HAP_RSS_AFTER"
printf '%s\n' "rss_before=$HAP_RSS_BEFORE" "rss_after=$HAP_RSS_AFTER" > benchmark/results/haproxy-rss.txt

# ---------- comparison ----------
if command -v jq >/dev/null 2>&1 \
  && [[ -f benchmark/results/oxigate-summary.json ]] \
  && [[ -f benchmark/results/haproxy-summary.json ]]; then
  ox_requests=$(jq -r '.metrics.http_reqs.count' benchmark/results/oxigate-summary.json)
  hap_requests=$(jq -r '.metrics.http_reqs.count' benchmark/results/haproxy-summary.json)
  ox_rps=$(jq -r '.metrics.http_reqs.rate' benchmark/results/oxigate-summary.json | awk '{printf "%.2f", $1}')
  hap_rps=$(jq -r '.metrics.http_reqs.rate' benchmark/results/haproxy-summary.json | awk '{printf "%.2f", $1}')
  ox_median=$(jq -r '.metrics.http_req_duration.med' benchmark/results/oxigate-summary.json | awk '{printf "%.3f ms", $1}')
  hap_median=$(jq -r '.metrics.http_req_duration.med' benchmark/results/haproxy-summary.json | awk '{printf "%.3f ms", $1}')
  ox_p95=$(jq -r '.metrics.http_req_duration["p(95)"]' benchmark/results/oxigate-summary.json | awk '{printf "%.3f ms", $1}')
  hap_p95=$(jq -r '.metrics.http_req_duration["p(95)"]' benchmark/results/haproxy-summary.json | awk '{printf "%.3f ms", $1}')
  ox_errors=$(jq -r '.metrics.http_req_failed.value * 100' benchmark/results/oxigate-summary.json | awk '{printf "%.3f%%", $1}')
  hap_errors=$(jq -r '.metrics.http_req_failed.value * 100' benchmark/results/haproxy-summary.json | awk '{printf "%.3f%%", $1}')
  cat > benchmark/results/comparison.txt <<EOF
=== 1-minute benchmark comparison ===

metric              OxiGate             HAProxy
requests            $ox_requests          $hap_requests
rps                 $ox_rps             $hap_rps
median              $ox_median            $hap_median
p95                 $ox_p95            $hap_p95
p99                 not recorded        not recorded
HTTP errors         $ox_errors             $hap_errors
RSS after load      $OX_RSS_AFTER             $HAP_RSS_AFTER

Notes:
- Backend: benchmark/backend Rust keep-alive responder.
- Results are from one 60-second k6 run per proxy.
EOF
fi

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
