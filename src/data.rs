use anyhow::Result;
use futures_util::stream::{FuturesUnordered, StreamExt};
use serde::Deserialize;
use serde_json::Value;
use std::time::Duration;

use crate::nip19::Pointer;
use crate::relay;
use crate::state::AppState;

/// Kind-0 profile metadata (relay-sourced, never from the DB).
#[derive(Clone, Default, Debug)]
pub struct ProfileMeta {
    pub name: Option<String>,
    pub display_name: Option<String>,
    pub picture: Option<String>,
    pub about: Option<String>,
    pub nip05: Option<String>,
}

impl ProfileMeta {
    pub fn from_kind0_content(content: &str) -> Result<Self> {
        let v: Value = serde_json::from_str(content)?;
        let s = |k: &str| {
            v.get(k)
                .and_then(Value::as_str)
                .map(str::to_string)
                .filter(|x| !x.trim().is_empty())
        };
        Ok(ProfileMeta {
            name: s("name"),
            display_name: s("display_name").or_else(|| s("displayName")),
            picture: s("picture"),
            about: s("about"),
            nip05: s("nip05"),
        })
    }

    pub fn best_name(&self) -> Option<String> {
        self.display_name.clone().or_else(|| self.name.clone())
    }

    fn is_useful(&self) -> bool {
        self.best_name().is_some() || self.picture.is_some() || self.about.is_some()
    }
}

/// Trust stats from brainstorm-server (the only DB-backed call).
#[derive(Clone, Default, Debug)]
pub struct Overview {
    pub influence: Option<f64>,
    pub followers: i64,
    pub following: i64,
}

#[derive(Deserialize)]
struct OverviewEnvelope {
    data: OverviewData,
}
#[derive(Deserialize)]
struct OverviewData {
    influence: Option<f64>,
    counts: OverviewCounts,
}
#[derive(Deserialize)]
struct OverviewCounts {
    followed_by: i64,
    following: i64,
}

/// Everything needed to render a card / meta tags for one pubkey.
#[derive(Clone, Debug)]
pub struct Card {
    pub meta: ProfileMeta,
    pub overview: Option<Overview>,
}

/// Assemble (and cache) a profile card: stats + relay metadata fetched concurrently.
pub async fn get_card(state: &AppState, pointer: &Pointer) -> Card {
    if let Some(cached) = state.card_cache.get(&pointer.hex).await {
        return cached;
    }

    let relays = relay_chain(state, pointer);
    let (overview, meta) = tokio::join!(
        fetch_overview(state, &pointer.hex),
        fetch_meta(state, &pointer.hex, &relays),
    );

    let card = Card {
        meta: meta.unwrap_or_default(),
        overview,
    };

    state.card_cache.insert(pointer.hex.clone(), card.clone()).await;
    card
}

/// local relay first, then nprofile hints, then public fallbacks — de-duplicated, order preserved.
fn relay_chain(state: &AppState, pointer: &Pointer) -> Vec<String> {
    let mut out = Vec::new();
    let mut push = |r: &str| {
        let r = r.trim().to_string();
        if !r.is_empty() && !out.contains(&r) {
            out.push(r);
        }
    };
    push(&state.config.local_relay_url);
    for r in &pointer.relays {
        push(r);
    }
    for r in &state.config.fallback_relays {
        push(r);
    }
    out
}

async fn fetch_overview(state: &AppState, hex: &str) -> Option<Overview> {
    let url = format!("{}/user/{}/overview", state.config.api_base_url, hex);
    let resp = state
        .http
        .get(&url)
        .timeout(Duration::from_secs(state.config.fetch_timeout_secs))
        .send()
        .await
        .map_err(|e| tracing::warn!("overview request failed for {hex}: {e}"))
        .ok()?;

    if !resp.status().is_success() {
        tracing::warn!("overview {hex} -> HTTP {}", resp.status());
        return None;
    }

    let env: OverviewEnvelope = resp
        .json()
        .await
        .map_err(|e| tracing::warn!("overview decode failed for {hex}: {e}"))
        .ok()?;

    Some(Overview {
        influence: env.data.influence,
        followers: env.data.counts.followed_by,
        following: env.data.counts.following,
    })
}

/// Try the local relay first (cheap, in-cluster); if it yields nothing, race the rest.
async fn fetch_meta(state: &AppState, hex: &str, relays: &[String]) -> Option<ProfileMeta> {
    let t = state.config.fetch_timeout_secs;

    if let Some(first) = relays.first() {
        if let Ok(m) = relay::fetch_kind0(first, hex, t).await {
            if m.is_useful() {
                return Some(m);
            }
        }
    }

    let mut inflight = FuturesUnordered::new();
    for r in relays.iter().skip(1) {
        let (r, hex) = (r.clone(), hex.to_string());
        inflight.push(async move { relay::fetch_kind0(&r, &hex, t).await.ok() });
    }
    while let Some(res) = inflight.next().await {
        if let Some(m) = res {
            if m.is_useful() {
                return Some(m);
            }
        }
    }
    None
}
