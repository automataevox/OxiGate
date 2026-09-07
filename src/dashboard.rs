//! Built-in web dashboard + JSON stats API for live monitoring.

use crate::metrics::Metrics;
use crate::ratelimit::RateLimiter;
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Instant;

pub struct DashboardState {
    pub metrics: Arc<Metrics>,
    pub rate_limiter: Option<Arc<RateLimiter>>,
    pub started: Instant,
    pub version: &'static str,
}

pub async fn handle_admin(
    req: Request<Incoming>,
    state: Arc<DashboardState>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let path = req.uri().path();
    match path {
        "/" | "/dashboard" | "/dashboard/" => Ok(html_response(DASHBOARD_HTML)),
        "/api/stats" => Ok(json_response(&stats_json(&state))),
        "/metrics" => {
            let body = state.metrics.gather();
            Ok(Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", "text/plain; version=0.0.4; charset=utf-8")
                .header("Cache-Control", "no-cache")
                .body(Full::new(Bytes::from(body)))
                .unwrap())
        }
        "/healthz" => Ok(Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "text/plain")
            .body(Full::new(Bytes::from("ok")))
            .unwrap()),
        _ => Ok(Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Full::new(Bytes::from("not found")))
            .unwrap()),
    }
}

fn stats_json(state: &DashboardState) -> String {
    let uptime = state.started.elapsed().as_secs();
    let (allows, rejects) = if let Some(rl) = &state.rate_limiter {
        (rl.allows(), rl.rejects())
    } else {
        (0, 0)
    };
    // Pull a few key metrics from the text gather (simple parse)
    let text = state.metrics.gather();
    let mut requests = 0u64;
    for line in text.lines() {
        if line.starts_with("oxigate_requests_total{") {
            if let Some(v) = line.split_whitespace().last() {
                if let Ok(n) = v.parse::<u64>() {
                    requests += n;
                }
            }
        }
    }
    format!(
        r#"{{
  "version": "{}",
  "uptime_secs": {},
  "requests_total": {},
  "rate_limit_allows": {},
  "rate_limit_rejects": {},
  "prometheus_path": "/metrics",
  "dashboard": "/dashboard"
}}"#,
        state.version, uptime, requests, allows, rejects
    )
}

fn html_response(body: &'static str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/html; charset=utf-8")
        .header("Cache-Control", "no-cache")
        .body(Full::new(Bytes::from(body)))
        .unwrap()
}

fn json_response(body: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .header("Cache-Control", "no-cache")
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap()
}

const DASHBOARD_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8"/>
<meta name="viewport" content="width=device-width, initial-scale=1"/>
<title>OxiGate Dashboard</title>
<style>
  :root { --bg:#0b1220; --card:#121a2b; --text:#e6eefc; --muted:#8aa0c0; --accent:#3b82f6; --ok:#22c55e; --bad:#ef4444; }
  * { box-sizing: border-box; }
  body { margin:0; font-family: ui-sans-serif, system-ui, -apple-system, Segoe UI, Roboto, sans-serif; background:var(--bg); color:var(--text); }
  header { padding:20px 24px; border-bottom:1px solid #1e293b; display:flex; justify-content:space-between; align-items:center; }
  h1 { margin:0; font-size:20px; letter-spacing:0.3px; }
  .badge { background:#1d4ed8; color:white; padding:4px 10px; border-radius:999px; font-size:12px; }
  main { padding:24px; display:grid; gap:16px; grid-template-columns: repeat(auto-fit, minmax(220px, 1fr)); }
  .card { background:var(--card); border:1px solid #1e293b; border-radius:12px; padding:16px; }
  .label { color:var(--muted); font-size:12px; text-transform:uppercase; letter-spacing:0.08em; }
  .value { font-size:28px; font-weight:700; margin-top:8px; }
  .sub { color:var(--muted); font-size:12px; margin-top:6px; }
  pre { background:#0a0f1a; border-radius:8px; padding:12px; overflow:auto; font-size:12px; color:#9ecbff; max-height:320px; }
  footer { padding:16px 24px; color:var(--muted); font-size:12px; }
  a { color:var(--accent); }
</style>
</head>
<body>
<header>
  <h1>OxiGate <span style="color:var(--muted);font-weight:400">Live Dashboard</span></h1>
  <span class="badge" id="status">loading…</span>
</header>
<main>
  <div class="card"><div class="label">Uptime</div><div class="value" id="uptime">—</div><div class="sub">seconds</div></div>
  <div class="card"><div class="label">Requests</div><div class="value" id="requests">—</div><div class="sub">total processed</div></div>
  <div class="card"><div class="label">Rate limit allows</div><div class="value" id="allows">—</div><div class="sub">passed</div></div>
  <div class="card"><div class="label">Rate limit rejects</div><div class="value" id="rejects">—</div><div class="sub" style="color:var(--bad)">blocked</div></div>
  <div class="card" style="grid-column: 1 / -1">
    <div class="label">Prometheus scrape</div>
    <div class="sub" style="margin:8px 0">Endpoint: <a href="/metrics">/metrics</a> · JSON: <a href="/api/stats">/api/stats</a> · Health: <a href="/healthz">/healthz</a></div>
    <pre id="prom">loading metrics…</pre>
  </div>
</main>
<footer>OxiGate · refresh every 2s · wire this into Grafana via Prometheus</footer>
<script>
async function refresh() {
  try {
    const s = await fetch('/api/stats').then(r => r.json());
    document.getElementById('status').textContent = 'live';
    document.getElementById('uptime').textContent = s.uptime_secs;
    document.getElementById('requests').textContent = s.requests_total;
    document.getElementById('allows').textContent = s.rate_limit_allows;
    document.getElementById('rejects').textContent = s.rate_limit_rejects;
    const m = await fetch('/metrics').then(r => r.text());
    document.getElementById('prom').textContent = m.split('\n').slice(0, 40).join('\n');
  } catch (e) {
    document.getElementById('status').textContent = 'error';
  }
}
refresh();
setInterval(refresh, 2000);
</script>
</body>
</html>
"#;
