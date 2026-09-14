//! What the page fetch and the robots.txt fetch share: redirects followed by
//! hand with every hop re-validated, and a body read under a cap.
//!
//! Redirects are never left to reqwest: its policy runs before we can re-check
//! the destination, so an automatic follow would let a public URL bounce us to
//! an internal one.

use std::time::Duration;

use axum::http::header;
use url::Url;

use super::{PreviewError, SCHEMES};
use crate::net;
use crate::state::AppState;

/// One more than the chains real sites use (http -> https -> www -> canonical),
/// and few enough that a chain cannot be a work amplifier.
pub(crate) const MAX_HOPS: u8 = 3;

/// A URL's origin as a cache key, or `None` for an opaque origin, which has no
/// host to hold anything against.
pub(crate) fn origin_key(url: &Url) -> Option<String> {
    let origin = url.origin().ascii_serialization();
    (origin != "null").then_some(origin)
}

/// What each hop is checked against besides its address.
#[derive(Clone, Copy)]
pub(crate) enum HopCheck {
    AddressOnly,
    /// Also the host's robots.txt and backoff — for the page, never for
    /// robots.txt itself.
    AddressAndHost,
}

/// The response a chain of redirects ended on, and where.
pub(crate) struct Arrived {
    pub response: reqwest::Response,
    pub url: Url,
}

pub(crate) enum FollowError {
    Refused(&'static str),
    Transport(reqwest::Error),
    Redirects(&'static str),
    Host(PreviewError),
}

pub(crate) async fn follow(
    st: &AppState,
    start: &str,
    accept: &'static str,
    timeout: Duration,
    check: HopCheck,
) -> Result<Option<Arrived>, FollowError> {
    let mut target = validate(st, start).await?;
    let mut hops = 0u8;
    loop {
        if !passes(st, &target, check).await? {
            // robots.txt disallows this hop — an answer, not a failure.
            return Ok(None);
        }
        let response = st
            .preview_http
            .get(target.clone())
            .header(header::ACCEPT, accept)
            .timeout(timeout)
            .send()
            .await
            .map_err(FollowError::Transport)?;
        if !response.status().is_redirection() {
            return Ok(Some(Arrived {
                response,
                url: target,
            }));
        }
        hops += 1;
        if hops > MAX_HOPS {
            return Err(FollowError::Redirects("too many redirects"));
        }
        let location = response
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .ok_or(FollowError::Redirects("redirect with no location"))?;
        // Relative Locations are legal and common.
        let next = target
            .join(location)
            .map_err(|_| FollowError::Refused("unparseable redirect target"))?;
        target = validate(st, next.as_str()).await?;
    }
}

async fn passes(st: &AppState, url: &Url, check: HopCheck) -> Result<bool, FollowError> {
    match check {
        HopCheck::AddressOnly => Ok(true),
        // Boxed: checking the host may fetch robots.txt, which follows its own
        // redirects through here — a cycle an unboxed future can't size.
        HopCheck::AddressAndHost => Box::pin(super::host_allows(st, url))
            .await
            .map_err(FollowError::Host),
    }
}

async fn validate(st: &AppState, raw: &str) -> Result<Url, FollowError> {
    net::validate_and_resolve_with(raw, SCHEMES, st.config.reserved_policy())
        .await
        // The error names the host it refused, which may not be logged.
        .map_err(|_| FollowError::Refused("url refused by address validation"))
}

/// Reads at most `max` bytes, streaming — `Content-Length` is only a claim —
/// and stops early once `done(body, len_before_this_chunk)` says so. The flag
/// is true when the cap, not `done`, ended the read.
pub(crate) async fn read_capped(
    response: reqwest::Response,
    max: usize,
    mut done: impl FnMut(&[u8], usize) -> bool,
) -> Result<(Vec<u8>, bool), reqwest::Error> {
    use futures_util::StreamExt as _;
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        let before = body.len();
        let room = max.saturating_sub(before);
        if chunk.len() > room {
            body.extend_from_slice(&chunk[..room]);
            return Ok((body, true));
        }
        body.extend_from_slice(&chunk);
        if done(&body, before) {
            break;
        }
    }
    Ok((body, false))
}
