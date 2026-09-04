pub mod config;
pub mod data;
pub mod link_preview;
pub mod net;
pub mod nip19;
pub mod relay;
pub mod render;
pub mod routes;
pub mod state;

use axum::{extract::Request, http::StatusCode, routing::get, Router};
use std::time::Duration;
use tower::{limit::ConcurrencyLimitLayer, ServiceBuilder};
use tower_http::{catch_panic::CatchPanicLayer, timeout::TimeoutLayer, trace::TraceLayer};

use crate::state::AppState;

/// The filter `main` installs when `RUST_ENV_FILTER` says nothing. Here rather
/// than in `main` so the test that pins what reaches the logs uses the same
/// string production does.
pub const DEFAULT_LOG_FILTER: &str = "brainstorm_og=info,tower_http=warn";

pub fn build_router(state: AppState) -> Router {
    let request_timeout = Duration::from_secs(state.config.router_timeout_secs());
    let max_renders = state.config.max_concurrent_renders;

    Router::new()
        .route("/healthz", get(routes::healthz))
        // `/p/{id}` is the canonical share route — see Brainstorm-UI App.tsx,
        // where `/profile/:npub` is marked deprecated and auth-gated. Axum path
        // segments are exact, so this can never swallow `/p/{id}/hops` or
        // `/p/{id}/{type}`.
        .route("/p/{id}", get(routes::profile))
        // Trailing-slash variants: nginx's `^/p/[^/]+/?$` accepts them, and a
        // crawler may follow either form.
        .route("/p/{id}/", get(routes::profile))
        // Back-compat only, for links already in the wild. Deliberately not
        // routed to by nginx; canonical/og:url still point at /p/.
        .route("/profile/{id}", get(routes::profile))
        .route("/profile/{id}/", get(routes::profile))
        // Short share links. Both cases, because nginx proxies the original URI
        // and the QR payload is uppercase (PRD D4b) — axum literal segments are
        // case-sensitive, so `/S/` needs its own route.
        .route("/s/{code}", get(routes::short_link))
        .route("/s/{code}/", get(routes::short_link))
        .route("/S/{code}", get(routes::short_link))
        .route("/S/{code}/", get(routes::short_link))
        // The cap is on this route alone, not the whole router. Stampede
        // protection only coalesces requests for the SAME pubkey; a burst of
        // DISTINCT ones still fans out one avatar fetch and one rasterisation
        // each. But the layer queues rather than rejects, so applying it
        // globally would sit /healthz behind those renders — and a liveness
        // probe timing out under load turns slow cards into no service.
        .route(
            "/og/{id}",
            get(routes::og_image).layer(ConcurrencyLimitLayer::new(max_renders)),
        )
        // A third-party page, fetched because a human opened a note that links
        // to it. The inverse of an unfurl — see CONTEXT.md.
        .route("/link-preview", get(link_preview::link_preview))
        .with_state(state)
        .layer(
            ServiceBuilder::new()
                .layer(TraceLayer::new_for_http().make_span_with(PathOnlySpan))
                // A panic in resvg/tiny-skia on malformed input becomes a 500
                // for that request instead of taking the process down.
                .layer(CatchPanicLayer::new())
                .layer(TimeoutLayer::with_status_code(
                    StatusCode::GATEWAY_TIMEOUT,
                    request_timeout,
                )),
        )
}

/// `DefaultMakeSpan` records the whole request URI, which on `/link-preview`
/// carries the third-party URL in `?url=`. We commit to not logging those, so
/// the span is built from the path alone. Nothing else here has a query worth
/// keeping — `/og/{id}?v=` is a content hash the log cannot use.
#[derive(Clone, Copy)]
struct PathOnlySpan;

impl<B> tower_http::trace::MakeSpan<B> for PathOnlySpan {
    fn make_span(&mut self, request: &Request<B>) -> tracing::Span {
        tracing::debug_span!(
            "request",
            method = %request.method(),
            path = %request.uri().path(),
            version = ?request.version(),
        )
    }
}

/// Kubernetes sends SIGTERM, not SIGINT. Handling only ctrl_c meant the pod
/// ignored every rolling update until the grace period expired and it was
/// SIGKILLed mid-request.
pub async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(e) => {
                tracing::error!("cannot install SIGTERM handler: {e}");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("SIGINT received, shutting down"),
        _ = terminate => tracing::info!("SIGTERM received, shutting down"),
    }
}
