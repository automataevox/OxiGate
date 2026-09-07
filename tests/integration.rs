//! Integration-style tests that do not require a full network stack.
//! Full end-to-end proxy tests: see docs/PRODUCTION.md (run locally).

use oxigate::config::{Config, LoadBalancingAlgorithm, Upstream};
use oxigate::lb::LoadBalancer;
use oxigate::ratelimit::{RateLimitConfig, RateLimiter};
use oxigate::security::Acl;
use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

// Note: library target needed for `oxigate::` imports in integration tests.
// If binary-only, these compile as unit tests under src/. This file is a template
// when the crate is published as both lib+bin.

#[test]
fn lb_round_robin_cycles() {
    let upstreams = vec![
        Upstream {
            address: "http://a".into(),
            weight: 1,
        },
        Upstream {
            address: "http://b".into(),
            weight: 1,
        },
    ];
    let lb = LoadBalancer::from_upstreams(&upstreams, LoadBalancingAlgorithm::RoundRobin);
    let a = lb.select().unwrap().address.clone();
    let b = lb.select().unwrap().address.clone();
    assert_ne!(a, b);
}

#[test]
fn rate_limiter_basic() {
    let rl = RateLimiter::new(RateLimitConfig {
        rate: 5,
        window: Duration::from_secs(1),
        burst: 1.0,
    });
    let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
    let mut ok = 0;
    for _ in 0..10 {
        if rl.check(ip) {
            ok += 1;
        }
    }
    assert!(ok <= 6);
    assert!(rl.rejects() > 0);
}

#[test]
fn acl_blocks_denied_cidr() {
    let acl = Acl::from_config(&[], &["10.0.0.0/8".into()]).unwrap();
    assert!(!acl.is_allowed(IpAddr::V4(Ipv4Addr::new(10, 1, 1, 1))));
    assert!(acl.is_allowed(IpAddr::V4(Ipv4Addr::new(11, 0, 0, 1))));
}
