use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Response},
};
use bytes::Bytes;
use serde::Deserialize;

use crate::cards::{self, EntityCard, Skip};
use crate::data::{self, Card, ShortTarget};
use crate::nip19::{self, Entity, Pointer};
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
        // Every memory figure for this service was previously arithmetic. moka
        // hands these over for free, so what the caches actually hold is a
        // fact rather than an estimate.
        "caches": {
            "card": cache_stats(&st.card_cache),
            "overview": cache_stats(&st.overview_cache),
            "entity": cache_stats(&st.entity_cache),
            "short_code": cache_stats(&st.short_code_cache),
            "png": cache_stats(&st.png_cache),
            "preview_rate": cache_stats(&st.preview_rate),
            "link_preview": cache_stats(&st.link_preview_cache),
            "robots": cache_stats(&st.robots_cache),
            "host_backoff": cache_stats(&st.host_backoff),
        },
    });
    (StatusCode::OK, axum::Json(body)).into_response()
}

/// Both figures moka keeps, per cache.
///
/// `run_pending_tasks()` is deliberately not called: it would make these exact
/// at the cost of doing the cache's housekeeping inline, and this endpoint is
/// on a readiness probe. Approximate and cheap is the right trade — these are
/// for capacity review, not accounting. A cache with no weigher weighs every
/// entry as 1, so `weighted_size` there is the entry count by another name.
fn cache_stats<K, V, S>(cache: &moka::future::Cache<K, V, S>) -> serde_json::Value
where
    K: std::hash::Hash + Eq + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
    S: std::hash::BuildHasher + Clone + Send + Sync + 'static,
{
    serde_json::json!({
        "entry_count": cache.entry_count(),
        "weighted_size": cache.weighted_size(),
    })
}

/// What a meta route answers with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Form {
    /// A whole HTML document: `/p/`, `/e/`, `/s/` asked of this service directly.
    Page,
    /// Only the tags, for the UI's nginx to include in the SPA's own `<head>`:
    /// `/meta/p/`, `/meta/e/`, `/meta/s/` (routed in `lib.rs`). Every visitor
    /// gets them, so no one has to guess which User-Agents are link previewers.
    Fragment,
}

/// Meta tags. `/p/{id}` (canonical) and `/profile/{id}` (legacy).
pub async fn profile(st: State<AppState>, id: Path<String>) -> Response {
    profile_as(st, id, Form::Page).await
}

pub async fn profile_as(
    State(st): State<AppState>,
    Path(id): Path<String>,
    form: Form,
) -> Response {
    let pointer = match nip19::decode(&id) {
        Ok(p) => p,
        Err(e) => {
            tracing::debug!("bad profile id {id}: {e}");
            return bad_id();
        }
    };

    let card = data::get_profile(&st, &pointer).await;
    meta_response(&st, &id, &card, form)
}

/// `/s/{code}` — a short share link. Renders the card its target's own route does.
///
/// An unresolvable code returns 404 rather than a card: a preview claiming
/// something that doesn't exist is worse than no preview.
pub async fn short_link(st: State<AppState>, code: Path<String>) -> Response {
    short_link_as(st, code, Form::Page).await
}

pub async fn short_link_as(
    State(st): State<AppState>,
    Path(code): Path<String>,
    form: Form,
) -> Response {
    match data::resolve_short_code(&st, &code).await {
        Some(ShortTarget::Profile(hex)) => {
            let pointer = Pointer { hex };
            let card = data::get_profile(&st, &pointer).await;
            // Canonical is the hint-free npub, so short-link and npub shares consolidate.
            let id = nip19::npub_from_hex(&pointer.hex).unwrap_or_else(|| pointer.hex.clone());
            meta_response(&st, &id, &card, form)
        }
        Some(ShortTarget::Entity(entity)) => entity_page(&st, &entity, form).await,
        Some(ShortTarget::Other) | None => unknown_code(),
    }
}

/// `/e/{nevent|note|naddr}` — a note, article, calendar or place card.
pub async fn entity(st: State<AppState>, id: Path<String>) -> Response {
    entity_as(st, id, Form::Page).await
}

pub async fn entity_as(State(st): State<AppState>, Path(id): Path<String>, form: Form) -> Response {
    match nip19::decode_entity(&id) {
        Ok(entity) => entity_page(&st, &entity, form).await,
        Err(e) => {
            tracing::debug!("bad entity id {id}: {e}");
            bad_id()
        }
    }
}

async fn entity_page(st: &AppState, entity: &Entity, form: Form) -> Response {
    match cards::get(st, entity).await {
        Ok(card) => entity_meta_response(st, &card, form),
        Err(skip) => skipped(st, skip),
    }
}

/// A skipped card: not-found may just be new so is short-lived; the rest cache like a card.
fn skipped(st: &AppState, skip: Skip) -> Response {
    let mut headers = HeaderMap::new();
    let max_age = match skip {
        Skip::NotFound => 60,
        _ => st.config.cache_ttl_secs,
    };
    set_cache(&mut headers, &format!("public, max-age={max_age}"));
    (StatusCode::NOT_FOUND, headers).into_response()
}

