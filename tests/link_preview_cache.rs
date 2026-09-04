//! What the response cache has to be true of, observed from outside. The stub
//! counts the requests it actually receives, so "we did not fetch" is measured
//! rather than inferred from a timing.
//!
//! Issue: .scratch/link-preview/issues/05-response-cache-and-normalisation.md

use axum::body::Body;
use axum::extract::RawQuery;
use axum::http::{header, Request, StatusCode};
use axum::{routing::get, Router};
use brainstorm_og::{build_router, config::Config, state::AppState};
use http_body_util::BodyExt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tower::ServiceExt;

const HTML: &str = "<!doctype html><html><head>\
    <meta property=\"og:title\" content=\"Cached\">\
    </head><body>hi</body></html>";

/// A third-party page that remembers who asked and with what.
struct Stub {
    base: String,
    /// Requests reaching `/counted` and `/fail` — the single-flight evidence.
    hits: Arc<AtomicUsize>,
    /// Query strings `/echo` was actually called with.
    queries: Arc<Mutex<Vec<String>>>,
}

impl Stub {
    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

async fn stub_site() -> Stub {
    let hits = Arc::new(AtomicUsize::new(0));
    let queries = Arc::new(Mutex::new(Vec::new()));

    let counted = hits.clone();
    let failing = hits.clone();
    let echoed = queries.clone();

    let app = Router::new()
        .route(
            "/counted",
            get(move || {
                let hits = counted.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    // Long enough that a second caller is certainly inside
                    // `get_with` while the first is still fetching.
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    ([(header::CONTENT_TYPE, "text/html")], HTML)
                }
            }),
        )
        .route(
            "/echo",
            get(move |RawQuery(q): RawQuery| {
                let queries = echoed.clone();
                async move {
                    queries.lock().unwrap().push(q.unwrap_or_default());
                    ([(header::CONTENT_TYPE, "text/html")], HTML)
                }
            }),
        )
        .route(
            "/fail",
            get(move || {
                let hits = failing.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    StatusCode::INTERNAL_SERVER_ERROR
                }
            }),
        );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Stub {
        base: format!("http://{addr}"),
        hits,
        queries,
    }
}

fn config_with(ttl_secs: u64, untrusted_rate: u32) -> Config {
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
        link_preview_cache_ttl_secs: ttl_secs,
        link_preview_cache_max_bytes: 1024 * 1024,
        link_preview_rate_trusted: 600,
        link_preview_rate_untrusted: untrusted_rate,
        link_preview_rate_window_secs: 60,
        trusted_proxy_hops: 2,
        allow_loopback_preview_targets: true,
    }
}

/// One router, so every request in a test shares one cache. Building a fresh
/// one per request — as the other suites do — would make every fetch a miss.
fn router_with(config: Config) -> Router {
    build_router(AppState::new(config).expect("fonts must load from assets/"))
}

/// A day's TTL and a rate nothing here will meet.
fn router() -> Router {
    router_with(config_with(86_400, 600))
}

async fn request(app: &Router, uri: &str) -> (StatusCode, serde_json::Value, String) {
    let res = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let cache = res
        .headers()
        .get(header::CACHE_CONTROL)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, body, cache)
}

async fn preview(app: &Router, target: &str) -> (StatusCode, serde_json::Value, String) {
    request(
        app,
        &format!("/link-preview?url={}", percent_encode(target)),
    )
    .await
}

fn percent_encode(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            c => c.to_string().bytes().map(|b| format!("%{b:02X}")).collect(),
        })
        .collect()
}

#[tokio::test]
async fn a_burst_on_one_cold_url_makes_a_single_upstream_fetch() {
    let stub = stub_site().await;
    let app = router();
    let url = format!("{}/counted", stub.base);

    let (a, b, c) = tokio::join!(
        preview(&app, &url),
        preview(&app, &url),
        preview(&app, &url),
    );

    for (status, body, _) in [&a, &b, &c] {
        assert_eq!(*status, StatusCode::OK);
        assert_eq!(body["data"]["title"], "Cached");
    }
    assert_eq!(
        stub.hits(),
        1,
        "three concurrent callers fanned out into {} fetches",
        stub.hits()
    );
}

