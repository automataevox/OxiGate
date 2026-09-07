//! Security baseline: ACL + hardened response headers.

use std::net::IpAddr;

#[derive(Debug, Clone, Default)]
pub struct Acl {
    allow: Vec<IpNet>,
    deny: Vec<IpNet>,
}

#[derive(Debug, Clone)]
struct IpNet {
    addr: IpAddr,
    prefix: u8,
}

impl Acl {
    pub fn from_config(allow: &[String], deny: &[String]) -> anyhow::Result<Self> {
        Ok(Self {
            allow: allow
                .iter()
                .map(|s| parse_cidr(s))
                .collect::<Result<_, _>>()?,
            deny: deny
                .iter()
                .map(|s| parse_cidr(s))
                .collect::<Result<_, _>>()?,
        })
    }

    /// Returns true if the IP is permitted.
    pub fn is_allowed(&self, ip: IpAddr) -> bool {
        if self.deny.iter().any(|n| n.contains(ip)) {
            return false;
        }
        if self.allow.is_empty() {
            return true;
        }
        self.allow.iter().any(|n| n.contains(ip))
    }
}

impl IpNet {
    fn contains(&self, ip: IpAddr) -> bool {
        match (self.addr, ip) {
            (IpAddr::V4(a), IpAddr::V4(b)) => {
                let mask = if self.prefix >= 32 {
                    u32::MAX
                } else if self.prefix == 0 {
                    0
                } else {
                    !((1u32 << (32 - self.prefix)) - 1)
                };
                (u32::from(a) & mask) == (u32::from(b) & mask)
            }
            (IpAddr::V6(a), IpAddr::V6(b)) => {
                let a = u128::from(a);
                let b = u128::from(b);
                let mask = if self.prefix >= 128 {
                    u128::MAX
                } else if self.prefix == 0 {
                    0
                } else {
                    !((1u128 << (128 - self.prefix)) - 1)
                };
                (a & mask) == (b & mask)
            }
            _ => false,
        }
    }
}

fn parse_cidr(s: &str) -> anyhow::Result<IpNet> {
    if let Some((ip, pfx)) = s.split_once('/') {
        Ok(IpNet {
            addr: ip.parse()?,
            prefix: pfx.parse()?,
        })
    } else {
        let addr: IpAddr = s.parse()?;
        Ok(IpNet {
            prefix: if addr.is_ipv4() { 32 } else { 128 },
            addr,
        })
    }
}

/// Baseline security headers (OWASP-ish, reverse-proxy safe).
pub fn apply_security_headers(headers: &mut hyper::HeaderMap) {
    let pairs = [
        ("X-Content-Type-Options", "nosniff"),
        ("X-Frame-Options", "SAMEORIGIN"),
        ("Referrer-Policy", "strict-origin-when-cross-origin"),
        ("X-XSS-Protection", "0"),
        (
            "Permissions-Policy",
            "geolocation=(), microphone=(), camera=()",
        ),
    ];
    for (k, v) in pairs {
        if !headers.contains_key(k) {
            if let (Ok(name), Ok(val)) = (
                hyper::header::HeaderName::from_bytes(k.as_bytes()),
                hyper::header::HeaderValue::from_str(v),
            ) {
                headers.insert(name, val);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn acl_deny_wins() {
        let acl = Acl::from_config(&["0.0.0.0/0".into()], &["10.0.0.0/8".into()]).unwrap();
        assert!(!acl.is_allowed(IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3))));
        assert!(acl.is_allowed(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4))));
    }

    #[test]
    fn acl_allow_list() {
        let acl = Acl::from_config(&["192.168.0.0/16".into()], &[]).unwrap();
        assert!(acl.is_allowed(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))));
        assert!(!acl.is_allowed(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))));
    }
}