/// Briefly cached, unlike `bad_id`: an unknown code is a fact about the
/// database, not a permanently malformed input.
fn unknown_code() -> Response {
    let mut headers = HeaderMap::new();
    set_cache(&mut headers, "public, max-age=60");
    (StatusCode::NOT_FOUND, headers).into_response()
}

/// `?v=` is the content hash. Never read; it exists so a changed card gets a
/// different URL.
#[derive(Deserialize)]
pub struct ImageQuery {
    #[allow(dead_code)]
    v: Option<String>,
}

/// `/og/{id}.png` — a profile card, or, for a `nevent`/`note`/`naddr`, the
/// entity card `/og/e/{id}.png` draws. Pages advertise `/og/e/` for entities;
/// this spelling is accepted too because it is the one people guess.
pub async fn og_image(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Query(_q): Query<ImageQuery>,
) -> Response {
    let id = id.strip_suffix(".png").unwrap_or(&id).to_string();
    let pointer = match nip19::decode(&id) {
        Ok(p) => p,
        Err(_) => match nip19::decode_entity(&id) {
            Ok(entity) => return entity_image(&st, &entity).await,
            Err(_) => return bad_id(),
        },
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
            render_failed(&st).await
        }
    }
}

/// Not cached: a transient failure must not outlive itself.
async fn render_failed(st: &AppState) -> Response {
    let mut headers = HeaderMap::new();
    set_cache(&mut headers, "no-store");
    match render::render_fallback(st).await {
        Some(bytes) => {
            headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/png"));
            (headers, Bytes::from(bytes)).into_response()
        }
        None => (StatusCode::SERVICE_UNAVAILABLE, headers).into_response(),
    }
}

/// `/og/e/{id}.png` — an entity card. The PNG cache keys on the entity.
pub async fn og_entity_image(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Query(_q): Query<ImageQuery>,
) -> Response {
    let id = id.strip_suffix(".png").unwrap_or(&id);
    let Ok(entity) = nip19::decode_entity(id) else {
        return bad_id();
    };
    entity_image(&st, &entity).await
}

