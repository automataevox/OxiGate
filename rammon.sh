#!/usr/bin/env bash
# Track RSS memory over time for OxiGate during a 4h run
LOG_FILE="benchmark/results/oxigate-4h-metrics.csv"
echo "timestamp,rss_mb,cpu_pct" > "$LOG_FILE"

PID=$1 # OxiGate PID

while kill -0 "$PID" 2>/dev/null; do
  TS=$(date +%s)
  STATS=$(ps -p "$PID" -o rss=,%cpu= 2>/dev/null)
  RSS=$(echo "$STATS" | awk '{print $1/1024}')
  CPU=$(echo "$STATS" | awk '{print $2}')
  echo "$TS,$RSS,$CPU" >> "$LOG_FILE"
  sleep 10
done