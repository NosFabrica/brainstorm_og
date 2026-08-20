//! The share card: 1200x630 PNG, SVG -> raster via resvg. Identity only —
//! wordmark, avatar, name, nip05. See CONTEXT.md.

use anyhow::{anyhow, Result};
use base64::Engine as _;
use resvg::{tiny_skia, usvg};
use std::io::Cursor;
use std::time::Duration;

use crate::data::Card;
use crate::state::AppState;

pub const WIDTH: u32 = 1200;
pub const HEIGHT: u32 = 630;

// Brand tokens. Note the slate ramp is REDEFINED in `tailwind.config.ts:115` —
// slate-900 is #151c2a here, not Tailwind's stock #0f172a. Taking the stock
// values would put the card a few shades off every other dark surface.
const SURFACE: &str = "#151c2a"; // slate-900 — the dark card base
const ACCENT: &str = "#13d2e5"; // Aurora Cyan
const PRIMARY: &str = "#7237ff"; // Aurora Purple
const INK: &str = "#f2f3f0"; // slate-50 — primary text on dark
const LINK: &str = "#a78bfa"; // --brand-link dark override, index.css:147
const MUTED: &str = "#9aa1ac"; // slate-400

// Avatar block, left of the identity text.
const AV_X: f32 = 72.0;
const AV_Y: f32 = 176.0;
const AV_SIZE: f32 = 300.0;
const AV_RADIUS: f32 = 56.0;

const TEXT_X: f32 = 424.0;

/// Compiled in, not read from `ASSETS_DIR`, so a bad path cannot strip the
/// card's branding. Its own fill is slate-50, matching `INK`.
const WORDMARK: &str = include_str!("../assets/wordmark-white.svg");
const WORDMARK_W: f32 = 208.0;
/// Preserves the asset's 100:23 aspect.
const WORDMARK_H: f32 = WORDMARK_W * 0.23;

/// Render a profile card to PNG. Missing avatar / metadata degrade gracefully.
pub async fn render_card(state: &AppState, card: &Card) -> Result<Vec<u8>> {
    let avatar = match &card.meta.picture {
        Some(url) => match fetch_avatar(state, url).await {
            Ok(bytes) => Some(bytes),
            Err(e) => {
                // Silently dropping this made a missing avatar indistinguishable
                // from a profile that has none.
                tracing::warn!("avatar fetch failed for {url}: {e:#}");
                None
            }
        },
        None => None,
    };
    let svg = build_svg(state, card, avatar.as_deref());
    rasterize(state, svg).await
}

/// Last-resort branded card. `None` rather than an empty `Vec`, which would be
/// served as a zero-byte `image/png`.
pub async fn render_fallback(state: &AppState) -> Option<Vec<u8>> {
    let card = Card {
        hex: String::new(),
        meta: Default::default(),
        overview: None,
        provisional: true,
    };
    let svg = build_svg(state, &card, None);
    rasterize(state, svg).await.ok()
}

