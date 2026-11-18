//! `/p/{id}`'s timing: a person's page load waits on these tags (the UI includes
//! them in every share page), so a slow `/overview` must cost only what it adds.

mod common;

use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::{routing::get, Router};
use brainstorm_og::config::Config;
use brainstorm_og::nip19;
use common::relay::{event, StubRelay};
use http_body_util::BodyExt;
use serde_json::json;
use tower::ServiceExt;

const ALICE: &str = "3bf0c63fcb93463407af97a5e5ee64fa883d107ef9e558472c4eb9aaaefa459d";

/// An API whose `/overview` never answers in time, as production's has not.
async fn stalled_api() -> String {
    let app = Router::new().route(
        "/user/{hex}/overview",
        get(|| async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            "{}"
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

async fn profile(kind0: serde_json::Value) -> (StatusCode, String, Duration) {
    let relay = StubRelay::start(vec![event(
        &"0".repeat(64),
        ALICE,
        0,
        &kind0.to_string(),
        json!([]),
    )])
    .await;
    let app = common::router_with(Config {
        local_relay_url: relay.url.clone(),
        api_base_url: stalled_api().await,
        // The deadline, not the per-call timeout, is what bounds the stall.
        fetch_timeout_secs: 30,
        request_deadline_secs: 1,
        ..common::config()
    });
    let npub = nip19::npub_from_hex(ALICE).unwrap();
    let started = Instant::now();
    let res = app
        .oneshot(
            Request::builder()
                .uri(format!("/meta/p/{npub}"))
                .body(Body::empty())
                .unwrap(),
        )
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
