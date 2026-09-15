#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

mkdir -p benchmark/results

echo "=== OxiGate vs HAProxy Benchmark (wrk) ==="
echo "Working directory: $ROOT"
echo

PROFILE="${1:-steady}"
TARGET_URL="${TARGET_URL:-http://127.0.0.1:8080/}"
BACKEND_PIDS=()
OX_PID=""
HAP_PID=""
OX_RSS_BEFORE="n/a"
OX_RSS_AFTER="n/a"
HAP_RSS_BEFORE="n/a"
HAP_RSS_AFTER="n/a"
OX_CPU_AVG="n/a"
OX_CPU_PEAK="n/a"
HAP_CPU_AVG="n/a"
HAP_CPU_PEAK="n/a"
SAMPLER_PID=""

# Profile → wrk params: duration threads connections (optional rate via --rate if wrk2)
# wrk is lightweight; connections control concurrency, not VU memory like k6.
case "$PROFILE" in
  lowTraffic)
    WRK_DURATION="30s"; WRK_THREADS=2; WRK_CONNECTIONS=50
    ;;
  steady)
    WRK_DURATION="60s"; WRK_THREADS=4; WRK_CONNECTIONS=200
    ;;
  overload)
    WRK_DURATION="60s"; WRK_THREADS=8; WRK_CONNECTIONS=1000
    ;;
  stress)
    WRK_DURATION="90s"; WRK_THREADS=8; WRK_CONNECTIONS=2000
    ;;
  soak_30m)
    WRK_DURATION="30m"; WRK_THREADS=4; WRK_CONNECTIONS=250
    ;;
  soak_4h)
    WRK_DURATION="4h"; WRK_THREADS=4; WRK_CONNECTIONS=250
    ;;
  *)
    echo "Unknown profile '$PROFILE' — using steady defaults"
    WRK_DURATION="60s"; WRK_THREADS=4; WRK_CONNECTIONS=200
    ;;
esac

# Optional overrides: ./run_benchmarks.sh steady  # or env:
#   WRK_DURATION=2h WRK_THREADS=4 WRK_CONNECTIONS=300 ./run_benchmarks.sh soak_custom
WRK_DURATION="${WRK_DURATION_OVERRIDE:-$WRK_DURATION}"
WRK_THREADS="${WRK_THREADS_OVERRIDE:-$WRK_THREADS}"
WRK_CONNECTIONS="${WRK_CONNECTIONS_OVERRIDE:-$WRK_CONNECTIONS}"

echo "Profile: $PROFILE  duration=$WRK_DURATION  threads=$WRK_THREADS  connections=$WRK_CONNECTIONS"
echo

# ---------- helpers ----------
cleanup() {
  echo "Cleaning up..."
  if [[ -n "${SAMPLER_PID:-}" ]]; then kill -9 "$SAMPLER_PID" 2>/dev/null || true; fi
  if [[ -n "${OX_PID:-}" ]]; then kill -9 "$OX_PID" 2>/dev/null || true; fi
  if [[ -n "${HAP_PID:-}" ]]; then kill -9 "$HAP_PID" 2>/dev/null || true; fi
  for pid in "${BACKEND_PIDS[@]:-}"; do kill -9 "$pid" 2>/dev/null || true; done

  fuser -k 8080/tcp 2>/dev/null || true
  fuser -k 8081/tcp 8082/tcp 8083/tcp 2>/dev/null || true
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
      if curl -fsS --max-time 1 "http://127.0.0.1:${port}/" >/dev/null 2>&1; then
        break
      fi
      sleep 0.1
    done
    if ! curl -fsS --max-time 1 "http://127.0.0.1:${port}/" >/dev/null 2>&1; then
      echo "Benchmark backend failed to start on port ${port}"
      exit 1
    fi
  done
}

measure_rss() {
  local pid=$1
  if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
    ps -o rss= -p "$pid" 2>/dev/null | awk '{printf "%.1f MB", $1/1024}' || echo "n/a"
  else
    echo "n/a"
  fi
}

