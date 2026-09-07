use crate::lb::{LoadBalancer, UpstreamRuntime};
use crate::metrics::Metrics;
use crate::proxy::client::{boxed_body, HttpClient};
use http_body_util::Empty;
use hyper::{Request, StatusCode};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{debug, warn};

pub async fn run_health_checks(
    lbs: Vec<Arc<LoadBalancer>>,
    client: HttpClient,
    metrics: Arc<Metrics>,
    interval: Duration,
    path: String,
) {
    loop {
        for lb in &lbs {
            for upstream in lb.upstreams() {
                let healthy = check_one(&client, upstream, &path).await;
                let was_healthy = upstream.is_healthy();
                upstream.set_healthy(healthy);

                metrics
                    .upstream_health
                    .with_label_values(&[&upstream.address])
                    .set(if healthy { 1 } else { 0 });

                if was_healthy && !healthy {
                    warn!(upstream = %upstream.address, "upstream marked unhealthy");
                } else if !was_healthy && healthy {
                    warn!(upstream = %upstream.address, "upstream recovered");
                } else {
                    debug!(upstream = %upstream.address, healthy, "health check");
                }
            }
        }
        sleep(interval).await;
    }
}

async fn check_one(client: &HttpClient, upstream: &UpstreamRuntime, path: &str) -> bool {
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

    match tokio::time::timeout(Duration::from_secs(2), client.request(req)).await {
        Ok(Ok(resp)) => {
            let status = resp.status();
            status.is_success() || status.is_redirection() || status == StatusCode::NOT_FOUND
        }
        _ => false,
    }
}
