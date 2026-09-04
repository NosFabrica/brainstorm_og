//! `GET /link-preview` fetches a page nobody vetted, at a URL anyone can put in
//! a note. Everything here is about what happens when that URL is hostile or
//! the server behind it misbehaves.
//!
//! Reserved addresses are refused in every deployment, which would also refuse
//! the stub server these tests need. `allow_loopback_preview_targets` is the
//! seam that lets a test point at loopback; no environment variable reaches it.
//!
//! Issues: .scratch/link-preview/issues/02-link-preview-route-safe-fetch.md,
//!         .scratch/link-preview/issues/03-parse-opengraph-fields.md

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::response::IntoResponse;
use axum::{routing::get, Router};
use brainstorm_og::{build_router, config::Config, state::AppState};
use futures_util::StreamExt as _;
use http_body_util::BodyExt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tower::ServiceExt;

const HTML: &str = "<!doctype html><html><head>\
    <title>Stub &amp; co</title>\
    <meta property=\"og:title\" content=\"Stub Title\">\
    <meta property=\"og:description\" content=\"Stub description.\">\
    <meta property=\"og:image\" content=\"https://cdn.example/card.png\">\
    <meta property=\"og:site_name\" content=\"Stub Site\">\
    </head><body>hi</body></html>";

/// No `og:`, no `twitter:`, no `<title>` — the common case, per the PRD's
/// measurements. Must be a 200 with nulls, not an error.
const BARE_HTML: &str = "<!doctype html><html><head></head><body>hi</body></html>";

/// Chunks `/big` will emit if nothing stops it. Far past any cap a test sets,
/// so "we stopped early" is observable rather than inferred.
const BIG_CHUNKS: usize = 4096;
const BIG_CHUNK_BYTES: usize = 4096;

/// A stand-in for a third-party page. `sent` counts the chunks `/big` actually
/// handed to the socket, which is how the streaming cap is observed.
struct Stub {
    base: String,
    sent: Arc<AtomicUsize>,
}

async fn stub_site() -> Stub {
    let sent = Arc::new(AtomicUsize::new(0));
    let counter = sent.clone();
    let early = sent.clone();

    let app = Router::new()
        .route(
            "/full",
            get(|| async { ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], HTML) }),
        )
        .route(
            "/plain",
            get(|| async { ([(header::CONTENT_TYPE, "text/plain")], "not a document") }),
        )
        .route("/no-type", get(|| async { HTML.to_string() }))
        .route(
            "/bare",
            get(|| async { ([(header::CONTENT_TYPE, "text/html")], BARE_HTML) }),
        )
        // A relative `og:image`, reached via a redirect, so the only URL that
        // resolves it correctly is the final one.
        .route("/moved", get(|| async { redirect("/deep/story") }))
        .route(
            "/deep/story",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/html")],
                    "<html><head><meta property='og:title'\n content='Moved'>\
                     <meta property=\"og:image\" content=\"card.png\"></head>",
                )
            }),
        )
        .route(
            "/declared-big",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/html")],
                    "x".repeat(256 * 1024),
                )
            }),
        )
        .route(
            "/big",
            get(move || {
                let counter = counter.clone();
                async move {
                    // Chunked, so `Content-Length` cannot short-circuit the
                    // check — this exercises the in-stream cap specifically.
                    let stream = futures_util::stream::iter((0..BIG_CHUNKS).map(move |_| {
                        counter.fetch_add(1, Ordering::SeqCst);
                        Ok::<_, std::io::Error>(vec![b'x'; BIG_CHUNK_BYTES])
                    }));
                    (
                        [(header::CONTENT_TYPE, "text/html")],
                        Body::from_stream(stream),
                    )
                }
            }),
        )
        .route(
            "/early",
            get(move || {
                let counter = early.clone();
                async move {
                    // 200 bytes of head, then 16 MB nobody should read.
                    let head = format!(
                        "<html><head><title>Early</title>\
                         <meta property=\"og:title\" content=\"Early\">{}</head><body>",
                        " ".repeat(64)
                    );
                    let stream = futures_util::stream::once(async move {
                        Ok::<_, std::io::Error>(head.into_bytes())
                    })
                    .chain(futures_util::stream::iter((0..BIG_CHUNKS).map(move |_| {
                        counter.fetch_add(1, Ordering::SeqCst);
                        Ok::<_, std::io::Error>(vec![b'x'; BIG_CHUNK_BYTES])
                    })));
                    (
                        [(header::CONTENT_TYPE, "text/html")],
                        Body::from_stream(stream),
                    )
                }
            }),
        )
        .route(
            // blockstream.com's real shape: `</head>` closes at ~1.5 KB and the
            // tags land in the body at ~4.6 KB.
            "/tags-after-head",
            get(|| async {
                let body = format!(
                    "<html><head><link rel=\"stylesheet\" href=\"/a.css\">{}</head>\
                     <body>{}<meta property=\"og:title\" content=\"Late\">{}",
                    " ".repeat(1400),
                    " ".repeat(3000),
                    "y".repeat(200_000)
                );
                ([(header::CONTENT_TYPE, "text/html")], body)
            }),
        )
        .route(
            "/slow",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                ([(header::CONTENT_TYPE, "text/html")], HTML)
            }),
        )
        .route(
            "/to-metadata",
            get(|| async { redirect("http://169.254.169.254/latest/meta-data/") }),
        )
        .route("/to-file", get(|| async { redirect("file:///etc/passwd") }))
        .route("/hop", get(|| async { redirect("/hop") }));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Stub {
        base: format!("http://{addr}"),
        sent,
    }
}

