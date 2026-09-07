use super::Config;
use anyhow::{bail, Result};
use std::net::IpAddr;
use std::path::Path;

pub fn validate(config: &Config) -> Result<()> {
    let has_default = !config.upstreams.is_empty();
    let has_routes = !config.routes.is_empty();
    if !has_default && !has_routes {
        bail!("configure either top-level `upstreams` or at least one `routes` entry");
    }

    for (i, u) in config.upstreams.iter().enumerate() {
        validate_upstream(i, u)?;
    }
    for (ri, route) in config.routes.iter().enumerate() {
        if route.upstreams.is_empty() {
            bail!("routes[{ri}] must have at least one upstream");
        }
        if route.path_prefix.is_empty() {
            bail!("routes[{ri}].path_prefix must not be empty (use \"/\")");
        }
        for (ui, u) in route.upstreams.iter().enumerate() {
            validate_upstream(ui, u).map_err(|e| anyhow::anyhow!("routes[{ri}]: {e}"))?;
        }
    }

    if config.health_check.interval_secs == 0 {
        bail!("health_check.interval_secs must be > 0");
    }
    if config.health_check.timeout_secs == 0 {
        bail!("health_check.timeout_secs must be > 0");
    }
    if config.health_check.timeout_secs > config.health_check.interval_secs * 10 {
        bail!("health_check.timeout_secs looks unreasonable vs interval");
    }
    if config.timeouts.connect_secs == 0 || config.timeouts.request_secs == 0 {
        bail!("timeouts.connect_secs and request_secs must be > 0");
    }
    if config.max_body_bytes == 0 {
        bail!("max_body_bytes must be > 0");
    }
    if let Some(ref rl) = config.rate_limit {
        if rl.rps == 0 {
            bail!("rate_limit.rps must be > 0");
        }
        if rl.burst <= 0.0 {
            bail!("rate_limit.burst must be > 0");
        }
    }

    if let Some(tls) = &config.tls {
        if !Path::new(&tls.cert).exists() {
            bail!("tls.cert file does not exist: {}", tls.cert);
        }
        if !Path::new(&tls.key).exists() {
            bail!("tls.key file does not exist: {}", tls.key);
        }
    }

    if let Some(acl) = &config.acl {
        for cidr in acl.allow.iter().chain(acl.deny.iter()) {
            validate_cidr(cidr)?;
        }
    }
    for cidr in &config.proxy_protocol_trusted_cidrs {
        validate_cidr(cidr)?;
    }

    if config.proxy_protocol && config.proxy_protocol_trusted_cidrs.is_empty() {
        tracing::warn!(
            "proxy_protocol=true without proxy_protocol_trusted_cidrs — any client can spoof source IP"
        );
    }
    if config
        .admin_token
        .as_ref()
        .map(|s| s.is_empty())
        .unwrap_or(true)
    {
        tracing::warn!(
            "admin_token is not set — /metrics and /dashboard are unauthenticated (set admin_token in production)"
        );
    }

    Ok(())
}

fn validate_upstream(i: usize, upstream: &super::Upstream) -> Result<()> {
    if upstream.address.is_empty() {
        bail!("upstream[{i}] address cannot be empty");
    }
    if upstream.weight == 0 {
        bail!("upstream[{i}] weight must be > 0");
    }
    let url = upstream
        .address
        .parse::<http::Uri>()
        .map_err(|e| anyhow::anyhow!("upstream[{i}] is not a valid URI: {e}"))?;
    match url.scheme_str() {
        Some("http") | Some("https") => {}
        other => bail!("upstream[{i}] scheme must be http or https, got {other:?}"),
    }
    if url.host().is_none() {
        bail!("upstream[{i}] must include a host");
    }
    Ok(())
}

fn validate_cidr(s: &str) -> Result<()> {
    if let Some((ip, pfx)) = s.split_once('/') {
        let _: IpAddr = ip
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid IP in CIDR: {s}"))?;
        let p: u8 = pfx
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid prefix in CIDR: {s}"))?;
        if p > 128 {
            bail!("CIDR prefix out of range: {s}");
        }
    } else {
        let _: IpAddr = s
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid IP or CIDR: {s}"))?;
    }
    Ok(())
}