/// Fetch and normalise a kind-0 `picture`. The URL is attacker-controlled:
/// `net` validates the address, the size cap and dimension check are here.
async fn fetch_avatar(state: &AppState, url: &str) -> Result<Vec<u8>> {
    let max = state.config.avatar_max_bytes;
    let mut target = crate::net::validate_and_resolve(url, &["http", "https"]).await?;

    // Redirects are followed here rather than by reqwest, which is configured
    // not to: its policy runs before we can re-check the destination, so an
    // automatic follow would let a public URL bounce us to an internal one.
    // Following manually means every hop goes back through the same address
    // validation. Without this, a 3xx sailed past `error_for_status`, its empty
    // body failed format detection, and the avatar silently vanished.
    let mut resp;
    let mut hops = 0;
    loop {
        resp = state
            .avatar_http
            .get(target.clone())
            .timeout(Duration::from_secs(state.config.avatar_timeout_secs))
            .send()
            .await?;

        if !resp.status().is_redirection() {
            break;
        }
        hops += 1;
        if hops > 3 {
            return Err(anyhow!("too many redirects"));
        }
        let location = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| anyhow!("{} with no Location header", resp.status()))?;
        // Relative Locations are legal and common.
        let next = target.join(location)?;
        target = crate::net::validate_and_resolve(next.as_str(), &["http", "https"]).await?;
    }
    let resp = resp.error_for_status()?;

    // Cheap rejection when the server is honest about the size...
    if let Some(len) = resp.content_length() {
        if len > max {
            return Err(anyhow!("avatar is {len} bytes, over the {max} cap"));
        }
    }

    // ...and a hard stop when it isn't. `bytes()` would buffer the whole body
    // regardless of what Content-Length claimed.
    let mut body: Vec<u8> = Vec::new();
    let mut stream = resp.bytes_stream();
    use futures_util::StreamExt as _;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if body.len() as u64 + chunk.len() as u64 > max {
            return Err(anyhow!("avatar body exceeded the {max} byte cap"));
        }
        body.extend_from_slice(&chunk);
    }

    let reader = image::ImageReader::new(Cursor::new(&body)).with_guessed_format()?;
    let format = reader
        .format()
        .ok_or_else(|| anyhow!("unknown image format"))?;
    if !matches!(
        format,
        image::ImageFormat::Png
            | image::ImageFormat::Jpeg
            | image::ImageFormat::WebP
            | image::ImageFormat::Gif
    ) {
        return Err(anyhow!("unsupported avatar format {format:?}"));
    }

    // Check the declared dimensions BEFORE decoding. `resize_to_fill` shrinks
    // to 240x240, but only after a full decode — a 30000x30000 PNG would
    // allocate gigabytes first.
    let (w, h) = reader.into_dimensions()?;
    if w > 8192 || h > 8192 {
        return Err(anyhow!("avatar is {w}x{h}, over the 8192 limit"));
    }

    let mut reader = image::ImageReader::new(Cursor::new(&body)).with_guessed_format()?;
    reader.limits({
        let mut l = image::Limits::default();
        l.max_image_width = Some(8192);
        l.max_image_height = Some(8192);
        l.max_alloc = Some(128 * 1024 * 1024);
        l
    });
    let img = reader
        .decode()?
        .resize_to_fill(240, 240, image::imageops::FilterType::Lanczos3);

    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png)?;
    Ok(out.into_inner())
}

/// Off the reactor: pure CPU that would otherwise block a runtime worker.
async fn rasterize(state: &AppState, svg: String) -> Result<Vec<u8>> {
    let fontdb = state.fontdb.clone();
    let family = state.config.font_family.clone();

    tokio::task::spawn_blocking(move || {
        let opt = usvg::Options {
            font_family: family,
            fontdb,
            ..Default::default()
        };
        let tree = usvg::Tree::from_str(&svg, &opt)?;
        let mut pixmap =
            tiny_skia::Pixmap::new(WIDTH, HEIGHT).ok_or_else(|| anyhow!("pixmap alloc failed"))?;
        resvg::render(
            &tree,
            tiny_skia::Transform::identity(),
            &mut pixmap.as_mut(),
        );
        Ok(pixmap.encode_png()?)
    })
    .await?
}

fn build_svg(state: &AppState, card: &Card, avatar_png: Option<&[u8]>) -> String {
    let family = esc(&state.config.font_family);
    let name = esc(&truncate(&sanitize(&card.display_name()), 24));
    let nip05 = card
        .meta
        .nip05
        .as_deref()
        .map(|s| esc(&truncate(&sanitize(s.trim_start_matches("_@")), 34)));

    let avatar_svg = avatar_block(&family, avatar_png, &name);
    let nip05_svg = nip05
        .map(|s| {
            format!(
                r#"<text x="{TEXT_X}" y="376" font-family="{family}" font-size="36" fill="{LINK}">{s}</text>"#
            )
        })
        .unwrap_or_default();

    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{WIDTH}" height="{HEIGHT}" viewBox="0 0 {WIDTH} {HEIGHT}">
<defs>
  <!-- The card surface, from YourNetworkCard.tsx:77's dark variant:
       `dark:from-slate-900 dark:via-slate-900 dark:to-brand-primary/[0.12]`.
       Flat for the first 55% so the tint stays a corner, exactly as `via-`
       holds the base colour through the middle stop. -->
  <linearGradient id="wash" x1="0" y1="0" x2="1" y2="1">
    <stop offset="0%" stop-color="{SURFACE}"/>
    <stop offset="55%" stop-color="{SURFACE}"/>
    <stop offset="100%" stop-color="{PRIMARY}" stop-opacity="0.12"/>
  </linearGradient>
</defs>
<rect width="{WIDTH}" height="{HEIGHT}" fill="{SURFACE}"/>
<rect width="{WIDTH}" height="{HEIGHT}" fill="url(#wash)"/>
{wordmark}
{avatar_svg}
<text x="{TEXT_X}" y="312" font-family="{family}" font-size="72" font-weight="700" fill="{INK}">{name}</text>
{nip05_svg}
<text x="72" y="574" font-family="{family}" font-size="30" fill="{MUTED}">{tagline}</text>
</svg>"##,
        wordmark = wordmark_svg(72.0, 72.0),
        tagline = esc(tagline_for(card)),
    )
}