fn redirect(to: &str) -> axum::response::Response {
    (StatusCode::FOUND, [(header::LOCATION, to.to_string())]).into_response()
}

/// Completes the TCP handshake, then closes without ever writing a status
/// line. The eucup.com case, minus TLS: a socket that connects and dies.
async fn dead_socket() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            drop(stream);
        }
    });
    format!("http://{addr}")
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
        // Both tiers set alike and high: nothing in this file is about the
        // limiter, and tests/rate_limit.rs is.
        link_preview_rate_trusted: 600,
        link_preview_rate_untrusted: 600,
        link_preview_rate_window_secs: 60,
        trusted_proxy_hops: 2,
        allow_loopback_preview_targets: true,
    }
}

fn router_with(config: Config) -> Router {
    build_router(AppState::new(config).expect("fonts must load from assets/"))
}

fn router() -> Router {
    router_with(test_config())
}

struct Res {
    status: StatusCode,
    cache: String,
    body: serde_json::Value,
}

async fn preview_on(app: &Router, target: &str) -> Res {
    let uri = format!("/link-preview?url={}", percent_encode(target));
    let res = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let cache = res
        .headers()
        .get(header::CACHE_CONTROL)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    Res {
        status,
        cache,
        body,
    }
}

async fn preview(target: &str) -> Res {
    preview_on(&router(), target).await
}

/// Enough of one to survive a query string; no percent-encoding crate here.
fn percent_encode(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            c => c.to_string().bytes().map(|b| format!("%{b:02X}")).collect(),
        })
        .collect()
}

async fn fetch_from(stub: &Stub, path: &str) -> brainstorm_og::link_preview::Page {
    let state = AppState::new(test_config()).expect("fonts must load from assets/");
    brainstorm_og::link_preview::fetch(&state, &format!("{}{path}", stub.base))
        .await
        .expect("fetch should have succeeded")
}

#[tokio::test]
async fn reading_stops_once_head_closes() {
    let stub = stub_site().await;
    let page = fetch_from(&stub, "/early").await;

    // The 8 KB floor binds here, not the 64 KB cap and not the 16 MB body.
    assert!(
        page.body.len() < 16 * 1024,
        "read {} bytes past a head that closed at ~200",
        page.body.len()
    );
    assert!(!page.truncated, "stopping at </head> is a complete read");
    assert!(String::from_utf8_lossy(&page.body).contains("og:title"));

    // And we stopped pulling from the socket, rather than reading and trimming.
    let sent = stub.sent.load(Ordering::SeqCst);
    assert!(
        sent < BIG_CHUNKS,
        "drained all {BIG_CHUNKS} chunks after the head had closed"
    );
}

#[tokio::test]
async fn tags_just_past_a_prematurely_closed_head_are_still_read() {
    // blockstream.com closes `<head>` at byte 1,483 and puts `og:title` at
    // 4.6 KB. Stopping dead at `</head>` loses it; the floor is why we don't.
    let stub = stub_site().await;
    let page = fetch_from(&stub, "/tags-after-head").await;

    assert!(
        String::from_utf8_lossy(&page.body).contains("og:title"),
        "stopped at </head> and lost the tags behind it"
    );
    assert!(
        page.body.len() < 32 * 1024,
        "read {} bytes",
        page.body.len()
    );
}

#[tokio::test]
async fn a_page_with_no_head_still_stops_at_the_cap() {
    // No `</head>` anywhere, so the early stop never fires and the cap is the
    // only thing bounding the read.
    let stub = stub_site().await;
    let res = preview(&format!("{}/big", stub.base)).await;
    assert_eq!(res.status, StatusCode::OK);
}

