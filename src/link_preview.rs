//! `GET /link-preview?url=…` — we fetch *someone else's* page. The inverse
//! direction from an unfurl, which is a crawler reading our share link.
//!
//! The URL is attacker-controlled, so nothing here trusts it: `net` judges the
//! address before every connection, `hop` follows redirects by hand so each hop
//! is judged too, and the body is capped while it streams rather than after it
//! has been buffered. `robots` and `backoff` decide whether a host may be asked
//! at all.
//!
//! `parse` turns those bytes into the fields a card needs; this file stops at
//! "we have the bytes". `rate_limit` keeps the whole thing from being an open
//! fetch proxy.

pub mod backoff;
pub mod cache;
pub(crate) mod hop;
pub mod parse;
pub mod rate_limit;
pub mod robots;

use axum::{
    extract::{Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use std::time::Duration;
use url::Url;

use crate::link_preview::cache::Outcome;
use crate::link_preview::hop::{Arrived, FollowError, HopCheck};
use crate::link_preview::parse::Kind;
use crate::state::AppState;

/// Honest, resolvable, and never another company's preview bot. The measured
/// cost is four sites out of 56; see the ADR. It also matches nginx's
/// `$is_og_bot` regex, so our own `/p/` links preview correctly.
pub const USER_AGENT: &str = "BrainstormBot/1.0 (+https://brainstorm.world/bot)";

/// The only schemes we dial — and, in `parse`, the only ones we will hand a
/// browser to load.
pub(crate) const SCHEMES: &[&str] = &["http", "https"];

/// We stop reading once `<head>` has closed — everything a preview needs is
/// inside it, and the rest is bytes nobody looks at. Measured over 48 sites,
/// this cuts what we pull by ~60%, with the median read dropping from the
/// whole page to 9.5 KB. Being a light fetcher is the other half of what ADR
/// (ii) promises, alongside honouring robots.txt: identify honestly, take only
/// what a preview needs, and respect the directive.
const HEAD_TAG: &[u8] = b"</head";

/// ...but never stop before this. A few real sites close `<head>` and then put
/// `<title>` and the `og:` tags in the body — blockstream.com closes at byte
/// 1,483 and its tags land at 4.6 KB. Measured: an 8 KB floor recovers every
/// such site in the sample and costs 1% of the saving. It is deliberately
/// below the median `</head>` (9.5 KB), so it binds only on the anomalies.
const HEAD_FLOOR: usize = 8 * 1024;

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
    /// Decided from `Content-Type` while fetching; media bodies are never read.
    pub kind: Kind,
    /// After redirects. `parse` resolves a relative `og:image` against this.
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
    /// The host is off-limits for now: backing off, or its robots.txt could not
    /// be read. Its state lives with the host, so this URL caches nothing.
    Unavailable(&'static str),
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
            Self::Upstream(_) | Self::Unavailable(_) => (
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
            Self::Refused(r) | Self::Upstream(r) | Self::Unavailable(r) => r,
            Self::NotHtml => "content type is not html",
            Self::Timeout => "deadline exceeded",
        }
    }
}

pub async fn link_preview(State(st): State<AppState>, Query(q): Query<PreviewQuery>) -> Response {
    let Some(raw) = q.url.filter(|u| !u.is_empty()) else {
        return refused(PreviewError::Refused("no url parameter"));
    };
    // A URL that will not parse never reaches the cache: it costs nothing to
    // reject and would otherwise be an entry per typo.
    let Some(target) = cache::normalise(&raw) else {
        return refused(PreviewError::Refused("unparseable url"));
    };

    let loader = st.clone();
    let key = target.to_string();
    let for_cache = key.clone();
    let outcome = st
        .link_preview_cache
        .get_with(for_cache, async move {
            match fetch_url(&loader, target).await {
                // A 200 with nulls is a real answer: a page with no usable
                // markup degrades the card rather than erroring.
                Ok(Some(page)) => {
                    Outcome::Ok(parse::preview(&page, loader.config.reserved_policy()))
                }
                // robots.txt refused us. Also a real answer, and the same one:
                // nothing went wrong, nothing is worth retrying, and the card
                // degrades exactly as it does for a page with no markup.
                Ok(None) => Outcome::Ok(parse::Preview {
                    url: key,
                    ..Default::default()
                }),
                Err(e) => Outcome::Failed(e),
            }
        })
        .await;

    match outcome {
        Outcome::Ok(preview) => {
            let data = serde_json::to_value(preview).expect("Preview is plain data");
            // The browser and nginx hold it for exactly as long as we do.
            let cache = format!("public, max-age={}", st.config.link_preview_cache_ttl_secs);
            envelope(StatusCode::OK, None, &cache, data)
        }
        Outcome::Failed(e) => refused(e),
    }
}

fn refused(e: PreviewError) -> Response {
    tracing::debug!(reason = e.reason(), "link preview failed");
    let (status, message, cache) = e.parts();
    envelope(status, Some(message), cache, serde_json::Value::Null)
}

/// Fetch a third-party page, or say why not. Public so a test can parse what
/// it returns without going back through the router.
pub async fn fetch(st: &AppState, raw: &str) -> Result<Option<Page>, PreviewError> {
    let target = cache::normalise(raw).ok_or(PreviewError::Refused("unparseable url"))?;
    fetch_url(st, target).await
}

/// The fetch proper. The handler already has a parsed URL — it normalised one
/// to build the cache key — so handing a string back would be re-parsing.
async fn fetch_url(st: &AppState, target: Url) -> Result<Option<Page>, PreviewError> {
    let deadline = Duration::from_secs(st.config.link_preview_deadline_secs);
    // Bounds the whole chain, not each hop: three hops that each answer just
    // inside the per-hop timeout would otherwise sit well past it.
    tokio::time::timeout(deadline, follow_and_read(st, target))
        .await
        .unwrap_or(Err(PreviewError::Timeout))
}

async fn follow_and_read(st: &AppState, target: Url) -> Result<Option<Page>, PreviewError> {
    let per_hop = Duration::from_secs(st.config.link_preview_timeout_secs);
    let accept = "text/html,application/xhtml+xml";
    let (response, final_url) = match hop::follow(
        st,
        target.as_str(),
        accept,
        per_hop,
        HopCheck::AddressAndHost,
    )
    .await?
    {
        Some(Arrived { response, url }) => (response, url),
        // robots.txt disallows a hop: an answer, not an error.
        None => return Ok(None),
    };

    let status = response.status();
    if backoff::is_slow_down(status) {
        backoff::back_off(st, &final_url, response.headers()).await;
        return Err(PreviewError::Unavailable("upstream asked us to slow down"));
    }
    if !status.is_success() {
        // Not `error_for_status`: its error renders the URL it was made from.
        return Err(PreviewError::Upstream("upstream returned an error status"));
    }

    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let kind = kind_of(&content_type).ok_or(PreviewError::NotHtml)?;
    // Media hosts serve pictures and clips from extensionless URLs
    // (m.stacker.news), which the UI cannot tell from a page. The headers can;
    // the body is not read.
    if kind != Kind::Page {
        return Ok(Some(Page {
            kind,
            final_url,
            content_type,
            body: Vec::new(),
            truncated: false,
        }));
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
    let mut head_end: Option<usize> = None;
    let (body, truncated) = hop::read_capped(
        response,
        st.config.link_preview_max_bytes as usize,
        |body, before| {
            // Scan from just behind the previous tail so a `</head` split across
            // two chunks is still found.
            if head_end.is_none() {
                head_end = find_head_end(body, before.saturating_sub(HEAD_TAG.len() - 1));
            }
            // Whole chunks are kept rather than trimmed to the stop point:
            // cutting at an exact offset could slice a tag in half.
            head_end.is_some_and(|end| body.len() >= end.max(HEAD_FLOOR))
        },
    )
    .await
    .map_err(classify)?;

    Ok(Some(Page {
        kind,
        final_url,
        content_type,
        body,
        truncated,
    }))
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

/// Whether a host may be asked at all: not while it asked us to slow down, and
/// only where its robots.txt allows. `Ok(false)` is a Disallow — an answer.
pub(crate) async fn host_allows(st: &AppState, target: &Url) -> Result<bool, PreviewError> {
    if backoff::is_backing_off(st, target).await {
        return Err(PreviewError::Unavailable("upstream asked us to slow down"));
    }
    match robots::verdict(st, target).await {
        robots::Verdict::Allowed => Ok(true),
        robots::Verdict::Disallowed => Ok(false),
        robots::Verdict::Unreadable => {
            Err(PreviewError::Unavailable("robots.txt could not be read"))
        }
    }
}

impl From<FollowError> for PreviewError {
    fn from(e: FollowError) -> Self {
        match e {
            FollowError::Refused(r) => Self::Refused(r),
            FollowError::Transport(e) => classify(e),
            FollowError::Redirects(r) => Self::Upstream(r),
            FollowError::Host(e) => e,
        }
    }
}

/// What the `Content-Type` says we fetched, or `None` for anything we won't use.
/// A missing type is refused too — guessing is how you end up parsing a PDF.
fn kind_of(content_type: &str) -> Option<Kind> {
    let essence = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    match essence.as_str() {
        "text/html" | "application/xhtml+xml" => Some(Kind::Page),
        // Formats every browser draws. TIFF and HEIC stay refused: telling the
        // UI "image" for something it can't render is worse than no card.
        "image/jpeg" | "image/png" | "image/gif" | "image/webp" | "image/avif" => Some(Kind::Image),
        // Played in a `<video>` without a plugin; `video/quicktime` is Safari-only.
        "video/mp4" | "video/webm" => Some(Kind::Video),
        _ => None,
    }
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
fn envelope(
    status: StatusCode,
    message: Option<&str>,
    cache: &str,
    data: serde_json::Value,
) -> Response {
    let body = serde_json::json!({
        "code": status.as_u16(),
        "message": message,
        "data": data,
    });
    let mut headers = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_str(cache) {
        headers.insert(header::CACHE_CONTROL, v);
    }
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
    fn content_types_are_classified_with_parameters_and_casing() {
        assert_eq!(kind_of("text/html"), Some(Kind::Page));
        assert_eq!(kind_of("text/html; charset=utf-8"), Some(Kind::Page));
        assert_eq!(kind_of("TEXT/HTML;charset=ISO-8859-1"), Some(Kind::Page));
        assert_eq!(kind_of(" application/xhtml+xml "), Some(Kind::Page));
        assert_eq!(kind_of("image/jpeg"), Some(Kind::Image));
        assert_eq!(kind_of("IMAGE/PNG; q=1"), Some(Kind::Image));
        assert_eq!(kind_of("video/mp4"), Some(Kind::Video));
        assert_eq!(kind_of("VIDEO/WEBM; codecs=vp9"), Some(Kind::Video));
        assert_eq!(kind_of("image/tiff"), None, "browsers cannot draw it");
        assert_eq!(kind_of("image/svg+xml"), None);
        assert_eq!(kind_of("video/quicktime"), None, "Safari-only");
        assert_eq!(kind_of("text/plain"), None);
        assert_eq!(kind_of("application/pdf"), None);
        assert_eq!(kind_of("application/json"), None);
        // A prefix match would wave this through.
        assert_eq!(kind_of("text/htmlish"), None);
        assert_eq!(kind_of(""), None);
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
