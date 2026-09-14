//! robots.txt, per RFC 9309.
//!
//! The half worth testing hardest is the difference between *disallowed* and
//! *unreadable*. Both stop the fetch; only one of them is an answer, and
//! confusing them would cache a momentary outage as "this link has no preview"
//! until tomorrow.
//!
//! Issue: .scratch/link-preview/issues/13-honour-robots-txt.md

mod common;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::response::IntoResponse;
use axum::{routing::get, Router};
use brainstorm_og::config::Config;
use http_body_util::BodyExt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const HTML: &str = "<!doctype html><html><head>\
    <meta property=\"og:title\" content=\"Stub Title\">\
    </head><body>hi</body></html>";

/// A site that serves a robots.txt of the test's choosing, and counts how often
/// its pages are actually fetched — which is how "we never asked" is observed
/// rather than inferred.
struct Stub {
    base: String,
    page_hits: Arc<AtomicUsize>,
    robots_hits: Arc<AtomicUsize>,
}

/// `robots` is the body to serve; `status` its status code.
async fn stub_site(robots: &str, status: StatusCode) -> Stub {
    let page_hits = Arc::new(AtomicUsize::new(0));
    let robots_hits = Arc::new(AtomicUsize::new(0));
    let body = Arc::new(Mutex::new(robots.to_string()));

    let (rb, rh, ph) = (body.clone(), robots_hits.clone(), page_hits.clone());
    let app = Router::new()
        .route(
            "/robots.txt",
            get(move || {
                let (rb, rh) = (rb.clone(), rh.clone());
                async move {
                    rh.fetch_add(1, Ordering::SeqCst);
                    let text = rb.lock().unwrap().clone();
                    (status, [(header::CONTENT_TYPE, "text/plain")], text).into_response()
                }
            }),
        )
        // Anything else is a page. Two distinct paths so one test can ask for
        // both and still expect a single robots.txt read.
        .fallback(get(move || {
            let ph = ph.clone();
            async move {
                ph.fetch_add(1, Ordering::SeqCst);
                ([(header::CONTENT_TYPE, "text/html")], HTML).into_response()
            }
        }));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Stub {
        base: format!("http://{addr}"),
        page_hits,
        robots_hits,
    }
}

