use crate::config::{HeaderConfig, StickyConfig};
use crate::lb::{LoadBalancer, UpstreamRuntime};
use crate::metrics::Metrics;
use crate::proxy::body_limit::{collect_limited, idle_wrap, BodyLimitError};
use crate::proxy::client::{boxed_body, HttpClient};
use crate::router::Router;
use crate::security::Acl;
use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::{HeaderName, HeaderValue, CONNECTION, HOST, SET_COOKIE, UPGRADE};
use hyper::{Request, Response, StatusCode, Uri};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

pub type ResponseBody = BoxBody<Bytes, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Clone)]
pub struct ProxyContext {
    pub router: Arc<Router>,
    pub client: HttpClient,
    pub metrics: Arc<Metrics>,
    pub retries: u32,
    pub request_timeout: Duration,
    pub sticky: Option<StickyConfig>,
    pub headers: HeaderConfig,
    pub access_log: bool,
    pub rate_limiter: Option<Arc<crate::ratelimit::RateLimiter>>,
    pub max_body_bytes: usize,
    pub retry_idempotent_only: bool,
    pub force_retry_with_body: bool,
    pub acl: Option<Acl>,
    pub security_headers: bool,
    /// Whether the client connection is TLS-terminated by us.
    pub client_is_tls: bool,
    /// Limits concurrent in-flight requests (HTTP/1.1 and HTTP/2 streams).
    pub request_limit: Option<std::sync::Arc<tokio::sync::Semaphore>>,
    pub idle_timeout: Duration,
}


