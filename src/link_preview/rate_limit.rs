//! Per-IP rate limiting for `/link-preview`.
//!
//! The endpoint is unauthenticated and fetches arbitrary URLs, so without a
//! limiter it is an open fetch proxy. Two ceilings: trusted traffic — what our
//! own SPA originated — gets one high enough that a real user never meets it,
//! untrusted traffic gets a tight one.
//!
//! A fixed-window counter over moka, mirroring `validate_rate_limit` in
//! `brainstorm_server` — the first request in a window creates the entry and
//! sets its expiry, and the rest of the window is counted against it. No new
//! crate, and og's CI runs `cargo audit`.

use axum::{
    extract::{ConnectInfo, Request, State},
    http::{header, HeaderMap, StatusCode},
    middleware::Next,
    response::Response,
};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use url::Url;

use crate::config::Config;
use crate::state::AppState;

/// Browsers set this and forbid page JavaScript from touching it, so a
/// third-party site cannot make its visitors look like ours. `Origin` would be
/// the obvious header and is the wrong one: a same-origin `GET` does not send
/// it, so the check would match zero real requests.
const SEC_FETCH_SITE: &str = "sec-fetch-site";

/// A 429 says something about *this caller* right now. A shared cache that
/// stored it would serve someone else's throttling, and a browser that stored
/// it would keep refusing after the window had rolled.
const LIMITED_CACHE: &str = "no-store";

/// Buckets held in memory. Entries live one window and hold a key plus a
/// counter, so this is a few MB at capacity.
///
/// It is a bound, which means flooding distinct IPs can evict a bucket and
/// reset its count. That is inherent to an in-process limiter and is the
/// reason the untrusted rate is tight rather than the reason to raise this.
const MAX_BUCKETS: u64 = 100_000;

/// Which ceiling a request is measured against. Named after the config it
/// reads (`LINK_PREVIEW_RATE_TRUSTED` / `_UNTRUSTED`). Not "tier": that already
/// names the GrapeRank score bucket `data::Overview` carries in this crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Caller {
    /// Our own SPA, by `Sec-Fetch-Site` or a `Referer` under `APP_BASE_URL`.
    Trusted,
    Untrusted,
}

impl Caller {
    /// Part of the bucket key, so the two are counted separately. A burst of
    /// scraping from a NAT'd address must not spend the ceiling the SPA users
    /// behind it are measured against.
    fn tag(self) -> &'static str {
        match self {
            Self::Trusted => "trusted",
            Self::Untrusted => "untrusted",
        }
    }

    fn limit(self, config: &Config) -> u32 {
        match self {
            Self::Trusted => config.link_preview_rate_trusted,
            Self::Untrusted => config.link_preview_rate_untrusted,
        }
    }
}

