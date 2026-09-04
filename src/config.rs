use std::env;

/// Runtime configuration, all from env so the same binary works in compose and k8s.
#[derive(Clone, Debug)]
pub struct Config {
    pub bind_addr: String,
    /// brainstorm-server base (overview stats). No trailing slash.
    pub api_base_url: String,
    /// This environment's canonical origin, no trailing slash. Every absolute
    /// URL we emit comes from here; the request `Host` is never read. CONTEXT.md.
    pub app_base_url: String,
    /// The only relay we query. Config, not an `nprofile` hint. CONTEXT.md.
    pub local_relay_url: String,
    pub card_cache_capacity: u64,
    pub png_cache_max_bytes: u64,
    pub cache_ttl_secs: u64,
    /// Short: the meta HTML is tiny and must pick up a new `?v=` promptly.
    pub html_cache_max_age: u64,
    /// Long: image URLs are content-addressed, so a given one never changes.
    pub image_cache_max_age: u64,
    /// TTL for provisional cards, so they self-correct quickly.
    pub provisional_ttl_secs: u64,
    pub fetch_timeout_secs: u64,
    pub avatar_timeout_secs: u64,
    /// Whole-request budget for gathering card data. Crawlers give up at 5-10s.
    pub request_deadline_secs: u64,
    /// Hard cap on an avatar response body, before decode.
    pub avatar_max_bytes: u64,
    /// In-flight renders. Stampede protection coalesces the same pubkey; this
    /// bounds a burst of distinct ones, each of which costs a fetch and a raster.
    ///
    /// Also the memory bound that matters: each render holds a ~3 MB pixmap plus
    /// a decoded avatar (capped at 32 MB), so this multiplies against the
    /// container limit. 8 x 32 MB plus the 64 MB card cache fits 512Mi.
    pub max_concurrent_renders: usize,
    /// Salt folded into the `?v=` image hash.
    ///
    /// The hash otherwise covers only the card's INPUTS, so a change to how the
    /// card is *drawn* produces identical URLs and never reaches anything that
    /// already cached one — and those are served immutable for a year. Setting
    /// this to something that moves per deploy (the image tag) makes a visual
    /// change propagate. Defaults to the crate version.
    pub render_epoch: String,
    /// Per-hop timeout for a link-preview fetch. A page that has not answered
    /// in this long is not worth a card.
    pub link_preview_timeout_secs: u64,
    /// Whole-request budget for a link preview, spanning every redirect hop.
    /// Must fit inside `router_timeout_secs`.
    pub link_preview_deadline_secs: u64,
    /// Hard cap on a previewed page body. The metadata lives in `<head>`, so
    /// this is generous rather than tight.
    pub link_preview_max_bytes: u64,
    /// In-flight link-preview fetches. Route-scoped, for the reason
    /// `build_router` gives.
    pub max_concurrent_previews: usize,
    /// Requests per window from traffic our own SPA originated. High enough
    /// that a real user never meets it.
    pub link_preview_rate_trusted: u32,
    /// Requests per window from everything else.
    pub link_preview_rate_untrusted: u32,
    /// The fixed window both rates are counted over.
    pub link_preview_rate_window_secs: u64,
    /// How many entries to count back from the **right** of `X-Forwarded-For`
    /// to find the address our own proxy wrote. Defaults to 2 — client ->
    /// ingress -> the UI's nginx -> here. The leftmost entry is whatever the
    /// client sent, so it is never read.
    pub trusted_proxy_hops: usize,
    /// Test seam, deliberately not configuration. `from_env` pins it false and
    /// no environment variable reaches it, so no deployment can turn the
    /// address guard off; integration tests set it by building `Config`
    /// in-process to reach a loopback stub. Loopback only — every other
    /// reserved range stays refused either way.
    pub allow_loopback_preview_targets: bool,
    /// Directory with the bundled fonts.
    pub assets_dir: String,
    /// Must match a family in `assets_dir`. Not inferred from load order.
    pub font_family: String,
}

fn var(key: &str, default: &str) -> String {
    env::var(key)
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default.to_string())
}