#[tokio::test]
async fn a_repeat_within_the_ttl_does_not_reach_upstream_at_all() {
    let stub = stub_site().await;
    let app = router();
    let url = format!("{}/counted", stub.base);

    let (status, _, _) = preview(&app, &url).await;
    assert_eq!(status, StatusCode::OK);

    for _ in 0..5 {
        let (status, body, _) = preview(&app, &url).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["data"]["title"], "Cached");
    }
    assert_eq!(stub.hits(), 1, "the repeats re-fetched");
}

#[tokio::test]
async fn noise_that_does_not_change_the_page_shares_one_entry() {
    let stub = stub_site().await;
    let app = router();
    let base = format!("{}/counted", stub.base);

    for variant in [
        base.clone(),
        format!("{base}#section"),
        format!("{base}?utm_source=nostr&utm_medium=note"),
        format!("{base}?fbclid=abc"),
        format!("{base}?gclid=1&msclkid=2&igshid=3#top"),
    ] {
        let (status, _, _) = preview(&app, &variant).await;
        assert_eq!(status, StatusCode::OK, "for {variant}");
    }
    assert_eq!(stub.hits(), 1, "the variants keyed separate entries");

    // A parameter that might mean something to the site is not noise, so it is
    // a different page and a different entry.
    let (status, _, _) = preview(&app, &format!("{base}?ref=hn")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(stub.hits(), 2, "`ref` was stripped along with the trackers");
}

#[tokio::test]
async fn tracking_parameters_are_stripped_from_the_request_we_send() {
    let stub = stub_site().await;
    let app = router();

    let (status, _, _) = preview(
        &app,
        &format!(
            "{}/echo?utm_source=nostr&id=7&fbclid=abc&utm_campaign=x&page=2#frag",
            stub.base
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let queries = stub.queries.lock().unwrap().clone();
    assert_eq!(
        queries,
        vec!["id=7&page=2".to_string()],
        "the sharer's campaign identifiers were forwarded to the destination"
    );
}

#[tokio::test]
async fn a_failure_is_cached_too_so_a_broken_site_is_not_re_hammered() {
    let stub = stub_site().await;
    let app = router();
    let url = format!("{}/fail", stub.base);

    for _ in 0..3 {
        let (status, _, cache) = preview(&app, &url).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        // Ours to remember, not a shared cache's: the failure TTL is minutes
        // and a downstream cache would hold it for its own idea of a while.
        assert_eq!(cache, "no-store");
    }
    assert_eq!(stub.hits(), 1, "a down site was re-fetched per request");
}

#[tokio::test]
async fn the_response_advertises_the_ttl_we_actually_hold_it_for() {
    let stub = stub_site().await;
    // Not the default, so a reverted config wiring cannot pass by coincidence.
    let app = router_with(config_with(1234, 600));

    let (status, _, cache) = preview(&app, &format!("{}/counted", stub.base)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cache, "public, max-age=1234");
}

#[tokio::test]
async fn a_cached_hit_still_counts_against_the_caller() {
    // The cache sits inside the limiter on purpose: otherwise a small set of
    // popular URLs is a free amplifier. Moving it outward would make the
    // second request here a 200.
    let stub = stub_site().await;
    let app = router_with(config_with(86_400, 2));
    let url = format!("{}/counted", stub.base);

    for expected in [
        StatusCode::OK,
        StatusCode::OK,
        StatusCode::TOO_MANY_REQUESTS,
    ] {
        let (status, _, _) = preview(&app, &url).await;
        assert_eq!(status, expected);
    }
    assert_eq!(
        stub.hits(),
        1,
        "the second request was not served from cache"
    );
}

#[tokio::test]
async fn healthz_measures_every_cache() {
    let stub = stub_site().await;
    let app = router();
    preview(&app, &format!("{}/counted", stub.base)).await;

    // The timeout is a hang guard, not a cost measurement — what keeps this
    // cheap enough for a probe is that `cache_stats` never forces moka's
    // housekeeping, which is a property of the code rather than of a timing.
    let (status, body, _) = tokio::time::timeout(Duration::from_secs(1), request(&app, "/healthz"))
        .await
        .expect("/healthz hung");
    assert_eq!(status, StatusCode::OK);

    let caches = &body["caches"];
    for name in ["card", "short_code", "png", "preview_rate", "link_preview"] {
        assert!(
            caches[name]["entry_count"].is_u64() && caches[name]["weighted_size"].is_u64(),
            "{name} is not reported: {body}"
        );
    }
}
