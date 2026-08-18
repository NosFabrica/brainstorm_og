pub mod config;
pub mod data;
pub mod net;
pub mod nip19;
pub mod relay;
pub mod render;
pub mod routes;
pub mod state;
pub mod tier;

use axum::{http::StatusCode, routing::get, Router};
use std::time::Duration;
use tower::{limit::ConcurrencyLimitLayer, ServiceBuilder};
use tower_http::{catch_panic::CatchPanicLayer, timeout::TimeoutLayer, trace::TraceLayer};

use crate::state::AppState;

pub fn build_router(state: AppState) -> Router {
    let request_timeout = Duration::from_secs(state.config.request_deadline_secs + 2);
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
        .with_state(state)
        .layer(
            ServiceBuilder::new()
                .layer(TraceLayer::new_for_http())
                // A panic in resvg/tiny-skia on malformed input becomes a 500
                // for that request instead of taking the process down.
                .layer(CatchPanicLayer::new())
                .layer(TimeoutLayer::with_status_code(
                    StatusCode::GATEWAY_TIMEOUT,
                    request_timeout,
                )),
        )
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
