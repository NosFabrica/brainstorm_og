//! Article cards (kind 30023): what a crawler sees for a shared long-form post.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::{routing::get, Json, Router};
use brainstorm_og::config::Config;
use brainstorm_og::nip19::{self, Entity};
use common::relay::{event, StubRelay};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

const APP: &str = "https://brainstorm.test";
const ALICE: &str = "3bf0c63fcb93463407af97a5e5ee64fa883d107ef9e558472c4eb9aaaefa459d";
const ID: &str = "b9f5441e45ca39179320e0031cfb18e34078673dcc3d3e3a3b3a981760aa5696";
const OLD: &str = "2222222222222222222222222222222222222222222222222222222222222222";

fn kind0(pubkey: &str, name: &str) -> Value {
    event(
        &"0".repeat(64),
        pubkey,
        0,
        &json!({ "name": name }).to_string(),
        json!([]),
    )
}

fn article(id: &str, content: &str, tags: Value) -> Value {
    event(id, ALICE, 30023, content, tags)
}

fn naddr(d: &str) -> String {
    Entity::Address {
        kind: 30023,
        pubkey: ALICE.into(),
        d: d.into(),
    }
    .canonical()
}

fn router(relay: &StubRelay, api: Option<String>) -> Router {
    common::router_with(Config {
        app_base_url: APP.into(),
        local_relay_url: relay.url.clone(),
        api_base_url: api.unwrap_or_else(|| "http://127.0.0.1:1".into()),
        ..common::config()
    })
}

async fn get_on(app: &Router, uri: &str) -> (StatusCode, String) {
    let res = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn meta<'a>(body: &'a str, property: &str) -> &'a str {
    let needle = format!(r#"property="{property}" content=""#);
    let start = body
        .find(&needle)
        .unwrap_or_else(|| panic!("no {property}: {body}"))
        + needle.len();
    &body[start..start + body[start..].find('"').unwrap()]
}

#[tokio::test]
async fn an_article_unfurls_by_naddr_with_title_summary_and_author() {
    let relay = StubRelay::start(vec![
        article(
            ID,
            "# Body\n\nlong text",
            json!([
                ["d", "my-post"],
                ["title", "Why Nostr"],
                ["summary", "A short case for relays."],
                ["published_at", "1700000000"],
                ["image", "https://x.test/cover.jpg"]
            ]),
        ),
        kind0(ALICE, "alice"),
    ])
    .await;
    let (status, body) = get_on(&router(&relay, None), &format!("/e/{}", naddr("my-post"))).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(meta(&body, "og:title"), "Why Nostr");
    assert_eq!(meta(&body, "og:description"), "A short case for relays.");
    assert_eq!(meta(&body, "og:type"), "article");
    assert_eq!(
        meta(&body, "article:published_time"),
        "2023-11-14T22:13:20Z"
    );
    let npub = nip19::npub_from_hex(ALICE).unwrap();
    assert_eq!(meta(&body, "article:author"), format!("{APP}/p/{npub}"));
    assert!(body.contains(r#"name="twitter:card" content="summary_large_image""#));
    assert_eq!(
        meta(&body, "og:url"),
        format!("{APP}/e/{}", naddr("my-post"))
    );
}

#[tokio::test]
async fn an_article_without_a_title_is_skipped() {
    for tags in [json!([["d", "p"]]), json!([["d", "p"], ["title", "  "]])] {
        let relay = StubRelay::start(vec![article(ID, "text", tags), kind0(ALICE, "alice")]).await;
        let (status, body) = get_on(&router(&relay, None), &format!("/e/{}", naddr("p"))).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert!(!body.contains("og:title"));
    }
}

#[tokio::test]
async fn without_a_summary_the_description_is_the_opening_text() {
    let relay = StubRelay::start(vec![
        article(
            ID,
            "Opening line https://x.test/img.png\n\nsecond paragraph",
            json!([["d", "p"], ["title", "T"]]),
        ),
        kind0(ALICE, "alice"),
    ])
    .await;
    let (status, body) = get_on(&router(&relay, None), &format!("/e/{}", naddr("p"))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        meta(&body, "og:description"),
        "Opening line second paragraph"
    );
    assert!(!body.contains("article:published_time"));
}

#[tokio::test]
async fn naddr_reads_the_latest_version() {
    let mut old = article(ID, "", json!([["d", "p"], ["title", "Old"]]));
    old["id"] = json!(OLD);
    let mut new = article(ID, "", json!([["d", "p"], ["title", "New"]]));
    new["created_at"] = json!(1_800_000_000);
    let relay = StubRelay::start(vec![old, new, kind0(ALICE, "alice")]).await;
    let (status, body) = get_on(&router(&relay, None), &format!("/e/{}", naddr("p"))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(meta(&body, "og:title"), "New");
}

#[tokio::test]
async fn an_article_also_unfurls_by_nevent() {
    let relay = StubRelay::start(vec![
        article(ID, "", json!([["d", "p"], ["title", "T"]])),
        kind0(ALICE, "alice"),
    ])
    .await;
    let nevent = Entity::Event { id: ID.into() }.canonical();
    let (status, body) = get_on(&router(&relay, None), &format!("/e/{nevent}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(meta(&body, "og:title"), "T");
}

#[tokio::test]
async fn an_address_code_renders_the_article_card() {
    let codes = Router::new().route(
        "/shorturl/{code}",
        get(|| async {
            Json(json!({ "data": {
                "kind": "address", "key": format!("30023:{ALICE}:p"), "relays": [], "path": "/x"
            }}))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let _ = axum::serve(listener, codes).await;
    });
    let relay = StubRelay::start(vec![
        article(ID, "", json!([["d", "p"], ["title", "T"]])),
        kind0(ALICE, "alice"),
    ])
    .await;
    let (status, body) = get_on(&router(&relay, Some(api)), "/s/CODE").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(meta(&body, "og:title"), "T");
    assert_eq!(meta(&body, "og:url"), format!("{APP}/e/{}", naddr("p")));
}

#[tokio::test]
async fn an_unreachable_cover_image_still_renders_a_card() {
    let relay = StubRelay::start(vec![
        article(
            ID,
            "",
            json!([
                ["d", "p"],
                ["title", "T"],
                ["image", "http://127.0.0.1:1/x.png"]
            ]),
        ),
        kind0(ALICE, "alice"),
    ])
    .await;
    let res = router(&relay, None)
        .oneshot(
            Request::builder()
                .uri(format!("/og/e/{}.png?v=x", naddr("p")))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()["content-type"], "image/png");
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
}
