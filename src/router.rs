use crate::config::Config;
use crate::lb::LoadBalancer;
use std::sync::Arc;

/// One matched route: path prefix + its own load balancer.
pub struct Route {
    pub path_prefix: String,
    pub lb: Arc<LoadBalancer>,
}

/// Ordered route table (first match wins).
pub struct Router {
    routes: Vec<Route>,
    /// Fallback when no path-specific route matches.
    default: Option<Arc<LoadBalancer>>,
}

impl Router {
    pub fn from_config(config: &Config) -> Self {
        let mut routes = Vec::new();

        for r in &config.routes {
            let algo = r
                .load_balancing
                .clone()
                .unwrap_or_else(|| config.load_balancing.clone());
            routes.push(Route {
                path_prefix: normalize_prefix(&r.path_prefix),
                lb: Arc::new(LoadBalancer::from_upstreams(&r.upstreams, algo)),
            });
        }

        // Longer prefixes first for more specific matches
        routes.sort_by_key(|route| std::cmp::Reverse(route.path_prefix.len()));

        let default = if !config.upstreams.is_empty() {
            Some(Arc::new(LoadBalancer::from_config(config)))
        } else if routes.is_empty() {
            None
        } else {
            // If only routes exist, use the last (usually "/") as soft default
            None
        };

        Self { routes, default }
    }

    /// Resolve load balancer for a request path.
    pub fn resolve(&self, path: &str) -> Option<Arc<LoadBalancer>> {
        for route in &self.routes {
            if path_matches(path, &route.path_prefix) {
                return Some(route.lb.clone());
            }
        }
        self.default.clone()
    }

    /// All load balancers (for health checks).
    pub fn all_lbs(&self) -> Vec<Arc<LoadBalancer>> {
        let mut out: Vec<_> = self.routes.iter().map(|r| r.lb.clone()).collect();
        if let Some(d) = &self.default {
            out.push(d.clone());
        }
        out
    }
}

fn normalize_prefix(p: &str) -> String {
    if p.is_empty() {
        "/".to_string()
    } else {
        p.to_string()
    }
}

fn path_matches(path: &str, prefix: &str) -> bool {
    if prefix == "/" {
        return true;
    }
    // Exact match or boundary at '/' so /api does not match /apiary
    path == prefix || path.starts_with(&(prefix.trim_end_matches('/').to_string() + "/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, LoadBalancingAlgorithm, RouteConfig, Upstream};

    fn base_cfg() -> Config {
        Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            metrics_listen: "127.0.0.1:0".parse().unwrap(),
            upstreams: vec![Upstream {
                address: "http://127.0.0.1:8081".into(),
                weight: 1,
            }],
            routes: vec![RouteConfig {
                path_prefix: "/api".into(),
                upstreams: vec![Upstream {
                    address: "http://127.0.0.1:9001".into(),
                    weight: 1,
                }],
                load_balancing: Some(LoadBalancingAlgorithm::LeastConnections),
            }],
            load_balancing: LoadBalancingAlgorithm::RoundRobin,
            health_check: Default::default(),
            timeouts: Default::default(),
            tls: None,
            retries: 1,
            max_connections: 0,
            sticky: None,
            headers: Default::default(),
            access_log: false,
            rate_limit: None,
            proxy_protocol: false,
            upstream_http2: false,
            pool_max_idle_per_host: 8,
            max_body_bytes: 1024,
            retry_idempotent_only: true,
            force_retry_with_body: false,
            acl: None,
            security_headers: true,
            admin_token: None,
            proxy_protocol_trusted_cidrs: vec![],
        }
    }

    #[test]
    fn routes_api_prefix() {
        let r = Router::from_config(&base_cfg());
        let lb = r.resolve("/api/users").unwrap();
        assert_eq!(lb.upstreams()[0].address, "http://127.0.0.1:9001");
    }

    #[test]
    fn routes_default() {
        let r = Router::from_config(&base_cfg());
        let lb = r.resolve("/static/x").unwrap();
        assert_eq!(lb.upstreams()[0].address, "http://127.0.0.1:8081");
    }
}