#[tokio::test]
async fn a_fetchable_html_page_yields_its_metadata() {
    let stub = stub_site().await;
    let res = preview(&format!("{}/full", stub.base)).await;

    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["code"], 200);
    assert_eq!(res.cache, "public, max-age=86400");

    let data = &res.body["data"];
    assert_eq!(data["title"], "Stub Title");
    assert_eq!(data["description"], "Stub description.");
    assert_eq!(data["image"], "https://cdn.example/card.png");
    assert_eq!(data["siteName"], "Stub Site");
    assert_eq!(data["url"], format!("{}/full", stub.base));
}

#[tokio::test]
async fn a_page_with_no_markup_is_a_200_with_nulls() {
    // The card degrades rather than disappearing, and the answer is cacheable
    // for as long as a successful one — "this page has nothing" is a result.
    let stub = stub_site().await;
    let res = preview(&format!("{}/bare", stub.base)).await;

    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.cache, "public, max-age=86400");

    let data = &res.body["data"];
    assert!(data["title"].is_null(), "got {data}");
    assert!(data["description"].is_null(), "got {data}");
    assert!(data["image"].is_null(), "got {data}");
    // The host stands in for a missing `og:site_name`.
    assert!(data["siteName"].is_string(), "got {data}");
}

#[tokio::test]
async fn the_image_resolves_against_the_url_after_redirects() {
    let stub = stub_site().await;
    let res = preview(&format!("{}/moved", stub.base)).await;

    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["data"]["title"], "Moved");
    assert_eq!(
        res.body["data"]["image"],
        format!("{}/deep/card.png", stub.base),
        "resolved against the requested URL rather than the final one"
    );
    assert_eq!(res.body["data"]["url"], format!("{}/deep/story", stub.base));
}

#[tokio::test]
async fn only_http_and_https_are_dialled() {
    for target in [
        "file:///etc/passwd",
        "data:text/html,<title>x</title>",
        "ws://relay.example/",
        "ftp://example.com/x",
        "javascript:alert(1)",
    ] {
        let res = preview(target).await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "dialled {target}");
        assert_eq!(res.cache, "public, max-age=3600", "for {target}");
    }
}

#[tokio::test]
async fn reserved_addresses_are_refused() {
    // The seam that lets the stub tests reach loopback must not be on for
    // these, or they would prove nothing.
    let mut config = test_config();
    config.allow_loopback_preview_targets = false;
    let app = router_with(config);

    for target in [
        "http://127.0.0.1:8000/",
        "http://[::1]:8000/",
        "http://169.254.169.254/latest/meta-data/",
        "http://[::ffff:169.254.169.254]/latest/",
        "http://10.0.0.5:6379/",
        "http://192.168.1.1/",
        "http://100.64.0.1/",
        "http://[fd00::1]/",
    ] {
        let res = preview_on(&app, target).await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "dialled {target}");
        assert_eq!(res.body["data"], serde_json::Value::Null);
    }
}

#[tokio::test]
async fn a_redirect_into_a_reserved_address_is_refused() {
    // The client follows nothing on its own: its policy runs before we can
    // re-check where it is going, so every hop is validated here instead.
    let stub = stub_site().await;
    let res = preview(&format!("{}/to-metadata", stub.base)).await;

    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    assert_eq!(res.cache, "public, max-age=3600");
}

