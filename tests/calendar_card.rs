//! `/e/{naddr}` for calendar events (31922 date, 31923 time).

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use brainstorm_og::config::Config;
use brainstorm_og::nip19::Entity;
use common::relay::{event, StubRelay};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

const APP: &str = "https://brainstorm.test";
const ALICE: &str = "3bf0c63fcb93463407af97a5e5ee64fa883d107ef9e558472c4eb9aaaefa459d";
const ID: &str = "b9f5441e45ca39179320e0031cfb18e34078673dcc3d3e3a3b3a981760aa5696";

fn naddr(kind: u32) -> String {
    Entity::Address {
        kind,
        pubkey: ALICE.into(),
        d: "meetup".into(),
    }
    .canonical()
}

fn calendar(kind: u32, tags: Value) -> Vec<Value> {
    let mut tags = tags.as_array().unwrap().clone();
    tags.push(json!(["d", "meetup"]));
    vec![
        event(ID, ALICE, kind, "", Value::Array(tags)),
        event(
            &"0".repeat(64),
            ALICE,
            0,
            &json!({ "name": "alice" }).to_string(),
            json!([]),
        ),
    ]
}

async fn get_bytes(relay: &StubRelay, uri: &str) -> (StatusCode, Vec<u8>) {
    let app: Router = common::router_with(Config {
        app_base_url: APP.into(),
        local_relay_url: relay.url.clone(),
        ..common::config()
    });
    let res = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, bytes.to_vec())
}

async fn get(relay: &StubRelay, uri: &str) -> (StatusCode, String) {
    let (status, bytes) = get_bytes(relay, uri).await;
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
async fn a_timed_event_unfurls_in_its_own_zone() {
    let relay = StubRelay::start(calendar(
        31923,
        json!([
            ["title", "Meetup bitcoiner Vignola"],
            ["start", "1793818800"],
            ["end", "1793826000"],
            ["start_tzid", "Europe/Rome"],
            ["location", "Circolo Paradosso"]
        ]),
    ))
    .await;
    let (status, body) = get(&relay, &format!("/e/{}", naddr(31923))).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(meta(&body, "og:title"), "Meetup bitcoiner Vignola");
    assert_eq!(
        meta(&body, "og:description"),
        "Wed, 4 Nov 2026 · 20:00 – 22:00 CET · Circolo Paradosso"
    );
    assert_eq!(meta(&body, "og:type"), "website");
    assert_eq!(meta(&body, "og:url"), format!("{APP}/e/{}", naddr(31923)));
}

#[tokio::test]
async fn a_date_event_unfurls_with_its_dates() {
    let relay = StubRelay::start(calendar(
        31922,
        json!([
            ["title", "Assemblea"],
            ["start", "2026-11-05"],
            ["end", "2026-11-08"]
        ]),
    ))
    .await;
    let (status, body) = get(&relay, &format!("/e/{}", naddr(31922))).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(meta(&body, "og:description"), "5 – 7 Nov 2026");
}

#[tokio::test]
async fn an_untitled_event_is_skipped() {
    let relay = StubRelay::start(calendar(31923, json!([["start", "1793818800"]]))).await;
    let (status, body) = get(&relay, &format!("/e/{}", naddr(31923))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(!body.contains("og:title"));
}

#[tokio::test]
async fn the_card_renders() {
    let relay = StubRelay::start(calendar(
        31923,
        json!([["title", "Meetup"], ["start", "1793818800"]]),
    ))
    .await;
    let (status, png) = get_bytes(&relay, &format!("/og/e/{}.png?v=x", naddr(31923))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
}
