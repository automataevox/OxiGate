# OxiGate

OxiGate is a Rust and Tokio HTTP/1.1 and HTTP/2 reverse proxy and load balancer. It is designed for low overhead, explicit configuration, health-aware routing, and built-in observability.

This project is still under active development. Benchmark results are informative, not a claim of production parity with HAProxy.

## Highlights

- HTTP/1.1 and HTTP/2 with Rustls TLS termination
- Streaming request and response bodies with size and idle limits
- Round-robin, weighted round-robin, least-connections, and P2C balancing
- Path routing, cookie affinity, retries, ACLs, and rate limiting
- Active health checks with configurable thresholds
- WebSocket and HTTP Upgrade forwarding
- Prometheus metrics, JSON access logs, and a live WebSocket dashboard
- SIGHUP runtime reload and graceful shutdown

## Quick Start

```bash
cargo build --release
./target/release/oxigate --config config.yaml
```

Test the proxy and metrics endpoint:

```bash
curl http://127.0.0.1:8080/
curl http://127.0.0.1:9090/healthz
curl http://127.0.0.1:9090/metrics
```

The root `config.yaml` expects backends on ports `8081`, `8082`, and `8083`. For a local smoke test, use the faster benchmark backend:

```bash
cargo build --manifest-path benchmark/backend/Cargo.toml --release \
  --target-dir target/benchmark-backend
target/benchmark-backend/release/oxigate-benchmark-backend 8081 &
```

## Configuration and Operations

Use [examples/config.full.yaml](examples/config.full.yaml) as the configuration reference. Set a strong `admin_token` and bind `metrics_listen` to a private address in production. `/healthz` remains public for liveness probes; `/dashboard`, `/api/stats`, and `/metrics` require the admin token when configured.

Reload configuration and TLS material with:

```bash
kill -HUP $(pgrep -x oxigate)
```

Stop gracefully with `SIGTERM` or `SIGINT`. Raise the file descriptor limit for high concurrency:

```bash
ulimit -n 100000
```

## Dashboard and Metrics

The admin listener exposes:

| Endpoint | Purpose |
|---|---|
| `/dashboard` | Live dashboard using WebSocket updates |
| `/ws` | JSON stats stream, one update per second |
| `/api/stats` | JSON snapshot |
| `/metrics` | Prometheus text format |
| `/healthz` | Public liveness endpoint |

Important metrics include `oxigate_requests_total`, `oxigate_request_duration_seconds`, `oxigate_upstream_health`, `oxigate_upstream_connections`, `oxigate_active_connections`, and `oxigate_retries_total`.

## Benchmark

Install `k6` and HAProxy, then run:

```bash
./benchmark/run_benchmarks.sh
```

The script builds and starts three low-latency Rust backends, benchmarks OxiGate and HAProxy with the same k6 scenario, records RSS, writes aggregate summaries, updates `benchmark/results/comparison.txt`, and cleans up all processes and ports.

The latest local one-minute run used the Rust backend and measured:

| Metric | OxiGate | HAProxy |
|---|---:|---:|
| Requests/s | 14,764.99 | 10,690.30 |
| Median | 0.187 ms | 0.285 ms |
| p95 | 0.558 ms | 0.878 ms |
| HTTP errors | 0% | 0% |
| RSS after load | 19.3 MB | 33.2 MB |

Results depend on CPU, kernel, backend, k6 version, and configuration. Repeat the benchmark before drawing conclusions.

## Docker

The Compose stack uses [docker/oxigate-docker.yaml](docker/oxigate-docker.yaml), which routes to the Compose service names. Start it with:

```bash
cd docker
docker compose up --build
```

Do not use the example `admin_token: changeme` in an exposed deployment.

## Development

```bash
cargo fmt --all -- --check
cargo check
cargo test
cargo build --release
```

See [test.md](test.md) for functional checks and soak testing. GitHub Actions run from `.github/workflows/`.

## License

MIT OR Apache-2.0
