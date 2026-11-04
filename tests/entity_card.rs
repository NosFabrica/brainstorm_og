//! `/e/{id}` and `/s/{code}` for events: what a crawler sees for a shared note.

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
const BOB: &str = "82341f882b6eabcd2ba7f1ef90aad961cf074af15b9ef44a09f9d2a8fbfbe6a2";
const NOTE: &str = "b9f5441e45ca39179320e0031cfb18e34078673dcc3d3e3a3b3a981760aa5696";
const OTHER: &str = "1111111111111111111111111111111111111111111111111111111111111111";

fn kind0(pubkey: &str, name: &str) -> Value {
    event(
        &format!("{:0>64}", name.len()),
        pubkey,
        0,
        &json!({ "name": name }).to_string(),
        json!([]),
    )
}

fn note(id: &str, content: &str, tags: Value) -> Value {
    event(id, ALICE, 1, content, tags)
}

fn nevent(id: &str) -> String {
    Entity::Event { id: id.into() }.canonical()
}

fn router(relay: &StubRelay, api: Option<String>) -> Router {
    common::router_with(Config {
        app_base_url: APP.into(),
        local_relay_url: relay.url.clone(),
        api_base_url: api.unwrap_or_else(|| "http://127.0.0.1:1".into()),
        ..common::config()
    })
}

