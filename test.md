# OxiGate – Testovací a benchmark příručka

Tento dokument popisuje, jak **funkčně otestovat** OxiGate a jak udělat **srovnávací benchmark proti HAProxy**.

---

## 1. Příprava prostředí

### Požadavky

```bash
# Rust (doporučeno 1.75+)
rustc --version
cargo --version

# HAProxy (pro srovnání)
sudo apt-get install -y haproxy   # Debian/Ubuntu
# nebo
brew install haproxy              # macOS

# Nástroje pro zátěž
# k6 (doporučeno) nebo hey / wrk / vegeta
# Instalace k6: https://k6.io/docs/getting-started/installation/
```

### Sestavení OxiGate

```bash
cd oxigate
cargo build --release
# binárka: target/release/oxigate
```

---

## 2. Funkční testy (smoke tests)

### 2.1 Spuštění testovacích backendů

Pro nízkolatenční benchmark použij vestavěný Rust backend. Benchmark skript jej
automaticky sestaví, spustí na portech 8081-8083 a po dokončení ukončí.

```bash
./benchmark/run_benchmarks.sh
```

Pro rychlý ruční smoke test můžeš spustit jednu instanci:

```bash
cargo build --manifest-path benchmark/backend/Cargo.toml --release \
  --target-dir target/benchmark-backend
target/benchmark-backend/release/oxigate-benchmark-backend 8081
```

### 2.2 Konfigurace OxiGate

Soubor `config.yaml` (již připraven v rootu projektu):

```yaml
listen: "0.0.0.0:8080"
metrics_listen: "0.0.0.0:9090"

upstreams:
  - address: "http://127.0.0.1:8081"
    weight: 1
  - address: "http://127.0.0.1:8082"
    weight: 1
  - address: "http://127.0.0.1:8083"
    weight: 2

load_balancing: round_robin

health_check:
  interval_secs: 3
  path: "/"

timeouts:
  connect_secs: 3
  request_secs: 10
```

### 2.3 Spuštění OxiGate

```bash
./target/release/oxigate --config config.yaml
```

### 2.4 Základní kontroly

```bash
# Proxy funguje
curl -v http://127.0.0.1:8080/

# Metriky
curl -s http://127.0.0.1:9090/metrics | head -30

# Health check – vypnutí jednoho backendu
kill %1   # vypne 8081
sleep 5
curl -s http://127.0.0.1:9090/metrics | grep oxigate_upstream_health

# Hot-reload
# Uprav config.yaml (např. změň load_balancing na least_connections)
kill -HUP $(pgrep -f oxigate)
# V logu by se mělo objevit "configuration reloaded successfully"
```

### 2.5 Očekávané chování

| Test                    | Očekávaný výsledek                          |
|-------------------------|---------------------------------------------|
| curl na :8080           | 200 + obsah z některého backendu            |
| /metrics                | Prometheus text format                      |
| vypnutí backendu        | health gauge klesne na 0, provoz jde dál    |
| SIGHUP                  | reload bez výpadku spojení                  |
| velké tělo (>16 MiB)    | 413 Payload Too Large                       |

---

## 3. Benchmark: OxiGate vs HAProxy

### 3.1 Společné backendy

Pro férový test použijeme stejné 3 backendy (nebo jeden velmi rychlý).

Doporučený backend pro vysoký throughput – jednoduchý Go nebo Rust server, případně:

```bash
# Rychlý statický backend (např. s nginxem nebo caddy v return módu)
# Pro jednoduchost použijeme python, ale pro reálná čísla raději:
#   - wrk's lua nebo
#   - vlastní minimal server
```

Pro produkční srovnání je ideální mít backend, který odpovídá okamžitě (např. `return 200` v nginx).

### 3.2 HAProxy konfigurace

Vytvoř soubor `benchmark/haproxy.cfg`:

```cfg
global
    maxconn 50000
    nbthread 4
    tune.bufsize 32768

defaults
    mode http
    timeout connect 5s
    timeout client  30s
    timeout server  30s
    option http-server-close
    option forwardfor

frontend fe
    bind *:8080
    default_backend be

backend be
    balance roundrobin
    option httpchk GET /
    http-check expect status 200
    server s1 127.0.0.1:8081 check inter 2s
    server s2 127.0.0.1:8082 check inter 2s
    server s3 127.0.0.1:8083 check inter 2s
```

Spuštění:

```bash
haproxy -f benchmark/haproxy.cfg -D
```

### 3.3 OxiGate pro benchmark

Použij stejný `config.yaml` jako výše (round_robin, 3 upstreamy).

```bash
./target/release/oxigate --config config.yaml
```