/// Trivially forged by curl — the ceiling is what handles that. What it does
/// buy is the attack per-IP limiting cannot touch: a third-party page
/// embedding this endpoint to turn its visitors into unwitting proxy users,
/// where every request arrives from a different address.
fn caller(headers: &HeaderMap, app_base_url: &str) -> Caller {
    let site = headers
        .get(SEC_FETCH_SITE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .trim();
    if site.eq_ignore_ascii_case("same-origin") {
        return Caller::Trusted;
    }

    // Older browsers send no `Sec-Fetch-*` at all. Compared by parsed origin,
    // never by prefix: `https://brainstorm.world.evil.test` starts with the
    // configured base and is a different site. The comparison is against
    // config, never the request's own `Host` — see CONTEXT.md.
    let referer = headers
        .get(header::REFERER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| Url::parse(v).ok());
    let app = Url::parse(app_base_url).ok();
    match (referer, app) {
        (Some(r), Some(a)) if r.origin() == a.origin() => Caller::Trusted,
        _ => Caller::Untrusted,
    }
}

/// The caller's address, read from the hop our own proxy wrote.
///
/// The ingress *appends* to `X-Forwarded-For` rather than replacing it, so the
/// leftmost entry is whatever the client sent — anyone could rotate a forged
/// header and bypass the limit outright. Count back `hops_back` from the
/// **right** instead. Mirrors `resolve_client_ip` in `brainstorm_server`
/// (commit `90ff628`).
///
/// The fallback is deliberately noisy — every time, not only when the header
/// is present but too short: behind the ingress the direct peer is the UI's
/// nginx pod, identical for every caller, so a silent fallback would throttle
/// unrelated callers together. A missing header there means traffic reached the
/// pod off the expected path, which is exactly what wants saying out loud.
pub fn client_ip(headers: &HeaderMap, peer: Option<IpAddr>, hops_back: usize) -> String {
    // Every line, not the first: repeated `X-Forwarded-For` headers stay
    // separate values, and reading one of them would count hops from the wrong
    // chain — the client's own, if it sent the line hyper happened to hand back.
    let hops: Vec<&str> = headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .collect();

    let trusted = (1..=hops.len())
        .contains(&hops_back)
        .then(|| hops[hops.len() - hops_back]);

    if let Some(hop) = trusted {
        // A hop our proxy wrote that is not an address is still that caller's
        // own token, so it keys its own bucket. Collapsing it onto the shared
        // peer instead would throttle every such caller together.
        return match parse_hop(hop) {
            Some(ip) => ip.to_string(),
            None => hop.chars().take(64).collect(),
        };
    }

    // Counts only. The addresses themselves are not ours to log.
    tracing::warn!(
        hops = hops.len(),
        trusted_proxy_hops = hops_back,
        "X-Forwarded-For did not carry a usable trusted hop; falling back to \
         the direct peer, which is shared behind a proxy and will throttle \
         unrelated callers together"
    );

    peer.map_or_else(|| "unknown".to_string(), |p| p.to_string())
}

/// Normalised, so `1.2.3.4:80`, `[::1]:443` and casing variants of an IPv6
/// address all key the same bucket.
fn parse_hop(hop: &str) -> Option<IpAddr> {
    hop.parse::<IpAddr>()
        .ok()
        .or_else(|| hop.parse::<SocketAddr>().ok().map(|a| a.ip()))
}

/// Route middleware. `build_router` explains why it is applied where it is.
pub async fn enforce(State(st): State<AppState>, req: Request, next: Next) -> Response {
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0.ip());
    let caller = caller(req.headers(), &st.config.app_base_url);
    let ip = client_ip(req.headers(), peer, st.config.trusted_proxy_hops);

    if over_limit(
        &st,
        format!("{}:{ip}", caller.tag()),
        caller.limit(&st.config),
    )
    .await
    {
        tracing::debug!(caller = caller.tag(), "link preview rate limited");
        return super::envelope(
            StatusCode::TOO_MANY_REQUESTS,
            Some("too many requests"),
            LIMITED_CACHE,
            serde_json::Value::Null,
        );
    }

    next.run(req).await
}

/// Count this request, and say whether it went past the ceiling.
///
/// `get_with` is what makes the count atomic across concurrent requests: moka
/// runs one initialiser per key and hands every other caller the same counter.
async fn over_limit(st: &AppState, key: String, limit: u32) -> bool {
    let counter = st
        .preview_rate
        .get_with(key, async { Arc::new(AtomicU64::new(0)) })
        .await;
    counter.fetch_add(1, Ordering::Relaxed) + 1 > u64::from(limit)
}

