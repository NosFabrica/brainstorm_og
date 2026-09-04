//! The client-IP fallback has to be loud: behind the ingress the direct peer
//! is the UI's nginx pod, identical for every caller, so a silent fallback
//! collapses unrelated callers into one bucket and throttles them together.
//!
//! Its own test binary on purpose. `tracing` caches per-callsite interest
//! globally, so a sibling test that reaches this `warn!` with no subscriber
//! installed can cache it as "never" and make a log assertion here fail at
//! random. One test per process removes the race rather than narrowing it.
//!
//! Issue: .scratch/link-preview/issues/04-rate-limit-and-client-ip.md

use axum::http::HeaderMap;
use brainstorm_og::link_preview::rate_limit::client_ip;
use std::sync::{Arc, Mutex};

/// Shared buffer a `tracing` subscriber writes into.
#[derive(Clone)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn falling_back_to_the_direct_peer_is_loud() {
    let buf = Arc::new(Mutex::new(Vec::new()));
    let sink = Captured(buf.clone());
    let subscriber = tracing_subscriber::fmt()
        // What actually ships, so this pins that the warning survives the
        // production filter rather than needing one turned up for it.
        .with_env_filter(tracing_subscriber::EnvFilter::new(
            brainstorm_og::DEFAULT_LOG_FILTER,
        ))
        .with_writer(move || sink.clone())
        .finish();

    let mut headers = HeaderMap::new();
    headers.insert("x-forwarded-for", "203.0.113.7".parse().unwrap());
    let peer = Some("10.9.9.9".parse().unwrap());

    let (short, absent) = tracing::subscriber::with_default(subscriber, || {
        // One entry in the chain, two hops configured: nothing our own proxy
        // wrote is present, so the peer is all that is left. And no header at
        // all, which behind the ingress means traffic reached the pod off the
        // expected path — the same collapse, and just as worth saying.
        (
            client_ip(&headers, peer, 2),
            client_ip(&HeaderMap::new(), peer, 2),
        )
    });
    assert_eq!(short, "10.9.9.9");
    assert_eq!(absent, "10.9.9.9");

    let logs = String::from_utf8_lossy(&buf.lock().unwrap().clone()).into_owned();
    assert_eq!(
        logs.matches("falling back to the direct peer").count(),
        2,
        "a fallback was silent:\n{logs}"
    );
    assert!(logs.contains("WARN"), "not logged at warn level:\n{logs}");
    // Counts, not addresses: this endpoint does not log who called it.
    assert!(
        !logs.contains("203.0.113.7"),
        "the caller's address reached the logs:\n{logs}"
    );
    assert!(
        !logs.contains("10.9.9.9"),
        "the peer address reached the logs:\n{logs}"
    );
}
