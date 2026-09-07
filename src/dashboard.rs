//! Admin dashboard + metrics (token reloadable via shared state).

use crate::metrics::Metrics;
use crate::ratelimit::RateLimiter;
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use std::convert::Infallible;
use std::sync::{Arc, RwLock};
use std::time::Instant;

pub struct DashboardState {
    pub metrics: Arc<Metrics>,
    pub rate_limiter: Arc<RwLock<Option<Arc<RateLimiter>>>>,
    pub started: Instant,
    pub version: &'static str,
    /// Reloadable admin token (None / empty = open; not recommended).
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
                .body(Full::new(Bytes::from("unauthorized")))
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
            .body(Full::new(Bytes::from("not found")))
            .unwrap()),
    }
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
        r#"{{"version":"{}","uptime_secs":{},"requests_total":{},"rate_limit_allows":{},"rate_limit_rejects":{}}}"#,
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
<html lang="en"><head><meta charset="utf-8"/><title>OxiGate</title>
<style>
body{font-family:system-ui;background:#0b1220;color:#e6eefc;margin:0;padding:24px}
.card{background:#121a2b;border-radius:12px;padding:16px;margin:8px 0}
.value{font-size:28px;font-weight:700}
.muted{color:#8aa0c0;font-size:12px}
pre{background:#0a0f1a;padding:12px;border-radius:8px;overflow:auto;max-height:300px;font-size:12px}
</style></head><body>
<h1>OxiGate Dashboard</h1>
<p class="muted">Pass <code>?token=YOUR_TOKEN</code> if admin_token is set</p>
<div class="card"><div class="muted">Uptime</div><div class="value" id="uptime">—</div></div>
<div class="card"><div class="muted">Requests</div><div class="value" id="requests">—</div></div>
<div class="card"><div class="muted">RL rejects</div><div class="value" id="rejects">—</div></div>
<pre id="prom">…</pre>
<script>
const q=new URLSearchParams(location.search);const tok=q.get('token');
const withTok=u=>tok?u+(u.includes('?')?'&':'?')+'token='+encodeURIComponent(tok):u;
async function refresh(){
  try{
    const s=await fetch(withTok('/api/stats')).then(r=>r.json());
    uptime.textContent=s.uptime_secs; requests.textContent=s.requests_total; rejects.textContent=s.rate_limit_rejects;
    prom.textContent=(await fetch(withTok('/metrics')).then(r=>r.text())).split('\n').slice(0,40).join('\n');
  }catch(e){uptime.textContent='auth/error'}
}
refresh(); setInterval(refresh,2000);
</script></body></html>"#;
