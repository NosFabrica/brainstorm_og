use std::env;

/// Runtime configuration, all from env so the same binary works in compose and k8s.
#[derive(Clone, Debug)]
pub struct Config {
    pub bind_addr: String,
    /// brainstorm-server base (overview stats). No trailing slash.
    pub api_base_url: String,
    /// Public app origin used in absolute og:url / og:image. No trailing slash.
    pub app_base_url: String,
    /// In-cluster relay that already ingests kind-0 (tried first).
    pub local_relay_url: String,
    /// Public profile relays, tried after the local relay / nprofile hints.
    pub fallback_relays: Vec<String>,
    pub card_cache_capacity: u64,
    pub png_cache_max_bytes: u64,
    pub cache_ttl_secs: u64,
    pub http_cache_max_age: u64,
    pub fetch_timeout_secs: u64,
    pub avatar_timeout_secs: u64,
    /// Directory with bundled font(s) and the default avatar/logo.
    pub assets_dir: String,
}

fn var(key: &str, default: &str) -> String {
    env::var(key).ok().filter(|s| !s.is_empty()).unwrap_or_else(|| default.to_string())
}

fn parse<T: std::str::FromStr>(key: &str, default: T) -> T {
    env::var(key).ok().and_then(|s| s.parse().ok()).unwrap_or(default)
}

impl Config {
    pub fn from_env() -> Self {
        let fallback = var(
            "FALLBACK_RELAYS",
            "wss://purplepag.es,wss://relay.damus.io,wss://nos.lol,wss://relay.primal.net,wss://nostr.wine",
        );
        Config {
            bind_addr: var("BIND_ADDR", "0.0.0.0:8080"),
            api_base_url: var("API_BASE_URL", "http://brainstorm-server:8000")
                .trim_end_matches('/')
                .to_string(),
            app_base_url: var("APP_BASE_URL", "https://brainstorm.nosfabrica.com")
                .trim_end_matches('/')
                .to_string(),
            local_relay_url: var("LOCAL_RELAY_URL", "ws://strfry:7777"),
            fallback_relays: fallback
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            card_cache_capacity: parse("CACHE_MAX_ENTRIES", 500),
            png_cache_max_bytes: parse("CACHE_MAX_BYTES", 128 * 1024 * 1024),
            cache_ttl_secs: parse("CACHE_TTL_SECS", 3600),
            http_cache_max_age: parse("HTTP_CACHE_MAX_AGE", 86_400),
            fetch_timeout_secs: parse("FETCH_TIMEOUT_SECS", 6),
            avatar_timeout_secs: parse("AVATAR_TIMEOUT_SECS", 4),
            assets_dir: var("ASSETS_DIR", "assets"),
        }
    }
}