pub async fn proxy_request(
    req: Request<Incoming>,
    _state: crate::state::AppState,
    ctx: Arc<ProxyContext>,
    remote_addr: SocketAddr,
) -> Result<Response<ResponseBody>, Infallible> {
    let start = Instant::now();
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let query = req.uri().query().map(|q| q.to_string());
    let is_upgrade = is_upgrade_request(&req);

    // Concurrent request / HTTP/2 stream limit (not just TCP accepts)
    let _request_permit = if let Some(ref sem) = ctx.request_limit {
        match sem.clone().try_acquire_owned() {
            Ok(p) => Some(p),
            Err(_) => {
                ctx.metrics
                    .requests_total
                    .with_label_values(&[method.as_str(), "503", "none"])
                    .inc();
                return Ok(error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Too Many Concurrent Requests",
                ));
            }
        }
    } else {
        None
    };

    if let Some(ref acl) = ctx.acl {
        if !acl.is_allowed(remote_addr.ip()) {
            ctx.metrics
                .requests_total
                .with_label_values(&[method.as_str(), "403", "none"])
                .inc();
            return Ok(error_response(StatusCode::FORBIDDEN, "Forbidden"));
        }
    }

    if let Some(ref rl) = ctx.rate_limiter {
        if !rl.check(remote_addr.ip()) {
            ctx.metrics.rate_limit_rejects.inc();
            ctx.metrics
                .requests_total
                .with_label_values(&[method.as_str(), "429", "none"])
                .inc();
            return Ok(error_response(StatusCode::TOO_MANY_REQUESTS, "Too Many Requests"));
        }
        ctx.metrics.rate_limit_allows.inc();
    }

    let lb = match ctx.router.resolve(&path) {
        Some(lb) => lb,
        None => {
            ctx.metrics
                .requests_total
                .with_label_values(&[method.as_str(), "404", "none"])
                .inc();
            return Ok(error_response(StatusCode::NOT_FOUND, "No route matched"));
        }
    };

    let sticky_val = ctx
        .sticky
        .as_ref()
        .and_then(|s| extract_cookie(req.headers(), &s.cookie_name));

    // Upgrade requests: stream without buffering; no retries (body is a tunnel).
    if is_upgrade {
        return Ok(proxy_upgrade(req, &lb, &ctx, remote_addr, start).await);
    }

    let (parts, body) = req.into_parts();

    // Bound memory: enforce limit while streaming frames into a buffer for retry support.
    let body_bytes = match collect_limited(body, ctx.max_body_bytes, ctx.idle_timeout).await {
        Ok(b) => b,
        Err(BodyLimitError::TooLarge) => {
            ctx.metrics
                .requests_total
                .with_label_values(&[method.as_str(), "413", "none"])
                .inc();
            return Ok(error_response(StatusCode::PAYLOAD_TOO_LARGE, "Payload Too Large"));
        }
        Err(BodyLimitError::IdleTimeout) => {
            ctx.metrics
                .requests_total
                .with_label_values(&[method.as_str(), "408", "none"])
                .inc();
            return Ok(error_response(StatusCode::REQUEST_TIMEOUT, "Request Body Idle Timeout"));
        }
        Err(BodyLimitError::Other(e)) => {
            error!(error = %e, "failed to read request body");
            return Ok(error_response(StatusCode::BAD_REQUEST, "Bad Request"));
        }
    };

    let may_retry = ctx.force_retry_with_body
        || body_bytes.is_empty()
        || !ctx.retry_idempotent_only
        || matches!(method.as_str(), "GET" | "HEAD" | "OPTIONS" | "PUT" | "DELETE");
    let max_attempts = if may_retry {
        ctx.retries.saturating_add(1)
    } else {
        1
    };

    let mut tried: Vec<String> = Vec::new();
    let mut last_error: Option<String> = None;

    for attempt in 0..max_attempts {
        let exclude: Vec<&str> = tried.iter().map(|s| s.as_str()).collect();
        let upstream = if attempt == 0 {
            lb.select_sticky(sticky_val.as_deref())
        } else {
            ctx.metrics.retries_total.inc();
            lb.select_excluding(&exclude)
        };
        let upstream = match upstream {
            Some(u) => u,
            None => break,
        };
        tried.push(upstream.address.clone());

        upstream.inc_connections();
        ctx.metrics
            .upstream_connections
            .with_label_values(&[&upstream.address])
            .inc();

        let result = attempt_proxy(
            &parts,
            body_bytes.clone(),
            &upstream,
            &ctx.client,
            &method,
            &path,
            query.as_deref(),
            remote_addr,
            &ctx.headers,
            ctx.request_timeout,
            ctx.idle_timeout,
            ctx.client_is_tls,
            false,
        )
        .await;

        upstream.dec_connections();
        ctx.metrics
            .upstream_connections
            .with_label_values(&[&upstream.address])
            .dec();

        match result {
            Ok(mut response) => {
                let status = response.status();
                let duration = start.elapsed().as_secs_f64();
                ctx.metrics
                    .requests_total
                    .with_label_values(&[method.as_str(), status.as_str(), &upstream.address])
                    .inc();
                ctx.metrics
                    .request_duration
                    .with_label_values(&[&upstream.address])
                    .observe(duration);

                if let Some(ref sticky) = ctx.sticky {
                    if let Ok(val) = HeaderValue::from_str(&format!(
                        "{}={}; Path=/; Max-Age={}; HttpOnly; SameSite=Lax",
                        sticky.cookie_name, upstream.address, sticky.max_age_secs
                    )) {
                        response.headers_mut().append(SET_COOKIE, val);
                    }
                }
                if ctx.security_headers {
                    crate::security::apply_security_headers(response.headers_mut());
                }
                for (k, v) in &ctx.headers.response_set {
                    if let (Ok(name), Ok(value)) = (
                        HeaderName::from_bytes(k.as_bytes()),
                        HeaderValue::from_str(v),
                    ) {
                        response.headers_mut().insert(name, value);
                    }
                }
                if ctx.access_log {
                    info!(
                        target: "access",
                        method = %method,
                        path = %path,
                        status = status.as_u16(),
                        upstream = %upstream.address,
                        duration_ms = %(duration * 1000.0) as u64,
                        remote = %remote_addr,
                        attempt = attempt + 1,
                        "request"
                    );
                }
                return Ok(response);
            }
            Err(e) => {
                last_error = Some(e);
                warn!(upstream = %upstream.address, attempt = attempt + 1, "upstream failed");
            }
        }
    }

    error!(path = %path, error = %last_error.unwrap_or_else(|| "no upstreams".into()), "all attempts failed");
    ctx.metrics
        .requests_total
        .with_label_values(&[method.as_str(), "502", "none"])
        .inc();
    Ok(error_response(StatusCode::BAD_GATEWAY, "Bad Gateway"))
}

async fn proxy_upgrade(
    req: Request<Incoming>,
    lb: &LoadBalancer,
    ctx: &ProxyContext,
    remote_addr: SocketAddr,
    start: Instant,
) -> Response<ResponseBody> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let query = req.uri().query().map(|q| q.to_string());
    let upstream = match lb.select() {
        Some(u) => u,
        None => return error_response(StatusCode::SERVICE_UNAVAILABLE, "No healthy upstreams"),
    };

    let (parts, body) = req.into_parts();
    // For upgrades we stream the body without full buffer.
    let body_bytes = match collect_limited(body, ctx.max_body_bytes, ctx.idle_timeout).await {
        Ok(b) => b,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "Bad Request"),
    };

    upstream.inc_connections();
    let result = attempt_proxy(
        &parts,
        body_bytes,
        &upstream,
        &ctx.client,
        &method,
        &path,
        query.as_deref(),
        remote_addr,
        &ctx.headers,
        ctx.request_timeout,
        ctx.idle_timeout,
        ctx.client_is_tls,
        true, // preserve upgrade headers
    )
    .await;
    upstream.dec_connections();

    match result {
        Ok(resp) => {
            let duration = start.elapsed().as_secs_f64();
            ctx.metrics
                .requests_total
                .with_label_values(&[method.as_str(), resp.status().as_str(), &upstream.address])
                .inc();
            ctx.metrics
                .request_duration
                .with_label_values(&[&upstream.address])
                .observe(duration);
            resp
        }
        Err(_) => error_response(StatusCode::BAD_GATEWAY, "Bad Gateway"),
    }
}

