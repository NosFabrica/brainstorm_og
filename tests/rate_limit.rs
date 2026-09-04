//! `/link-preview` is unauthenticated and fetches arbitrary URLs, so the
//! limiter is the only thing between it and being an open fetch proxy.
//!
//! The part worth testing hard is *which address a request is counted
//! against*: the ingress appends to `X-Forwarded-For` rather than replacing
//! it, so the leftmost entry is attacker-controlled and reading it would let
//! anyone rotate a header and bypass the limit outright.
//!
//! Issue: .scratch/link-preview/issues/04-rate-limit-and-client-ip.md

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{header, Request, StatusCode};
use axum::{routing::get, Router};
use brainstorm_og::{build_router, config::Config, state::AppState};
use http_body_util::BodyExt;
use std::net::SocketAddr;
use std::time::Duration;
use tower::ServiceExt;

const APP: &str = "https://brainstorm.test";
const HTML: &str = "<!doctype html><html><head><title>Stub</title></head><body>hi</body></html>";

/// The direct peer axum reports. Behind the ingress this is the UI's nginx
/// pod — shared by every caller, which is why falling back to it is loud.
const PEER: &str = "10.9.9.9:40000";

/// A page to preview. Nothing here is about fetching, so it is the smallest
/// thing that produces a 200.
async fn stub_site() -> String {
    let app = Router::new()
        .route(
            "/ok",
            get(|| async { ([(header::CONTENT_TYPE, "text/html")], HTML) }),
        )
        .route(
            "/slow",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(5)).await;
                ([(header::CONTENT_TYPE, "text/html")], HTML)
            }),
        );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

fn test_config() -> Config {
    Config {
        bind_addr: "127.0.0.1:0".into(),
        api_base_url: "http://127.0.0.1:1".into(),
        app_base_url: APP.into(),
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
        link_preview_rate_trusted: 5,
        link_preview_rate_untrusted: 2,
        link_preview_rate_window_secs: 60,
        trusted_proxy_hops: 2,
        allow_loopback_preview_targets: true,
    }
}

fn router_with(config: Config) -> Router {
    build_router(AppState::new(config).expect("fonts must load from assets/"))
}

/// One preview, with whatever headers the caller wants to claim. `ConnectInfo`
/// is inserted the way `into_make_service_with_connect_info` does in `main`.
async fn preview(app: &Router, target: &str, headers: &[(&str, &str)]) -> StatusCode {
    request(
        app,
        &format!("/link-preview?url={}", percent_encode(target)),
        headers,
    )
    .await
    .0
}

async fn request(
    app: &Router,
    uri: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, String, serde_json::Value) {
    let mut builder = Request::builder().uri(uri);
    for (k, v) in headers {
        builder = builder.header(*k, *v);
    }
    let mut req = builder.body(Body::empty()).unwrap();
    req.extensions_mut()
        .insert(ConnectInfo(PEER.parse::<SocketAddr>().unwrap()));

    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let cache = res
        .headers()
        .get(header::CACHE_CONTROL)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, cache, body)
}

fn percent_encode(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            c => c.to_string().bytes().map(|b| format!("%{b:02X}")).collect(),
        })
        .collect()
}

/// A chain the way the ingress and the UI's nginx build it: the client's own
/// header, then the ingress's view of it, then the UI nginx's.
fn forwarded(client_claims: &str, real: &str) -> String {
    format!("{client_claims}, {real}, 10.42.0.9")
}

