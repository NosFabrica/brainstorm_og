//! Router-level tests: the contract a crawler actually sees.
//!
//! Upstreams point at 127.0.0.1:1, which refuses immediately, so every request
//! exercises the degraded path — no relay, no overview — deterministically and
//! without network. That is the case worth pinning: it is what a crawler gets
//! for a pubkey we have never synced, and it must still be a valid card rather
//! than an error.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use brainstorm_og::config::Config;
use http_body_util::BodyExt;
use tower::ServiceExt;

const NPUB: &str = "npub180cvv07tjdrrgpa0j7j7tmnyl2yr6yr7l8j4s3evf6u64th6gkwsyjh6w6";
const APP: &str = "https://brainstorm.test";

fn router() -> axum::Router {
    common::router_with(Config {
        app_base_url: APP.into(),
        link_preview_timeout_secs: 1,
        link_preview_deadline_secs: 2,
        ..common::config()
    })
}

async fn get(uri: &str) -> (StatusCode, Vec<(String, String)>, String) {
    let res = router()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let headers = res
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> &'a str {
    headers
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
        .unwrap_or("")
}

#[tokio::test]
async fn healthz_reports_fonts() {
    let (status, _, body) = get("/healthz").await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["status"], "ok");
    // A fontless build is the one failure invisible from outside, so the count
    // being non-zero is the thing worth asserting.
    assert!(v["faces"].as_u64().unwrap() > 0, "no font faces: {body}");
    assert_eq!(v["font_family"], "Figtree");
}

#[tokio::test]
async fn profile_emits_absolute_meta_from_config() {
    let (status, headers, body) = get(&format!("/p/{NPUB}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(header(&headers, "content-type").starts_with("text/html"));
    assert_eq!(header(&headers, "cache-control"), "public, max-age=300");

    for tag in [
        "og:title",
        "og:description",
        "og:image",
        "og:url",
        "twitter:card",
    ] {
        assert!(body.contains(tag), "missing {tag}");
    }
    // Both must be the CANONICAL origin, and the page URL must be /p/ even
    // though nothing was fetched successfully.
    assert!(
        body.contains(&format!(r#"content="{APP}/p/{NPUB}""#)),
        "{body}"
    );
    assert!(
        body.contains(&format!(r#"href="{APP}/p/{NPUB}""#)),
        "{body}"
    );
    // The image URL carries a content hash.
    assert!(body.contains(&format!("{APP}/og/{NPUB}.png?v=")), "{body}");
}

/// The Host header must never reach the output. Before this was fixed, a forged
/// header put an attacker's origin into og:image for a Brainstorm link.
#[tokio::test]
async fn forged_host_headers_cannot_reach_the_output() {
    for (name, value) in [
        ("host", "evil.example"),
        ("x-forwarded-host", "evil.example"),
    ] {
        let res = router()
            .oneshot(
                Request::builder()
                    .uri(format!("/p/{NPUB}"))
                    .header(name, value)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8_lossy(&bytes);
        assert!(
            !body.contains("evil.example"),
            "{name} leaked into the output"
        );
        assert!(body.contains(&format!("{APP}/p/{NPUB}")));
    }
}

/// A profile with nothing behind it still renders an identity rather than a
/// blank — the npub, truncated, exactly as SharePage does.
#[tokio::test]
async fn unknown_profile_degrades_to_a_truncated_npub() {
    let (status, _, body) = get(&format!("/p/{NPUB}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("npub180cvv07"),
        "expected npub fallback: {body}"
    );
}

#[tokio::test]
async fn og_image_is_a_png_and_immutable() {
    let (status, headers, _) = get(&format!("/og/{NPUB}.png")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(header(&headers, "content-type"), "image/png");
    let cc = header(&headers, "cache-control");
    assert!(cc.contains("immutable"), "not immutable: {cc}");
}

/// `?v=` exists to change the URL, never to be read — the handler must ignore
/// it and still serve the card.
#[tokio::test]
async fn og_image_ignores_the_version_query() {
    let (status, _, _) = get(&format!("/og/{NPUB}.png?v=whatever")).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn png_magic_bytes() {
    let res = router()
        .oneshot(
            Request::builder()
                .uri(format!("/og/{NPUB}.png"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "not a PNG");
    assert!(
        bytes.len() > 1000,
        "suspiciously small card: {}",
        bytes.len()
    );
}

/// Both routes agree on 400, and say so cacheably — a malformed id never
/// becomes valid. They used to disagree (404 vs 400).
#[tokio::test]
async fn malformed_ids_are_cacheable_400s() {
    for uri in [
        "/p/not-an-npub",
        "/og/not-an-npub.png",
        "/p/deadbeef",
        "/profile/xxx",
    ] {
        let (status, headers, _) = get(uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
        assert!(
            header(&headers, "cache-control").contains("max-age"),
            "{uri}"
        );
    }
    // An nsec must never be accepted as an identifier to look up and render.
    let (status, _, _) =
        get("/p/nsec1vl029mgpspedva04g90vltkh6fvh240zqtv9k0t9af8935ke9laqsnlfe5").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// nginx's `^/p/[^/]+/?$` accepts a trailing slash, so the service must too —
/// axum does not match it implicitly, and this 404'd before it was fixed.
#[tokio::test]
async fn trailing_slash_is_accepted_on_both_routes() {
    for uri in [format!("/p/{NPUB}/"), format!("/profile/{NPUB}/")] {
        let (status, _, body) = get(&uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert!(body.contains("og:image"), "{uri}");
    }
}

/// Reached via /profile/, the canonical still points at /p/ — old links in the
/// wild must consolidate rather than entrench the deprecated route.
#[tokio::test]
async fn legacy_route_canonicalises_to_p() {
    let (status, _, body) = get(&format!("/profile/{NPUB}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains(&format!(r#"href="{APP}/p/{NPUB}""#)),
        "{body}"
    );
    assert!(
        !body.contains(&format!("{APP}/profile/")),
        "leaked the legacy path"
    );
}

/// The render route is concurrency-limited; the limiter queues rather than
/// rejects, so every request must still succeed.
#[tokio::test]
async fn concurrent_renders_all_succeed() {
    let app = router();
    let reqs = (0..12).map(|_| {
        app.clone().oneshot(
            Request::builder()
                .uri(format!("/og/{NPUB}.png"))
                .body(Body::empty())
                .unwrap(),
        )
    });
    for res in futures_util::future::join_all(reqs).await {
        assert_eq!(res.unwrap().status(), StatusCode::OK);
    }
}
