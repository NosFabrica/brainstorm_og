//! The response cache for `/link-preview`, and the URL normalisation that
//! keys it.
//!
//! Every viewer of a note linking to the same page would otherwise trigger its
//! own outbound fetch. That is wasteful, and it is precisely the behaviour that
//! gets a preview bot blocked by the sites we depend on — so this gates public
//! exposure of the endpoint rather than being a performance nicety.
//!
//! Loaded through `get_with`, which is the single flight: one initialiser per
//! key, as `data::get_card` already does.

use moka::future::Cache;
use std::time::Duration;
use url::Url;

use super::{parse::Preview, PreviewError};

/// How long a failure is remembered. Short enough that a site coming back up
/// is picked up within minutes, long enough that a broken link in a popular
/// note is not a fetch per viewer.
///
/// A const rather than a knob: the success TTL is worth tuning per deployment,
/// this is a property of how quickly a broken site should self-heal.
pub const FAILURE_TTL: Duration = Duration::from_secs(300);

/// Query parameters dropped from both the cache key and the URL we fetch.
///
/// Shared links carry these constantly, so it is a real hit-rate win, and it
/// stops us forwarding the sharer's campaign identifiers to the destination.
/// `ref` is deliberately absent — some sites treat it as meaningful.
const TRACKING: &[&str] = &["fbclid", "gclid", "msclkid", "igshid"];
const TRACKING_PREFIX: &str = "utm_";

/// Roughly what an entry costs beyond its string bytes: four `Option<String>`
/// headers and a `String` one at 24 bytes each, the enum tag, and moka's own
/// per-entry bookkeeping. Approximate on purpose — the point is that the
/// configured ceiling is not out by a third, not that it is exact.
const ENTRY_OVERHEAD: usize = 128;

/// What the cache holds: a parsed preview, or the reason there isn't one.
#[derive(Debug, Clone)]
pub enum Outcome {
    Ok(Preview),
    Failed(PreviewError),
}

pub type PreviewCache = Cache<String, Outcome>;

/// Key and fetch target in one, so two URLs that share a cache entry also
/// produce byte-identical outbound requests and cannot drift apart.
///
/// Lowercasing the scheme and host is `url`'s own doing for http(s) — they are
/// "special" schemes, which it normalises on parse. Pinned by a test rather
/// than trusted.
pub fn normalise(raw: &str) -> Option<Url> {
    let mut url = Url::parse(raw.trim()).ok()?;
    url.set_fragment(None);

    let kept: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| !is_tracking(k))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();

    // Only rewritten when something was actually dropped. Re-serialising a
    // query round-trips it through `query_pairs`, which decodes `+` as a space
    // and re-encodes it as `%20` — harmless but gratuitous on the vast
    // majority of URLs that carry no tracking at all.
    if kept.len() != url.query_pairs().count() {
        if kept.is_empty() {
            url.set_query(None);
        } else {
            url.query_pairs_mut().clear().extend_pairs(kept);
        }
    }

    Some(url)
}

fn is_tracking(key: &str) -> bool {
    let key = key.trim().to_ascii_lowercase();
    key.starts_with(TRACKING_PREFIX) || TRACKING.contains(&key.as_str())
}

/// Weight in bytes, counting the **key as well as the value**.
///
/// `png_cache` holds a few hundred large values, so weighing the value alone is
/// close enough there. This one holds tens of thousands of small ones: a 16 MB
/// budget at a typical ~400-600 bytes of metadata is ~30k entries, each also
/// carrying an ~80-byte URL key and moka's per-entry overhead. Weighing the
/// value alone would under-count by 30-50% and the ceiling would not mean what
/// it says.
pub fn weigh(key: &str, outcome: &Outcome) -> u32 {
    let payload = match outcome {
        Outcome::Ok(p) => p.byte_size(),
        // A failure is a static reason; it costs the key and the tag.
        Outcome::Failed(_) => 0,
    };
    key.len()
        .saturating_add(payload)
        .saturating_add(ENTRY_OVERHEAD)
        .min(u32::MAX as usize) as u32
}

/// Successes live a day, failures minutes. Mirrors `data::CardExpiry`, which
/// splits provisional from settled cards the same way.
pub struct PreviewExpiry {
    pub ok: Duration,
    pub failed: Duration,
}

impl moka::Expiry<String, Outcome> for PreviewExpiry {
    fn expire_after_create(
        &self,
        _key: &String,
        value: &Outcome,
        _now: std::time::Instant,
    ) -> Option<Duration> {
        Some(match value {
            Outcome::Ok(_) => self.ok,
            Outcome::Failed(_) => self.failed,
        })
    }
}