async fn get_on(app: &Router, uri: &str) -> (StatusCode, String, String) {
    let res = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let cache = res
        .headers()
        .get("cache-control")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, cache, String::from_utf8_lossy(&bytes).into_owned())
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
async fn a_note_unfurls_with_its_text_and_author() {
    let bob = nip19::npub_from_hex(BOB).unwrap();
    let relay = StubRelay::start(vec![
        note(
            NOTE,
            &format!("gm nostr:{bob} https://x.com/a.png see you"),
            json!([]),
        ),
        kind0(ALICE, "alice"),
        kind0(BOB, "bob"),
    ])
    .await;
    let (status, cache, body) =
        get_on(&router(&relay, None), &format!("/e/{}", nevent(NOTE))).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(cache, "public, max-age=300");
    assert_eq!(meta(&body, "og:title"), "alice on Brainstorm");
    assert_eq!(meta(&body, "og:description"), "gm @bob see you");
    assert_eq!(meta(&body, "og:type"), "article");
    assert!(body.contains(r#"name="twitter:card" content="summary_large_image""#));
    let canonical = format!("{APP}/e/{}", nevent(NOTE));
    assert!(
        body.contains(&format!(r#"<link rel="canonical" href="{canonical}"/>"#)),
        "{body}"
    );
    assert_eq!(meta(&body, "og:url"), canonical);
    assert!(
        meta(&body, "og:image").starts_with(&format!("{APP}/og/e/{}.png?v=", nevent(NOTE))),
        "{body}"
    );
}

#[tokio::test]
async fn note_ids_and_hinted_nevents_share_the_hint_free_canonical() {
    let relay = StubRelay::start(vec![note(NOTE, "hello", json!([])), kind0(ALICE, "alice")]).await;
    let app = router(&relay, None);
    let note1 = nip19::note_from_hex(NOTE).unwrap();
    let (status, _, body) = get_on(&app, &format!("/e/{note1}/")).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(meta(&body, "og:url"), format!("{APP}/e/{}", nevent(NOTE)));
}

#[tokio::test]
async fn the_image_is_a_png() {
    let relay = StubRelay::start(vec![note(NOTE, "hello", json!([])), kind0(ALICE, "alice")]).await;
    let app = router(&relay, None);
    let res = app
        .oneshot(
            Request::builder()
                .uri(format!("/og/e/{}.png?v=x", nevent(NOTE)))
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

#[tokio::test]
async fn a_missing_note_is_retried_once_then_skipped_briefly() {
    let relay = StubRelay::start(vec![]).await;
    let app = router(&relay, None);
    let (status, cache, body) = get_on(&app, &format!("/e/{}", nevent(NOTE))).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(cache, "public, max-age=60");
    assert!(!body.contains("og:title"));
    assert_eq!(
        relay.filters_with("ids").len(),
        2,
        "expected exactly one retry"
    );

    // Not cached here: once it reaches the relay, the next hit gets a card.
    relay.push(note(NOTE, "hello", json!([])));
    let (status, _, _) = get_on(&app, &format!("/e/{}", nevent(NOTE))).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn a_slow_first_read_and_slow_delete_check_still_draw_the_card() {
    // Worst case: the first read times out, the retry lands, the delete read times out.
    let relay = StubRelay::start(vec![note(NOTE, "hello", json!([])), kind0(ALICE, "alice")]).await;
    relay.stall("ids", 1);
    relay.stall("#e", 1);
    let (status, _, body) = get_on(&router(&relay, None), &format!("/e/{}", nevent(NOTE))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn reads_name_the_spam_lens() {
    let relay = StubRelay::start(vec![note(NOTE, "hello", json!([]))]).await;
    get_on(&router(&relay, None), &format!("/e/{}", nevent(NOTE))).await;
    let filters = relay.filters.lock().unwrap().clone();
    assert!(!filters.is_empty());
    for f in filters {
        assert_eq!(f["search"], "include:spam", "{f}");
    }
}

async fn assert_skipped_and_cached(relay: StubRelay, id: &str) {
    let app = router(&relay, None);
    let (status, cache, body) = get_on(&app, &format!("/e/{}", nevent(id))).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(
        cache,
        "public, max-age=60".replace("60", &common::config().cache_ttl_secs.to_string())
    );
    assert!(!body.contains("og:title"));
}

#[tokio::test]
async fn nsfw_notes_are_skipped() {
    for tags in [
        json!([["content-warning", "spoilers"]]),
        json!([["t", "nsfw"]]),
    ] {
        let relay = StubRelay::start(vec![note(NOTE, "hello", tags), kind0(ALICE, "alice")]).await;
        assert_skipped_and_cached(relay, NOTE).await;
    }
}

#[tokio::test]
async fn notes_deleted_by_their_author_are_skipped() {
    let relay = StubRelay::start(vec![
        note(NOTE, "hello", json!([])),
        kind0(ALICE, "alice"),
        event(OTHER, ALICE, 5, "", json!([["e", NOTE]])),
    ])
    .await;
    assert_skipped_and_cached(relay, NOTE).await;
}

#[tokio::test]
async fn a_delete_by_someone_else_does_not_count() {
    let relay = StubRelay::start(vec![
        note(NOTE, "hello", json!([])),
        kind0(ALICE, "alice"),
        event(OTHER, BOB, 5, "", json!([["e", NOTE]])),
    ])
    .await;
    let (status, _, _) = get_on(&router(&relay, None), &format!("/e/{}", nevent(NOTE))).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn a_failed_delete_check_caches_the_card_briefly() {
    let relay = StubRelay::start(vec![note(NOTE, "hello", json!([])), kind0(ALICE, "alice")]).await;
    relay.stall("#e", u32::MAX);
    let app = common::router_with(Config {
        local_relay_url: relay.url.clone(),
        provisional_ttl_secs: 1,
        ..common::config()
    });
    let (status, _, _) = get_on(&app, &format!("/e/{}", nevent(NOTE))).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "an unknown delete state still draws"
    );

    relay.stall("#e", 0);
    relay.push(event(OTHER, ALICE, 5, "", json!([["e", NOTE]])));
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    let (status, _, _) = get_on(&app, &format!("/e/{}", nevent(NOTE))).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "cached past the provisional TTL"
    );
}

#[tokio::test]
async fn media_only_and_bare_quotes_are_skipped() {
    let quote = format!("nostr:{}", nevent(OTHER));
    for content in ["https://x.com/a.jpg", quote.as_str()] {
        let relay =
            StubRelay::start(vec![note(NOTE, content, json!([])), kind0(ALICE, "alice")]).await;
        assert_skipped_and_cached(relay, NOTE).await;
    }
}

#[tokio::test]
async fn unsupported_kinds_are_skipped() {
    let relay = StubRelay::start(vec![
        event(NOTE, ALICE, 7, "+", json!([])),
        kind0(ALICE, "alice"),
    ])
    .await;
    assert_skipped_and_cached(relay, NOTE).await;
}

#[tokio::test]
async fn an_author_without_kind0_gets_a_provisional_card() {
    let relay = StubRelay::start(vec![note(NOTE, "hello", json!([]))]).await;
    let (status, _, body) = get_on(&router(&relay, None), &format!("/e/{}", nevent(NOTE))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(meta(&body, "og:title").starts_with("npub1"), "{body}");
}

#[tokio::test]
async fn bad_ids_are_400() {
    let relay = StubRelay::start(vec![]).await;
    let npub = nip19::npub_from_hex(ALICE).unwrap();
    for id in ["garbage", npub.as_str()] {
        let (status, _, _) = get_on(&router(&relay, None), &format!("/e/{id}")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{id}");
    }
}

async fn stub_codes() -> String {
    let app = Router::new().route(
        "/shorturl/{code}",
        get(
            |axum::extract::Path(code): axum::extract::Path<String>| async move {
                let (kind, key) = match code.as_str() {
                    "EVENT" => ("event", NOTE.to_string()),
                    "ADDR" => ("address", format!("30023:{ALICE}:my:article")),
                    "HASH" => ("hashtag", "nostr".to_string()),
                    "TAG" => ("tag", format!("{ALICE}:slug")),
                    _ => return Err(StatusCode::NOT_FOUND),
                };
                Ok(Json(
                    json!({ "data": { "kind": kind, "key": key, "relays": [], "path": "/x" } }),
                ))
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn an_event_code_renders_the_note_card() {
    let relay = StubRelay::start(vec![note(NOTE, "hello", json!([])), kind0(ALICE, "alice")]).await;
    let (status, _, body) = get_on(&router(&relay, Some(stub_codes().await)), "/s/EVENT").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(meta(&body, "og:url"), format!("{APP}/e/{}", nevent(NOTE)));
}

#[tokio::test]
async fn an_address_code_reads_by_coordinate() {
    let relay = StubRelay::start(vec![]).await;
    let (status, _, _) = get_on(&router(&relay, Some(stub_codes().await)), "/s/ADDR").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let reads = relay.filters_with("#d");
    assert!(!reads.is_empty(), "no coordinate read");
    assert_eq!(reads[0]["#d"], json!(["my:article"]));
    assert_eq!(reads[0]["authors"], json!([ALICE]));
    assert_eq!(reads[0]["kinds"], json!([30023]));
}

#[tokio::test]
async fn hashtag_and_tag_codes_stay_404() {
    let relay = StubRelay::start(vec![]).await;
    let app = router(&relay, Some(stub_codes().await));
    for code in ["HASH", "TAG"] {
        let (status, _, body) = get_on(&app, &format!("/s/{code}")).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{code}");
        assert!(!body.contains("og:title"));
    }
    assert!(relay.filters.lock().unwrap().is_empty());
}
