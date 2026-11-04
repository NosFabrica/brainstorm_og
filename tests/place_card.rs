//! `/e/{naddr|nevent}` for DList items under the place concept.

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
const IMPORTER: &str = "4bded2172075221ead393a0baec9c530238ec192c2a9cdbbc7754ba8c3357b64";
const PLACES: &str =
    "39998:b83a28b7e4e5d20bd960c5faeb6625f95529166b8bdb045d42634a2f35919450:food-and-drink-places";
const ID: &str = "0019c116a5e7b7929a41761d9f21e8ee942912ffd114d143a754c907c613f978";

/// A btcmap item as read from the search relay on 2026-11-02.
fn coque(z: &str) -> Value {
    event(
        ID,
        IMPORTER,
        39999,
        "",
        json!([
            ["d", "osm-node-13730112185"],
            ["z", z],
            ["name", "Coque"],
            ["category", "restaurant"],
            ["address", "San Salvador de Jujuy"],
            ["locality", "San Salvador de Jujuy"],
            ["country", "AR"],
            ["cuisine", "argentinian"],
            ["lat", "-24.172492"],
            ["lon", "-65.3713545"],
            ["cuisine", "sorrentinos"],
            ["t", "restaurant"],
            ["alt", "Food and drink place: Coque, San Salvador de Jujuy"]
        ]),
    )
}

fn naddr() -> String {
    Entity::Address {
        kind: 39999,
        pubkey: IMPORTER.into(),
        d: "osm-node-13730112185".into(),
    }
    .canonical()
}

fn router(relay: &StubRelay) -> Router {
    common::router_with(Config {
        app_base_url: APP.into(),
        local_relay_url: relay.url.clone(),
        ..common::config()
    })
}

async fn get_on(app: &Router, uri: &str) -> (StatusCode, String, Vec<u8>) {
    let res = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let ctype = res
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    let bytes = res.into_body().collect().await.unwrap().to_bytes().to_vec();
    (status, ctype, bytes)
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
async fn a_place_unfurls_with_name_and_what_and_where() {
    let relay = StubRelay::start(vec![coque(PLACES)]).await;
    let (status, _, body) = get_on(&router(&relay), &format!("/e/{}", naddr())).await;
    let body = String::from_utf8(body).unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(meta(&body, "og:title"), "Coque");
    assert_eq!(
        meta(&body, "og:description"),
        "Restaurant · Argentinian · San Salvador de Jujuy"
    );
    assert_eq!(meta(&body, "og:type"), "website");
}

#[tokio::test]
async fn the_place_image_is_a_png() {
    let relay = StubRelay::start(vec![coque(PLACES)]).await;
    let (status, ctype, bytes) =
        get_on(&router(&relay), &format!("/og/e/{}.png?v=x", naddr())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ctype, "image/png");
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
}

#[tokio::test]
async fn an_item_under_another_concept_is_skipped() {
    let relay = StubRelay::start(vec![coque("39998:aa:books")]).await;
    let (status, _, _) = get_on(&router(&relay), &format!("/e/{}", naddr())).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_place_concept_is_config() {
    let relay = StubRelay::start(vec![coque("39998:aa:books")]).await;
    let app = common::router_with(Config {
        local_relay_url: relay.url.clone(),
        place_concept: "39998:aa:books".into(),
        ..common::config()
    });
    let (status, _, _) = get_on(&app, &format!("/e/{}", naddr())).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn a_9999_item_by_nevent_gets_the_card() {
    let mut item = coque(PLACES);
    item["kind"] = json!(9999);
    let relay = StubRelay::start(vec![item]).await;
    let nevent = Entity::Event { id: ID.into() }.canonical();
    let (status, _, body) = get_on(&router(&relay), &format!("/e/{nevent}")).await;
    let body = String::from_utf8(body).unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(meta(&body, "og:title"), "Coque");
}

/// A tiles server answering every tile with `status` and the Sedgefield fixture, recording the key it was sent.
async fn stub_tiles(
    status: u16,
) -> (
    String,
    std::sync::Arc<std::sync::Mutex<Vec<Option<String>>>>,
) {
    use axum::http::HeaderMap;
    let seen: std::sync::Arc<std::sync::Mutex<Vec<Option<String>>>> = Default::default();
    let s = seen.clone();
    let app = Router::new().route(
        "/planet/{z}/{x}/{y}",
        axum::routing::get(move |headers: HeaderMap| {
            let s = s.clone();
            async move {
                let key = headers
                    .get("x-nf-tiles-key")
                    .map(|v| v.to_str().unwrap().to_string());
                s.lock().unwrap().push(key);
                (
                    StatusCode::from_u16(status).unwrap(),
                    include_bytes!("../src/map/testdata/14-9228-9839.mvt").as_slice(),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (url, seen)
}

/// Brightness inside the card's map panel, clear of its rounded corner and the pin.
async fn panel_brightness(tiles: &str) -> u32 {
    let relay = StubRelay::start(vec![coque(PLACES)]).await;
    let app = common::router_with(Config {
        local_relay_url: relay.url.clone(),
        tiles_base_url: tiles.into(),
        tiles_key: Some("sekrit".into()),
        ..common::config()
    });
    let (status, _, bytes) = get_on(&app, &format!("/og/e/{}.png?v=x", naddr())).await;
    assert_eq!(status, StatusCode::OK);
    let img = image::load_from_memory(&bytes).unwrap().to_rgb8();
    img.get_pixel(760, 220)
        .0
        .iter()
        .map(|&c| u32::from(c))
        .sum::<u32>()
        / 3
}

#[tokio::test]
async fn a_place_with_coordinates_gets_a_map_from_the_tiles_server() {
    let (tiles, seen) = stub_tiles(200).await;
    assert!(panel_brightness(&tiles).await > 150);
    let seen = seen.lock().unwrap();
    assert!(!seen.is_empty());
    assert!(seen.iter().all(|k| k.as_deref() == Some("sekrit")));
}

#[tokio::test]
async fn a_failing_tiles_server_leaves_the_card_without_a_map() {
    let (tiles, _) = stub_tiles(500).await;
    assert!(panel_brightness(&tiles).await < 60);
    assert!(panel_brightness("http://127.0.0.1:1").await < 60);
}