#[tokio::test]
async fn untrusted_traffic_is_limited_at_its_own_rate() {
    let stub = stub_site().await;
    let app = router_with(test_config());
    let url = format!("{stub}/ok");
    let chain = forwarded("1.2.3.4", "198.51.100.4");
    let hdrs = vec![("x-forwarded-for", chain.as_str())];

    // The configured untrusted rate is 2.
    for i in 1..=2 {
        assert_eq!(
            preview(&app, &url, &hdrs).await,
            StatusCode::OK,
            "request {i}"
        );
    }

    let (status, cache, body) = request(
        &app,
        &format!("/link-preview?url={}", percent_encode(&url)),
        &hdrs,
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    // A 429 is about this caller right now; a shared cache storing it would
    // hand someone else's throttling to an unrelated user.
    assert_eq!(cache, "no-store");
    assert_eq!(body["code"], 429);
    assert_eq!(body["data"], serde_json::Value::Null);
}

#[tokio::test]
async fn same_origin_traffic_is_counted_separately_and_higher() {
    let stub = stub_site().await;
    let app = router_with(test_config());
    let url = format!("{stub}/ok");
    let chain = forwarded("1.2.3.4", "198.51.100.4");
    let other: Vec<(&str, &str)> = vec![("x-forwarded-for", &chain)];
    let spa: Vec<(&str, &str)> = vec![
        ("x-forwarded-for", &chain),
        ("sec-fetch-site", "same-origin"),
    ];

    // Spend the untrusted bucket for this address entirely.
    for _ in 0..2 {
        assert_eq!(preview(&app, &url, &other).await, StatusCode::OK);
    }
    assert_eq!(
        preview(&app, &url, &other).await,
        StatusCode::TOO_MANY_REQUESTS
    );

    // The SPA's own traffic from the same address is untouched by that, and
    // gets the higher ceiling — the whole point of the two tiers.
    for i in 1..=5 {
        assert_eq!(
            preview(&app, &url, &spa).await,
            StatusCode::OK,
            "request {i}"
        );
    }
    assert_eq!(
        preview(&app, &url, &spa).await,
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[tokio::test]
async fn a_referer_under_the_app_base_url_earns_the_higher_ceiling() {
    let stub = stub_site().await;
    let app = router_with(test_config());
    let url = format!("{stub}/ok");
    let chain = forwarded("1.2.3.4", "198.51.100.4");

    // Third past the untrusted rate of 2, so a plain caller would be refused.
    for _ in 0..3 {
        let hdrs = vec![
            ("x-forwarded-for", chain.as_str()),
            ("referer", "https://brainstorm.test/p/npub1abc"),
        ];
        assert_eq!(preview(&app, &url, &hdrs).await, StatusCode::OK);
    }

    // A lookalike host does not: prefix-matching `APP_BASE_URL` would let
    // `brainstorm.test.evil.example` claim our ceiling.
    let evil = forwarded("1.2.3.4", "198.51.100.99");
    for _ in 0..2 {
        let hdrs = vec![
            ("x-forwarded-for", evil.as_str()),
            ("referer", "https://brainstorm.test.evil.example/"),
        ];
        assert_eq!(preview(&app, &url, &hdrs).await, StatusCode::OK);
    }
    let hdrs = vec![
        ("x-forwarded-for", evil.as_str()),
        ("referer", "https://brainstorm.test.evil.example/"),
    ];
    assert_eq!(
        preview(&app, &url, &hdrs).await,
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[tokio::test]
async fn a_forged_leftmost_x_forwarded_for_does_not_shift_the_bucket() {
    // The bypass this whole scheme exists to stop: the ingress *appends*, so a
    // client can put anything it likes at the front of the chain. Rotating it
    // per request must not mint a fresh bucket.
    let stub = stub_site().await;
    let app = router_with(test_config());
    let url = format!("{stub}/ok");

    let mut statuses = Vec::new();
    for forged in ["1.1.1.1", "2.2.2.2", "3.3.3.3", "4.4.4.4"] {
        let chain = forwarded(forged, "198.51.100.4");
        let hdrs = vec![("x-forwarded-for", chain.as_str())];
        statuses.push(preview(&app, &url, &hdrs).await);
    }

    assert_eq!(
        statuses,
        vec![
            StatusCode::OK,
            StatusCode::OK,
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::TOO_MANY_REQUESTS,
        ],
        "a rotating leftmost entry bought extra requests"
    );
}

#[tokio::test]
async fn the_trusted_hop_count_is_configurable() {
    let stub = stub_site().await;
    let url = format!("{stub}/ok");

    // One hop back reads the rightmost entry instead, so two callers the
    // default would separate now share a bucket.
    let mut config = test_config();
    config.trusted_proxy_hops = 1;
    let app = router_with(config);

    let a = forwarded("1.2.3.4", "198.51.100.4");
    let b = forwarded("5.6.7.8", "203.0.113.7");
    assert_eq!(
        preview(&app, &url, &[("x-forwarded-for", a.as_str())]).await,
        StatusCode::OK
    );
    assert_eq!(
        preview(&app, &url, &[("x-forwarded-for", b.as_str())]).await,
        StatusCode::OK
    );
    assert_eq!(
        preview(&app, &url, &[("x-forwarded-for", b.as_str())]).await,
        StatusCode::TOO_MANY_REQUESTS,
        "hops=1 should have keyed both callers on the shared 10.42.0.9 entry"
    );
}

#[tokio::test]
async fn an_over_limit_request_is_refused_without_waiting_for_a_fetch_permit() {
    // Layer order. With the limiter inside the concurrency cap instead of
    // outside it, this 429 would arrive only once the in-flight fetch below
    // released its permit — turning a cheap rejection into a queue an attacker
    // can fill.
    let stub = stub_site().await;
    let mut config = test_config();
    config.max_concurrent_previews = 1;
    config.link_preview_rate_untrusted = 1;
    let app = router_with(config);
    let chain = forwarded("1.2.3.4", "198.51.100.4");
    let hdrs = vec![("x-forwarded-for", chain.as_str())];

    // Spends the bucket and takes the only permit, for five seconds.
    let held = app.clone();
    let slow = format!("{stub}/slow");
    let chain_owned = chain.clone();
    tokio::spawn(async move {
        preview(&held, &slow, &[("x-forwarded-for", chain_owned.as_str())]).await;
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    let refused = tokio::time::timeout(
        Duration::from_secs(1),
        preview(&app, &format!("{stub}/ok"), &hdrs),
    )
    .await
    .expect("the 429 queued behind the in-flight fetch");
    assert_eq!(refused, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn a_chain_shorter_than_the_hop_count_falls_back_to_the_peer() {
    let stub = stub_site().await;
    let app = router_with(test_config());
    let url = format!("{stub}/ok");

    // Only one entry, but two hops are configured. Both callers collapse onto
    // the shared peer address rather than going uncounted.
    for (i, claimed) in ["9.9.9.9", "8.8.8.8", "7.7.7.7"].iter().enumerate() {
        let expected = if i < 2 {
            StatusCode::OK
        } else {
            StatusCode::TOO_MANY_REQUESTS
        };
        assert_eq!(
            preview(&app, &url, &[("x-forwarded-for", claimed)]).await,
            expected,
            "request {i}"
        );
    }
}

#[tokio::test]
async fn the_health_check_is_not_queued_behind_in_flight_previews() {
    let stub = stub_site().await;
    let mut config = test_config();
    config.max_concurrent_previews = 1;
    config.link_preview_rate_trusted = 100;
    let app = router_with(config);

    let slow = format!("{stub}/slow");
    for _ in 0..2 {
        let app = app.clone();
        let slow = slow.clone();
        tokio::spawn(async move {
            preview(&app, &slow, &[("sec-fetch-site", "same-origin")]).await;
        });
    }
    // Long enough for both to be polled, so the single permit is taken and the
    // second request is genuinely waiting for it.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let health = tokio::time::timeout(Duration::from_secs(1), request(&app, "/healthz", &[]))
        .await
        .expect("/healthz queued behind the in-flight previews");
    assert_eq!(health.0, StatusCode::OK);
}

#[tokio::test]
async fn the_window_resets() {
    // A fixed window, not a permanent ban: the first request of a window
    // creates the counter and its expiry, and the next window starts clean.
    let stub = stub_site().await;
    let mut config = test_config();
    config.link_preview_rate_window_secs = 1;
    let app = router_with(config);
    let url = format!("{stub}/ok");
    let chain = forwarded("1.2.3.4", "198.51.100.4");
    let hdrs = vec![("x-forwarded-for", chain.as_str())];

    for _ in 0..2 {
        assert_eq!(preview(&app, &url, &hdrs).await, StatusCode::OK);
    }
    assert_eq!(
        preview(&app, &url, &hdrs).await,
        StatusCode::TOO_MANY_REQUESTS
    );

    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(
        preview(&app, &url, &hdrs).await,
        StatusCode::OK,
        "the bucket outlived its window"
    );
}
