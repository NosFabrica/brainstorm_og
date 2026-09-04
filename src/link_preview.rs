//! `GET /link-preview?url=…` — we fetch *someone else's* page. The inverse
//! direction from an unfurl, which is a crawler reading our share link.
//!
//! CONTEXT.md still lists "link preview" as a banned synonym for **share
//! card**; ticket 12 amends it, since this is a distinct concept rather than a
//! second name for that one. Handler and fetch live together here rather than
//! in `routes.rs` because ticket 03 grows the same module with a parser.
//!
//! The URL is attacker-controlled, so nothing here trusts it: `net` judges the
//! address before every connection, redirects are followed by hand so each hop
//! is judged too, and the body is capped while it streams rather than after it
//! has been buffered.
//!
//! This module stops at "we have the bytes". Parsing them into card fields is
//! ticket 03; `data` is null until then.

use axum::{
    extract::{Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use std::time::Duration;
use url::Url;

use crate::net::{self, Reserved};
use crate::state::AppState;

/// Honest, resolvable, and never another company's preview bot. The measured
/// cost is four sites out of 56; see the ADR. It also matches nginx's
/// `$is_og_bot` regex, so our own `/p/` links preview correctly.
pub const USER_AGENT: &str = "BrainstormBot/1.0 (+https://brainstorm.world/bot)";

const SCHEMES: &[&str] = &["http", "https"];

/// We stop reading once `<head>` has closed — everything a preview needs is
/// inside it, and the rest is bytes nobody looks at. Measured over 48 sites,
/// this cuts what we pull by ~60%, with the median read dropping from the
/// whole page to 9.5 KB. Being a light fetcher is also the commitment ADR (ii)
/// rests on when it declines to consult robots.txt.
const HEAD_TAG: &[u8] = b"</head";

/// ...but never stop before this. A few real sites close `<head>` and then put
/// `<title>` and the `og:` tags in the body — blockstream.com closes at byte
/// 1,483 and its tags land at 4.6 KB. Measured: an 8 KB floor recovers every
/// such site in the sample and costs 1% of the saving. It is deliberately
/// below the median `</head>` (9.5 KB), so it binds only on the anomalies.
const HEAD_FLOOR: usize = 8 * 1024;

/// One more than the redirect chains real sites use (http -> https -> www ->
/// canonical) and few enough that a chain cannot be a work amplifier.
const MAX_HOPS: u8 = 3;

/// What the browser may hold. Ticket 05 ties this to the response cache's TTL;
/// they are the same duration for the same reason.
const OK_CACHE: &str = "public, max-age=86400";
/// A rejected URL is rejected forever, so caches may absorb the retry.
const REFUSED_CACHE: &str = "public, max-age=3600";
/// An upstream failure is a fact about right now.
const FAILED_CACHE: &str = "no-store";

#[derive(Deserialize)]
pub struct PreviewQuery {
    /// Optional so a missing parameter is our own envelope rather than axum's
    /// plain-text rejection, which carries no `Cache-Control`.
    url: Option<String>,
}

/// A fetched page, before anything has been read out of it.
pub struct Page {
    /// After redirects. Ticket 03 resolves relative `og:image` against this.
    pub final_url: Url,
    pub content_type: String,
    /// Normally everything up to `</head>`; at most `LINK_PREVIEW_MAX_BYTES`.
    pub body: Vec<u8>,
    /// The **cap** stopped us, not `</head>` — so a tag may be missing because
    /// it fell past the cut rather than because the page lacks it. Stopping at
    /// `</head>` does not set this: that is a complete read for our purposes.
    pub truncated: bool,
}

/// Why a preview did not happen.
///
/// Every variant carries a *static* reason. Nothing derived from the request
/// may end up in a log line, and reqwest's own errors render the URL they were
/// made from — so they are classified here and then dropped.
#[derive(Debug, Clone, Copy)]
pub enum PreviewError {
    /// Not a URL we will ever dial.
    Refused(&'static str),
    /// Fetched, but not a document.
    NotHtml,
    /// The upstream did not answer usefully.
    Upstream(&'static str),
    /// We ran out of time.
    Timeout,
}

impl PreviewError {
    /// Status, client message and `Cache-Control`, in one place so the table
    /// reads the way the PRD writes it. A rejected URL and an unusable content
    /// type are permanent facts about that URL, so caches may absorb the
    /// retry; an upstream failure is a fact about right now.
    fn parts(self) -> (StatusCode, &'static str, &'static str) {
        match self {
            Self::Refused(_) => (
                StatusCode::BAD_REQUEST,
                "invalid or blocked url",
                REFUSED_CACHE,
            ),
            Self::NotHtml => (
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "not an html document",
                REFUSED_CACHE,
            ),
            Self::Upstream(_) => (
                StatusCode::BAD_GATEWAY,
                "upstream fetch failed",
                FAILED_CACHE,
            ),
            Self::Timeout => (
                StatusCode::GATEWAY_TIMEOUT,
                "upstream timed out",
                FAILED_CACHE,
            ),
        }
    }

    /// Safe to log: fixed strings, chosen here, never the request.
    fn reason(self) -> &'static str {
        match self {
            Self::Refused(r) | Self::Upstream(r) => r,
            Self::NotHtml => "content type is not html",
            Self::Timeout => "deadline exceeded",
        }
    }
}

pub async fn link_preview(State(st): State<AppState>, Query(q): Query<PreviewQuery>) -> Response {
    let Some(raw) = q.url.filter(|u| !u.is_empty()) else {
        return refused(PreviewError::Refused("no url parameter"));
    };

    match fetch(&st, &raw).await {
        // Ticket 03 fills `data` in. A 200 with nulls is a real answer: a page
        // with no usable markup degrades the card rather than erroring.
        Ok(_page) => envelope(StatusCode::OK, None, OK_CACHE),
        Err(e) => refused(e),
    }
}

fn refused(e: PreviewError) -> Response {
    tracing::debug!(reason = e.reason(), "link preview failed");
    let (status, message, cache) = e.parts();
    envelope(status, Some(message), cache)
}

/// Fetch a third-party page, or say why not. Public so ticket 03 can parse
/// what it returns without going back through the router.
pub async fn fetch(st: &AppState, raw: &str) -> Result<Page, PreviewError> {
    let deadline = Duration::from_secs(st.config.link_preview_deadline_secs);
    // Bounds the whole chain, not each hop: three hops that each answer just
    // inside the per-hop timeout would otherwise sit well past it.
    tokio::time::timeout(deadline, follow_and_read(st, raw))
        .await
        .unwrap_or(Err(PreviewError::Timeout))
}

async fn follow_and_read(st: &AppState, raw: &str) -> Result<Page, PreviewError> {
    let max = st.config.link_preview_max_bytes;
    let per_hop = Duration::from_secs(st.config.link_preview_timeout_secs);
    let mut target = validate(st, raw).await?;

    // Redirects are followed here rather than by reqwest, which is configured
    // not to: its policy runs before we can re-check the destination, so an
    // automatic follow would let a public URL bounce us to an internal one.
    let mut hops = 0u8;
    let resp = loop {
        let resp = st
            .preview_http
            .get(target.clone())
            .header(header::ACCEPT, "text/html,application/xhtml+xml")
            .timeout(per_hop)
            .send()
            .await
            .map_err(classify)?;

        if !resp.status().is_redirection() {
            break resp;
        }
        hops += 1;
        if hops > MAX_HOPS {
            return Err(PreviewError::Upstream("too many redirects"));
        }
        let location = resp
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .ok_or(PreviewError::Upstream("redirect with no location"))?;
        // Relative Locations are legal and common.
        let next = target
            .join(location)
            .map_err(|_| PreviewError::Refused("unparseable redirect target"))?;
        target = validate(st, next.as_str()).await?;
    };

    if !resp.status().is_success() {
        // Not `error_for_status`: its error renders the URL it was made from.
        return Err(PreviewError::Upstream("upstream returned an error status"));
    }

    let content_type = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    if !is_html(&content_type) {
        return Err(PreviewError::NotHtml);
    }

    // Read at most the cap and stop, rather than buffering the whole body and
    // measuring afterwards — `bytes()` would do exactly that, whatever
    // Content-Length claimed.
    //
    // Over-cap truncates rather than failing. The metadata sits far earlier
    // than `</head>` does, so the cap keeps it and discards the rest: measured
    // 2026-09-04, the Guardian's `<title>` is at byte 195 with `</head>` at
    // 661 KB, and CNN's `og:title` at 310 KB with `</head>` at 2.4 MB. Failing
    // at the cap would drop both.
    //
    // The cap is sized from that: over 48 sites, recovering every available
    // field needs a median of 1.5 KB and a p95 of 28 KB, with two outliers —
    // CNN at 310 KB and YouTube at 705 KB. 512 KB is the smallest round value
    // that keeps CNN; 256 KB would lose it. Raising to 1 MB buys back only
    // YouTube, which never arrives here because the UI has a bespoke branch
    // for it. Don't move this without re-measuring.
    //
    // So there is deliberately no `content_length()` pre-check, which the PRD
    // asked for: refusing an honestly-declared 654 KB while happily truncating
    // the same page when it arrives chunked would be incoherent. We read at
    // most the cap either way, so the check bought nothing.
    let mut body: Vec<u8> = Vec::new();
    let mut truncated = false;
    let mut head_end: Option<usize> = None;
    let mut stream = resp.bytes_stream();
    use futures_util::StreamExt as _;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(classify)?;
        // Scan from just behind the tail so a `</head` split across two chunks
        // is still found.
        let scan_from = body.len().saturating_sub(HEAD_TAG.len() - 1);
        let room = (max as usize).saturating_sub(body.len());
        if chunk.len() > room {
            body.extend_from_slice(&chunk[..room]);
            truncated = true;
            break;
        }
        body.extend_from_slice(&chunk);

        if head_end.is_none() {
            head_end = find_head_end(&body, scan_from);
        }
        // Whole chunks are kept rather than trimmed to the stop point: the
        // extra bytes are already in hand, and cutting at an exact offset
        // could slice a tag in half.
        if head_end.is_some_and(|end| body.len() >= end.max(HEAD_FLOOR)) {
            break;
        }
    }

    Ok(Page {
        final_url: target,
        content_type,
        body,
        truncated,
    })
}

/// Offset just past a case-insensitive `</head`, searching from `from`.
///
/// The closing `>` is deliberately not required — `</head >` is legal, and
/// everything we came for is behind us either way.
fn find_head_end(hay: &[u8], from: usize) -> Option<usize> {
    hay.get(from..)?
        .windows(HEAD_TAG.len())
        .position(|w| w.eq_ignore_ascii_case(HEAD_TAG))
        .map(|i| from + i + HEAD_TAG.len())
}

async fn validate(st: &AppState, raw: &str) -> Result<Url, PreviewError> {
    let reserved = if st.config.allow_loopback_preview_targets {
        Reserved::AllowLoopback
    } else {
        Reserved::Refuse
    };
    net::validate_and_resolve_with(raw, SCHEMES, reserved)
        .await
        // The error names the host it refused, which is the one thing that may
        // not be logged. Only the fact survives.
        .map_err(|_| PreviewError::Refused("url refused by address validation"))
}

/// Ticket 03 needs `<head>` markup; anything else is not worth the bytes. A
/// missing type is refused too — guessing is how you end up parsing a PDF.
fn is_html(content_type: &str) -> bool {
    let essence = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    essence == "text/html" || essence == "application/xhtml+xml"
}

/// A `reqwest::Error` is discarded rather than wrapped: it renders the URL it
/// was made from, and this endpoint does not log those.
fn classify(e: reqwest::Error) -> PreviewError {
    if e.is_timeout() {
        PreviewError::Timeout
    } else if e.is_connect() {
        PreviewError::Upstream("could not connect")
    } else {
        // Includes the eucup case: TLS completes, then the connection dies
        // without a status line. A 502, not a panic and not a hang.
        PreviewError::Upstream("upstream request failed")
    }
}

/// The wrapped `{code, message, data}` shape the UI already expects.
fn envelope(status: StatusCode, message: Option<&str>, cache: &'static str) -> Response {
    let body = serde_json::json!({
        "code": status.as_u16(),
        "message": message,
        "data": serde_json::Value::Null,
    });
    let mut headers = HeaderMap::new();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    (status, headers, axum::Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_end_is_found_case_insensitively_and_across_a_split() {
        assert_eq!(find_head_end(b"<html><head></head><body>", 0), Some(18));
        assert_eq!(find_head_end(b"<HEAD></HEAD>", 0), Some(12));
        assert_eq!(find_head_end(b"</Head >", 0), Some(6));
        assert_eq!(find_head_end(b"<html><body>", 0), None);
        // The split case: a chunk ended mid-needle at offset 8, so the scan
        // backs up behind the tail instead of starting at the new bytes.
        let split = b"...</head>...";
        assert_eq!(find_head_end(split, 8 - (HEAD_TAG.len() - 1)), Some(9));
        assert_eq!(find_head_end(split, 8), None, "backing up is what finds it");
        // A `from` past the end must not panic.
        assert_eq!(find_head_end(b"abc", 99), None);
    }

    #[test]
    fn html_types_are_recognised_with_parameters_and_casing() {
        assert!(is_html("text/html"));
        assert!(is_html("text/html; charset=utf-8"));
        assert!(is_html("TEXT/HTML;charset=ISO-8859-1"));
        assert!(is_html(" application/xhtml+xml "));
        assert!(!is_html("text/plain"));
        assert!(!is_html("application/pdf"));
        assert!(!is_html("application/json"));
        // A prefix match would wave this through.
        assert!(!is_html("text/htmlish"));
        assert!(!is_html(""));
    }

    #[test]
    fn the_status_matrix_matches_the_prd() {
        let cases = [
            (PreviewError::Refused("x"), 400, REFUSED_CACHE),
            (PreviewError::NotHtml, 415, REFUSED_CACHE),
            (PreviewError::Upstream("x"), 502, FAILED_CACHE),
            (PreviewError::Timeout, 504, FAILED_CACHE),
        ];
        for (e, code, cache) in cases {
            let (status, _, got) = e.parts();
            assert_eq!(status.as_u16(), code);
            assert_eq!(got, cache);
        }
    }

    /// The UA is load-bearing in two directions: it must not name another
    /// company's bot, and the disclosure URL in it has to be reachable.
    #[test]
    fn the_user_agent_is_honest_and_points_somewhere() {
        assert!(USER_AGENT.starts_with("BrainstormBot/"));
        assert!(USER_AGENT.contains("+https://brainstorm.world/bot"));
        for impostor in ["Twitterbot", "facebookexternalhit", "Slackbot", "Mozilla"] {
            assert!(!USER_AGENT.contains(impostor), "impersonates {impostor}");
        }
    }
}
