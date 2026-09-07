use crate::metrics::Metrics;
use crate::ratelimit::RateLimiter;
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::header::{CONNECTION, SEC_WEBSOCKET_ACCEPT, SEC_WEBSOCKET_KEY, UPGRADE};
use hyper::{Request, Response, StatusCode};
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, RwLock};
use std::time::Instant;

pub struct DashboardState {
    pub metrics: Arc<Metrics>,
    pub rate_limiter: Arc<RwLock<Option<Arc<RateLimiter>>>>,
    pub started: Instant,
    pub version: &'static str,
    pub admin_token: Arc<RwLock<Option<String>>>,
}

pub async fn handle_admin(
    req: Request<Incoming>,
    state: Arc<DashboardState>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let path = req.uri().path();

    if path == "/healthz" {
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "text/plain")
            .body(Full::new(Bytes::from("ok")))
            .unwrap());
    }

    let token = state.admin_token.read().unwrap().clone();
    if let Some(ref token) = token {
        if !token.is_empty() && !authorized(&req, token) {
            return Ok(Response::builder()
                .status(StatusCode::UNAUTHORIZED)
                .header("WWW-Authenticate", "Bearer")
                .header("Content-Type", "application/json")
                .body(Full::new(Bytes::from(
                    r#"{"error":"unauthorized","hint":"Authorization: Bearer <token> or ?token="}"#,
                )))
                .unwrap());
        }
    }

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
        _ => Ok(Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header("Content-Type", "application/json")
            .body(Full::new(Bytes::from(r#"{"error":"not_found"}"#)))
            .unwrap()),
    }
}

pub fn handle_dashboard_ws(
    req: Request<Incoming>,
    state: Arc<DashboardState>,
) -> Response<Full<Bytes>> {
    let token = state.admin_token.read().unwrap().clone();
    if let Some(ref token) = token {
        if !token.is_empty() && !authorized(&req, token) {
            return Response::builder()
                .status(StatusCode::UNAUTHORIZED)
                .header("WWW-Authenticate", "Bearer")
                .body(Full::new(Bytes::from("unauthorized")))
                .unwrap();
        }
    }

    let key = match req.headers().get(SEC_WEBSOCKET_KEY) {
        Some(key) => key.as_bytes(),
        None => return error_response(StatusCode::BAD_REQUEST, "missing websocket key"),
    };
    let accept = tokio_tungstenite::tungstenite::handshake::derive_accept_key(key);
    let on_upgrade = hyper::upgrade::on(req);
    tokio::spawn(async move {
        let upgraded = match on_upgrade.await {
            Ok(stream) => stream,
            Err(_) => return,
        };
        let io = hyper_util::rt::TokioIo::new(upgraded);
        let mut socket = tokio_tungstenite::WebSocketStream::from_raw_socket(
            io,
            tokio_tungstenite::tungstenite::protocol::Role::Server,
            None,
        )
        .await;
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            tokio::select! {
              _ = ticker.tick() => {
                if socket.send(tokio_tungstenite::tungstenite::Message::Text(stats_json(&state).into())).await.is_err() {
                  break;
                }
              }
              message = socket.next() => {
                match message {
                  Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))) | None => break,
                  Some(Err(_)) => break,
                  _ => {}
                }
              }
            }
        }
        let _ = socket.close(None).await;
    });

    Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(CONNECTION, "Upgrade")
        .header(UPGRADE, "websocket")
        .header(SEC_WEBSOCKET_ACCEPT, accept)
        .body(Full::new(Bytes::new()))
        .unwrap()
}

fn error_response(status: StatusCode, message: &'static str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("Content-Type", "text/plain; charset=utf-8")
        .body(Full::new(Bytes::from(message)))
        .unwrap()
}

fn authorized(req: &Request<Incoming>, token: &str) -> bool {
    if let Some(h) = req.headers().get(hyper::header::AUTHORIZATION) {
        if let Ok(s) = h.to_str() {
            if let Some(rest) = s.strip_prefix("Bearer ") {
                return rest == token;
            }
        }
    }
    if let Some(q) = req.uri().query() {
        for part in q.split('&') {
            if let Some(v) = part.strip_prefix("token=") {
                return v == token;
            }
        }
    }
    false
}

