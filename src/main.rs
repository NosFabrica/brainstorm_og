use brainstorm_og::{
    build_router, config::Config, shutdown_signal, state::AppState, DEFAULT_LOG_FILTER,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| DEFAULT_LOG_FILTER.into()),
        )
        .init();

    let config = Config::from_env();
    let bind = config.bind_addr.clone();
    tracing::info!(?config, "starting brainstorm-og");

    let state = AppState::new(config)?;
    let app = build_router(state);

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!("listening on {bind}");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}