async fn entity_image(st: &AppState, entity: &Entity) -> Response {
    let key = entity.key();
    if let Some(cached) = st.png_cache.get(&key).await {
        return image_response(st, cached);
    }
    let card = match cards::get(st, entity).await {
        Ok(card) => card,
        Err(skip) => return skipped(st, skip),
    };
    match render::render_entity(st, &card).await {
        Ok(bytes) => {
            let bytes = Bytes::from(bytes);
            st.png_cache.insert(key, bytes.clone()).await;
            image_response(st, bytes)
        }
        Err(e) => {
            tracing::warn!("render failed for {key}: {e}");
            render_failed(st).await
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

/// The profile page `/p/{id}` and a profile `/s/{code}` return.
fn meta_response(st: &AppState, id: &str, card: &Card, form: Form) -> Response {
    let app = &st.config.app_base_url;
    let title = format!("{} on Brainstorm", card.display_name());
    html_response(
        st,
        &Page {
            description: meta_description(card),
            // From config, never the request Host. See CONTEXT.md.
            page_url: format!("{app}/p/{id}"),
            image: PageImage::Card(format!(
                "{app}/og/{id}.png?v={}",
                card.version(&st.config.render_epoch)
            )),
            image_alt: title.clone(),
            title,
            og_type: "profile",
            extra: vec![],
        },
        form,
    )
}

/// The `/e/` page: canonical is the hint-free `/e/` link. The entity's own
/// picture when it has one, else its share card.
fn entity_meta_response(st: &AppState, card: &EntityCard, form: Form) -> Response {
    let app = &st.config.app_base_url;
    let id = card.entity.canonical();
    let meta = &card.built.meta;
    let image = match &meta.image {
        Some(url) => own_image(st, url),
        None => PageImage::Card(format!(
            "{app}/og/e/{id}.png?v={}",
            card.version(&st.config.render_epoch)
        )),
    };
    html_response(
        st,
        &Page {
            title: meta.title.clone(),
            description: meta.description.clone(),
            page_url: format!("{app}/e/{id}"),
            image,
            image_alt: meta.title.clone(),
            og_type: meta.og_type,
            extra: meta.extra.clone(),
        },
        form,
    )
}

struct Page {
    title: String,
    description: String,
    page_url: String,
    image: PageImage,
    image_alt: String,
    og_type: &'static str,
    extra: Vec<(&'static str, String)>,
}

/// What `og:image` points at.
enum PageImage {
    /// Our 1200×630 PNG, so its type and size can be stated.
    Card(String),
    /// The entity's own picture through our image proxy: a JPEG, size unknown.
    Proxied(String),
    /// The entity's own picture where its author put it: nothing known.
    Original(String),
}

impl PageImage {
    fn url(&self) -> &str {
        match self {
            PageImage::Card(u) | PageImage::Proxied(u) | PageImage::Original(u) => u,
        }
    }
}

/// An entity's own picture, as previewers should fetch it: through the UI's
/// imgproxy (`IMG_PROXY_PATH`), resized to 1280 wide and re-encoded as JPEG.
///
/// The original can be megabytes (WhatsApp shows no image past ~600 KB), AVIF
/// or WebP (not every previewer decodes them), or plain `http://`; the proxied
/// copy is none of those, on our https origin. Same URL shape as the UI's
/// `proxiedSrc` (lib/imageProxy.ts), with `@jpg` asking for the format. When
/// the proxy is down, nginx redirects a non-browser fetch to the original.
fn own_image(st: &AppState, url: &str) -> PageImage {
    let proxy = &st.config.img_proxy_path;
    if proxy.is_empty() {
        return PageImage::Original(url.to_string());
    }
    PageImage::Proxied(format!(
        "{}{proxy}/insecure/media_1280/plain/{}@jpg",
        st.config.app_base_url,
        encode_uri_component(url)
    ))
}

/// JavaScript's `encodeURIComponent`: everything but `A-Z a-z 0-9 - _ . ! ~ * ' ( )`.
fn encode_uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn html_response(st: &AppState, page: &Page, form: Form) -> Response {
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
    let body = match form {
        Form::Page => page_html(page),
        Form::Fragment => head_tags(page),
    };
    (headers, Html(body)).into_response()
}

fn set_cache(headers: &mut HeaderMap, value: &str) {
    if let Ok(v) = HeaderValue::from_str(value) {
        headers.insert(header::CACHE_CONTROL, v);
    }
}

/// The tags that describe the page: everything in its `<head>` except what
/// the SPA's own `index.html` already has (charset, viewport, `<title>`).
///
/// `<title>` stays out on purpose: it is the browser tab's name for a human,
/// and a page load's title outlives the screen it named (the app's share
/// screens restore the title they found). `og:title` is what previews read.
fn head_tags(page: &Page) -> String {
    let title = esc(&page.title);
    let desc = esc(&page.description);
    let page_url = esc(&page.page_url);
    let image_url = esc(page.image.url());
    let image_alt = esc(&page.image_alt);
    let og_type = esc(page.og_type);
    let extra: String = page
        .extra
        .iter()
        .map(|(p, c)| format!("<meta property=\"{}\" content=\"{}\"/>\n", esc(p), esc(c)))
        .collect();
    // A note that is only a picture has no words to describe it with.
    let (desc_tag, og_desc, tw_desc) = if desc.is_empty() {
        Default::default()
    } else {
        (
            format!("<meta name=\"description\" content=\"{desc}\"/>\n"),
            format!("<meta property=\"og:description\" content=\"{desc}\"/>\n"),
            format!("<meta name=\"twitter:description\" content=\"{desc}\"/>\n"),
        )
    };
    let card_facts = match page.image {
        PageImage::Card(_) => concat!(
            "<meta property=\"og:image:type\" content=\"image/png\"/>\n",
            "<meta property=\"og:image:width\" content=\"1200\"/>\n",
            "<meta property=\"og:image:height\" content=\"630\"/>\n",
        ),
        PageImage::Proxied(_) => "<meta property=\"og:image:type\" content=\"image/jpeg\"/>\n",
        PageImage::Original(_) => "",
    };

    format!(
        r#"{desc_tag}<link rel="canonical" href="{page_url}"/>
<meta property="og:type" content="{og_type}"/>
{extra}<meta property="og:site_name" content="Brainstorm"/>
<meta property="og:title" content="{title}"/>
{og_desc}<meta property="og:url" content="{page_url}"/>
<meta property="og:image" content="{image_url}"/>
{card_facts}<meta property="og:image:alt" content="{image_alt}"/>
<meta name="twitter:card" content="summary_large_image"/>
<meta name="twitter:title" content="{title}"/>
{tw_desc}<meta name="twitter:image" content="{image_url}"/>
<meta name="twitter:image:alt" content="{image_alt}"/>
"#
    )
}

fn page_html(page: &Page) -> String {
    let title = esc(&page.title);
    let page_url = esc(&page.page_url);
    let tags = head_tags(page);

    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8"/>
<meta name="viewport" content="width=device-width, initial-scale=1"/>
<title>{title}</title>
{tags}</head>
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
    fn encode_uri_component_matches_javascript() {
        // encodeURIComponent("https://x.com/a b.png?w=1&h=2#f") in a browser.
        assert_eq!(
            encode_uri_component("https://x.com/a b.png?w=1&h=2#f"),
            "https%3A%2F%2Fx.com%2Fa%20b.png%3Fw%3D1%26h%3D2%23f"
        );
        assert_eq!(encode_uri_component("-_.!~*'()"), "-_.!~*'()");
        assert_eq!(encode_uri_component("é@"), "%C3%A9%40");
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