/// Redirects its robots.txt at the cloud metadata address — the one fetch a
/// preview service must never make, arriving by the one route that is easy to
/// forget to guard.
async fn stub_with_redirecting_robots() -> String {
    let app = Router::new()
        .route(
            "/robots.txt",
            get(|| async {
                (
                    StatusCode::FOUND,
                    [(header::LOCATION, "http://169.254.169.254/latest/meta-data/")],
                )
                    .into_response()
            }),
        )
        .fallback(get(|| async {
            ([(header::CONTENT_TYPE, "text/html")], HTML)
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

fn test_config() -> Config {
    Config {
        link_preview_timeout_secs: 2,
        robots_timeout_secs: 1,
        link_preview_deadline_secs: 4,
        ..common::stub_config()
    }
}

fn router() -> Router {
    common::router_with(test_config())
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
    Res {
        status,
        cache,
        body: serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    }
}

fn percent_encode(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            c => c.to_string().bytes().map(|b| format!("%{b:02X}")).collect(),
        })
        .collect()
}

#[tokio::test]
async fn a_disallowed_path_is_an_answer_and_the_page_is_never_fetched() {
    let stub = stub_site("User-agent: *\nDisallow: /private/\n", StatusCode::OK).await;
    let res = preview_on(&router(), &format!("{}/private/page", stub.base)).await;

    assert_eq!(res.status, StatusCode::OK, "a refusal is not an error");
    assert!(res.body["data"]["title"].is_null());
    assert!(
        res.cache.contains("max-age=86400"),
        "a Disallow is still true tomorrow, so it caches like any other answer: {}",
        res.cache
    );
    assert_eq!(
        stub.page_hits.load(Ordering::SeqCst),
        0,
        "the page must never be requested at all"
    );
}

#[tokio::test]
async fn a_path_outside_the_disallow_is_fetched_normally() {
    let stub = stub_site("User-agent: *\nDisallow: /private/\n", StatusCode::OK).await;
    let res = preview_on(&router(), &format!("{}/public/page", stub.base)).await;

    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["data"]["title"], "Stub Title");
    assert_eq!(stub.page_hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_group_naming_us_beats_the_wildcard() {
    // RFC 9309 §2.2.1: the most specific group wins outright. A site that bans
    // everyone but lets us through must let us through.
    let stub = stub_site(
        "User-agent: *\nDisallow: /\n\nUser-agent: BrainstormBot\nAllow: /\n",
        StatusCode::OK,
    )
    .await;
    let res = preview_on(&router(), &format!("{}/anything", stub.base)).await;

    assert_eq!(res.body["data"]["title"], "Stub Title");
    assert_eq!(stub.page_hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_group_naming_us_can_also_ban_us_alone() {
    let stub = stub_site(
        "User-agent: *\nAllow: /\n\nUser-agent: BrainstormBot\nDisallow: /\n",
        StatusCode::OK,
    )
    .await;
    let res = preview_on(&router(), &format!("{}/anything", stub.base)).await;

    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body["data"]["title"].is_null());
    assert_eq!(stub.page_hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_missing_robots_txt_allows_everything() {
    // §2.3.1.3: 4xx means "unavailable", which is full allowance — and it is
    // the common case across most of the web.
    let stub = stub_site("", StatusCode::NOT_FOUND).await;
    let res = preview_on(&router(), &format!("{}/page", stub.base)).await;

    assert_eq!(res.body["data"]["title"], "Stub Title");
    assert_eq!(stub.page_hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn an_unreadable_robots_txt_stops_us_without_becoming_an_answer() {
    // §2.3.1.4: a 5xx means assume disallow. Taking that branch is what makes
    // the compliance claim true rather than decorative — but it must not cache
    // as "no preview", or one bad minute hides the domain until tomorrow.
    let stub = stub_site("", StatusCode::INTERNAL_SERVER_ERROR).await;
    let res = preview_on(&router(), &format!("{}/page", stub.base)).await;

    assert_eq!(res.status, StatusCode::BAD_GATEWAY);
    assert_eq!(res.cache, "no-store");
    assert_eq!(stub.page_hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn robots_is_read_once_per_origin_however_many_links_land_there() {
    let stub = stub_site("User-agent: *\nAllow: /\n", StatusCode::OK).await;
    let app = router();
    for path in ["/one", "/two", "/three"] {
        preview_on(&app, &format!("{}{path}", stub.base)).await;
    }
    assert_eq!(stub.page_hits.load(Ordering::SeqCst), 3);
    assert_eq!(
        stub.robots_hits.load(Ordering::SeqCst),
        1,
        "only the first preview per host should pay the round trip"
    );
}

#[tokio::test]
async fn crawl_delay_is_ignored_deliberately() {
    // Not in RFC 9309, and the values real sites publish (arxiv 15s, Hacker
    // News 30s) are unworkable for a fetch a reader is waiting on. Honouring
    // it would blow the deadline; this test is what says that is on purpose.
    let stub = stub_site("User-agent: *\nCrawl-delay: 30\nAllow: /\n", StatusCode::OK).await;
    let res = preview_on(&router(), &format!("{}/page", stub.base)).await;

    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["data"]["title"], "Stub Title");
}

#[tokio::test]
async fn robots_txt_cannot_redirect_us_into_a_reserved_address() {
    // The page fetch guards every hop. The robots fetch is an easy place to
    // forget, and it runs first — so a Location header is a way in unless it
    // goes through the same validation.
    let base = stub_with_redirecting_robots().await;
    let res = preview_on(&router(), &format!("{base}/page")).await;

    assert_eq!(
        res.status,
        StatusCode::BAD_GATEWAY,
        "a robots.txt we could not safely read is unreadable, not permission"
    );
    assert_eq!(res.cache, "no-store");
}

#[tokio::test]
async fn a_refused_connection_is_retried_once_before_blanking_the_domain() {
    // The blip that blanked nostrmag.com in testing: nothing answers the first
    // robots.txt request. Here nothing is listening yet — the connection is
    // refused in milliseconds — and the site comes up before the retry.
    let port = {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        probe.local_addr().unwrap().port()
    };
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let app = Router::new()
            .route("/robots.txt", get(|| async { "User-agent: *\nAllow: /\n" }))
            .fallback(get(|| async {
                ([(header::CONTENT_TYPE, "text/html")], HTML)
            }));
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .unwrap();
        let _ = axum::serve(listener, app).await;
    });

    let res = preview_on(&router(), &format!("http://127.0.0.1:{port}/page")).await;
    assert_eq!(
        res.status,
        StatusCode::OK,
        "one refused connection must not blank the domain"
    );
    assert_eq!(res.body["data"]["title"], "Stub Title");
}

#[tokio::test]
async fn a_server_error_is_not_retried() {
    // A 5xx means the site is struggling; asking again straight away is how a
    // preview bot earns a block.
    let stub = stub_site("", StatusCode::SERVICE_UNAVAILABLE).await;
    let res = preview_on(&router(), &format!("{}/page", stub.base)).await;
    assert_eq!(res.status, StatusCode::BAD_GATEWAY);
    assert_eq!(stub.robots_hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_429_on_robots_pauses_the_whole_host() {
    let stub = stub_site("", StatusCode::TOO_MANY_REQUESTS).await;
    let app = router();
    for path in ["/one", "/two"] {
        let res = preview_on(&app, &format!("{}{path}", stub.base)).await;
        assert_eq!(res.status, StatusCode::BAD_GATEWAY);
        assert_eq!(res.cache, "no-store");
    }
    assert_eq!(
        stub.robots_hits.load(Ordering::SeqCst),
        1,
        "asked to slow down, so we don't ask again"
    );
    assert_eq!(stub.page_hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_429_on_a_page_pauses_every_link_on_that_host() {
    // One rate-limited article must stop us touching the rest of the site too,
    // or a feed full of its links keeps hitting it and gets us blocked.
    let page_hits = Arc::new(AtomicUsize::new(0));
    let hits = page_hits.clone();
    let app_stub = Router::new()
        .route("/robots.txt", get(|| async { "User-agent: *\nAllow: /\n" }))
        .route(
            "/limited",
            get(|| async {
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    [(header::RETRY_AFTER, "120")],
                    "slow down",
                )
                    .into_response()
            }),
        )
        .fallback(get(move || {
            let hits = hits.clone();
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                ([(header::CONTENT_TYPE, "text/html")], HTML).into_response()
            }
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app_stub).await;
    });

    let app = router();
    assert_eq!(
        preview_on(&app, &format!("{base}/limited")).await.status,
        StatusCode::BAD_GATEWAY
    );
    let other = preview_on(&app, &format!("{base}/other")).await;
    assert_eq!(other.status, StatusCode::BAD_GATEWAY);
    assert_eq!(
        page_hits.load(Ordering::SeqCst),
        0,
        "the rest of the host must wait too"
    );
}
