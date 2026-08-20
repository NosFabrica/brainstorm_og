use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Response},
};
use bytes::Bytes;
use serde::Deserialize;

use crate::data::{self, Card};
use crate::nip19;
use crate::render;
use crate::state::AppState;

pub async fn healthz(State(st): State<AppState>) -> Response {
    // Face count is here because a fontless image renders every card blank,
    // and `scratch` gives you no shell to diagnose it with.
    let body = serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "font_family": st.config.font_family,
        "faces": st.fontdb.len(),
    });
    (StatusCode::OK, axum::Json(body)).into_response()
}

/// Meta tags for crawlers. `/p/{id}` (canonical) and `/profile/{id}` (legacy).
pub async fn profile(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    let pointer = match nip19::decode(&id) {
        Ok(p) => p,
        Err(e) => {
            tracing::debug!("bad profile id {id}: {e}");
            return bad_id();
        }
    };

    let card = data::get_card(&st, &pointer).await;
    let html = build_meta_html(&st, &id, &card);

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    // Short: this document exists to advertise the current `?v=`.
    set_cache(
        &mut headers,
        &format!("public, max-age={}", st.config.html_cache_max_age),
    );
    (headers, Html(html)).into_response()
}

/// `?v=` is the content hash. Never read; it exists so a changed card gets a
/// different URL.
#[derive(Deserialize)]
pub struct ImageQuery {
    #[allow(dead_code)]
    v: Option<String>,
}

pub async fn og_image(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Query(_q): Query<ImageQuery>,
) -> Response {
    let id = id.strip_suffix(".png").unwrap_or(&id).to_string();
    let pointer = match nip19::decode(&id) {
        Ok(p) => p,
        Err(_) => return bad_id(),
    };

    if let Some(cached) = st.png_cache.get(&pointer.hex).await {
        return image_response(&st, cached);
    }

    let card = data::get_card(&st, &pointer).await;
    match render::render_card(&st, &card).await {
        Ok(bytes) => {
            let bytes = Bytes::from(bytes);
            st.png_cache
                .insert(pointer.hex.clone(), bytes.clone())
                .await;
            image_response(&st, bytes)
        }
        Err(e) => {
            tracing::warn!("render failed for {}: {e}", pointer.hex);
            // Not cached: a transient failure must not outlive itself.
            match render::render_fallback(&st).await {
                Some(bytes) => {
                    let mut headers = png_headers();
                    set_cache(&mut headers, "no-store");
                    (headers, Bytes::from(bytes)).into_response()
                }
                None => {
                    let mut headers = HeaderMap::new();
                    set_cache(&mut headers, "no-store");
                    (StatusCode::SERVICE_UNAVAILABLE, headers).into_response()
                }
            }
        }
    }
}

/// A malformed id never becomes valid, so caches may absorb the retry.
fn bad_id() -> Response {
    let mut headers = HeaderMap::new();
    set_cache(&mut headers, "public, max-age=3600");
    (StatusCode::BAD_REQUEST, headers).into_response()
}

fn png_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/png"));
    headers
}

fn image_response(st: &AppState, bytes: Bytes) -> Response {
    let mut headers = png_headers();
    // Immutable: the URL carries a content hash, so this mapping never changes.
    set_cache(
        &mut headers,
        &format!(
            "public, max-age={}, immutable",
            st.config.image_cache_max_age
        ),
    );
    (headers, bytes).into_response()
}

fn set_cache(headers: &mut HeaderMap, value: &str) {
    if let Ok(v) = HeaderValue::from_str(value) {
        headers.insert(header::CACHE_CONTROL, v);
    }
}

fn build_meta_html(st: &AppState, id: &str, card: &Card) -> String {
    let app = &st.config.app_base_url;
    let title = esc(&format!("{} on Brainstorm", card.display_name()));
    let desc = esc(&meta_description(card));

    // From config, never the request Host. See CONTEXT.md.
    let page_url = esc(&format!("{app}/p/{id}"));
    let image_url = esc(&format!(
        "{app}/og/{id}.png?v={}",
        card.version(&st.config.render_epoch)
    ));
    let image_alt = esc(&format!("{} on Brainstorm", card.display_name()));

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
<meta property="og:image:type" content="image/png"/>
<meta property="og:image:width" content="1200"/>
<meta property="og:image:height" content="630"/>
<meta property="og:image:alt" content="{image_alt}"/>
<meta name="twitter:card" content="summary_large_image"/>
<meta name="twitter:title" content="{title}"/>
<meta name="twitter:description" content="{desc}"/>
<meta name="twitter:image" content="{image_url}"/>
<meta name="twitter:image:alt" content="{image_alt}"/>
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
        // Deliberately no score here either — the card does not show one, and
        // putting it in the description would still be publishing it.
        Some(o) => format!(
            "{} followers · {} following on Brainstorm.",
            o.followers, o.following
        ),
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

/// HTML escaping. `&#39;` here vs `&apos;` in `render::esc` — `&apos;` is not
/// an HTML4 entity. Do not unify them.
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPOCH: &str = "test-epoch";
    use crate::data::{Overview, ProfileMeta};

    fn card_with(about: Option<&str>, influence: Option<f64>) -> Card {
        Card {
            hex: "a".repeat(64),
            meta: ProfileMeta {
                display_name: Some("Alice".into()),
                about: about.map(str::to_string),
                ..Default::default()
            },
            overview: Some(Overview {
                influence,
                followers: 12,
                following: 34,
                tier: Some("high".into()),
            }),
            provisional: false,
        }
    }

    #[test]
    fn esc_covers_the_html_five() {
        assert_eq!(esc(r#"<&>"'"#), "&lt;&amp;&gt;&quot;&#39;");
        // Ampersand must be escaped first or the others double-escape.
        assert_eq!(esc("&lt;"), "&amp;lt;");
    }

    #[test]
    fn clip_counts_chars_not_bytes() {
        assert_eq!(clip("abc", 10), "abc");
        assert_eq!(clip("abcdef", 4), "abc…");
        assert_eq!(clip("さとうさとう", 3), "さと…");
    }

    #[test]
    fn description_prefers_about_then_stats() {
        assert_eq!(
            meta_description(&card_with(Some("  hello   world "), Some(0.4))),
            "hello world"
        );
        // Whitespace-only `about` must fall through, not render blank.
        let s = meta_description(&card_with(Some("   "), Some(0.4)));
        assert!(s.contains("12 followers"), "got {s}");
        assert!(s.contains("34 following"), "got {s}");

        // The score is never published — not on the card, not here.
        for c in [card_with(None, Some(0.98)), card_with(None, None)] {
            let s = meta_description(&c).to_lowercase();
            assert!(
                !s.contains("score"),
                "score leaked into the description: {s}"
            );
            assert!(!s.contains("98"), "score leaked into the description: {s}");
        }
    }

    #[test]
    fn image_url_carries_the_content_hash() {
        let st_cfg = crate::config::Config::from_env();
        let card = card_with(None, Some(0.42));
        // Build the URL the same way the template does.
        let url = format!(
            "{}/og/{}.png?v={}",
            st_cfg.app_base_url,
            "npubxyz",
            card.version(EPOCH)
        );
        assert!(url.contains("?v="));
        // A differing score leaves it alone — the card does not draw one.
        let other = card_with(None, Some(0.55));
        assert_eq!(card.version(EPOCH), other.version(EPOCH));
    }
}
