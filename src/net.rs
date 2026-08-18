//! Validates outbound URLs built from attacker-controlled input (kind-0
//! `picture`). Pre-connect only, so DNS rebinding is unmitigated — see the
//! accepted-risk note in CONTEXT.md.

use anyhow::{bail, Result};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use url::Url;

/// Reserved / internal IPv4 space we refuse to dial.
fn v4_blocked(ip: Ipv4Addr) -> bool {
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_unspecified()
        || ip.is_multicast()
        || ip.is_documentation()
        || ip.octets()[0] == 0
        // 100.64.0.0/10 carrier-grade NAT
        || (ip.octets()[0] == 100 && (64..128).contains(&ip.octets()[1]))
        // 192.0.0.0/24 IETF protocol assignments
        || (ip.octets()[0] == 192 && ip.octets()[1] == 0 && ip.octets()[2] == 0)
        // 198.18.0.0/15 benchmarking
        || (ip.octets()[0] == 198 && (18..20).contains(&ip.octets()[1]))
        // 240.0.0.0/4 reserved
        || ip.octets()[0] >= 240
}

fn v6_blocked(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return v4_blocked(v4);
    }
    let seg = ip.segments();
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        // fc00::/7 unique local
        || (seg[0] & 0xfe00) == 0xfc00
        // fe80::/10 link local
        || (seg[0] & 0xffc0) == 0xfe80
}

pub fn ip_blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4_blocked(v4),
        IpAddr::V6(v6) => v6_blocked(v6),
    }
}

/// Syntactic checks that need no DNS. Split out so it is cheap to unit-test.
pub fn validate_url(raw: &str, allowed_schemes: &[&str]) -> Result<Url> {
    let url = Url::parse(raw)?;

    if !allowed_schemes.contains(&url.scheme()) {
        bail!("scheme {:?} not allowed", url.scheme());
    }
    // Credentials are never legitimate here and confuse downstream host parsing.
    if !url.username().is_empty() || url.password().is_some() {
        bail!("credentials in URL");
    }

    // Match on the typed host, NOT `host_str()`. That returns IPv6 literals
    // bracketed (`[::1]`), which does not parse as an `IpAddr` — so a string
    // check silently waves every IPv6 literal straight through.
    match url.host() {
        None => bail!("no host"),
        Some(url::Host::Ipv4(v4)) => {
            if ip_blocked(IpAddr::V4(v4)) {
                bail!("host is a reserved address: {v4}");
            }
        }
        Some(url::Host::Ipv6(v6)) => {
            if ip_blocked(IpAddr::V6(v6)) {
                bail!("host is a reserved address: {v6}");
            }
        }
        Some(url::Host::Domain("")) => bail!("empty host"),
        Some(url::Host::Domain(_)) => {}
    }
    Ok(url)
}

/// Full check: syntax, then every address the host resolves to.
pub async fn validate_and_resolve(raw: &str, allowed_schemes: &[&str]) -> Result<Url> {
    let url = validate_url(raw, allowed_schemes)?;

    // Literals were already judged above; only names need a resolver.
    let host = match url.host() {
        Some(url::Host::Domain(d)) => d.to_string(),
        _ => return Ok(url),
    };

    let port = url.port_or_known_default().unwrap_or(80);
    let mut resolved = tokio::net::lookup_host((host.as_str(), port))
        .await?
        .peekable();

    let mut any = false;
    for addr in resolved.by_ref() {
        any = true;
        if ip_blocked(addr.ip()) {
            bail!("{host} resolves to a reserved address: {}", addr.ip());
        }
    }
    if !any {
        bail!("{host} did not resolve");
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocked(s: &str) -> bool {
        s.parse::<IpAddr>().map(ip_blocked).unwrap_or(false)
    }

    #[test]
    fn blocks_internal_addresses() {
        // The cloud metadata endpoint is the canonical SSRF target.
        assert!(blocked("169.254.169.254"));
        assert!(blocked("127.0.0.1"));
        assert!(blocked("10.0.0.1"));
        assert!(blocked("172.16.0.1"));
        assert!(blocked("192.168.1.1"));
        assert!(blocked("0.0.0.0"));
        assert!(blocked("100.64.0.1"));
        assert!(blocked("::1"));
        assert!(blocked("fe80::1"));
        assert!(blocked("fd00::1"));
        // IPv4-mapped IPv6 must not be a bypass.
        assert!(blocked("::ffff:169.254.169.254"));
        assert!(blocked("::ffff:10.0.0.1"));
    }

    #[test]
    fn allows_public_addresses() {
        assert!(!blocked("1.1.1.1"));
        assert!(!blocked("8.8.8.8"));
        assert!(!blocked("2606:4700:4700::1111"));
    }

    #[test]
    fn rejects_bad_schemes_and_credentials() {
        assert!(validate_url("file:///etc/passwd", &["http", "https"]).is_err());
        assert!(validate_url("data:image/png;base64,AAAA", &["http", "https"]).is_err());
        assert!(validate_url("ws://relay.example", &["http", "https"]).is_err());
        assert!(validate_url("http://user:pw@example.com/a.png", &["http", "https"]).is_err());
        // ...and the relay side must not accept http.
        assert!(validate_url("http://relay.example", &["ws", "wss"]).is_err());
    }

    #[test]
    fn rejects_internal_literals_without_dns() {
        assert!(validate_url("http://169.254.169.254/latest/", &["http", "https"]).is_err());
        assert!(validate_url("http://10.0.0.5:6379", &["http", "https"]).is_err());
        assert!(validate_url("ws://10.0.0.5:6379", &["ws", "wss"]).is_err());
        assert!(validate_url("http://127.0.0.1:8000/", &["http", "https"]).is_err());
    }

    /// Regression: `host_str()` renders IPv6 literals bracketed (`[::1]`),
    /// which never parses as an `IpAddr`, so a string-based check waved every
    /// IPv6 literal through. Matching on `url::Host` is what closes it.
    #[test]
    fn rejects_bracketed_ipv6_literals() {
        for raw in [
            "http://[::1]:8000/",
            "http://[::ffff:169.254.169.254]/latest/",
            "http://[fd00::1]/",
            "http://[fe80::1]/",
            "http://[::]/",
        ] {
            assert!(
                validate_url(raw, &["http", "https"]).is_err(),
                "should have rejected {raw}"
            );
        }
        // A public IPv6 literal is still fine.
        assert!(validate_url("http://[2606:4700:4700::1111]/", &["http", "https"]).is_ok());
    }

    #[test]
    fn accepts_ordinary_urls() {
        assert!(validate_url("https://cdn.example/a.jpg", &["http", "https"]).is_ok());
        assert!(validate_url("wss://relay.damus.io", &["ws", "wss"]).is_ok());
    }
}