#[tokio::test]
async fn a_redirect_into_another_scheme_is_refused() {
    let stub = stub_site().await;
    let res = preview(&format!("{}/to-file", stub.base)).await;

    assert_eq!(res.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_redirect_loop_terminates() {
    let stub = stub_site().await;
    let res = preview(&format!("{}/hop", stub.base)).await;

    assert_eq!(res.status, StatusCode::BAD_GATEWAY);
    assert_eq!(res.cache, "no-store");
}

#[tokio::test]
async fn a_non_html_content_type_is_refused() {
    let stub = stub_site().await;
    let res = preview(&format!("{}/plain", stub.base)).await;

    assert_eq!(res.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(res.cache, "public, max-age=3600");
}

#[tokio::test]
async fn a_missing_content_type_is_refused() {
    let stub = stub_site().await;
    let res = preview(&format!("{}/no-type", stub.base)).await;

    assert_eq!(res.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

#[tokio::test]
async fn an_oversized_body_aborts_mid_stream() {
    let stub = stub_site().await;
    let res = preview(&format!("{}/big", stub.base)).await;

    // Truncated, not refused: `<head>` is already in hand by the cap, and
    // failing here would drop every large page that has a perfectly good title.
    assert_eq!(res.status, StatusCode::OK);
    // The point is that we stopped pulling. Buffering the whole body and
    // measuring afterwards is exactly the bug this guards.
    let sent = stub.sent.load(Ordering::SeqCst);
    assert!(sent > 0, "the stub never streamed anything");
    assert!(
        sent < BIG_CHUNKS,
        "read all {BIG_CHUNKS} chunks ({} bytes) before giving up",
        sent * BIG_CHUNK_BYTES
    );
}

#[tokio::test]
async fn a_body_declaring_an_oversized_length_is_still_read_to_the_cap() {
    // An honest Content-Length past the cap is not a reason to refuse — the
    // Guardian serves 654 KB with nothing but a `<title>` worth having.
    let stub = stub_site().await;
    let res = preview(&format!("{}/declared-big", stub.base)).await;

    assert_eq!(res.status, StatusCode::OK);
}

#[tokio::test]
async fn a_socket_that_dies_without_a_status_line_is_a_502() {
    // eucup.com: DNS resolves, TCP connects, TLS completes, then the server
    // kills the stream with no status line. Must not hang or panic.
    let base = dead_socket().await;
    let res = preview(&format!("{base}/anything")).await;

    assert_eq!(res.status, StatusCode::BAD_GATEWAY);
    assert_eq!(res.cache, "no-store");
    assert_eq!(res.body["code"], 502);
}

#[tokio::test]
async fn a_slow_upstream_hits_the_deadline() {
    // Deadline under the per-hop timeout, so this pins the whole-request
    // budget rather than the per-hop one.
    let mut config = test_config();
    config.link_preview_timeout_secs = 30;
    config.link_preview_deadline_secs = 1;
    let app = router_with(config);

    let stub = stub_site().await;
    let res = preview_on(&app, &format!("{}/slow", stub.base)).await;

    assert_eq!(res.status, StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(res.cache, "no-store");
}

#[tokio::test]
async fn a_missing_url_parameter_is_a_400_envelope() {
    let res = router()
        .oneshot(
            Request::builder()
                .uri("/link-preview")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).expect("not an envelope");
    assert_eq!(body["code"], 400);
}

/// Shared buffer a `tracing` subscriber writes into.
#[derive(Clone)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Run one preview of each shape under `filter` and return everything logged.
///
/// Not a `#[tokio::test]`: the subscriber is installed per-thread, so the
/// runtime has to be driven from inside the closure that holds it.
fn logs_from_three_previews(filter: &str) -> String {
    let buf = Arc::new(Mutex::new(Vec::new()));
    let sink = Captured(buf.clone());

    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::NEW)
        .with_writer(move || sink.clone())
        .finish();

    tracing::subscriber::with_default(subscriber, || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let stub = stub_site().await;
                let dead = dead_socket().await;
                let app = router();
                // Fetched, refused and dead-upstream. The tracing layer builds
                // a span for all three; only the last two also log a reason.
                preview_on(&app, &format!("{}/full?s={MARKER}", stub.base)).await;
                preview_on(&app, &format!("http://169.254.169.254/{MARKER}")).await;
                preview_on(&app, &format!("{dead}/{MARKER}")).await;
            });
    });

    let logs = buf.lock().unwrap().clone();
    String::from_utf8_lossy(&logs).into_owned()
}

const MARKER: &str = "NEVERLOGGED";

fn assert_nothing_leaked(logs: &str) {
    assert!(!logs.is_empty(), "captured nothing, so this proves nothing");
    assert!(
        !logs.contains(MARKER),
        "the fetched url reached the logs:\n{logs}"
    );
    assert!(
        !logs.contains("169.254.169.254"),
        "the fetched host reached the logs:\n{logs}"
    );
}

/// What actually ships. Nothing anywhere in the process may name the page a
/// user asked us to preview — the /bot page promises exactly this.
#[test]
fn the_url_is_never_written_to_logs() {
    assert_nothing_leaked(&logs_from_three_previews(brainstorm_og::DEFAULT_LOG_FILTER));
}

/// And our own code stays clean however loudly it is turned up, which is the
/// part we control. `hyper` logs the host it dials at DEBUG, so this is scoped
/// to this crate and the trace layer rather than the whole process.
#[test]
fn our_own_layers_do_not_log_the_url_even_at_trace() {
    assert_nothing_leaked(&logs_from_three_previews(
        "brainstorm_og=trace,tower_http=trace",
    ));
}