/// Byte-bounded, not entry-bounded: entry size varies by more than an order of
/// magnitude (a bare `<title>` against a capped 1000-character description in
/// CJK), so an entry ceiling would express nothing about memory.
pub fn store(max_bytes: u64, ttl: Duration) -> PreviewCache {
    Cache::builder()
        .weigher(|k: &String, v: &Outcome| weigh(k, v))
        .max_capacity(max_bytes)
        .expire_after(PreviewExpiry {
            ok: ttl,
            failed: FAILURE_TTL,
        })
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use moka::Expiry as _;

    fn norm(raw: &str) -> String {
        normalise(raw).expect("should parse").to_string()
    }

    #[test]
    fn normalisation_drops_the_noise_and_keeps_the_rest() {
        // Scheme and host lowercased, path and query casing left alone.
        assert_eq!(
            norm("HTTP://EXAMPLE.COM/Path?A=B"),
            "http://example.com/Path?A=B"
        );
        // The fragment is not sent to the server and must not split the key.
        assert_eq!(
            norm("https://example.com/a#section"),
            "https://example.com/a"
        );
        // Stripping the only parameters leaves no `?` behind, so the bare URL
        // and the tagged one are the same key.
        assert_eq!(
            norm("https://example.com/a?utm_source=x&utm_medium=y"),
            "https://example.com/a"
        );
        assert_eq!(
            norm("https://example.com/a?fbclid=1&gclid=2&msclkid=3&igshid=4"),
            "https://example.com/a"
        );
        // Real parameters survive, in their original order.
        assert_eq!(
            norm("https://example.com/a?id=7&utm_campaign=spring&page=2"),
            "https://example.com/a?id=7&page=2"
        );
        // Casing of the parameter name does not smuggle one past.
        assert_eq!(
            norm("https://example.com/a?UTM_Source=x"),
            "https://example.com/a"
        );
        // `ref` is meaningful to some sites and is left alone.
        assert_eq!(
            norm("https://example.com/a?ref=hn"),
            "https://example.com/a?ref=hn"
        );
        // A query with nothing to strip is not round-tripped through the
        // encoder, so `+` survives as `+`.
        assert_eq!(
            norm("https://example.com/a?q=a+b"),
            "https://example.com/a?q=a+b"
        );
    }

    #[test]
    fn what_will_not_parse_is_not_a_target() {
        // The handler refuses these outright rather than minting an entry per
        // typo, so `normalise` has to say so.
        for raw in ["not a url", "", "/relative/only", "example.com/no-scheme"] {
            assert_eq!(normalise(raw), None, "for {raw:?}");
        }
    }

    #[test]
    fn the_weigher_counts_the_key_as_well_as_the_value() {
        let preview = Preview {
            kind: Default::default(),
            title: Some("Title".into()),
            description: Some("Description".into()),
            image: None,
            site_name: Some("Site".into()),
            url: "https://example.com/a".into(),
        };
        let key = "https://example.com/a";
        let short = weigh(key, &Outcome::Ok(preview.clone()));
        let long = weigh(&format!("{key}{}", "a".repeat(200)), &Outcome::Ok(preview));
        assert!(
            long > short,
            "identical metadata weighed the same under a 200-byte-longer key \
             ({long} vs {short}) — the key is not being counted"
        );
        assert_eq!(
            long - short,
            200,
            "the difference is exactly the key growth"
        );
    }

    #[tokio::test]
    async fn the_ceiling_is_in_bytes_and_is_enforced() {
        // Room for a handful of entries, fed a few hundred. An entry-bounded
        // cache would hold all of them.
        let cache = store(4096, Duration::from_secs(60));
        let preview = Preview {
            title: Some("T".repeat(200)),
            url: "https://example.com/".into(),
            ..Default::default()
        };
        for i in 0..500 {
            cache
                .insert(
                    format!("https://example.com/{i}"),
                    Outcome::Ok(preview.clone()),
                )
                .await;
        }
        cache.run_pending_tasks().await;

        assert!(cache.entry_count() < 500, "nothing was evicted");
        assert!(
            cache.weighted_size() <= 4096,
            "held {} bytes against a 4096-byte ceiling",
            cache.weighted_size()
        );
    }

    #[test]
    fn failures_expire_much_sooner_than_successes() {
        let expiry = PreviewExpiry {
            ok: Duration::from_secs(86_400),
            failed: FAILURE_TTL,
        };
        let now = std::time::Instant::now();
        let ok = expiry.expire_after_create(&String::new(), &Outcome::Ok(Preview::default()), now);
        let failed = expiry.expire_after_create(
            &String::new(),
            &Outcome::Failed(PreviewError::Timeout),
            now,
        );
        assert_eq!(ok, Some(Duration::from_secs(86_400)));
        assert_eq!(failed, Some(FAILURE_TTL));
        assert!(failed < ok);

        // And that ordering has to survive the configured default, or a broken
        // site is remembered as long as a working one.
        let configured = crate::config::Config::from_env();
        assert_eq!(configured.link_preview_cache_ttl_secs, 86_400);
        assert_eq!(configured.link_preview_cache_max_bytes, 16_777_216);
        assert!(FAILURE_TTL.as_secs() < configured.link_preview_cache_ttl_secs);
    }
}