cpu_sampler() {
  local pid=$1
  local outfile=$2
  local interval=${3:-0.5}
  local hz
  hz=$(getconf CLK_TCK 2>/dev/null || echo 100)

  : >"$outfile"
  local prev_ticks="" prev_wall=""

  while kill -0 "$pid" 2>/dev/null; do
    local stat
    stat=$(cat "/proc/$pid/stat" 2>/dev/null) || break
    local rest="${stat#*) }"
    local utime stime
    utime=$(echo "$rest" | awk '{print $12}')
    stime=$(echo "$rest" | awk '{print $13}')
    local ticks=$((utime + stime))
    local wall
    wall=$(date +%s%N)

    if [[ -n "$prev_ticks" ]]; then
      local d_ticks=$((ticks - prev_ticks))
      local d_ns=$((wall - prev_wall))
      if (( d_ns > 0 )); then
        local cpu
        cpu=$(awk -v t="$d_ticks" -v hz="$hz" -v ns="$d_ns" \
          'BEGIN { printf "%.1f", (t / hz) / (ns / 1e9) * 100 }')
        echo "$cpu" >>"$outfile"
      fi
    fi
    prev_ticks=$ticks
    prev_wall=$wall
    sleep "$interval"
  done
}

start_cpu_sampler() {
  local pid=$1
  local name=$2
  local outfile="benchmark/results/${name}-cpu-samples.txt"
  cpu_sampler "$pid" "$outfile" 0.5 &
  SAMPLER_PID=$!
}

stop_cpu_sampler() {
  local name=$1
  local outfile="benchmark/results/${name}-cpu-samples.txt"
  if [[ -n "${SAMPLER_PID:-}" ]]; then
    kill "$SAMPLER_PID" 2>/dev/null || true
    wait "$SAMPLER_PID" 2>/dev/null || true
    SAMPLER_PID=""
  fi
  if [[ -s "$outfile" ]]; then
    local avg peak
    avg=$(awk '{s+=$1; n++} END {if(n>0) printf "%.1f", s/n; else print "n/a"}' "$outfile")
    peak=$(awk 'BEGIN{m=0} {if($1>m)m=$1} END{printf "%.1f", m}' "$outfile")
    echo "$avg" "$peak"
  else
    echo "n/a n/a"
  fi
}

# Parse wrk stdout into a simple key=value summary file.
parse_wrk_output() {
  local raw=$1
  local summary=$2

  local requests rps transfer p50 p75 p90 p99 latency_avg errors
  requests=$(grep -E '^[0-9]+ requests in' "$raw" | awk '{print $1}' | head -1)
  rps=$(grep -E 'Requests/sec:' "$raw" | awk '{print $2}' | head -1)
  transfer=$(grep -E 'Transfer/sec:' "$raw" | awk '{print $2 $3}' | head -1)
  latency_avg=$(grep -E '^\s+Latency' "$raw" | head -1 | awk '{print $2}')
  p50=$(grep -E '^\s+50%' "$raw" | awk '{print $2}' | head -1)
  p75=$(grep -E '^\s+75%' "$raw" | awk '{print $2}' | head -1)
  p90=$(grep -E '^\s+90%' "$raw" | awk '{print $2}' | head -1)
  p99=$(grep -E '^\s+99%' "$raw" | awk '{print $2}' | head -1)
  # Socket errors line: Socket errors: connect 0, read 0, write 0, timeout 0
  errors=$(grep -E 'Socket errors:' "$raw" | head -1 || true)
  if [[ -z "$errors" ]]; then
    errors="0"
  else
    # sum connect+read+write+timeout if present
    errors=$(echo "$errors" | grep -oE '[0-9]+' | awk '{s+=$1} END{print s+0}')
  fi

  requests=${requests:-0}
  rps=${rps:-0}
  p50=${p50:-n/a}
  p75=${p75:-n/a}
  p90=${p90:-n/a}
  p99=${p99:-n/a}
  latency_avg=${latency_avg:-n/a}
  transfer=${transfer:-n/a}

  cat >"$summary" <<SUM
requests=$requests
rps=$rps
latency_avg=$latency_avg
p50=$p50
p75=$p75
p90=$p90
p99=$p99
transfer=$transfer
errors=$errors
SUM
}

