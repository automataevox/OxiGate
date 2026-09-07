//! Per-IP GCRA-style rate limiter (Generic Cell Rate Algorithm).
//! Industry-standard approach used by Envoy / HAProxy stick-tables equivalents.

use dashmap::DashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone)]
pub struct RateLimitConfig {
    /// Max requests per window.
    pub rate: u32,
    /// Window length.
    pub window: Duration,
    /// Optional burst multiplier (default 1.0 = strict).
    pub burst: f64,
}

impl RateLimitConfig {
    pub fn from_rps(rps: u32, burst: f64) -> Self {
        Self {
            rate: rps.max(1),
            window: Duration::from_secs(1),
            burst: burst.max(1.0),
        }
    }
}

struct Bucket {
    /// Theoretical arrival time (GCRA TAT).
    tat: Instant,
}

pub struct RateLimiter {
    cfg: RateLimitConfig,
    buckets: DashMap<IpAddr, Bucket>,
    /// Emission interval = window / rate
    emission: Duration,
    rejects: AtomicU64,
    allows: AtomicU64,
}

impl RateLimiter {
    pub fn new(cfg: RateLimitConfig) -> Arc<Self> {
        let emission = cfg.window / cfg.rate.max(1);
        Arc::new(Self {
            emission,
            cfg,
            buckets: DashMap::new(),
            rejects: AtomicU64::new(0),
            allows: AtomicU64::new(0),
        })
    }

    /// Returns true if the request is allowed.
    pub fn check(&self, ip: IpAddr) -> bool {
        let now = Instant::now();
        let cost = self.emission;
        // Burst is a multiplier of the configured rate. The first request
        // consumes the current slot, so tolerance covers the remaining
        // requests in the burst.
        let burst_requests = self.cfg.rate as f64 * self.cfg.burst;
        let max_credit = self.emission.mul_f64((burst_requests - 1.0).max(0.0));

        let mut entry = self
            .buckets
            .entry(ip)
            .or_insert_with(|| Bucket { tat: now });

        // GCRA: allow if now + max_credit >= TAT
        let tat = entry.tat;
        if now + max_credit < tat {
            self.rejects.fetch_add(1, Ordering::Relaxed);
            return false;
        }

        // Advance TAT
        let new_tat = if tat > now { tat + cost } else { now + cost };
        entry.tat = new_tat;
        self.allows.fetch_add(1, Ordering::Relaxed);
        true
    }

    pub fn rejects(&self) -> u64 {
        self.rejects.load(Ordering::Relaxed)
    }

    pub fn allows(&self) -> u64 {
        self.allows.load(Ordering::Relaxed)
    }

    /// Opportunistic cleanup of idle buckets (call periodically).
    pub fn cleanup(&self, max_idle: Duration) {
        let now = Instant::now();
        self.buckets
            .retain(|_, b| now.duration_since(b.tat) < max_idle + self.cfg.window);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn allows_under_limit() {
        let rl = RateLimiter::new(RateLimitConfig::from_rps(10, 1.0));
        let ip = IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1));
        for _ in 0..10 {
            assert!(rl.check(ip));
        }
    }

    #[test]
    fn rejects_over_burst() {
        let rl = RateLimiter::new(RateLimitConfig {
            rate: 2,
            window: Duration::from_secs(1),
            burst: 1.0,
        });
        let ip = IpAddr::V4(Ipv4Addr::new(2, 2, 2, 2));
        assert!(rl.check(ip));
        assert!(rl.check(ip));
        // third immediate should fail with burst=1
        assert!(!rl.check(ip));
        assert!(rl.rejects() >= 1);
    }
}
