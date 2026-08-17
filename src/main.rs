mod config;
mod data;
mod nip19;
mod relay;
mod render;
mod routes;
mod state;

use axum::{routing::get, Router};
use tower_http::trace::TraceLayer;

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
    tracing::info!(?config, "starting brainstorm-og");

    let state = AppState::new(config)?;

    let app = Router::new()
        .route("/healthz", get(routes::healthz))
        .route("/profile/{id}", get(routes::profile))
        .route("/og/{id}", get(routes::og_image))
        .with_state(state)
        .layer(TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!("listening on {bind}");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