/// The bucket store. TTL is the window: an entry created by the first request
/// of a window expires at the end of it, which is the reset.
pub fn buckets(window: std::time::Duration) -> moka::future::Cache<String, Arc<AtomicU64>> {
    moka::future::Cache::builder()
        .max_capacity(MAX_BUCKETS)
        .time_to_live(window)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    const APP: &str = "https://brainstorm.test";

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    fn peer() -> Option<IpAddr> {
        Some("10.9.9.9".parse().unwrap())
    }

    #[test]
    fn the_trusted_hop_is_counted_from_the_right() {
        // client -> ingress -> UI nginx -> here. The entry the UI's nginx wrote
        // is the ingress's view of the caller, two back.
        let h = headers(&[("x-forwarded-for", "203.0.113.7, 198.51.100.4, 10.42.0.9")]);
        assert_eq!(client_ip(&h, peer(), 2), "198.51.100.4");
        assert_eq!(client_ip(&h, peer(), 1), "10.42.0.9");
        assert_eq!(client_ip(&h, peer(), 3), "203.0.113.7");
    }

    #[test]
    fn a_forged_leftmost_entry_cannot_move_the_bucket() {
        // Whatever the client prepends survives at the front of the chain, so
        // rotating it must leave the resolved address alone.
        let real = "198.51.100.4, 10.42.0.9";
        let base = client_ip(&headers(&[("x-forwarded-for", real)]), peer(), 2);
        for forged in ["1.2.3.4", "9.9.9.9, 8.8.8.8", "not-an-ip"] {
            let h = headers(&[("x-forwarded-for", &format!("{forged}, {real}"))]);
            assert_eq!(client_ip(&h, peer(), 2), base, "shifted by {forged}");
        }
    }

    #[test]
    fn too_few_hops_falls_back_to_the_peer() {
        let h = headers(&[("x-forwarded-for", "203.0.113.7")]);
        assert_eq!(client_ip(&h, peer(), 2), "10.9.9.9");
        // And with no peer either, everything shares one bucket rather than
        // silently going unlimited.
        assert_eq!(client_ip(&h, None, 2), "unknown");
    }

    #[test]
    fn an_unparseable_trusted_hop_still_keys_its_own_bucket() {
        // A proxy writing a non-address token (`unknown`, an RFC 7239
        // obfuscated identity) must not collapse every such caller onto the
        // shared peer — that throttles them all together.
        let h = headers(&[("x-forwarded-for", "1.2.3.4, _hidden, 10.42.0.9")]);
        assert_eq!(client_ip(&h, peer(), 2), "_hidden");
        // And it cannot grow without bound just because a header can.
        let long = "z".repeat(4096);
        let h = headers(&[("x-forwarded-for", &format!("1.2.3.4, {long}, 10.42.0.9"))]);
        assert_eq!(client_ip(&h, peer(), 2).len(), 64);
    }

    #[test]
    fn repeated_header_lines_are_read_as_one_chain() {
        // hyper keeps repeated headers as separate values. Reading only the
        // first would count hops from the client's own line and reopen the
        // bypass entirely.
        let mut h = HeaderMap::new();
        h.append("x-forwarded-for", "1.2.3.4".parse().unwrap());
        h.append(
            "x-forwarded-for",
            "198.51.100.4, 10.42.0.9".parse().unwrap(),
        );
        assert_eq!(client_ip(&h, peer(), 2), "198.51.100.4");
    }

    #[test]
    fn hops_are_normalised_so_one_caller_is_one_bucket() {
        let with_port = headers(&[("x-forwarded-for", " 198.51.100.4:51234 , 10.42.0.9")]);
        assert_eq!(client_ip(&with_port, peer(), 2), "198.51.100.4");

        // Same address, three spellings, one bucket.
        for spelling in ["2001:db8::1", "2001:DB8:0:0:0:0:0:1", "[2001:db8::1]:443"] {
            let h = headers(&[("x-forwarded-for", &format!("{spelling}, 10.42.0.9"))]);
            assert_eq!(client_ip(&h, peer(), 2), "2001:db8::1", "for {spelling}");
        }
    }

    #[test]
    fn no_forwarded_header_at_all_uses_the_peer() {
        assert_eq!(client_ip(&HeaderMap::new(), peer(), 2), "10.9.9.9");
    }

    #[test]
    fn sec_fetch_site_selects_the_ceiling() {
        for value in ["same-origin", "Same-Origin", " same-origin "] {
            let h = headers(&[("sec-fetch-site", value)]);
            assert_eq!(caller(&h, APP), Caller::Trusted, "for {value:?}");
        }
        // `none` is a typed-in URL, `same-site` a sibling subdomain, and
        // `cross-site` someone else's page — none of them our SPA's fetch.
        for value in ["cross-site", "same-site", "none"] {
            let h = headers(&[("sec-fetch-site", value)]);
            assert_eq!(caller(&h, APP), Caller::Untrusted, "for {value:?}");
        }
        assert_eq!(caller(&HeaderMap::new(), APP), Caller::Untrusted);
    }

    #[test]
    fn a_referer_is_compared_by_origin_not_by_prefix() {
        let same = headers(&[("referer", "https://brainstorm.test/p/npub1abc")]);
        assert_eq!(caller(&same, APP), Caller::Trusted);

        for lookalike in [
            "https://brainstorm.test.evil.example/",
            "http://brainstorm.test/",
            "https://brainstorm.test:8443/",
            "https://evil.example/?u=https://brainstorm.test",
            "not a url",
        ] {
            let h = headers(&[("referer", lookalike)]);
            assert_eq!(caller(&h, APP), Caller::Untrusted, "for {lookalike}");
        }
    }

    #[test]
    fn sec_fetch_site_wins_over_a_missing_referer() {
        // The SPA's `fetch()` sends `Sec-Fetch-Site` and, under a
        // `no-referrer` policy, no `Referer` at all.
        let h = headers(&[("sec-fetch-site", "same-origin")]);
        assert_eq!(caller(&h, APP), Caller::Trusted);
    }
}
