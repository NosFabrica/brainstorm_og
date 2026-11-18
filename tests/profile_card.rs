//! `/p/{id}`'s timing: a person's page load waits on these tags (the UI includes
//! them in every share page), so a slow `/overview` must cost only what it adds.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::{routing::get, Json, Router};
use brainstorm_og::config::Config;
use brainstorm_og::nip19;
use common::relay::{event, StubRelay};
use http_body_util::BodyExt;
use serde_json::json;
use tower::ServiceExt;

const ALICE: &str = "3bf0c63fcb93463407af97a5e5ee64fa883d107ef9e558472c4eb9aaaefa459d";

/// An API whose `/overview` answers after `delay`, counting the calls.
async fn api(delay: Duration) -> (String, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let app = Router::new().route(
        "/user/{hex}/overview",
        get(move || {
            let seen = seen.clone();
            async move {
                seen.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(delay).await;
                Json(json!({ "data": {
                    "influence": 0.5,
                    "counts": { "followed_by": 12, "following": 34 },
                    "tier": "high"
                }}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), calls)
}

/// As production's has: it never answers in time.
async fn stalled_api() -> (String, Arc<AtomicUsize>) {
    api(Duration::from_secs(30)).await
}

fn kind0_event(pubkey: &str, kind0: &serde_json::Value) -> serde_json::Value {
    event(&"0".repeat(64), pubkey, 0, &kind0.to_string(), json!([]))
}

fn app_with(relay: &StubRelay, api: String) -> Router {
    common::router_with(Config {
        local_relay_url: relay.url.clone(),
        api_base_url: api,
        // The deadline, not the per-call timeout, is what bounds the stall.
        fetch_timeout_secs: 30,
        request_deadline_secs: 1,
        ..common::config()
    })
}

async fn get_timed(app: &Router, uri: &str) -> (StatusCode, String, Duration) {
    let started = Instant::now();
    let res = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        String::from_utf8_lossy(&bytes).into_owned(),
        started.elapsed(),
    )
}

async fn profile(kind0: serde_json::Value) -> (StatusCode, String, Duration) {
    let relay = StubRelay::start(vec![kind0_event(ALICE, &kind0)]).await;
    let app = app_with(&relay, stalled_api().await.0);
    let npub = nip19::npub_from_hex(ALICE).unwrap();
    get_timed(&app, &format!("/meta/p/{npub}")).await
}

#[tokio::test]
async fn a_profile_with_a_bio_does_not_wait_for_counts() {
    let (status, body, took) = profile(json!({ "name": "alice", "about": "hi" })).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(r#"content="alice on Brainstorm""#), "{body}");
    assert!(
        body.contains(r#"property="og:description" content="hi""#),
        "{body}"
    );
    assert!(
        took < Duration::from_millis(800),
        "waited {took:?} for unused counts"
    );
}

#[tokio::test]
async fn counts_that_miss_the_deadline_keep_the_name() {
    // Before: the deadline dropped the kind-0 along with the counts, and the
    // card fell back to a bare npub for a profile that had been found.
    let (status, body, took) = profile(json!({ "name": "alice" })).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(r#"content="alice on Brainstorm""#), "{body}");
    assert!(took < Duration::from_millis(1800), "took {took:?}");
}

#[tokio::test]
async fn a_profile_without_a_bio_shows_its_counts() {
    let relay = StubRelay::start(vec![kind0_event(ALICE, &json!({ "name": "alice" }))]).await;
    let (api, calls) = api(Duration::ZERO).await;
    let app = app_with(&relay, api);
    let npub = nip19::npub_from_hex(ALICE).unwrap();
    for _ in 0..2 {
        let (status, body, _) = get_timed(&app, &format!("/meta/p/{npub}")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("12 followers · 34 following"), "{body}");
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1, "counts are cached");
}

#[tokio::test]
async fn a_profile_with_a_bio_never_asks_for_counts() {
    let relay = StubRelay::start(vec![kind0_event(
        ALICE,
        &json!({ "name": "alice", "about": "hi" }),
    )])
    .await;
    let (api, calls) = stalled_api().await;
    let app = app_with(&relay, api);
    let npub = nip19::npub_from_hex(ALICE).unwrap();
    get_timed(&app, &format!("/meta/p/{npub}")).await;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_notes_author_strip_never_waits_for_counts() {
    // An author with no bio: the profile page would want counts; the note card never does.
    const NOTE: &str = "b9f5441e45ca39179320e0031cfb18e34078673dcc3d3e3a3b3a981760aa5696";
    let relay = StubRelay::start(vec![
        event(NOTE, ALICE, 1, "hello", json!([])),
        kind0_event(ALICE, &json!({ "name": "alice" })),
    ])
    .await;
    let (api, calls) = stalled_api().await;
    let app = app_with(&relay, api);
    let nevent = nip19::Entity::Event { id: NOTE.into() }.canonical();
    let (status, body, took) = get_timed(&app, &format!("/meta/e/{nevent}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains(r#"content="alice on Brainstorm""#), "{body}");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(took < Duration::from_millis(800), "took {took:?}");
}
