//! Kind-0 metadata from the in-cluster relay.
//!
//! One relay, from config — deliberately not a fan-out. The original design
//! raced six public relays behind `nprofile` hints, which is what produced a
//! ~12s worst case on a cache miss and dialled attacker-supplied addresses.
//! A single in-cluster strfry is authoritative (Vespa is downstream of it),
//! one network hop against LMDB, and carries no SSRF surface because the URL
//! is ours, not the pointer's.

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
    timeout(
        Duration::from_secs(timeout_secs),
        fetch_inner(relay_url, pubkey_hex),
    )
    .await
    .map_err(|_| anyhow!("relay {relay_url} timed out"))?
}

async fn fetch_inner(relay_url: &str, pubkey_hex: &str) -> Result<ProfileMeta> {
    let (mut ws, _) = tokio_tungstenite::connect_async(relay_url).await?;

    let req = json!(["REQ", "og", { "authors": [pubkey_hex], "kinds": [0], "limit": 1 }]);
    ws.send(Message::Text(req.to_string().into())).await?;

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

        match arr.first().and_then(Value::as_str) {
            Some("EVENT") => {
                if let Some(event) = arr.get(2) {
                    if event.get("kind").and_then(Value::as_u64) == Some(0) {
                        let content = event.get("content").and_then(Value::as_str).unwrap_or("{}");
                        // A malformed `content` is a bad event, not a failed
                        // fetch — don't propagate it as an error and don't
                        // leave the socket open.
                        let meta = ProfileMeta::from_kind0_content(content);
                        let _ = ws.close(None).await;
                        return meta;
                    }
                }
            }
            Some("EOSE") => {
                let _ = ws.close(None).await;
                break;
            }
            _ => {}
        }
    }

    Err(anyhow!("no kind-0 event from {relay_url}"))
}
