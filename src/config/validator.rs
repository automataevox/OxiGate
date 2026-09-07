use super::Config;
use anyhow::{bail, Result};
use std::path::Path;

pub fn validate(config: &Config) -> Result<()> {
    let has_default = !config.upstreams.is_empty();
    let has_routes = !config.routes.is_empty();

    if !has_default && !has_routes {
        bail!("configure either top-level `upstreams` or at least one `routes` entry");
    }

    for (i, upstream) in config.upstreams.iter().enumerate() {
        validate_upstream(i, upstream)?;
    }

    for (ri, route) in config.routes.iter().enumerate() {
        if route.upstreams.is_empty() {
            bail!("routes[{ri}] must have at least one upstream");
        }
        for (ui, upstream) in route.upstreams.iter().enumerate() {
            validate_upstream(ui, upstream).map_err(|e| anyhow::anyhow!("routes[{ri}]: {e}"))?;
        }
    }

    if config.health_check.interval_secs == 0 {
        bail!("health_check.interval_secs must be > 0");
    }

    if let Some(tls) = &config.tls {
        if !Path::new(&tls.cert).exists() {
            bail!("tls.cert file does not exist: {}", tls.cert);
        }
        if !Path::new(&tls.key).exists() {
            bail!("tls.key file does not exist: {}", tls.key);
        }
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
    if !upstream.address.starts_with("http://") && !upstream.address.starts_with("https://") {
        bail!("upstream[{i}] address must start with http:// or https://");
    }
    Ok(())
}