### 3.4 Zátěžový skript (k6)

Soubor `benchmark/k6-test.js`:

```javascript
import http from 'k6/http';
import { check, sleep } from 'k6';

export const options = {
  scenarios: {
    steady: {
      executor: 'constant-arrival-rate',
      rate: 5000,          // requests per second target
      timeUnit: '1s',
      duration: '60s',
      preAllocatedVUs: 200,
      maxVUs: 1000,
    },
  },
  thresholds: {
    http_req_duration: ['p(95)<50', 'p(99)<150'],
    http_req_failed: ['rate<0.01'],
  },
};

export default function () {
  const res = http.get('http://127.0.0.1:8080/');
  check(res, {
    'status is 200': (r) => r.status === 200,
  });
}
```

### 3.5 Spuštění benchmarku

```bash
# Terminal 1 – backendy
python3 -m http.server 8081 &
python3 -m http.server 8082 &
python3 -m http.server 8083 &

# Terminal 2 – OxiGate
./target/release/oxigate --config config.yaml

# Terminal 3 – k6
k6 run --out json=oxigate-results.json benchmark/k6-test.js

# Zastav OxiGate (Ctrl+C), spusť HAProxy
haproxy -f benchmark/haproxy.cfg

# Znovu k6
k6 run --out json=haproxy-results.json benchmark/k6-test.js
```

### 3.6 Alternativa s `hey` (jednodušší)

```bash
# OxiGate
hey -z 30s -c 200 -q 0 http://127.0.0.1:8080/

# HAProxy
hey -z 30s -c 200 -q 0 http://127.0.0.1:8080/
```

### 3.7 Co porovnávat

| Metrika              | OxiGate očekávání          | HAProxy                  |
|----------------------|----------------------------|--------------------------|
| Requests / sec       | velmi vysoké               | velmi vysoké             |
| p50 / p95 / p99      | sub-ms až nízké ms         | sub-ms až nízké ms       |
| Memory (RSS)         | cílově < 20–40 MB          | obvykle vyšší            |
| CPU                  | závisí na počtu jader       | velmi efektivní          |
| Chyby při reloadu    | 0 (SIGHUP)                 | 0 (pokud správně)        |

**Poznámka k férovému srovnání:**
- Stejný počet worker threadů / cores
- Stejné backendy
- Stejný zátěžový profil
- Měřit i paměť: `ps -o rss= -p $(pgrep oxigate)` vs HAProxy

---

## 4. Automatizovaný benchmark skript

Viz `benchmark/run_benchmarks.sh` – skript, který:

1. Spustí backendy
2. Spustí OxiGate → změří
3. Zastaví OxiGate, spustí HAProxy → změří
4. Vypíše shrnutí

```bash
chmod +x benchmark/run_benchmarks.sh
./benchmark/run_benchmarks.sh
```

---

## 5. Tip pro maximální výkon OxiGate

```bash
# Release build je povinný
cargo build --release

# Případně nastav počet workerů (Tokio automaticky použije počet CPU)
# Pro ještě nižší latenci lze experimentovat s:
export TOKIO_WORKER_THREADS=4

# Spouštěj s vyšším nofile limitem
ulimit -n 100000
```

---

## 6. Známá omezení současné verze (pro interpretaci výsledků)

- Těla requestů/response se zatím **buffrují** (limit 16 MiB) → při velkých uploadech/downloadech bude OxiGate paměťově náročnější než HAProxy.
- Health-check task drží původní LB Arc (po reloadu se nové upstreamy nekontrolují, dokud se proces nerestartuje).
- Zatím pouze HTTP/1.1.

Tyto body jsou na roadmapě a po jejich dokončení by měl být rozdíl ve prospěch OxiGate výraznější zejména v paměti a bezpečnosti.

---

**Hotovo.** Pokud chceš, můžu ještě doplnit Docker Compose pro celý testovací stack (OxiGate + 3 backendy + Prometheus + Grafana) nebo vylepšit streaming těl.

---

## TLS + HTTP/2 test

Generate a self-signed certificate:

```bash
mkdir -p certs
openssl req -x509 -newkey rsa:2048 -keyout certs/server.key -out certs/server.crt \
  -days 365 -nodes -subj "/CN=localhost"
```

Enable in `config.yaml`:

```yaml
tls:
  cert: "certs/server.crt"
  key: "certs/server.key"
```

Restart OxiGate and test:

```bash
# HTTP/1.1 over TLS
curl -vk https://127.0.0.1:8080/

# Force HTTP/2
curl -vk --http2 https://127.0.0.1:8080/
```

Metriky zůstávají na plain HTTP (`:9090`).