/// The tagline slot.
///
/// Deliberately a single seam: the copy decision (one line vs. a list keyed by
/// pubkey vs. per-tier wording) is still open, and whatever it lands on, the
/// result must be STABLE for a given card — an unstable tagline would desync
/// from the `?v=` content hash and make the image URL lie.
fn tagline_for(_card: &Card) -> &'static str {
    "Reputation from real human connections."
}

/// Nested `<svg>` rather than splicing the path out, so the asset keeps its own
/// viewBox and survives being re-exported.
fn wordmark_svg(x: f32, y: f32) -> String {
    let inner = WORDMARK
        .trim()
        .replacen(
            r#"width="100" height="23""#,
            &format!(r#"width="{WORDMARK_W}" height="{WORDMARK_H}""#),
            1,
        )
        .replacen("<svg ", &format!(r#"<svg x="{x}" y="{y}" "#), 1);
    inner
}

fn avatar_block(family: &str, avatar_png: Option<&[u8]>, name: &str) -> String {
    match avatar_png {
        Some(png) => {
            let b64 = base64::engine::general_purpose::STANDARD.encode(png);
            format!(
                r##"<clipPath id="av"><rect x="{AV_X}" y="{AV_Y}" width="{AV_SIZE}" height="{AV_SIZE}" rx="{AV_RADIUS}"/></clipPath>
<image x="{AV_X}" y="{AV_Y}" width="{AV_SIZE}" height="{AV_SIZE}" href="data:image/png;base64,{b64}" clip-path="url(#av)" preserveAspectRatio="xMidYMid slice"/>
<rect x="{AV_X}" y="{AV_Y}" width="{AV_SIZE}" height="{AV_SIZE}" rx="{AV_RADIUS}" fill="none" stroke="{ACCENT}" stroke-opacity="0.25" stroke-width="3"/>"##
            )
        }
        None => {
            let initial = esc(&first_initial(name));
            format!(
                r##"<rect x="{AV_X}" y="{AV_Y}" width="{AV_SIZE}" height="{AV_SIZE}" rx="{AV_RADIUS}" fill="{PRIMARY}" fill-opacity="0.18"/>
<text x="{cx}" y="{cy}" font-family="{family}" font-size="128" font-weight="700" fill="{LINK}" text-anchor="middle" dominant-baseline="central">{initial}</text>"##,
                cx = AV_X + AV_SIZE / 2.0,
                cy = AV_Y + AV_SIZE / 2.0,
            )
        }
    }
}

/// Drop control characters and bidi overrides — an RTL override in a display
/// name can visually reorder the rest of the card.
fn sanitize(s: &str) -> String {
    s.chars()
        .filter(|c| {
            !c.is_control()
                && !matches!(
                    *c,
                    '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}'
                )
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn truncate(s: &str, max_chars: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_chars {
        s.to_string()
    } else {
        let mut out: String = chars[..max_chars.saturating_sub(1)].iter().collect();
        out.push('…');
        out
    }
}

fn first_initial(name: &str) -> String {
    name.chars()
        .next()
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_else(|| "?".to_string())
}

/// XML escaping. `&apos;` here vs `&#39;` in `routes::esc` — SVG is XML, that
/// is HTML. Do not unify them.
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_counts_chars_not_bytes() {
        assert_eq!(truncate("abc", 10), "abc");
        assert_eq!(truncate("abcdef", 4), "abc…");
        // Multi-byte must not split mid-codepoint.
        assert_eq!(truncate("さとうさとう", 3), "さと…");
        assert_eq!(truncate("🌻🌻🌻🌻", 3), "🌻🌻…");
    }

    #[test]
    fn sanitize_strips_controls_and_bidi() {
        assert_eq!(sanitize("a\u{202E}b"), "ab");
        assert_eq!(sanitize("a\u{0000}b"), "ab");
        assert_eq!(sanitize("  a   b  "), "a b");
        // Ordinary text, including emoji and CJK, is untouched.
        assert_eq!(sanitize("Alice 🌻 佐藤"), "Alice 🌻 佐藤");
    }

    #[test]
    fn esc_covers_the_xml_five() {
        assert_eq!(esc(r#"<&>"'"#), "&lt;&amp;&gt;&quot;&apos;");
    }

    #[test]
    fn first_initial_handles_empty_and_unicode() {
        assert_eq!(first_initial("alice"), "A");
        assert_eq!(first_initial(""), "?");
        assert_eq!(first_initial("さとう"), "さ");
    }
}