fn parse<T: std::str::FromStr>(key: &str, default: T) -> T {
    env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

impl Config {
    pub fn from_env() -> Self {
        let app_base_url = var("APP_BASE_URL", "https://brainstorm.world")
            .trim_end_matches('/')
            .to_string();

        Config {
            bind_addr: var("BIND_ADDR", "0.0.0.0:8080"),
            api_base_url: var("API_BASE_URL", "http://brainstorm-server:8000")
                .trim_end_matches('/')
                .to_string(),
            app_base_url,
            local_relay_url: var("LOCAL_RELAY_URL", "ws://strfry:7777"),
            card_cache_capacity: parse("CACHE_MAX_ENTRIES", 500),
            png_cache_max_bytes: parse("CACHE_MAX_BYTES", 64 * 1024 * 1024),
            cache_ttl_secs: parse("CACHE_TTL_SECS", 600),
            html_cache_max_age: parse("HTML_CACHE_MAX_AGE", 300),
            image_cache_max_age: parse("IMAGE_CACHE_MAX_AGE", 31_536_000),
            provisional_ttl_secs: parse("PROVISIONAL_TTL_SECS", 60),
            fetch_timeout_secs: parse("FETCH_TIMEOUT_SECS", 3),
            avatar_timeout_secs: parse("AVATAR_TIMEOUT_SECS", 5),
            request_deadline_secs: parse("REQUEST_DEADLINE_SECS", 4),
            avatar_max_bytes: parse("AVATAR_MAX_BYTES", 5 * 1024 * 1024),
            max_concurrent_renders: parse("MAX_CONCURRENT_RENDERS", 8),
            render_epoch: var("RENDER_EPOCH", env!("CARGO_PKG_VERSION")),
            link_preview_timeout_secs: parse("LINK_PREVIEW_TIMEOUT_SECS", 3),
            link_preview_deadline_secs: parse("LINK_PREVIEW_DEADLINE_SECS", 5),
            link_preview_max_bytes: parse("LINK_PREVIEW_MAX_BYTES", 512_000),
            max_concurrent_previews: parse("MAX_CONCURRENT_PREVIEWS", 16),
            link_preview_rate_trusted: parse("LINK_PREVIEW_RATE_TRUSTED", 600),
            link_preview_rate_untrusted: parse("LINK_PREVIEW_RATE_UNTRUSTED", 20),
            link_preview_rate_window_secs: parse("LINK_PREVIEW_RATE_WINDOW_SECS", 60),
            // Clamped, not just defaulted: 0 reads nothing and silently drops
            // every caller into the shared peer bucket, which is the bug
            // counting from the right exists to avoid.
            trusted_proxy_hops: parse::<usize>("TRUSTED_PROXY_HOPS", 2).max(1),
            allow_loopback_preview_targets: false,
            assets_dir: var("ASSETS_DIR", "assets"),
            font_family: var("FONT_FAMILY", "Figtree"),
        }
    }

    /// The router's whole-request budget. Card assembly and the avatar fetch
    /// are sequential and separately bounded, so it has to cover both or the
    /// timeout layer 504s a render that was going to succeed.
    ///
    /// Lives here rather than inline in `build_router` so the config tests can
    /// assert other deadlines fit inside it without restating the formula.
    pub fn router_timeout_secs(&self) -> u64 {
        self.request_deadline_secs + self.avatar_timeout_secs + 2
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_coherent() {
        let c = Config::from_env();
        // The canonical origin must never carry a trailing slash, or every
        // emitted URL gets a double one.
        assert!(!c.app_base_url.ends_with('/'));
        assert!(!c.api_base_url.ends_with('/'));
        // The HTML must expire long before the image it points at, or a
        // changed `?v=` would never be advertised.
        assert!(c.html_cache_max_age < c.image_cache_max_age);
        // Provisional cards must expire sooner than settled ones.
        assert!(c.provisional_ttl_secs <= c.cache_ttl_secs);
        // The per-call timeout has to fit inside the whole-request budget.
        assert!(c.fetch_timeout_secs <= c.request_deadline_secs);
        // Same for a link preview: per hop inside the request deadline, and
        // that deadline inside the router's, or the 504 comes from the timeout
        // layer with no Cache-Control instead of from the endpoint.
        assert!(c.link_preview_timeout_secs <= c.link_preview_deadline_secs);
        assert!(c.link_preview_deadline_secs < c.router_timeout_secs());
        // Nothing in the environment may relax the address guard.
        assert!(!c.allow_loopback_preview_targets);
        // The two tiers only mean something if one is a ceiling and the other
        // a throttle; equal rates would make the Sec-Fetch-Site check dead code.
        assert!(c.link_preview_rate_untrusted < c.link_preview_rate_trusted);
        assert!(c.link_preview_rate_window_secs > 0);
        // Counting back zero hops reads nothing and silently falls through to
        // the shared peer address, which is the bug this whole scheme exists
        // to avoid.
        assert!(c.trusted_proxy_hops >= 1);
    }
}