run_wrk() {
  local name=$1
  local raw="benchmark/results/${name}-wrk.txt"
  local summary="benchmark/results/${name}-summary.txt"

  echo "→ Running wrk ($PROFILE) against $name ..."
  echo "  wrk -t$WRK_THREADS -c$WRK_CONNECTIONS -d$WRK_DURATION --latency $TARGET_URL"

  if ! command -v wrk >/dev/null 2>&1; then
    echo "  wrk not found. Install wrk (or wrk2) and re-run."
    echo "  e.g. sudo apt install wrk  OR  build from https://github.com/wg/wrk"
    return 1
  fi

  set +e
  wrk -t"$WRK_THREADS" -c"$WRK_CONNECTIONS" -d"$WRK_DURATION" --latency \
    "$TARGET_URL" 2>&1 | tee "$raw"
  local wrk_rc=${PIPESTATUS[0]}
  set -e

  parse_wrk_output "$raw" "$summary"

  if [[ "$wrk_rc" -ne 0 ]]; then
    echo "  (wrk exited $wrk_rc — continuing benchmark suite)"
  fi

  echo "  Parsed: $(tr '\n' ' ' <"$summary")"
  return 0
}

read_summary_field() {
  local file=$1
  local key=$2
  if [[ -f "$file" ]]; then
    grep -E "^${key}=" "$file" | head -1 | cut -d= -f2-
  else
    echo "n/a"
  fi
}

# ---------- prepare ----------
cleanup
start_backends

if [[ ! -x target/release/oxigate ]]; then
  echo "Building OxiGate (release)..."
  cargo build --release
fi

# ---------- OxiGate ----------
echo
echo ">>> Testing OxiGate"
./target/release/oxigate --config benchmark/oxigate-benchmark.yaml \
  >benchmark/results/oxigate-run.log 2>&1 &
OX_PID=$!
sleep 2

if ! kill -0 "$OX_PID" 2>/dev/null; then
  echo "OxiGate failed to start"
  cat benchmark/results/oxigate-run.log
  exit 1
fi

OX_RSS_BEFORE=$(measure_rss "$OX_PID")
echo "  PID: $OX_PID  RSS: $OX_RSS_BEFORE"

start_cpu_sampler "$OX_PID" "oxigate"
run_wrk "oxigate"
read -r OX_CPU_AVG OX_CPU_PEAK <<<"$(stop_cpu_sampler oxigate)"

OX_RSS_AFTER=$(measure_rss "$OX_PID")
echo "  RSS after load: $OX_RSS_AFTER"
echo "  CPU avg/peak:   ${OX_CPU_AVG}% / ${OX_CPU_PEAK}%  (100% = 1 core)"

kill -TERM "$OX_PID" 2>/dev/null || true
wait "$OX_PID" 2>/dev/null || true
OX_PID=""
sleep 1

# ---------- HAProxy ----------
echo
echo ">>> Testing HAProxy"
if ! command -v haproxy >/dev/null 2>&1; then
  echo "  HAProxy not installed – skipping comparison."
  exit 0
fi

rm -f benchmark/results/haproxy.pid
haproxy -f benchmark/haproxy.cfg -p benchmark/results/haproxy.pid -D
sleep 1

HAP_PID=""
if [[ -f benchmark/results/haproxy.pid ]]; then
  HAP_PID=$(cat benchmark/results/haproxy.pid)
fi

if [[ -z "$HAP_PID" ]] || ! kill -0 "$HAP_PID" 2>/dev/null; then
  echo "HAProxy failed to start"
  exit 1
fi

HAP_RSS_BEFORE=$(measure_rss "$HAP_PID")
echo "  PID: $HAP_PID  RSS: $HAP_RSS_BEFORE"

start_cpu_sampler "$HAP_PID" "haproxy"
run_wrk "haproxy"
read -r HAP_CPU_AVG HAP_CPU_PEAK <<<"$(stop_cpu_sampler haproxy)"

HAP_RSS_AFTER=$(measure_rss "$HAP_PID")
echo "  RSS after load: $HAP_RSS_AFTER"
echo "  CPU avg/peak:   ${HAP_CPU_AVG}% / ${HAP_CPU_PEAK}%  (100% = 1 core)"

kill -TERM "$HAP_PID" 2>/dev/null || true
HAP_PID=""

# ---------- comparison ----------
OX_SUM=benchmark/results/oxigate-summary.txt
HAP_SUM=benchmark/results/haproxy-summary.txt

