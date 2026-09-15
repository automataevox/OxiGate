<p align="center">
  <img src="docs/logo.svg" alt="OxiGate logo" width="128" height="128" />
</p>

<h1 align="center">OxiGate</h1>

<p align="center">
  <strong>Ultra-fast, memory-safe L7 reverse proxy &amp; load balancer in pure Rust</strong>
</p>

<p align="center">
  HTTP/1.1 · HTTP/2 · Rustls TLS · health-aware LB · Prometheus · live dashboard
</p>

<p align="center">
  <a href="#benchmarks">Benchmarks</a> ·
  <a href="#quick-start">Quick start</a> ·
  <a href="#features">Features</a> ·
  <a href="#benchmark-it-yourself">Benchmark it yourself</a>
</p>

---

OxiGate is a Tokio + Hyper reverse proxy focused on **low latency**, **high CPU efficiency**, and **bounded memory** under load. Configuration is explicit YAML. Observability is built in (Prometheus, JSON access logs, WebSocket dashboard).

> **Status:** v0.1 — actively developed. Benchmarks below are reproducible on commodity hardware; they are not a claim of full production feature parity with HAProxy.

## Why pick OxiGate?

| You care about… | OxiGate |
|-----------------|--------|
| **Latency under load** | Sub-ms–low-ms p50/p99 on open-load soaks in our tests |
| **CPU cost per request** | Often ~½ the cores of HAProxy at the same fixed RPS |
| **Memory under bursts** | Bounded task channels + buffer pool — no multi‑GB spikes |
| **Rust / memory safety** | Pure Rust data path (Rustls, Tokio, Hyper) |
| **Ops ergonomics** | SIGHUP reload, `/metrics`, live dashboard, graceful shutdown |

**Prefer HAProxy when** you need maximum RAM density, decades of edge-case coverage, or its full ACL/Lua ecosystem.

**Prefer OxiGate when** you want a modern Rust proxy with strong latency/CPU numbers, explicit config, and a small operational surface.

## Claims (measured)

Same machine, same three Rust keep-alive backends, **wrk** `-t4 -c250` unless noted.

### 30-minute matched soak (open load)

| Metric | OxiGate | HAProxy |
|--------|--------:|--------:|
| **Requests/sec** | **275,170** | 132,064 |
| **p50** | **0.79 ms** | 1.66 ms |
| **p90** | **1.32 ms** | 3.21 ms |
| **p99** | **1.95 ms** | 818.73 ms |
| Timeouts | **0** | 20 |
| RSS (start of run) | **67.2 MB** | 94.5 MB |

### 4-hour endurance (OxiGate only)

| Metric | Result |
|--------|--------|
| Duration | **4 hours** |
| Total requests | **3.16 billion** |
| Average RPS | **~219,600** |
| p50 / p99 | **0.98 ms / 2.87 ms** |
| RSS | **~67–85 MB** (stable; no multi‑GB growth) |
| Connect / timeout errors | **0** |

### Fixed-rate efficiency (k6, 40k RPS target)

| Metric | OxiGate | HAProxy |
|--------|--------:|--------:|
| RPS (target met) | ~40,000 | ~40,000 |
| p50 | **0.057 ms** | 0.183 ms |
| p95 | **0.107 ms** | 0.749 ms |
| CPU avg | **~87%** (≈0.9 core) | **~201%** (≈2.0 cores) |
| **rps / CPU-core** | **~46,000** | ~20,000 |
| RSS after load | ~75 MB | **~33 MB** |

**Summary:** At fixed RPS, OxiGate used roughly **half the CPU** with tighter tails; HAProxy stayed leaner on RAM. Under open wrk load, OxiGate delivered **higher RPS**, **far better p99**, and **lower RSS** in our 30m comparison.

