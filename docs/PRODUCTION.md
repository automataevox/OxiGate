# OxiGate – Production readiness guide

Tento dokument popisuje, **co je v kódu hotové**, a **krok za krokem**, co musíš spustit u sebe (build, testy, benchmark, soak).

## 1. Build & unit/integration testy

```bash
cd oxigate

# Závislosti (Linux)
sudo apt-get install -y build-essential pkg-config libssl-dev

# Testy
cargo test --all

# Release build
cargo build --release
./target/release/oxigate --config config.yaml
```

Očekávané unit testy:
- `proxy_protocol` – v1 parse + non-PROXY bytes preserved
- `ratelimit` – allow / reject
- `security` – ACL deny/allow
- `router` – path prefix matching
- LB / rate limit v `tests/integration.rs`

## 2. CI (GitHub Actions)

Workflow `.github/workflows/ci.yml` obsahuje:
- `cargo fmt`, `clippy`, `test`, `build --release`
- multi-arch Docker push na `ghcr.io`

Zapni Actions ve forku a pushni na `main`.

## 3. Benchmark vs HAProxy

```bash
sudo apt-get install -y haproxy
# k6: https://k6.io/docs/getting-started/installation/

./benchmark/run_benchmarks.sh
```

Srovnávej RPS, p95/p99, RSS (`ps -o rss= -p PID`).

## 4. Body limit & retry politika

```yaml
max_body_bytes: 16777216
retry_idempotent_only: true
force_retry_with_body: false
retries: 2
```

- Body > limit → **413**
- Retry default jen GET/HEAD/OPTIONS/PUT/DELETE nebo prázdné body
- POST s tělem se neopakuje

## 5. PROXY protocol + TLS cert reload

```yaml
proxy_protocol: true
tls:
  cert: "certs/server.crt"
  key: "certs/server.key"
```

```bash
kill -HUP $(pgrep oxigate)   # reload config + TLS certs
```

PROXY parser je buffered – nePROXY traffic se nezahodí.

## 6. Security baseline

```yaml
security_headers: true
acl:
  allow: ["10.0.0.0/8"]
  deny: []
```

## 7. Soak test 24–48h

```bash
./benchmark/soak.sh 24
./benchmark/soak.sh 48
```

Sleduj RSS, panics, healthz, p99.

## 8. Monitoring

- Dashboard: `http://127.0.0.1:9090/dashboard`
- Metrics: `http://127.0.0.1:9090/metrics`
- Compose: `cd docker && docker compose up --build`

## 9. Checklist

- [ ] `cargo test --all` green
- [ ] `cargo build --release` green
- [ ] CI green
- [ ] Benchmark vs HAProxy
- [ ] TLS HUP reload
- [ ] ACL / rate limit
- [ ] Soak ≥ 24h
- [ ] Prometheus v monitoringu

## 10. Co musíš spustit ty (sandbox to nedotáhne)

| Úkol | Příkaz |
|------|--------|
| Testy + release | `cargo test --all && cargo build --release` |
| Soak 24h | `./benchmark/soak.sh 24` |
| HAProxy bench | `./benchmark/run_benchmarks.sh` |
| CI / Docker | push na GitHub `main` |