if [[ -f "$OX_SUM" && -f "$HAP_SUM" ]]; then
  ox_requests=$(read_summary_field "$OX_SUM" requests)
  hap_requests=$(read_summary_field "$HAP_SUM" requests)
  ox_rps=$(read_summary_field "$OX_SUM" rps)
  hap_rps=$(read_summary_field "$HAP_SUM" rps)
  ox_median=$(read_summary_field "$OX_SUM" p50)
  hap_median=$(read_summary_field "$HAP_SUM" p50)
  ox_p95=$(read_summary_field "$OX_SUM" p90)   # wrk has 90%, not 95%
  hap_p95=$(read_summary_field "$HAP_SUM" p90)
  ox_p99=$(read_summary_field "$OX_SUM" p99)
  hap_p99=$(read_summary_field "$HAP_SUM" p99)
  ox_errors=$(read_summary_field "$OX_SUM" errors)
  hap_errors=$(read_summary_field "$HAP_SUM" errors)

  ox_rps_per_core="n/a"
  hap_rps_per_core="n/a"
  ox_rps_per_mb="n/a"
  hap_rps_per_mb="n/a"
  if [[ "$OX_CPU_AVG" != "n/a" && "$OX_CPU_AVG" != "0.0" && "$OX_CPU_AVG" != "0" ]]; then
    ox_rps_per_core=$(awk -v r="$ox_rps" -v c="$OX_CPU_AVG" 'BEGIN{printf "%.0f", r/(c/100)}')
  fi
  if [[ "$HAP_CPU_AVG" != "n/a" && "$HAP_CPU_AVG" != "0.0" && "$HAP_CPU_AVG" != "0" ]]; then
    hap_rps_per_core=$(awk -v r="$hap_rps" -v c="$HAP_CPU_AVG" 'BEGIN{printf "%.0f", r/(c/100)}')
  fi
  ox_rss_num=$(echo "$OX_RSS_AFTER" | awk '{print $1}')
  hap_rss_num=$(echo "$HAP_RSS_AFTER" | awk '{print $1}')
  if [[ "$ox_rss_num" != "n/a" && -n "$ox_rss_num" ]]; then
    ox_rps_per_mb=$(awk -v r="$ox_rps" -v m="$ox_rss_num" 'BEGIN{if(m>0) printf "%.1f", r/m; else print "n/a"}')
  fi
  if [[ "$hap_rss_num" != "n/a" && -n "$hap_rss_num" ]]; then
    hap_rps_per_mb=$(awk -v r="$hap_rps" -v m="$hap_rss_num" 'BEGIN{if(m>0) printf "%.1f", r/m; else print "n/a"}')
  fi

  cat > benchmark/results/comparison.txt <<CMP
=== Benchmark comparison (profile: $PROFILE, tool: wrk) ===

metric                  OxiGate                 HAProxy
requests                $ox_requests                  $hap_requests
rps                     $ox_rps                   $hap_rps
median (p50)            $ox_median                  $hap_median
p90                     $ox_p95                  $hap_p95
p99                     $ox_p99                  $hap_p99
socket errors           $ox_errors                   $hap_errors
RSS after load          $OX_RSS_AFTER                   $HAP_RSS_AFTER
CPU avg (load)          ${OX_CPU_AVG}%                  ${HAP_CPU_AVG}%
CPU peak (load)         ${OX_CPU_PEAK}%                  ${HAP_CPU_PEAK}%
rps / CPU-core          $ox_rps_per_core                   $hap_rps_per_core
rps / MB RSS            $ox_rps_per_mb                   $hap_rps_per_mb

wrk params: -t$WRK_THREADS -c$WRK_CONNECTIONS -d$WRK_DURATION --latency
Notes:
- CPU% is relative to one core (100% = 1 full core; can exceed 100% if multi-threaded).
- Samples taken every 0.5s from /proc/<pid>/stat during the wrk run only.
- Backend: benchmark/backend Rust keep-alive responder.
- wrk reports p50/p90/p99 (no p95); p90 is shown in the p90 row.
- wrk uses far less RAM than k6 — suitable for soak_30m / soak_4h.
CMP

  echo
  cat benchmark/results/comparison.txt
fi

echo
echo "=== Benchmark finished ==="
echo "Results in benchmark/results/ (*-wrk.txt, *-summary.txt, comparison.txt, cpu samples)"
echo