async fn attempt_proxy(
    parts: &http::request::Parts,
    body_bytes: Bytes,
    upstream: &UpstreamRuntime,
    client: &HttpClient,
    method: &hyper::Method,
    path: &str,
    query: Option<&str>,
    remote_addr: SocketAddr,
    headers_cfg: &HeaderConfig,
    request_timeout: Duration,
    idle_timeout: Duration,
    client_is_tls: bool,
    preserve_upgrade: bool,
) -> Result<Response<ResponseBody>, String> {
    let target_uri = build_target_uri(&upstream.address, path, query)
        .map_err(|e| format!("bad target uri: {e}"))?;

    let mut builder = Request::builder()
        .method(method.clone())
        .uri(target_uri)
        .version(parts.version);

    for (name, value) in parts.headers.iter() {
        if name == HOST {
            continue;
        }
        if preserve_upgrade {
            // Keep Connection / Upgrade for WebSocket
            if is_hop_by_hop_except_upgrade(name) {
                continue;
            }
        } else if is_hop_by_hop(name) {
            continue;
        }
        builder = builder.header(name, value);
    }

    // Never trust inbound X-Forwarded-For alone; set from real socket / PROXY.
    let client_ip = remote_addr.ip().to_string();
    builder = builder.header("X-Real-IP", &client_ip);
    builder = builder.header("X-Forwarded-For", &client_ip);
    builder = builder.header(
        "X-Forwarded-Proto",
        if client_is_tls { "https" } else { "http" },
    );

    for (k, v) in &headers_cfg.request_set {
        builder = builder.header(k.as_str(), v.as_str());
    }

    if let Some(host) = extract_host(&upstream.address) {
        if let Ok(hv) = HeaderValue::from_str(&host) {
            builder = builder.header(HOST, hv);
        }
    }

    let proxy_req = builder
        .body(boxed_body(Full::new(body_bytes)))
        .map_err(|e| format!("build request: {e}"))?;

    // Timeout until response headers (connect + TTFB). Body is then streamed;
    // idle clients are still bounded by TCP keepalive / process-level drain on shutdown.
    let response = tokio::time::timeout(request_timeout, client.request(proxy_req))
        .await
        .map_err(|_| "upstream request timed out".to_string())?
        .map_err(|e| format!("upstream error: {e}"))?;

    let (resp_parts, resp_body) = response.into_parts();

    let mut resp_builder = Response::builder()
        .status(resp_parts.status)
        .version(resp_parts.version);

    for (name, value) in resp_parts.headers.iter() {
        if preserve_upgrade {
            if is_hop_by_hop_except_upgrade(name) {
                continue;
            }
        } else if is_hop_by_hop(name) {
            continue;
        }
        resp_builder = resp_builder.header(name, value);
    }

    let streamed = idle_wrap(resp_body, idle_timeout)
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })
        .boxed();

    resp_builder
        .body(streamed)
        .map_err(|e| format!("build response: {e}"))
}

fn is_upgrade_request(req: &Request<Incoming>) -> bool {
    req.headers().contains_key(UPGRADE)
        || req
            .headers()
            .get(CONNECTION)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_ascii_lowercase().contains("upgrade"))
            .unwrap_or(false)
}

fn extract_cookie(headers: &hyper::HeaderMap, name: &str) -> Option<String> {
    let cookie = headers.get(hyper::header::COOKIE)?.to_str().ok()?;
    for part in cookie.split(';') {
        let part = part.trim();
        if let Some((k, v)) = part.split_once('=') {
            if k.trim() == name {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

fn build_target_uri(
    upstream_base: &str,
    path: &str,
    query: Option<&str>,
) -> Result<Uri, Box<dyn std::error::Error + Send + Sync>> {
    let base = upstream_base.trim_end_matches('/');
    let path = if path.is_empty() { "/" } else { path };
    let full = match query {
        Some(q) => format!("{base}{path}?{q}"),
        None => format!("{base}{path}"),
    };
    Ok(full.parse()?)
}

fn extract_host(upstream: &str) -> Option<String> {
    let without_scheme = upstream
        .strip_prefix("http://")
        .or_else(|| upstream.strip_prefix("https://"))?;
    Some(without_scheme.split('/').next()?.to_string())
}

fn is_hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailers"
            | "transfer-encoding"
            | "upgrade"
    )
}

fn is_hop_by_hop_except_upgrade(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailers"
            | "transfer-encoding"
    )
}

fn error_response(status: StatusCode, msg: &'static str) -> Response<ResponseBody> {
    let body = Full::new(Bytes::from(msg))
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })
        .boxed();
    Response::builder()
        .status(status)
        .header("Content-Type", "text/plain; charset=utf-8")
        .header("Connection", "close")
        .body(body)
        .unwrap()
}
