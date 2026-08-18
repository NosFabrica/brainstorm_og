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
            avatar_timeout_secs: parse("AVATAR_TIMEOUT_SECS", 2),
            request_deadline_secs: parse("REQUEST_DEADLINE_SECS", 4),
            avatar_max_bytes: parse("AVATAR_MAX_BYTES", 5 * 1024 * 1024),
            assets_dir: var("ASSETS_DIR", "assets"),
            font_family: var("FONT_FAMILY", "Figtree"),
        }
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
    }
}
