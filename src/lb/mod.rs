pub mod least_conn;
pub mod round_robin;

use crate::config::{Config, LoadBalancingAlgorithm, Upstream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Runtime representation of an upstream with health + connection tracking.
#[derive(Debug)]
pub struct UpstreamRuntime {
    pub address: String,
    pub weight: u32,
    pub healthy: std::sync::atomic::AtomicBool,
    pub active_connections: AtomicUsize,
}

impl UpstreamRuntime {
    pub fn new(upstream: &Upstream) -> Self {
        Self {
            address: upstream.address.clone(),
            weight: upstream.weight,
            healthy: std::sync::atomic::AtomicBool::new(true),
            active_connections: AtomicUsize::new(0),
        }
    }

    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }

    pub fn set_healthy(&self, healthy: bool) {
        self.healthy.store(healthy, Ordering::Relaxed);
    }

    pub fn inc_connections(&self) {
        self.active_connections.fetch_add(1, Ordering::Relaxed);
    }

    pub fn dec_connections(&self) {
        self.active_connections.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn connections(&self) -> usize {
        self.active_connections.load(Ordering::Relaxed)
    }
}

/// Load balancer that selects an upstream according to the configured algorithm.
pub struct LoadBalancer {
    upstreams: Vec<Arc<UpstreamRuntime>>,
    algorithm: LoadBalancingAlgorithm,
    rr_counter: AtomicUsize,
}

impl LoadBalancer {
    pub fn from_upstreams(upstreams: &[Upstream], algorithm: LoadBalancingAlgorithm) -> Self {
        let upstreams = upstreams
            .iter()
            .map(|u| Arc::new(UpstreamRuntime::new(u)))
            .collect();
        Self {
            upstreams,
            algorithm,
            rr_counter: AtomicUsize::new(0),
        }
    }

    pub fn from_config(config: &Config) -> Self {
        Self::from_upstreams(&config.upstreams, config.load_balancing.clone())
    }

    pub fn upstreams(&self) -> &[Arc<UpstreamRuntime>] {
        &self.upstreams
    }

    /// Prefer sticky upstream if cookie matches a healthy backend address.
    pub fn select_sticky(&self, sticky_value: Option<&str>) -> Option<Arc<UpstreamRuntime>> {
        if let Some(val) = sticky_value {
            if let Some(u) = self
                .upstreams
                .iter()
                .find(|u| u.is_healthy() && u.address == val)
            {
                return Some(u.clone());
            }
        }
        self.select_excluding(&[])
    }

    /// Select next healthy upstream, optionally skipping some addresses (retries).
    pub fn select_excluding(&self, exclude: &[&str]) -> Option<Arc<UpstreamRuntime>> {
        let healthy: Vec<_> = self
            .upstreams
            .iter()
            .filter(|u| u.is_healthy() && !exclude.iter().any(|e| *e == u.address))
            .cloned()
            .collect();

        if healthy.is_empty() {
            return None;
        }

        match self.algorithm {
            LoadBalancingAlgorithm::RoundRobin => {
                let idx = self.rr_counter.fetch_add(1, Ordering::Relaxed) % healthy.len();
                Some(healthy[idx].clone())
            }
            LoadBalancingAlgorithm::WeightedRoundRobin => {
                let mut expanded = Vec::with_capacity(healthy.len() * 4);
                for u in &healthy {
                    for _ in 0..u.weight.max(1) {
                        expanded.push(u.clone());
                    }
                }
                if expanded.is_empty() {
                    return None;
                }
                let idx = self.rr_counter.fetch_add(1, Ordering::Relaxed) % expanded.len();
                Some(expanded[idx].clone())
            }
            LoadBalancingAlgorithm::LeastConnections => least_conn::select(&healthy),
            LoadBalancingAlgorithm::PowerOfTwoChoices => {
                if healthy.len() == 1 {
                    return Some(healthy[0].clone());
                }
                let base = self.rr_counter.fetch_add(2, Ordering::Relaxed);
                let a = &healthy[base % healthy.len()];
                let b = &healthy[(base + 1) % healthy.len()];
                if a.connections() <= b.connections() {
                    Some(a.clone())
                } else {
                    Some(b.clone())
                }
            }
        }
    }

    pub fn select(&self) -> Option<Arc<UpstreamRuntime>> {
        self.select_excluding(&[])
    }
}
