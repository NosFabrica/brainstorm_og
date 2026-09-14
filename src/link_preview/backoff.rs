//! Hosts that asked us to slow down.
//!
//! A 429 or 503, on robots.txt or a page, pauses every fetch to that host for
//! its `Retry-After`, held between the bounds below. A feed full of one site's
//! links would otherwise keep hitting a site that asked us to stop, which is
//! how a preview bot gets blocked.

use std::time::Duration;

use axum::http::{header, HeaderMap, StatusCode};
use moka::future::Cache;
use url::Url;

use super::hop::origin_key;
use crate::state::AppState;

pub const MIN_BACKOFF: Duration = Duration::from_secs(60);
pub const MAX_BACKOFF: Duration = Duration::from_secs(600);

/// Origin → how long it asked us to stay away. The entry expires then.
pub type BackoffCache = Cache<String, Duration>;

struct UntilAsked;

impl moka::Expiry<String, Duration> for UntilAsked {
    fn expire_after_create(
        &self,
        _key: &String,
        value: &Duration,
        _now: std::time::Instant,
    ) -> Option<Duration> {
        Some(*value)
    }
}

pub fn store(capacity: u64) -> BackoffCache {
    Cache::builder()
        .max_capacity(capacity)
        .expire_after(UntilAsked)
        .build()
}

pub fn is_slow_down(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status == StatusCode::SERVICE_UNAVAILABLE
}

/// `Retry-After` in seconds, clamped. The HTTP-date form and anything
/// unparseable get the minimum.
pub fn retry_after(headers: &HeaderMap) -> Duration {
    headers
        .get(header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(MIN_BACKOFF)
        .clamp(MIN_BACKOFF, MAX_BACKOFF)
}

pub async fn back_off(st: &AppState, url: &Url, headers: &HeaderMap) {
    if let Some(origin) = origin_key(url) {
        st.host_backoff.insert(origin, retry_after(headers)).await;
    }
}

pub async fn is_backing_off(st: &AppState, url: &Url) -> bool {
    match origin_key(url) {
        Some(origin) => st.host_backoff.get(&origin).await.is_some(),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moka::Expiry as _;

    #[test]
    fn retry_after_is_honoured_within_bounds() {
        let with = |v: &str| {
            let mut h = HeaderMap::new();
            h.insert(header::RETRY_AFTER, v.parse().unwrap());
            retry_after(&h)
        };
        assert_eq!(with("120"), Duration::from_secs(120));
        assert_eq!(with("5"), MIN_BACKOFF, "never shorter than a minute");
        assert_eq!(with("86400"), MAX_BACKOFF, "never longer than ten minutes");
        assert_eq!(with("Wed, 21 Oct 2026 07:28:00 GMT"), MIN_BACKOFF);
        assert_eq!(retry_after(&HeaderMap::new()), MIN_BACKOFF);
    }

    #[test]
    fn a_backoff_expires_when_the_host_asked() {
        let asked = Duration::from_secs(300);
        assert_eq!(
            UntilAsked.expire_after_create(&String::new(), &asked, std::time::Instant::now()),
            Some(asked)
        );
    }
}