Results depend on CPU, kernel, NIC, backend, and config. **Reproduce before you trust them** — see [Benchmark it yourself](#benchmark-it-yourself).

## Features

- **Protocols:** HTTP/1.1 and HTTP/2; Rustls TLS termination
- **Bodies:** Streaming with size and idle limits
- **Load balancing:** Round-robin, weighted RR, least-connections, power-of-two-choices (P2C)
- **Routing:** Path-based routes, cookie affinity (sticky)
- **Resilience:** Retries (idempotent-aware), active health checks
- **Security:** Optional ACLs, rate limiting (GCRA), security headers, PROXY protocol
- **Observability:** Prometheus metrics, JSON access logs, live WebSocket dashboard
- **Operations:** SIGHUP config/TLS reload, graceful shutdown, connection limits
- **Memory path:** Bounded accept/task channels and a pre-allocated buffer pool so traffic bursts cannot unbounded-grow task/buffer memory

## Quick start

```bash
cargo build --release
./target/release/oxigate --config config.yaml
```

Smoke test:

```bash
curl http://127.0.0.1:8080/
curl http://127.0.0.1:9090/healthz
curl http://127.0.0.1:9090/metrics
```

Default `config.yaml` expects backends on `8081`–`8083`. Fast local backends:

```bash
cargo build --manifest-path benchmark/backend/Cargo.toml --release \
  --target-dir target/benchmark-backend

for p in 8081 8082 8083; do
  target/benchmark-backend/release/oxigate-benchmark-backend "$p" &
done
```

## Configuration

Full reference: [examples/config.full.yaml](examples/config.full.yaml).

Production notes:

- Set a strong `admin_token`
- Bind `metrics_listen` to a private address
- `/healthz` stays public for probes; `/dashboard`, `/api/stats`, `/metrics` can require the token

```bash
# reload config + TLS
kill -HUP $(pgrep -x oxigate)

# graceful stop
kill -TERM $(pgrep -x oxigate)

# high concurrency
ulimit -n 100000
```

## Dashboard and metrics

Admin listener (default `0.0.0.0:9090`):

| Endpoint | Purpose |
|----------|---------|
| `/dashboard` | Live control-plane UI |
| `/ws` | JSON stats stream (~1 Hz) |
| `/api/stats` | JSON snapshot |
| `/metrics` | Prometheus text |
| `/healthz` | Public liveness |

Useful series: `oxigate_requests_total`, `oxigate_request_duration_seconds`, `oxigate_upstream_health`, `oxigate_upstream_connections`, `oxigate_active_connections`, `oxigate_retries_total`.

## Benchmark it yourself

### Dependencies

```bash
# OxiGate
cargo build --release

# Backends used in published numbers
cargo build --manifest-path benchmark/backend/Cargo.toml --release \
  --target-dir target/benchmark-backend

# Load generator (preferred for soaks — low RAM vs k6)
# Debian/Ubuntu:
sudo apt install wrk
# or build: https://github.com/wg/wrk

# Optional: HAProxy comparison
sudo apt install haproxy

# Optional: k6 for fixed arrival-rate profiles
# https://k6.io/docs/get-started/installation/
```

### One-command suite (wrk)

The script starts three backends, runs OxiGate, then HAProxy (if installed), samples **RSS + CPU**, and writes `benchmark/results/comparison.txt`:

```bash
./benchmark/run_benchmarks.sh steady      # ~60s
./benchmark/run_benchmarks.sh soak_30m    # 30 minutes
./benchmark/run_benchmarks.sh soak_4h     # 4 hours
./benchmark/run_benchmarks.sh overload
./benchmark/run_benchmarks.sh stress
```

Overrides:

```bash
WRK_DURATION=10m WRK_THREADS=4 WRK_CONNECTIONS=250 \
  ./benchmark/run_benchmarks.sh soak_30m
```

Configs used:

- OxiGate: [benchmark/oxigate-benchmark.yaml](benchmark/oxigate-benchmark.yaml)
- HAProxy: [benchmark/haproxy.cfg](benchmark/haproxy.cfg)

### Manual wrk (match published 30m / 4h numbers)

```bash
# terminals: 3× backend on 8081–8083, then:
./target/release/oxigate --config benchmark/oxigate-benchmark.yaml

# load:
wrk -t4 -c250 -d30m --latency http://127.0.0.1:8080/
# or:
wrk -t4 -c250 -d4h --latency http://127.0.0.1:8080/
```

Watch proxy RSS while it runs:

```bash
watch -n 2 'ps -o rss=,pid=,comm= -p $(pgrep -n oxigate) | awk "{printf \"%.1f MB  %s\n\", \$1/1024, \$0}"'
```

### Fixed-rate tests (k6)

```bash
k6 run -e PROFILE=steady --discard-response-bodies benchmark/k6-test.js
# profiles: steady | lowTraffic | overload | stress | soak_30m | …
```

**Note:** k6 can use many GB of RAM on long/high-VU runs. Prefer **wrk** for multi-hour soaks; use k6 when you need a capped arrival rate (e.g. exactly 40k RPS).

### What to record

For each proxy, log:

1. `Requests/sec`, latency p50 / p90 or p95 / p99  
2. RSS before and after (or during) load  
3. CPU avg/peak if using `./benchmark/run_benchmarks.sh`  
4. Hardware: CPU model, cores, kernel version  

## Docker

```bash
cd docker
docker compose up --build
```

Compose uses [docker/oxigate-docker.yaml](docker/oxigate-docker.yaml). Do not expose the example `admin_token` on a public network.

## Development

```bash
cargo fmt --all -- --check
cargo check
cargo test
cargo build --release
```

CI: `.github/workflows/`. Functional notes: [test.md](test.md) (if present).

## License

See [LICENSE](LICENSE).

---

<p align="center">
  <img src="docs/logo-banner.svg" alt="OxiGate" width="320" />
</p>