fn stats_json(state: &DashboardState) -> String {
    let uptime = state.started.elapsed().as_secs();
    let (allows, rejects) = {
        let rl = state.rate_limiter.read().unwrap();
        if let Some(ref rl) = *rl {
            (rl.allows(), rl.rejects())
        } else {
            (0, 0)
        }
    };

    let text = state.metrics.gather();
    let mut requests_total: u64 = 0;
    let mut retries: u64 = 0;
    let mut active: i64 = 0;
    let mut by_status: HashMap<String, u64> = HashMap::new();
    let mut by_method: HashMap<String, u64> = HashMap::new();
    let mut upstreams: HashMap<String, UpstreamSnap> = HashMap::new();

    for line in text.lines() {
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        let (name_labels, value_s) = match line.rsplit_once(' ') {
            Some(p) => p,
            None => continue,
        };
        let value_f: f64 = value_s.parse().unwrap_or(0.0);
        let value_u = value_f as u64;

        if name_labels.starts_with("oxigate_requests_total{") {
            requests_total += value_u;
            if let Some(status) = label(name_labels, "status") {
                *by_status.entry(status).or_insert(0) += value_u;
            }
            if let Some(method) = label(name_labels, "method") {
                *by_method.entry(method).or_insert(0) += value_u;
            }
            if let Some(up) = label(name_labels, "upstream") {
                if up != "none" {
                    upstreams.entry(up).or_default().requests += value_u;
                }
            }
        } else if name_labels.starts_with("oxigate_upstream_health{") {
            if let Some(up) = label(name_labels, "upstream") {
                upstreams.entry(up).or_default().healthy = value_f >= 1.0;
            }
        } else if name_labels.starts_with("oxigate_upstream_connections{") {
            if let Some(up) = label(name_labels, "upstream") {
                upstreams.entry(up).or_default().connections = value_f as i64;
            }
        } else if name_labels == "oxigate_active_connections" {
            active = value_f as i64;
        } else if name_labels == "oxigate_retries_total" {
            retries = value_u;
        } else if name_labels.starts_with("oxigate_rate_limit_rejects_total") {
            // prefer counter from metrics if present
        }
    }

    // Build upstreams JSON array
    let mut up_json = String::from("[");
    for (i, (addr, s)) in upstreams.iter().enumerate() {
        if i > 0 {
            up_json.push(',');
        }
        up_json.push_str(&format!(
            r#"{{"address":{},"healthy":{},"connections":{},"requests":{}}}"#,
            json_str(addr),
            s.healthy,
            s.connections,
            s.requests
        ));
    }
    up_json.push(']');

    let status_json = map_to_json_obj(&by_status);
    let method_json = map_to_json_obj(&by_method);

    format!(
        r#"{{"version":{ver},"uptime_secs":{up},"requests_total":{req},"active_connections":{act},"retries_total":{ret},"rate_limit_allows":{ra},"rate_limit_rejects":{rr},"by_status":{st},"by_method":{me},"upstreams":{ups}}}"#,
        ver = json_str(state.version),
        up = uptime,
        req = requests_total,
        act = active,
        ret = retries,
        ra = allows,
        rr = rejects,
        st = status_json,
        me = method_json,
        ups = up_json,
    )
}

#[derive(Default)]
struct UpstreamSnap {
    healthy: bool,
    connections: i64,
    requests: u64,
}

fn label(name_labels: &str, key: &str) -> Option<String> {
    let needle = format!(r#"{key}=""#);
    let idx = name_labels.find(&needle)?;
    let rest = &name_labels[idx + needle.len()..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn map_to_json_obj(m: &HashMap<String, u64>) -> String {
    let mut s = String::from("{");
    for (i, (k, v)) in m.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!("{}:{}", json_str(k), v));
    }
    s.push('}');
    s
}

fn json_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
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
        .header("Content-Type", "application/json; charset=utf-8")
        .header("Cache-Control", "no-cache")
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap()
}

