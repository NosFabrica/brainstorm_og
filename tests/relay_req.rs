//! The kind-0 REQ as the search relay sees it. That relay refuses a read naming
//! no lens, so the filter must carry `include:spam`.

use brainstorm_og::relay::fetch_kind0;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

const HEX: &str = "3bf0c63fcb93463407af97a5e5ee64fa883d107ef9e558472c4eb9aaaefa459d";

#[tokio::test]
async fn kind0_req_names_a_lens_and_returns_the_profile() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());

    let relay = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
        let Some(Ok(Message::Text(req))) = ws.next().await else {
            panic!("no REQ");
        };
        let req: Value = serde_json::from_str(&req).unwrap();
        let event = json!({ "kind": 0, "pubkey": HEX, "content": "{\"name\":\"fiatjaf\"}" });
        ws.send(Message::Text(
            json!(["EVENT", req[1], event]).to_string().into(),
        ))
        .await
        .unwrap();
        req
    });

    let meta = fetch_kind0(&url, HEX, 2).await.unwrap();
    let req = relay.await.unwrap();

    assert_eq!(meta.name.as_deref(), Some("fiatjaf"));
    assert_eq!(req[0], "REQ");
    assert_eq!(
        req[2],
        json!({ "authors": [HEX], "kinds": [0], "limit": 1, "search": "include:spam" })
    );
}
