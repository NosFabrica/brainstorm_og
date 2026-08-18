mod config;
mod data;
mod net;
mod nip19;
mod relay;
mod render;
mod routes;
mod state;
mod tier;

use axum::{http::StatusCode, routing::get, Router};
use std::time::Duration;
use tower::ServiceBuilder;
use tower_http::{catch_panic::CatchPanicLayer, timeout::TimeoutLayer, trace::TraceLayer};

use crate::state::AppState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "brainstorm_og=info,tower_http=warn".into()),
        )
        .init();

    let config = config::Config::from_env();
    let bind = config.bind_addr.clone();
    let request_timeout = Duration::from_secs(config.request_deadline_secs + 2);
    tracing::info!(?config, "starting brainstorm-og");

    let state = AppState::new(config)?;

    let app = Router::new()
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
        .route("/og/{id}", get(routes::og_image))
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
        );

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!("listening on {bind}");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// Kubernetes sends SIGTERM, not SIGINT. Handling only ctrl_c meant the pod
/// ignored every rolling update until the grace period expired and it was
/// SIGKILLed mid-request.
async fn shutdown_signal() {
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
