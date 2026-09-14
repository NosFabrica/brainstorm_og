//! Short share links must unfurl, or they preview as nothing.
//!
//! Crawlers don't run JavaScript, and `/s/{code}` resolves client-side, so
//! without a server-rendered card every shared short link is a blank preview —
//! which is most of the point of a share link.
//!
//! A stub API stands in for brainstorm_server so the resolve hop is exercised
//! for real. Relay and overview upstreams still point at a refused port, so the
//! card is the degraded one — that is the case worth pinning, since it is what
//! a crawler gets for a profile we have never synced.
//!
//! Issue: .scratch/shorturl/issues/06-unfurl.md

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::{routing::get, Json, Router};
use brainstorm_og::config::Config;
use http_body_util::BodyExt;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tower::ServiceExt;

const HEX: &str = "3bf0c63fcb93463407af97a5e5ee64fa883d107ef9e558472c4eb9aaaefa459d";
const APP: &str = "https://brainstorm.test";
const CODE: &str = "AB3XK9QZ";

/// A stand-in for `GET /shorturl/{code}`. Counts hits so caching is observable.
async fn stub_api() -> (String, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();

    let app = Router::new().route(
        "/shorturl/{code}",
        get(
            move |axum::extract::Path(code): axum::extract::Path<String>| {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    if code.to_uppercase() == CODE {
                        Ok(Json(json!({ "data": { "pubkey": HEX, "relays": [] } })))
                    } else {
                        Err(StatusCode::NOT_FOUND)
                    }
                }
            },
        ),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), hits)
}

fn router(api_base_url: String) -> axum::Router {
    common::router_with(Config {
        api_base_url,
        app_base_url: APP.into(),
        link_preview_timeout_secs: 1,
        link_preview_deadline_secs: 2,
        ..common::config()
    })
}

async fn get_on(app: &axum::Router, uri: &str) -> (StatusCode, String) {
    let res = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

#[tokio::test]
async fn a_short_link_unfurls_to_a_real_card() {
    let (api, _) = stub_api().await;
    let (status, body) = get_on(&router(api), &format!("/s/{CODE}")).await;

    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("og:title"), "no title: {body}");
    assert!(body.contains("og:image"), "no image: {body}");
}

#[tokio::test]
async fn the_uppercase_form_a_qr_produces_also_unfurls() {
    // nginx proxies the original URI, so an uppercase `/S/` arrives here as-is.
    let (api, _) = stub_api().await;
    let (status, body) = get_on(&router(api), &format!("/S/{CODE}")).await;

    assert_eq!(status, StatusCode::OK, "uppercase path did not route");
    assert!(body.contains("og:title"));
}

#[tokio::test]
async fn the_card_points_at_the_profile_not_the_short_link() {
    // Canonical names the resource, not a redirect to it — and as an npub, so a
    // short-link share and a plain npub share consolidate for crawlers.
    let (api, _) = stub_api().await;
    let (_, body) = get_on(&router(api), &format!("/s/{CODE}")).await;

    let npub = brainstorm_og::nip19::npub_from_hex(HEX).expect("hex must encode");
    assert!(
        body.contains(&format!(r#"<link rel="canonical" href="{APP}/p/{npub}"/>"#)),
        "canonical should be the profile url as an npub: {body}"
    );
    assert!(
        !body.contains("/s/"),
        "no short link should appear in the meta"
    );
}

#[tokio::test]
async fn an_unknown_code_does_not_claim_a_profile() {
    let (api, _) = stub_api().await;
    let (status, body) = get_on(&router(api), "/s/ZZZZZZZZ").await;

    assert_ne!(status, StatusCode::OK);
    assert!(!body.contains("og:title"), "invented a card: {body}");
}

#[tokio::test]
async fn the_api_being_down_does_not_claim_a_profile() {
    let (status, body) = get_on(&router("http://127.0.0.1:1".into()), &format!("/s/{CODE}")).await;

    assert_ne!(status, StatusCode::OK);
    assert!(!body.contains("og:title"));
}

#[tokio::test]
async fn a_resolved_code_is_only_looked_up_once() {
    // The mapping is immutable once minted, so re-resolving it per crawler hit
    // would be pure waste — and crawlers arrive in bursts.
    let (api, hits) = stub_api().await;
    let app = router(api);

    for _ in 0..3 {
        get_on(&app, &format!("/s/{CODE}")).await;
    }

    assert_eq!(hits.load(Ordering::SeqCst), 1, "resolution was not cached");
}

#[tokio::test]
async fn confusable_variants_share_one_cache_entry() {
    // The server folds I/L -> 1 and O -> 0 when resolving, so a code retyped
    // with an I for a 1 is the same mapping and must not be a second lookup.
    let (api, hits) = stub_api().await;
    let app = router(api);

    get_on(&app, "/s/AB3XK9QZ").await;
    get_on(&app, "/s/AB3XK9QZ".replace('1', "I").as_str()).await;

    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "folded variants missed the cache"
    );
}

#[tokio::test]
async fn case_variants_share_one_cache_entry() {
    let (api, hits) = stub_api().await;
    let app = router(api);

    get_on(&app, &format!("/s/{CODE}")).await;
    get_on(&app, &format!("/s/{}", CODE.to_lowercase())).await;

    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "case variants missed the cache"
    );
}
