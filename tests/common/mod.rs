//! One `Config` for every integration test, so a new field is added once.
//! Suites change only what they are about.

#![allow(dead_code)]

use axum::Router;
use brainstorm_og::{build_router, config::Config, state::AppState};

/// Upstreams point at `127.0.0.1:1`, which refuses instantly — the degraded
/// path, deterministic and offline. Loopback preview targets are refused, as in
/// every deployment; suites with a local stub site opt in.
pub fn config() -> Config {
    Config {
        bind_addr: "127.0.0.1:0".into(),
        api_base_url: "http://127.0.0.1:1".into(),
        app_base_url: "https://brainstorm.test".into(),
        local_relay_url: "ws://127.0.0.1:1".into(),
        card_cache_capacity: 16,
        png_cache_max_bytes: 8 * 1024 * 1024,
        cache_ttl_secs: 60,
        html_cache_max_age: 300,
        image_cache_max_age: 31_536_000,
        provisional_ttl_secs: 5,
        fetch_timeout_secs: 1,
        avatar_timeout_secs: 1,
        request_deadline_secs: 2,
        avatar_max_bytes: 1024 * 1024,
        max_concurrent_renders: 4,
        render_epoch: "test".into(),
        assets_dir: "assets".into(),
        font_family: "Figtree".into(),
        link_preview_timeout_secs: 3,
        link_preview_deadline_secs: 5,
        link_preview_max_bytes: 64 * 1024,
        max_concurrent_previews: 4,
        link_preview_cache_ttl_secs: 86_400,
        robots_cache_ttl_secs: 86_400,
        robots_cache_capacity: 4_096,
        robots_timeout_secs: 2,
        link_preview_cache_max_bytes: 1024 * 1024,
        // High and alike: only tests/rate_limit.rs is about the limiter.
        link_preview_rate_trusted: 600,
        link_preview_rate_untrusted: 600,
        link_preview_rate_window_secs: 60,
        trusted_proxy_hops: 2,
        allow_loopback_preview_targets: false,
    }
}

/// `config()` with loopback targets allowed, for suites that run a stub site.
/// No environment variable reaches this seam.
pub fn stub_config() -> Config {
    Config {
        allow_loopback_preview_targets: true,
        ..config()
    }
}

pub fn router_with(config: Config) -> Router {
    build_router(AppState::new(config).expect("fonts must load from assets/"))
}
