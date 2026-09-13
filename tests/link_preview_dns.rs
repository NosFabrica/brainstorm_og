//! The DNS-rebinding window. `net::validate_and_resolve` judges a host's
//! addresses before the connection; a name can answer differently by the time
//! the socket opens. The fetching client resolves through a filter so the
//! address the connector dials is the address that was checked.
//!
//! Issue: .scratch/link-preview/issues/06-dns-rebinding-resolver.md

use axum::http::header;
use axum::{routing::get, Router};
use brainstorm_og::{config::Config, net, state::AppState};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use std::net::SocketAddr;

const HTML: &str = "<!doctype html><html><head><title>Stub</title></head><body>hi</body></html>";

/// A resolver that answers every name with one fixed address — the rebinding
/// attacker's second answer, minus the waiting.
struct Fixed(SocketAddr);

impl Resolve for Fixed {
    fn resolve(&self, _name: Name) -> Resolving {
        let addr = self.0;
        Box::pin(async move { Ok(Box::new(std::iter::once(addr)) as Addrs) })
    }
}

async fn stub() -> SocketAddr {
    let app = Router::new().route(
        "/full",
        get(|| async { ([(header::CONTENT_TYPE, "text/html")], HTML) }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    addr
}

fn client_resolving_to(addr: SocketAddr, reserved: net::Reserved) -> reqwest::Client {
    reqwest::Client::builder()
        .dns_resolver(net::resolver::FilteringResolver::new(Fixed(addr), reserved))
        .build()
        .unwrap()
}

/// The heart of it: the URL names a domain, so nothing pre-connect ever saw an
/// address — and the address the name hands back at dial time is refused.
#[tokio::test]
async fn a_name_answering_with_a_reserved_address_is_never_dialled() {
    let addr = stub().await;
    let refused = client_resolving_to(addr, net::Reserved::Refuse)
        .get("http://rebound.test/full")
        .send()
        .await;
    assert!(refused.is_err(), "the filter let a loopback answer through");
}

/// ...and the failure above is the filter, not a resolver that never answers.
/// Same name, same address, policy relaxed: the fetch completes.
#[tokio::test]
async fn the_same_name_is_reached_when_the_policy_allows_the_address() {
    let addr = stub().await;
    let body = client_resolving_to(addr, net::Reserved::AllowLoopback)
        .get("http://rebound.test/full")
        .send()
        .await
        .expect("the stub should have been reached")
        .text()
        .await
        .unwrap();
    assert!(body.contains("<title>Stub</title>"));
}

/// The wiring, through the client `AppState` actually builds. `preview_http`
/// is used here directly rather than through the route, so the pre-connect
/// check is out of the picture and only the resolver can refuse.
#[tokio::test]
async fn the_deployed_preview_client_refuses_a_name_that_resolves_to_loopback() {
    let addr = stub().await;
    let url = format!("http://localhost:{}/full", addr.port());

    // Not vacuous: the port is listening and an unfiltered client reaches it.
    let plain = reqwest::Client::new().get(&url).send().await;
    assert!(
        plain.is_ok(),
        "the stub was unreachable for unrelated reasons"
    );

    let mut config = test_config();
    config.allow_loopback_preview_targets = false;
    let st = AppState::new(config).expect("fonts must load from assets/");
    assert!(
        st.preview_http.get(&url).send().await.is_err(),
        "preview_http dialled a name resolving to loopback"
    );
}

fn test_config() -> Config {
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
        link_preview_cache_ttl_secs: 86_400,
        robots_cache_ttl_secs: 86_400,
        robots_cache_capacity: 4_096,
        robots_timeout_secs: 2,
        link_preview_cache_max_bytes: 1024 * 1024,
        link_preview_rate_trusted: 600,
        link_preview_rate_untrusted: 600,
        link_preview_rate_window_secs: 60,
        trusted_proxy_hops: 2,
        allow_loopback_preview_targets: true,
    }
}
