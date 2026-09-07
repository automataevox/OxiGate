# OxiGate

**Ultra-fast, memory-safe Layer 7 HTTP/HTTPS Reverse Proxy & Load Balancer written in pure Rust.**

Designed as a modern cloud-native alternative to HAProxy and Nginx with focus on:

- Zero-downtime hot-reload (SIGHUP + ArcSwap)
- Extremely low memory footprint
- Native Prometheus observability
- Multiple load-balancing algorithms
- Active health checks
- Memory safety (no GC, no buffer overflows)

## Quick start

```bash
# Build
cargo build --release

# Start test backends
python3 -m http.server 8081 &
python3 -m http.server 8082 &
python3 -m http.server 8083 &

# Run
./target/release/oxigate --config config.yaml

# Test
curl http://127.0.0.1:8080/
curl http://127.0.0.1:9090/metrics
```

## Features

| Feature                         | Status |
|--------------------------------|--------|
| HTTP/1.1 + HTTP/2 (ALPN)       | ✅     |
| TLS termination (rustls)       | ✅     |
| Streaming request/response     | ✅     |
| Path-based routing             | ✅     |
| Round-Robin / Weighted RR      | ✅     |
| Least Connections / P2C        | ✅     |
| Sticky sessions (cookie)       | ✅     |
| Automatic retries              | ✅     |
| Active health checks           | ✅     |
| Global max connections         | ✅     |
| Request timeout                | ✅     |
| Header injection               | ✅     |
| WebSocket / HTTP Upgrade       | ✅     |
| Prometheus metrics             | ✅     |
| JSON access logs               | ✅     |
| SIGHUP zero-downtime reload    | ✅     |
| Graceful shutdown              | ✅     |
| Memory-safe (pure Rust)        | ✅     |

## Configuration

See `examples/config.full.yaml` or root `config.yaml`.

```yaml
listen: "0.0.0.0:8080"
metrics_listen: "0.0.0.0:9090"

upstreams:
  - address: "http://127.0.0.1:8081"
    weight: 1
  - address: "http://127.0.0.1:8082"
    weight: 1

load_balancing: round_robin   # round_robin | weighted_round_robin | least_connections | power_of_two_choices

health_check:
  interval_secs: 5
  path: "/"

timeouts:
  connect_secs: 5
  request_secs: 30
```

## Testing & Benchmarks

Full instructions are in **[test.md](test.md)**.

Quick comparison against HAProxy:

```bash
# Install k6 and haproxy first
./benchmark/run_benchmarks.sh
```

## Production notes

- Always use `--release` build.
- Increase file descriptor limit: `ulimit -n 100000`
- Metrics are exposed on a separate port (default 9090).
- Config reload: `kill -HUP $(pgrep oxigate)`
- Graceful shutdown: SIGTERM / SIGINT

## Project layout

```
src/
├── config/       # YAML config + validation
├── lb/           # Load balancing algorithms
├── metrics/      # Prometheus
├── proxy/        # Core proxy logic
├── health/       # Active health checks
├── state.rs      # ArcSwap global state
└── main.rs       # Entrypoint
```

## License

MIT OR Apache-2.0


## Monitoring

OxiGate exposes a built-in admin surface on `metrics_listen` (default `:9090`):

| Path | Purpose |
|------|---------|
| `/dashboard` | Live HTML dashboard |
| `/api/stats` | JSON stats |
| `/metrics` | Prometheus scrape |
| `/healthz` | Liveness probe |

```bash
# Dashboard
open http://127.0.0.1:9090/dashboard

# Prometheus
curl -s http://127.0.0.1:9090/metrics | head

# Full stack (OxiGate + Prometheus + Grafana)
cd docker && docker compose up --build
# Grafana: http://localhost:3000  (admin/admin)
```

### Key Prometheus metrics

- `oxigate_requests_total{method,status,upstream}`
- `oxigate_request_duration_seconds`
- `oxigate_upstream_health`
- `oxigate_upstream_connections`
- `oxigate_rate_limit_rejects_total`
- `oxigate_rate_limit_allows_total`
- `oxigate_active_connections`
- `oxigate_retries_total`

Tracing spans use the `tracing` crate (`proxy_request` span with `otel.kind`, `http.method`, `client.address`). Ship JSON logs to any OpenTelemetry collector / Loki / Elastic pipeline.

## Rate limiting & PROXY protocol

```yaml
rate_limit:
  rps: 1000
  burst: 1.5

proxy_protocol: false   # true only behind HAProxy/NLB sending PROXY v1/v2
upstream_http2: true
pool_max_idle_per_host: 64
```