const DASHBOARD_HTML: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <meta name="color-scheme" content="dark" />
  <title>OxiGate · Dashboard</title>
  <style>
    :root {
      --bg: #070b14;
      --bg-elevated: #0e1524;
      --bg-card: #121a2b;
      --bg-card-hover: #172033;
      --border: rgba(148, 163, 184, 0.12);
      --text: #e8eef9;
      --text-muted: #8b9bb4;
      --text-dim: #5c6b84;
      --accent: #38bdf8;
      --accent-soft: rgba(56, 189, 248, 0.12);
      --ok: #34d399;
      --ok-soft: rgba(52, 211, 153, 0.14);
      --warn: #fbbf24;
      --warn-soft: rgba(251, 191, 36, 0.14);
      --bad: #f87171;
      --bad-soft: rgba(248, 113, 113, 0.14);
      --radius: 14px;
      --shadow: 0 8px 32px rgba(0, 0, 0, 0.35);
      --font: "Inter", ui-sans-serif, system-ui, -apple-system, "Segoe UI", Roboto, sans-serif;
      --mono: ui-monospace, "SF Mono", "Cascadia Code", Menlo, monospace;
    }

    * { box-sizing: border-box; }
    html { -webkit-text-size-adjust: 100%; }
    body {
      margin: 0;
      min-height: 100vh;
      font-family: var(--font);
      background:
        radial-gradient(1200px 600px at 10% -10%, rgba(56, 189, 248, 0.08), transparent 55%),
        radial-gradient(900px 500px at 90% 0%, rgba(52, 211, 153, 0.05), transparent 50%),
        var(--bg);
      color: var(--text);
      line-height: 1.45;
    }

    a { color: var(--accent); text-decoration: none; }
    a:hover { text-decoration: underline; }

    .shell {
      max-width: 1120px;
      margin: 0 auto;
      padding: 28px 20px 48px;
    }

    /* Header */
    header.top {
      display: flex;
      flex-wrap: wrap;
      align-items: center;
      justify-content: space-between;
      gap: 16px;
      margin-bottom: 28px;
    }
    .brand {
      display: flex;
      align-items: center;
      gap: 12px;
    }
    .logo {
      width: 40px; height: 40px;
      border-radius: 11px;
      background: linear-gradient(135deg, #0ea5e9, #22d3ee 55%, #34d399);
      display: grid; place-items: center;
      font-weight: 800; font-size: 15px; color: #042f2e;
      box-shadow: 0 0 0 1px rgba(255,255,255,0.08), var(--shadow);
    }
    .brand h1 {
      margin: 0;
      font-size: 1.25rem;
      font-weight: 700;
      letter-spacing: -0.02em;
    }
    .brand .sub {
      margin: 2px 0 0;
      font-size: 0.8rem;
      color: var(--text-muted);
    }

    .header-meta {
      display: flex;
      flex-wrap: wrap;
      align-items: center;
      gap: 10px;
    }
    .pill {
      display: inline-flex;
      align-items: center;
      gap: 6px;
      padding: 6px 12px;
      border-radius: 999px;
      font-size: 0.78rem;
      font-weight: 600;
      border: 1px solid var(--border);
      background: var(--bg-elevated);
      color: var(--text-muted);
    }
    .pill .dot {
      width: 7px; height: 7px;
      border-radius: 50%;
      background: var(--text-dim);
    }
    .pill.live .dot {
      background: var(--ok);
      box-shadow: 0 0 0 3px var(--ok-soft);
      animation: pulse 2s ease infinite;
    }
    .pill.error .dot { background: var(--bad); box-shadow: 0 0 0 3px var(--bad-soft); }
    @keyframes pulse {
      0%, 100% { opacity: 1; }
      50% { opacity: 0.55; }
    }

    /* KPI grid */
    .kpis {
      display: grid;
      grid-template-columns: repeat(auto-fit, minmax(150px, 1fr));
      gap: 12px;
      margin-bottom: 20px;
    }
    .kpi {
      background: var(--bg-card);
      border: 1px solid var(--border);
      border-radius: var(--radius);
      padding: 16px 18px;
      transition: background 0.15s ease, border-color 0.15s ease;
    }
    .kpi:hover { background: var(--bg-card-hover); }
    .kpi .label {
      font-size: 0.72rem;
      font-weight: 600;
      text-transform: uppercase;
      letter-spacing: 0.06em;
      color: var(--text-muted);
      margin-bottom: 8px;
    }
    .kpi .value {
      font-size: 1.65rem;
      font-weight: 700;
      letter-spacing: -0.03em;
      font-variant-numeric: tabular-nums;
      line-height: 1.1;
    }
    .kpi .hint {
      margin-top: 6px;
      font-size: 0.75rem;
      color: var(--text-dim);
    }
    .kpi.warn .value { color: var(--warn); }
    .kpi.bad .value { color: var(--bad); }
    .kpi.ok .value { color: var(--ok); }

    /* Panels */
    .grid-2 {
      display: grid;
      grid-template-columns: 1fr 1fr;
      gap: 16px;
      margin-bottom: 16px;
    }
    @media (max-width: 768px) {
      .grid-2 { grid-template-columns: 1fr; }
    }

    .panel {
      background: var(--bg-card);
      border: 1px solid var(--border);
      border-radius: var(--radius);
      overflow: hidden;
    }
    .panel-head {
      display: flex;
      align-items: center;
      justify-content: space-between;
      padding: 14px 18px;
      border-bottom: 1px solid var(--border);
    }
    .panel-head h2 {
      margin: 0;
      font-size: 0.9rem;
      font-weight: 600;
    }
    .panel-body { padding: 8px 0; }

    /* Table */
    table {
      width: 100%;
      border-collapse: collapse;
      font-size: 0.85rem;
    }
    th, td {
      text-align: left;
      padding: 10px 18px;
      border-bottom: 1px solid var(--border);
    }
    th {
      font-size: 0.7rem;
      text-transform: uppercase;
      letter-spacing: 0.05em;
      color: var(--text-muted);
      font-weight: 600;
    }
    tr:last-child td { border-bottom: 0; }
    td.mono { font-family: var(--mono); font-size: 0.8rem; }
    .badge {
      display: inline-flex;
      align-items: center;
      gap: 5px;
      padding: 3px 9px;
      border-radius: 999px;
      font-size: 0.72rem;
      font-weight: 600;
    }
    .badge.ok { background: var(--ok-soft); color: var(--ok); }
    .badge.bad { background: var(--bad-soft); color: var(--bad); }
    .badge .dot { width: 6px; height: 6px; border-radius: 50%; background: currentColor; }

    /* Bars for status / method */
    .bar-list { padding: 4px 18px 14px; }
    .bar-row {
      display: grid;
      grid-template-columns: 64px 1fr 52px;
      align-items: center;
      gap: 10px;
      margin: 8px 0;
      font-size: 0.8rem;
    }
    .bar-row .key { color: var(--text-muted); font-family: var(--mono); font-size: 0.75rem; }
    .bar-row .val { text-align: right; font-variant-numeric: tabular-nums; color: var(--text-muted); }
    .track {
      height: 7px;
      background: rgba(255,255,255,0.05);
      border-radius: 99px;
      overflow: hidden;
    }
    .fill {
      height: 100%;
      border-radius: 99px;
      background: linear-gradient(90deg, var(--accent), #22d3ee);
      min-width: 2px;
      transition: width 0.4s ease;
    }
    .fill.s2xx { background: linear-gradient(90deg, #34d399, #6ee7b7); }
    .fill.s3xx { background: linear-gradient(90deg, #38bdf8, #7dd3fc); }
    .fill.s4xx { background: linear-gradient(90deg, #fbbf24, #fde68a); }
    .fill.s5xx { background: linear-gradient(90deg, #f87171, #fca5a5); }

    .empty {
      padding: 28px 18px;
      text-align: center;
      color: var(--text-dim);
      font-size: 0.85rem;
    }

    /* Alert banner */
    .banner {
      display: none;
      margin-bottom: 16px;
      padding: 12px 16px;
      border-radius: var(--radius);
      border: 1px solid var(--border);
      font-size: 0.85rem;
    }
    .banner.show { display: block; }
    .banner.error {
      background: var(--bad-soft);
      border-color: rgba(248, 113, 113, 0.35);
      color: #fecaca;
    }
    .banner code {
      font-family: var(--mono);
      font-size: 0.8rem;
      background: rgba(0,0,0,0.25);
      padding: 2px 6px;
      border-radius: 4px;
    }

    footer {
      margin-top: 28px;
      padding-top: 16px;
      border-top: 1px solid var(--border);
      display: flex;
      flex-wrap: wrap;
      gap: 12px 20px;
      justify-content: space-between;
      font-size: 0.78rem;
      color: var(--text-dim);
    }

    .sr-only {
      position: absolute; width: 1px; height: 1px;
      padding: 0; margin: -1px; overflow: hidden;
      clip: rect(0,0,0,0); border: 0;
    }

    /* Skeleton */
    .skel {
      background: linear-gradient(90deg, rgba(255,255,255,0.04), rgba(255,255,255,0.08), rgba(255,255,255,0.04));
      background-size: 200% 100%;
      animation: shimmer 1.2s infinite;
      border-radius: 6px;
      color: transparent !important;
    }
    @keyframes shimmer {
      0% { background-position: 200% 0; }
      100% { background-position: -200% 0; }
    }
  </style>
</head>
<body>
  <div class="shell">
    <header class="top">
      <div class="brand">
        <div class="logo" aria-hidden="true">Ox</div>
        <div>
          <h1>OxiGate</h1>
          <p class="sub">Reverse proxy · live control plane</p>
        </div>
      </div>
      <div class="header-meta">
        <span class="pill" id="statusPill" role="status">
          <span class="dot" aria-hidden="true"></span>
          <span id="statusText">Connecting…</span>
        </span>
        <span class="pill" id="versionPill">v—</span>
        <span class="pill" id="refreshPill">Auto-refresh 2s</span>
      </div>
    </header>

    <div id="banner" class="banner error" role="alert"></div>

    <section class="kpis" aria-label="Key metrics">
      <article class="kpi">
        <div class="label">Requests</div>
        <div class="value skel" id="kpiRequests">0000</div>
        <div class="hint" id="kpiRequestsHint">total processed</div>
      </article>
      <article class="kpi">
        <div class="label">Active conns</div>
        <div class="value skel" id="kpiActive">00</div>
        <div class="hint">client TCP connections</div>
      </article>
      <article class="kpi">
        <div class="label">Uptime</div>
        <div class="value skel" id="kpiUptime">0s</div>
        <div class="hint" id="kpiUptimeHint">since process start</div>
      </article>
      <article class="kpi" id="kpiRetriesCard">
        <div class="label">Retries</div>
        <div class="value skel" id="kpiRetries">0</div>
        <div class="hint">upstream retry attempts</div>
      </article>
      <article class="kpi" id="kpiRlCard">
        <div class="label">Rate-limit rejects</div>
        <div class="value skel" id="kpiRejects">0</div>
        <div class="hint" id="kpiRlHint">allowed: —</div>
      </article>
    </section>

    <div class="grid-2">
      <section class="panel" aria-labelledby="upTitle">
        <div class="panel-head">
          <h2 id="upTitle">Upstreams</h2>
          <span class="pill" id="upCount">0</span>
        </div>
        <div class="panel-body" id="upstreamsBody">
          <div class="empty">Loading upstreams…</div>
        </div>
      </section>

      <section class="panel" aria-labelledby="stTitle">
        <div class="panel-head">
          <h2 id="stTitle">Responses by status</h2>
        </div>
        <div class="bar-list" id="statusBars">
          <div class="empty">No traffic yet</div>
        </div>
      </section>
    </div>

    <div class="grid-2">
      <section class="panel" aria-labelledby="meTitle">
        <div class="panel-head">
          <h2 id="meTitle">Requests by method</h2>
        </div>
        <div class="bar-list" id="methodBars">
          <div class="empty">No traffic yet</div>
        </div>
      </section>

      <section class="panel" aria-labelledby="helpTitle">
        <div class="panel-head">
          <h2 id="helpTitle">Quick links</h2>
        </div>
        <div class="panel-body" style="padding:16px 18px;font-size:0.85rem;color:var(--text-muted);line-height:1.7">
          <div><a id="linkMetrics" href="/metrics">Prometheus metrics</a> · scrape target</div>
          <div><a id="linkStats" href="/api/stats">JSON stats API</a> · machine readable</div>
          <div><a href="/healthz">Health check</a> · always public</div>
          <div style="margin-top:10px;color:var(--text-dim);font-size:0.78rem">
            Auth: <code style="font-family:var(--mono)">Authorization: Bearer &lt;token&gt;</code>
            or <code style="font-family:var(--mono)">?token=</code>
          </div>
        </div>
      </section>
    </div>

    <footer>
      <span>OxiGate dashboard · data from in-process Prometheus registry</span>
      <span id="lastUpdated">Last update: —</span>
    </footer>
  </div>

  <script>
  (function () {
    "use strict";

    const params = new URLSearchParams(location.search);
    const token = params.get("token");

    function withAuth(url) {
      if (!token) return url;
      const u = new URL(url, location.origin);
      u.searchParams.set("token", token);
      return u.pathname + u.search;
    }

    // Preserve token on internal links
    ["linkMetrics", "linkStats"].forEach((id) => {
      const el = document.getElementById(id);
      if (el) el.href = withAuth(el.getAttribute("href"));
    });

    const $ = (id) => document.getElementById(id);

    function fmtNum(n) {
      n = Number(n) || 0;
      if (n >= 1_000_000) return (n / 1_000_000).toFixed(1).replace(/\.0$/, "") + "M";
      if (n >= 10_000) return (n / 1_000).toFixed(1).replace(/\.0$/, "") + "k";
      return n.toLocaleString();
    }

    function fmtUptime(sec) {
      sec = Number(sec) || 0;
      const d = Math.floor(sec / 86400);
      const h = Math.floor((sec % 86400) / 3600);
      const m = Math.floor((sec % 3600) / 60);
      const s = sec % 60;
      if (d > 0) return d + "d " + h + "h";
      if (h > 0) return h + "h " + m + "m";
      if (m > 0) return m + "m " + s + "s";
      return s + "s";
    }

    function statusClass(code) {
      const c = String(code);
      if (c.startsWith("2")) return "s2xx";
      if (c.startsWith("3")) return "s3xx";
      if (c.startsWith("4")) return "s4xx";
      if (c.startsWith("5")) return "s5xx";
      return "";
    }

    function renderBars(el, obj) {
      const entries = Object.entries(obj || {}).sort((a, b) => b[1] - a[1]);
      if (!entries.length) {
        el.innerHTML = '<div class="empty">No traffic yet</div>';
        return;
      }
      const max = Math.max(...entries.map(([, v]) => v), 1);
      el.innerHTML = entries.map(([k, v]) => {
        const pct = Math.max(2, Math.round((v / max) * 100));
        const cls = statusClass(k);
        return (
          '<div class="bar-row">' +
            '<span class="key">' + escapeHtml(k) + '</span>' +
            '<div class="track"><div class="fill ' + cls + '" style="width:' + pct + '%"></div></div>' +
            '<span class="val">' + fmtNum(v) + '</span>' +
          '</div>'
        );
      }).join("");
    }

    function renderUpstreams(list) {
      const body = $("upstreamsBody");
      $("upCount").textContent = String((list || []).length);
      if (!list || !list.length) {
        body.innerHTML = '<div class="empty">No upstream metrics yet — send traffic first</div>';
        return;
      }
      const rows = list
        .slice()
        .sort((a, b) => (b.requests || 0) - (a.requests || 0))
        .map((u) => {
          const ok = !!u.healthy;
          return (
            "<tr>" +
              '<td class="mono">' + escapeHtml(u.address || "—") + "</td>" +
              "<td><span class=\"badge " + (ok ? "ok" : "bad") + "\">" +
                '<span class="dot"></span>' + (ok ? "healthy" : "down") +
              "</span></td>" +
              "<td>" + fmtNum(u.connections || 0) + "</td>" +
              "<td>" + fmtNum(u.requests || 0) + "</td>" +
            "</tr>"
          );
        })
        .join("");
      body.innerHTML =
        "<table>" +
          "<thead><tr><th>Address</th><th>Health</th><th>Conns</th><th>Reqs</th></tr></thead>" +
          "<tbody>" + rows + "</tbody>" +
        "</table>";
    }

    function escapeHtml(s) {
      return String(s)
        .replace(/&/g, "&amp;")
        .replace(/</g, "&lt;")
        .replace(/>/g, "&gt;")
        .replace(/"/g, "&quot;");
    }

    function setLive(ok, msg) {
      const pill = $("statusPill");
      const text = $("statusText");
      pill.classList.toggle("live", ok);
      pill.classList.toggle("error", !ok);
      text.textContent = msg;
    }

    function showBanner(html) {
      const b = $("banner");
      if (!html) {
        b.classList.remove("show");
        b.innerHTML = "";
        return;
      }
      b.innerHTML = html;
      b.classList.add("show");
    }

    function applyStats(s) {
      showBanner("");
      setLive(true, "Live");
      document.querySelectorAll(".skel").forEach((el) => el.classList.remove("skel"));
      $("kpiRequests").textContent = fmtNum(s.requests_total);
      $("kpiActive").textContent = fmtNum(s.active_connections);
      $("kpiUptime").textContent = fmtUptime(s.uptime_secs);
      $("kpiRetries").textContent = fmtNum(s.retries_total);
      $("kpiRejects").textContent = fmtNum(s.rate_limit_rejects);
      $("kpiRlHint").textContent = "allowed: " + fmtNum(s.rate_limit_allows);
      $("kpiRlCard").classList.toggle("warn", (Number(s.rate_limit_rejects) || 0) > 0);
      $("kpiRetriesCard").classList.toggle("warn", (Number(s.retries_total) || 0) > 0);
      $("versionPill").textContent = "v" + (s.version || "?");
      renderUpstreams(s.upstreams || []);
      renderBars($("statusBars"), s.by_status || {});
      renderBars($("methodBars"), s.by_method || {});
      $("lastUpdated").textContent = "Last update: " + new Date().toLocaleTimeString();
    }

    async function refreshFallback() {
      try {
        const res = await fetch(withAuth("/api/stats"), {
          headers: { Accept: "application/json" },
          cache: "no-store",
        });
        if (res.status === 401) {
          setLive(false, "Unauthorized");
          showBanner("Admin token required. Open with <code>?token=YOUR_TOKEN</code>.");
          return;
        }
        if (!res.ok) throw new Error("HTTP " + res.status);
        applyStats(await res.json());
      } catch (err) {
        setLive(false, "Offline");
        showBanner("Cannot reach dashboard: " + escapeHtml(err.message || String(err)));
      }
    }

    function connectWebSocket() {
      const url = new URL("/ws", window.location.href);
      url.protocol = window.location.protocol === "https:" ? "wss:" : "ws:";
      const token = new URLSearchParams(window.location.search).get("token");
      if (token) url.searchParams.set("token", token);
      const socket = new WebSocket(url);
      socket.onopen = () => setLive(true, "Live");
      socket.onmessage = (event) => {
        try { applyStats(JSON.parse(event.data)); }
        catch (_) { setLive(false, "Bad data"); }
      };
      socket.onerror = () => socket.close();
      socket.onclose = () => {
        setLive(false, "Reconnecting");
        setTimeout(connectWebSocket, 2000);
      };
    }

    refreshFallback();
    connectWebSocket();
  })();
  </script>
</body>
</html>
"##;
