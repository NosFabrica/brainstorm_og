//! Kind-0 metadata from the search relay.
//!
//! One relay, from config — deliberately not a fan-out. The original design
//! raced six public relays behind `nprofile` hints, which is what produced a
//! ~12s worst case on a cache miss and dialled attacker-supplied addresses.
//! Profiles come from the shared search relay (`wss://search.brainstorm.world`),
//! which carries no SSRF surface because the URL is ours, not the pointer's.
//! It refuses a read naming no lens, so the REQ carries `include:spam`: plain
//! recall, no ranking, nothing filtered.

use anyhow::{anyhow, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;

use crate::data::ProfileMeta;

/// Query the relay for a pubkey's kind-0 event, bounded by `timeout_secs`.
pub async fn fetch_kind0(
    relay_url: &str,
    pubkey_hex: &str,
    timeout_secs: u64,
) -> Result<ProfileMeta> {
    let filter = json!({ "authors": [pubkey_hex], "kinds": [0], "limit": 1 });
    let events = fetch_events(relay_url, filter, timeout_secs).await?;
    let event = events
        .iter()
        .filter(|e| e.get("kind").and_then(Value::as_u64) == Some(0))
        .max_by_key(|e| e.get("created_at").and_then(Value::as_u64))
        .ok_or_else(|| anyhow!("no kind-0 event from {relay_url}"))?;
    // A malformed `content` is a bad event, not a failed fetch.
    ProfileMeta::from_kind0_content(event.get("content").and_then(Value::as_str).unwrap_or("{}"))
}

/// Every event the relay returns for `filter` up to EOSE. `include:spam` is
/// added here so no read can forget the lens.
pub async fn fetch_events(
    relay_url: &str,
    mut filter: Value,
    timeout_secs: u64,
) -> Result<Vec<Value>> {
    filter["search"] = json!("include:spam");
    timeout(
        Duration::from_secs(timeout_secs),
        fetch_inner(relay_url, filter),
    )
    .await
    .map_err(|_| anyhow!("relay {relay_url} timed out"))?
}

async fn fetch_inner(relay_url: &str, filter: Value) -> Result<Vec<Value>> {
    let (mut ws, _) = tokio_tungstenite::connect_async(relay_url).await?;
    let limit = filter.get("limit").and_then(Value::as_u64);
    let req = json!(["REQ", "og", filter]);
    ws.send(Message::Text(req.to_string().into())).await?;

    let mut events = Vec::new();
    while let Some(msg) = ws.next().await {
        let text = match msg? {
            Message::Text(t) => t.to_string(),
            Message::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
            Message::Close(_) => break,
            _ => continue,
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let Some(arr) = value.as_array() else {
            continue;
        };
        // Anything else, the AUTH challenge included, is ignored.
        match arr.first().and_then(Value::as_str) {
            Some("EVENT") => {
                if let Some(event) = arr.get(2) {
                    events.push(event.clone());
                    if limit.is_some_and(|l| events.len() as u64 >= l) {
                        break;
                    }
                }
            }
            Some("EOSE") | Some("CLOSED") => break,
            _ => {}
        }
    }
    let _ = ws.close(None).await;
    Ok(events)
}
