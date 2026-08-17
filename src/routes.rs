use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Response},
};
use std::sync::Arc;

use crate::data::{self, Card};
use crate::nip19;
use crate::render;
use crate::state::AppState;

pub async fn healthz() -> &'static str {
    "ok"
}

/// Crawler-facing HTML: per-profile OG/Twitter meta tags. Humans are routed to the
/// SPA by the ingress and never reach this; the body link is just a courtesy.
pub async fn profile(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let pointer = match nip19::decode(&id) {
        Ok(p) => p,
        Err(e) => {
            tracing::debug!("bad profile id {id}: {e}");
            return StatusCode::NOT_FOUND.into_response();
        }
    };

    let card = data::get_card(&st, &pointer).await;
    let base = request_base_url(&st, &headers);
    let html = build_meta_html(&base, &id, &card);

    (cache_headers("text/html; charset=utf-8", &st), Html(html)).into_response()
}

/// Build the public origin from the request itself so og:url / og:image always
/// match the address the crawler used (prod domain, tunnel, or localhost). Falls
/// back to APP_BASE_URL only when no Host is present.
fn request_base_url(st: &AppState, headers: &HeaderMap) -> String {
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(header::HOST))
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|h| !h.is_empty());

    match host {
        Some(h) => {
            let scheme = if h.starts_with("localhost")
                || h.starts_with("127.0.0.1")
                || h.starts_with("[::1]")
            {
                "http"
            } else {
                "https"
            };
            format!("{scheme}://{h}")
        }
        None => st.config.app_base_url.clone(),
    }
}

/// Generated PNG card. `/og/{id}.png`.
pub async fn og_image(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    let id = id.strip_suffix(".png").unwrap_or(&id).to_string();
    let pointer = match nip19::decode(&id) {
        Ok(p) => p,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };

    let png = if let Some(cached) = st.png_cache.get(&pointer.hex).await {
        cached
    } else {
        let card = data::get_card(&st, &pointer).await;
        let bytes = match render::render_card(&st, &card).await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("render failed for {}: {e}", pointer.hex);
                render::render_fallback(&st)
            }
        };
        let arc = Arc::new(bytes);
        st.png_cache.insert(pointer.hex.clone(), arc.clone()).await;
        arc
    };

    (cache_headers("image/png", &st), png.to_vec()).into_response()
}

fn cache_headers(content_type: &'static str, st: &AppState) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    let cc = format!("public, max-age={}", st.config.http_cache_max_age);
    if let Ok(v) = HeaderValue::from_str(&cc) {
        headers.insert(header::CACHE_CONTROL, v);
    }
    headers
}

fn build_meta_html(app: &str, id: &str, card: &Card) -> String {
    let title = esc(&card.meta.best_name().unwrap_or_else(|| "Nostr profile on Brainstorm".to_string()));
    let desc = esc(&meta_description(card));
    let page_url = esc(&format!("{app}/profile/{id}"));
    let image_url = esc(&format!("{app}/og/{id}.png"));

    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8"/>
<meta name="viewport" content="width=device-width, initial-scale=1"/>
<title>{title}</title>
<meta name="description" content="{desc}"/>
<link rel="canonical" href="{page_url}"/>
<meta property="og:type" content="profile"/>
<meta property="og:site_name" content="Brainstorm"/>
<meta property="og:title" content="{title}"/>
<meta property="og:description" content="{desc}"/>
<meta property="og:url" content="{page_url}"/>
<meta property="og:image" content="{image_url}"/>
<meta property="og:image:width" content="1200"/>
<meta property="og:image:height" content="630"/>
<meta name="twitter:card" content="summary_large_image"/>
<meta name="twitter:title" content="{title}"/>
<meta name="twitter:description" content="{desc}"/>
<meta name="twitter:image" content="{image_url}"/>
</head>
<body>
<p>Viewing <a href="{page_url}">{title}</a> on Brainstorm — the Web of Trust layer for Nostr.</p>
</body>
</html>"#
    )
}

fn meta_description(card: &Card) -> String {
    if let Some(about) = card.meta.about.as_deref() {
        let about = about.split_whitespace().collect::<Vec<_>>().join(" ");
        if !about.is_empty() {
            return clip(&about, 180);
        }
    }
    match &card.overview {
        Some(o) => {
            let inf = o
                .influence
                .map(|v| {
                    let scaled = if v <= 1.0 { v * 100.0 } else { v };
                    format!("Influence {:.0} · ", scaled.round())
                })
                .unwrap_or_default();
            format!("{inf}{} followers · {} following on Brainstorm.", o.followers, o.following)
        }
        None => "A Nostr profile on Brainstorm — the Web of Trust layer for Nostr.".to_string(),
    }
}

fn clip(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        s.to_string()
    } else {
        let mut out: String = chars[..max.saturating_sub(1)].iter().collect();
        out.push('…');
        out
    }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
