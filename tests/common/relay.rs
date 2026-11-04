//! A stand-in for the search relay: serves a mutable event set, answers each
//! REQ by matching filters, records every filter it was sent, and opens with an
//! AUTH challenge like the real one.

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

#[derive(Clone, Default)]
pub struct StubRelay {
    pub url: String,
    pub events: Arc<Mutex<Vec<Value>>>,
    pub filters: Arc<Mutex<Vec<Value>>>,
    /// Filter key -> how many more REQs carrying it get no answer (so the read times out).
    stalls: Arc<Mutex<HashMap<&'static str, u32>>>,
}

impl StubRelay {
    pub async fn start(events: Vec<Value>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let relay = StubRelay {
            url: format!("ws://{}", listener.local_addr().unwrap()),
            events: Arc::new(Mutex::new(events)),
            filters: Arc::default(),
            stalls: Arc::default(),
        };
        let r = relay.clone();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let r = r.clone();
                tokio::spawn(async move {
                    let Ok(mut ws) = tokio_tungstenite::accept_async(tcp).await else {
                        return;
                    };
                    let _ = ws
                        .send(Message::Text(
                            json!(["AUTH", "challenge"]).to_string().into(),
                        ))
                        .await;
                    while let Some(Ok(Message::Text(req))) = ws.next().await {
                        let Ok(req) = serde_json::from_str::<Value>(&req) else {
                            continue;
                        };
                        if req[0] != "REQ" {
                            continue;
                        }
                        let sub = req[1].clone();
                        let filters: Vec<Value> = req.as_array().unwrap()[2..].to_vec();
                        r.filters.lock().unwrap().extend(filters.clone());
                        if r.stalled(&filters) {
                            continue;
                        }
                        let mut hits: Vec<Value> = r
                            .events
                            .lock()
                            .unwrap()
                            .iter()
                            .filter(|e| filters.iter().any(|f| matches(f, e)))
                            .cloned()
                            .collect();
                        // Newest first, as relays answer a `limit`.
                        hits.sort_by_key(|e| std::cmp::Reverse(e["created_at"].as_u64()));
                        if let Some(l) = filters.iter().map(|f| f["limit"].as_u64()).max().flatten()
                        {
                            hits.truncate(l as usize);
                        }
                        for e in hits {
                            let _ = ws
                                .send(Message::Text(json!(["EVENT", sub, e]).to_string().into()))
                                .await;
                        }
                        let _ = ws
                            .send(Message::Text(json!(["EOSE", sub]).to_string().into()))
                            .await;
                    }
                });
            }
        });
        relay
    }

    /// Leave the next `times` REQs mentioning `key` unanswered.
    pub fn stall(&self, key: &'static str, times: u32) {
        self.stalls.lock().unwrap().insert(key, times);
    }

    fn stalled(&self, filters: &[Value]) -> bool {
        let mut stalls = self.stalls.lock().unwrap();
        for (key, left) in stalls.iter_mut() {
            if *left > 0 && filters.iter().any(|f| f.get(*key).is_some()) {
                *left -= 1;
                return true;
            }
        }
        false
    }

    pub fn push(&self, event: Value) {
        self.events.lock().unwrap().push(event);
    }

    /// Filters sent so far that mention `key` (e.g. `"ids"`, `"#e"`).
    pub fn filters_with(&self, key: &str) -> Vec<Value> {
        self.filters
            .lock()
            .unwrap()
            .iter()
            .filter(|f| f.get(key).is_some())
            .cloned()
            .collect()
    }
}

fn matches(f: &Value, e: &Value) -> bool {
    let within = |key: &str, val: &Value| match f.get(key).and_then(Value::as_array) {
        Some(list) => list.contains(val),
        None => true,
    };
    if !within("ids", &e["id"]) || !within("authors", &e["pubkey"]) || !within("kinds", &e["kind"])
    {
        return false;
    }
    for (key, want) in f.as_object().unwrap() {
        let Some(name) = key.strip_prefix('#') else {
            continue;
        };
        let want = want.as_array().unwrap();
        let has = e["tags"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t[0] == name && t.get(1).is_some_and(|v| want.contains(v)));
        if !has {
            return false;
        }
    }
    true
}

pub fn event(id: &str, pubkey: &str, kind: u32, content: &str, tags: Value) -> Value {
    json!({
        "id": id, "pubkey": pubkey, "kind": kind, "created_at": 1_700_000_000,
        "tags": tags, "content": content, "sig": "0".repeat(128),
    })
}
