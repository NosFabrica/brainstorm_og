use anyhow::{bail, Context};
use bytes::Bytes;
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
    /// Trusted destinations (the overview API). Follows redirects.
    pub http: reqwest::Client,
    /// Attacker-controlled avatar URLs. No redirects: one would bypass the
    /// pre-connect address check.
    pub avatar_http: reqwest::Client,
    pub fontdb: Arc<fontdb::Database>,
    pub card_cache: Cache<String, Card>,
    pub png_cache: Cache<String, Bytes>,
}

impl AppState {
    pub fn new(config: Config) -> anyhow::Result<Self> {
        let mut db = fontdb::Database::new();
        db.load_fonts_dir(&config.assets_dir);

        // No `load_system_fonts()` fallback on purpose — it kept a fontless
        // build alive, rendering every card blank. CONTEXT.md.
        if db.faces().next().is_none() {
            bail!(
                "no fonts found in {} — the image must bundle them (see assets/README.md)",
                config.assets_dir
            );
        }

        let has_family = db.faces().any(|f| {
            f.families
                .iter()
                .any(|(name, _)| name == &config.font_family)
        });
        if !has_family {
            let available: Vec<_> = db
                .faces()
                .filter_map(|f| f.families.first().map(|(n, _)| n.clone()))
                .collect();
            bail!(
                "FONT_FAMILY {:?} not present in {} (found: {:?})",
                config.font_family,
                config.assets_dir,
                available
            );
        }
        tracing::info!(
            font_family = %config.font_family,
            faces = db.len(),
            "fonts loaded"
        );

        let http = reqwest::Client::builder()
            .user_agent("brainstorm-og/0.1")
            .build()
            .context("building http client")?;
        let avatar_http = reqwest::Client::builder()
            .user_agent("brainstorm-og/0.1")
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("building avatar client")?;

        let ttl = Duration::from_secs(config.cache_ttl_secs);
        let card_cache = Cache::builder()
            .max_capacity(config.card_cache_capacity)
            // Provisional cards expire sooner; see CardExpiry.
            .expire_after(crate::data::CardExpiry {
                provisional: Duration::from_secs(config.provisional_ttl_secs),
                normal: ttl,
            })
            .build();
        let png_cache = Cache::builder()
            .weigher(|_k: &String, v: &Bytes| v.len().min(u32::MAX as usize) as u32)
            .max_capacity(config.png_cache_max_bytes)
            .time_to_live(ttl)
            .build();

        Ok(Self {
            config: Arc::new(config),
            http,
            avatar_http,
            fontdb: Arc::new(db),
            card_cache,
            png_cache,
        })
    }
}
