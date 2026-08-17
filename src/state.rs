use moka::future::Cache;
use resvg::usvg::fontdb;
use std::sync::Arc;
use std::time::Duration;

use crate::config::Config;
use crate::data::Card;

/// Shared, cheaply-cloneable application state.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub http: reqwest::Client,
    pub fontdb: Arc<fontdb::Database>,
    pub font_family: String,
    pub card_cache: Cache<String, Card>,
    pub png_cache: Cache<String, Arc<Vec<u8>>>,
}

impl AppState {
    pub fn new(config: Config) -> anyhow::Result<Self> {
        let mut db = fontdb::Database::new();
        db.load_fonts_dir(&config.assets_dir);
        if db.faces().next().is_none() {
            // Dev convenience: when assets/ ships no font, borrow the OS's.
            db.load_system_fonts();
        }
        let font_family = db
            .faces()
            .next()
            .and_then(|f| f.families.first().map(|(name, _)| name.clone()))
            .unwrap_or_else(|| "sans-serif".to_string());
        tracing::info!(font_family = %font_family, faces = db.len(), "fonts loaded");

        let http = reqwest::Client::builder()
            .user_agent("brainstorm-og/0.1")
            .build()?;

        let ttl = Duration::from_secs(config.cache_ttl_secs);
        let card_cache = Cache::builder()
            .max_capacity(config.card_cache_capacity)
            .time_to_live(ttl)
            .build();
        let png_cache = Cache::builder()
            .weigher(|_k: &String, v: &Arc<Vec<u8>>| v.len().min(u32::MAX as usize) as u32)
            .max_capacity(config.png_cache_max_bytes)
            .time_to_live(ttl)
            .build();

        Ok(Self {
            config: Arc::new(config),
            http,
            fontdb: Arc::new(db),
            font_family,
            card_cache,
            png_cache,
        })
    }
}
