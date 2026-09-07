use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

pub mod validator;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub listen: SocketAddr,
    #[serde(default = "default_metrics_listen")]
    pub metrics_listen: SocketAddr,

    /// Default upstream pool (used when no `routes` match, or as sole pool).
    #[serde(default)]
    pub upstreams: Vec<Upstream>,

    /// Path-based routes. First match wins (longest prefix recommended first).
    #[serde(default)]
    pub routes: Vec<RouteConfig>,

    #[serde(default)]
    pub load_balancing: LoadBalancingAlgorithm,

    #[serde(default)]
    pub health_check: HealthCheckConfig,

    #[serde(default)]
    pub timeouts: TimeoutConfig,

    #[serde(default)]
    pub tls: Option<TlsConfig>,

    /// Automatic retries on upstream connection failure (not on 4xx/5xx from app).
    #[serde(default = "default_retries")]
    pub retries: u32,

    /// Global max concurrent in-flight requests (0 = unlimited).
    #[serde(default)]
    pub max_connections: usize,

    /// Sticky session (cookie affinity).
    #[serde(default)]
    pub sticky: Option<StickyConfig>,

    /// Extra headers to inject.
    #[serde(default)]
    pub headers: HeaderConfig,

    /// Emit structured JSON access logs (one line per request).
    #[serde(default = "default_true")]
    pub access_log: bool,

    /// Per-IP rate limiting (GCRA token bucket).
    #[serde(default)]
    pub rate_limit: Option<RateLimitConfigYaml>,

    /// Expect PROXY protocol v1/v2 on inbound connections.
    #[serde(default)]
    pub proxy_protocol: bool,

    /// Upstream HTTP client tuning (HTTP/2, pool sizes).
    #[serde(default)]
    pub upstream_http2: bool,

    #[serde(default = "default_pool_max_idle")]
    pub pool_max_idle_per_host: usize,

    /// Max request body size in bytes (default 16 MiB). Larger => 413.
    #[serde(default = "default_max_body")]
    pub max_body_bytes: usize,

    /// Retry only safe/idempotent methods (GET/HEAD/OPTIONS/PUT/DELETE) unless force_retry_body is set.
    #[serde(default = "default_true")]
    pub retry_idempotent_only: bool,

    /// Allow retry even with non-empty body for non-idempotent methods (dangerous).
    #[serde(default)]
    pub force_retry_with_body: bool,

    /// Optional IP allow/deny ACL (deny wins if both match).
    #[serde(default)]
    pub acl: Option<AclConfig>,

    /// Security response headers baseline.
    #[serde(default = "default_true")]
    pub security_headers: bool,

    /// Bearer token required for /dashboard, /api/stats, /metrics (empty = open, not recommended).
    #[serde(default)]
    pub admin_token: Option<String>,

    /// When proxy_protocol is true, only these source CIDRs may send PROXY headers.
    #[serde(default)]
    pub proxy_protocol_trusted_cidrs: Vec<String>,
}


fn default_max_body() -> usize {
    16 * 1024 * 1024
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AclConfig {
    /// CIDRs or single IPs allowed (empty = allow all unless deny matches).
    #[serde(default)]
    pub allow: Vec<String>,
    /// CIDRs or single IPs denied.
    #[serde(default)]
    pub deny: Vec<String>,
}


fn default_pool_max_idle() -> usize {
    64
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitConfigYaml {
    /// Requests per second per client IP.
    pub rps: u32,
    /// Burst multiplier (1.0 = no extra burst).
    #[serde(default = "default_burst")]
    pub burst: f64,
}

fn default_burst() -> f64 {
    1.5
}


fn default_metrics_listen() -> SocketAddr {
    "0.0.0.0:9090".parse().unwrap()
}
fn default_retries() -> u32 {
    1
}
fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteConfig {
    /// Path prefix to match (e.g. "/api"). Empty or "/" matches everything.
    pub path_prefix: String,
    pub upstreams: Vec<Upstream>,
    #[serde(default)]
    pub load_balancing: Option<LoadBalancingAlgorithm>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsConfig {
    pub cert: String,
    pub key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Upstream {
    pub address: String,
    #[serde(default = "default_weight")]
    pub weight: u32,
}

fn default_weight() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LoadBalancingAlgorithm {
    #[default]
    RoundRobin,
    WeightedRoundRobin,
    LeastConnections,
    PowerOfTwoChoices,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StickyConfig {
    #[serde(default = "default_cookie_name")]
    pub cookie_name: String,
    #[serde(default = "default_cookie_max_age")]
    pub max_age_secs: u64,
}

fn default_cookie_name() -> String {
    "OXIGATE".to_string()
}
fn default_cookie_max_age() -> u64 {
    3600
}

impl Default for StickyConfig {
    fn default() -> Self {
        Self {
            cookie_name: default_cookie_name(),
            max_age_secs: default_cookie_max_age(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HeaderConfig {
    /// Headers added to every proxied request.
    #[serde(default)]
    pub request_set: HashMap<String, String>,
    /// Headers added to every response.
    #[serde(default)]
    pub response_set: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCheckConfig {
    #[serde(default = "default_health_interval")]
    pub interval_secs: u64,
    #[serde(default = "default_health_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "default_health_path")]
    pub path: String,
    #[serde(default = "default_unhealthy_threshold")]
    pub unhealthy_threshold: u32,
    #[serde(default = "default_healthy_threshold")]
    pub healthy_threshold: u32,
}

impl Default for HealthCheckConfig {
    fn default() -> Self {
        Self {
            interval_secs: 5,
            timeout_secs: 2,
            path: "/".to_string(),
            unhealthy_threshold: 3,
            healthy_threshold: 2,
        }
    }
}

fn default_health_interval() -> u64 {
    5
}
fn default_health_timeout() -> u64 {
    2
}
fn default_health_path() -> String {
    "/".to_string()
}
fn default_unhealthy_threshold() -> u32 {
    3
}
fn default_healthy_threshold() -> u32 {
    2
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeoutConfig {
    #[serde(default = "default_connect_timeout")]
    pub connect_secs: u64,
    #[serde(default = "default_request_timeout")]
    pub request_secs: u64,
    #[serde(default = "default_idle_timeout")]
    pub idle_secs: u64,
}

impl Default for TimeoutConfig {
    fn default() -> Self {
        Self {
            connect_secs: 5,
            request_secs: 30,
            idle_secs: 60,
        }
    }
}

fn default_connect_timeout() -> u64 {
    5
}
fn default_request_timeout() -> u64 {
    30
}
fn default_idle_timeout() -> u64 {
    60
}

impl Config {
    pub fn from_file(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: Config = serde_yaml::from_str(&content)?;
        validator::validate(&config)?;
        Ok(config)
    }

    pub fn connect_timeout(&self) -> Duration {
        Duration::from_secs(self.timeouts.connect_secs)
    }

    pub fn request_timeout(&self) -> Duration {
        Duration::from_secs(self.timeouts.request_secs)
    }

    pub fn health_interval(&self) -> Duration {
        Duration::from_secs(self.health_check.interval_secs)
    }
}
