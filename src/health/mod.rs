use crate::config::HealthCheckConfig;
use crate::lb::{LoadBalancer, UpstreamRuntime};
use crate::metrics::Metrics;
use crate::proxy::client::{boxed_body, HttpClient};
use http_body_util::Empty;
use hyper::{Request, StatusCode};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::time::sleep;
use tracing::{debug, warn};

struct ThresholdState {
    fails: u32,
    successes: u32,
}

/// Reloadable health inputs (updated on SIGHUP).
pub struct HealthRuntime {
    pub client: HttpClient,
    pub config: HealthCheckConfig,
    pub lbs: Box<dyn Fn() -> Vec<Arc<LoadBalancer>> + Send + Sync>,
}

pub async fn run_health_checks(
    shared: Arc<RwLock<HealthRuntime>>,
    metrics: Arc<Metrics>,
) {
    let mut state: HashMap<String, ThresholdState> = HashMap::new();

    loop {
        let (client, health_cfg, lbs) = {
            let g = shared.read().unwrap();
            (g.client.clone(), g.config.clone(), (g.lbs)())
        };

        let interval = Duration::from_secs(health_cfg.interval_secs.max(1));
        let timeout = Duration::from_secs(health_cfg.timeout_secs.max(1));
        let path = health_cfg.path.clone();

        for lb in &lbs {
            for upstream in lb.upstreams() {
                let healthy_now = check_one(&client, upstream, &path, timeout).await;
                let entry = state
                    .entry(upstream.address.clone())
                    .or_insert(ThresholdState {
                        fails: 0,
                        successes: 0,
                    });

                if healthy_now {
                    entry.successes += 1;
                    entry.fails = 0;
                    if !upstream.is_healthy()
                        && entry.successes >= health_cfg.healthy_threshold.max(1)
                    {
                        upstream.set_healthy(true);
                        warn!(upstream = %upstream.address, "upstream recovered");
                    }
                } else {
                    entry.fails += 1;
                    entry.successes = 0;
                    if upstream.is_healthy()
                        && entry.fails >= health_cfg.unhealthy_threshold.max(1)
                    {
                        upstream.set_healthy(false);
                        warn!(upstream = %upstream.address, "upstream marked unhealthy");
                    }
                }

                metrics
                    .upstream_health
                    .with_label_values(&[&upstream.address])
                    .set(if upstream.is_healthy() { 1 } else { 0 });

                debug!(
                    upstream = %upstream.address,
                    healthy = upstream.is_healthy(),
                    "health check"
                );
            }
        }
        sleep(interval).await;
    }
}

async fn check_one(
    client: &HttpClient,
    upstream: &UpstreamRuntime,
    path: &str,
    timeout: Duration,
) -> bool {
    let url = format!(
        "{}{}",
        upstream.address.trim_end_matches('/'),
        if path.starts_with('/') {
            path.to_string()
        } else {
            format!("/{path}")
        }
    );

    let body = boxed_body(Empty::<bytes::Bytes>::new());
    let req = match Request::builder()
        .method("GET")
        .uri(&url)
        .header("User-Agent", "OxiGate-HealthCheck/0.1")
        .header("Connection", "close")
        .body(body)
    {
        Ok(r) => r,
        Err(_) => return false,
    };

    match tokio::time::timeout(timeout, client.request(req)).await {
        Ok(Ok(resp)) => {
            let status = resp.status();
            status.is_success() || status.is_redirection() || status == StatusCode::NOT_FOUND
        }
        _ => false,
    }
}
