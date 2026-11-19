//! The share card: 1200x630 PNG, SVG -> raster via resvg. Every card shares
//! one frame (surface, wash, wordmark); the profile card draws identity only —
//! avatar, name, nip05 — and entity cards a per-kind body plus an author strip.
//! See CONTEXT.md.

use anyhow::{anyhow, Result};
use base64::Engine as _;
use resvg::{tiny_skia, usvg};
use std::io::Cursor;
use std::time::Duration;

use crate::cards::{DrawCx, EntityCard, Visual};
use crate::data::Card;
use crate::state::AppState;

pub const WIDTH: u32 = 1200;
pub const HEIGHT: u32 = 630;

// Brand tokens. Note the slate ramp is REDEFINED in `tailwind.config.ts:115` —
// slate-900 is #151c2a here, not Tailwind's stock #0f172a. Taking the stock
// values would put the card a few shades off every other dark surface.
pub const SURFACE: &str = "#151c2a"; // slate-900 — the dark card base
pub const ACCENT: &str = "#13d2e5"; // Aurora Cyan
pub const PRIMARY: &str = "#7237ff"; // Aurora Purple
pub const INK: &str = "#f2f3f0"; // slate-50 — primary text on dark
pub const LINK: &str = "#a78bfa"; // --brand-link dark override, index.css:147
pub const MUTED: &str = "#9aa1ac"; // slate-400

// Avatar block, left of the identity text.
const AV_X: f32 = 72.0;
const AV_Y: f32 = 176.0;
const AV_SIZE: f32 = 300.0;
const AV_RADIUS: f32 = 56.0;

const TEXT_X: f32 = 424.0;

/// Entity cards: the content box below the wordmark and above the author strip.
pub const CONTENT_X: f32 = 72.0;
pub const CONTENT_TOP: f32 = 160.0;
pub const CONTENT_W: f32 = 1056.0;
pub const CONTENT_BOTTOM: f32 = 490.0;
/// Content box bottom when there is no author strip.
pub const CONTENT_BOTTOM_NO_AUTHOR: f32 = 574.0;

const STRIP_AV: f32 = 64.0;
const STRIP_Y: f32 = 526.0;

/// What the avatar is re-encoded to before being embedded. Derived from the
/// drawn size so the two cannot drift: the card is rasterised 1:1 at its
/// viewBox, so anything above `AV_SIZE` is bytes nobody sees, and anything
/// below is upscaled and blurry. It used to be a hardcoded 240 against a 300px
/// box, which was exactly that.
const AVATAR_PX: u32 = AV_SIZE as u32;

/// Ceilings for DECODING an attacker-supplied avatar, which is unavoidably the
/// full source resolution — `image` has no scaling decoder. Decoded RGBA costs
/// width*height*4, so this is the transient allocation `MAX_CONCURRENT_RENDERS`
/// multiplies against the container's memory limit: 2048^2 * 4 = 16 MB, times 8
/// concurrent, plus the card cache, fits 512Mi.
///
/// Not lowered to `AVATAR_PX`: real profile pictures are routinely 1000-2000px,
/// and this rejects rather than resamples, so a tight cap would drop avatars
/// rather than shrink them.
const MAX_AVATAR_DIM: u32 = 2048;
const MAX_AVATAR_ALLOC: u64 = 32 * 1024 * 1024;

/// Compiled in, not read from `ASSETS_DIR`, so a bad path cannot strip the
/// card's branding. Its own fill is slate-50, matching `INK`.
const WORDMARK: &str = include_str!("../assets/wordmark-white.svg");
const WORDMARK_W: f32 = 208.0;
/// Preserves the asset's 100:23 aspect.
const WORDMARK_H: f32 = WORDMARK_W * 0.23;

/// Render a profile card to PNG. Missing avatar / metadata degrade gracefully.
pub async fn render_card(state: &AppState, card: &Card) -> Result<Vec<u8>> {
    let avatar = fetch_optional(state, card.meta.picture.as_deref(), AVATAR_PX, AVATAR_PX).await;
    let svg = build_svg(state, card, avatar.as_deref());
    rasterize(state, svg).await
}

/// Render an entity card: shared frame, the kind's body, the author strip.
pub async fn render_entity(state: &AppState, card: &EntityCard) -> Result<Vec<u8>> {
    let built = &card.built;
    let picture = built.author.as_ref().and_then(|a| a.picture.as_deref());
    let (avatar, image) = tokio::join!(
        fetch_optional(state, picture, STRIP_AV as u32, STRIP_AV as u32),
        async {
            match built.body.visual() {
                Some(Visual::Map(m)) => crate::map::draw(state, m).await,
                Some(Visual::Image(r)) => {
                    fetch_optional(state, Some(&r.url), r.width, r.height).await
                }
                None => None,
            }
        }
    );

    let family = state.config.font_family.clone();
    let body = {
        let measurer = state.metrics.measurer();
        built.body.draw(&DrawCx {
            family: &family,
            measure: &|text, size, weight| measurer.measure(text, size, weight),
            image: image.as_deref(),
        })
    };
    let strip = built
        .author
        .as_ref()
        .map(|a| author_strip(&esc(&family), &a.name, avatar.as_deref()))
        .unwrap_or_default();
    rasterize(state, frame(&format!("{body}\n{strip}"))).await
}

/// Loads the bundled faces in file-name order: resvg falls back in load order, and `read_dir`'s isn't fixed.
pub fn load_fonts(db: &mut usvg::fontdb::Database, dir: &str) {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            matches!(
                p.extension().and_then(|e| e.to_str()),
                Some("ttf" | "otf" | "ttc" | "otc")
            )
        })
        .collect();
    files.sort();
    for f in files {
        if let Err(e) = db.load_font_file(&f) {
            tracing::warn!(file = %f.display(), error = %e, "font not loaded");
        }
    }
}

type FontData = std::sync::Arc<dyn AsRef<[u8]> + Send + Sync>;

/// The bundled faces in resvg's fallback order (primary, then the rest as loaded), for measuring text before layout.
pub struct Metrics {
    faces: Vec<(FontData, u32)>,
}

impl Metrics {
    /// Shares fontdb's own mapping of each file, so measuring holds no second copy of the fonts.
    pub fn load(db: &mut usvg::fontdb::Database, family: &str) -> Option<Self> {
        let primary = db.query(&usvg::fontdb::Query {
            families: &[usvg::fontdb::Family::Name(family)],
            ..Default::default()
        })?;
        let ids: Vec<_> = std::iter::once(primary)
            .chain(db.faces().map(|f| f.id).filter(|&id| id != primary))
            .collect();
        let faces = ids
            .into_iter()
            // SAFETY: maps the bundled font files read-only, as fontdb already does on every
            // render; nothing rewrites them while the process runs (read-only image).
            .map(|id| unsafe { db.make_shared_face_data(id) })
            .collect::<Option<Vec<_>>>()?;
        Some(Metrics { faces })
    }

    pub fn measurer(&self) -> Measurer<'_> {
        Measurer {
            base: self
                .faces
                .iter()
                .filter_map(|(data, index)| ttf_parser::Face::parse((**data).as_ref(), *index).ok())
                .collect(),
            weighted: Default::default(),
        }
    }

    pub fn measure(&self, text: &str, size: f32, weight: u16) -> f32 {
        self.measurer().measure(text, size, weight)
    }
}

/// Parsed faces, with a `wght` instance per weight asked for.
pub struct Measurer<'a> {
    base: Vec<ttf_parser::Face<'a>>,
    weighted: std::cell::RefCell<Vec<(u16, Vec<ttf_parser::Face<'a>>)>>,
}

impl Measurer<'_> {
    /// Advance width of `text`, each char from the first face that has it, as resvg falls back.
    pub fn measure(&self, text: &str, size: f32, weight: u16) -> f32 {
        // resvg leaves the default instance alone at 400.
        if weight == 400 {
            return advance(&self.base, text) * size;
        }
        let mut weighted = self.weighted.borrow_mut();
        let i = match weighted.iter().position(|(w, _)| *w == weight) {
            Some(i) => i,
            None => {
                let mut faces = self.base.clone();
                for f in &mut faces {
                    f.set_variation(ttf_parser::Tag::from_bytes(b"wght"), weight as f32);
                }
                weighted.push((weight, faces));
                weighted.len() - 1
            }
        };
        advance(&weighted[i].1, text) * size
    }
}

/// In ems. A char no face has is drawn as the primary's missing-glyph box.
/// Faces picked as usvg does: the primary, then for the first char still missing the first unused face
/// that has it, which takes every missing char it has; stops at a char no face has.
fn advance(faces: &[ttf_parser::Face], text: &str) -> f32 {
    let em = |f: &ttf_parser::Face, g| {
        f.glyph_hor_advance(g).unwrap_or(0) as f32 / f.units_per_em() as f32
    };
    let chars: Vec<char> = text.chars().collect();
    let mut by: Vec<Option<(usize, ttf_parser::GlyphId)>> = vec![None; chars.len()];
    let mut used = vec![false; faces.len()];
    let mut next = (!faces.is_empty()).then_some(0);
    while let Some(fi) = next {
        used[fi] = true;
        for (slot, c) in by.iter_mut().zip(&chars) {
            if slot.is_none() {
                *slot = faces[fi].glyph_index(*c).map(|g| (fi, g));
            }
        }
        next = chars
            .iter()
            .zip(&by)
            .find(|(_, b)| b.is_none())
            .and_then(|(c, _)| {
                (0..faces.len()).find(|&j| !used[j] && faces[j].glyph_index(*c).is_some())
            });
    }
    by.iter()
        .map(|b| match (b, faces.first()) {
            (Some((fi, g)), _) => em(&faces[*fi], *g),
            (None, Some(f)) => em(f, ttf_parser::GlyphId(0)),
            (None, None) => 0.6,
        })
        .sum()
}

async fn fetch_optional(state: &AppState, url: Option<&str>, w: u32, h: u32) -> Option<Vec<u8>> {
    let url = url?;
    match fetch_image(state, url, w, h).await {
        Ok(bytes) => Some(bytes),
        Err(e) => {
            // Otherwise a failed fetch looks like no image at all.
            tracing::warn!("image fetch failed for {url}: {e:#}");
            None
        }
    }
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

/// Fetch an image and fill it to `w`x`h`. The URL is attacker-controlled:
/// `net` validates the address, the size cap and dimension check are here.
async fn fetch_image(state: &AppState, url: &str, out_w: u32, out_h: u32) -> Result<Vec<u8>> {
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

    // Check the declared dimensions BEFORE decoding. The resize happens only
    // after a full decode, so a 30000x30000 PNG would allocate gigabytes first.
    //
    // The ceiling is deliberately close to what the output needs. Decoded RGBA
    // is width*height*4, so 4096 is already ~67 MB for one image; the previous
    // 8192 was ~268 MB, and MAX_CONCURRENT_RENDERS of those at once dwarfed the
    // container's memory limit.
    let (w, h) = reader.into_dimensions()?;
    if w > MAX_AVATAR_DIM || h > MAX_AVATAR_DIM {
        return Err(anyhow!(
            "avatar is {w}x{h}, over the {MAX_AVATAR_DIM} limit"
        ));
    }

    let mut reader = image::ImageReader::new(Cursor::new(&body)).with_guessed_format()?;
    reader.limits({
        let mut l = image::Limits::default();
        l.max_image_width = Some(MAX_AVATAR_DIM);
        l.max_image_height = Some(MAX_AVATAR_DIM);
        l.max_alloc = Some(MAX_AVATAR_ALLOC);
        l
    });
    let img = reader
        .decode()?
        .resize_to_fill(out_w, out_h, image::imageops::FilterType::Lanczos3);

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

    frame(&format!(
        r##"{avatar_svg}
<text x="{TEXT_X}" y="312" font-family="{family}" font-size="72" font-weight="700" fill="{INK}">{name}</text>
{nip05_svg}
<text x="72" y="574" font-family="{family}" font-size="30" fill="{MUTED}">{tagline}</text>"##,
        tagline = esc(tagline_for(card)),
    ))
}

/// The card family's shared frame: surface, wash, wordmark. `inner` is the content.
fn frame(inner: &str) -> String {
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
{inner}
</svg>"##,
        wordmark = wordmark_svg(72.0, 72.0),
    )
}

/// A PNG filling `w`x`h` at (`x`, `y`), clipped to corner radius `r`. `id` must be unique in the card.
pub fn embed_png(id: &str, x: f32, y: f32, w: f32, h: f32, r: f32, png: &[u8]) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(png);
    format!(
        r##"<clipPath id="{id}"><rect x="{x}" y="{y}" width="{w}" height="{h}" rx="{r}"/></clipPath>
<image x="{x}" y="{y}" width="{w}" height="{h}" href="data:image/png;base64,{b64}" clip-path="url(#{id})" preserveAspectRatio="xMidYMid slice"/>"##
    )
}

/// Small avatar + name at the bottom of an entity card.
fn author_strip(family: &str, name: &str, avatar_png: Option<&[u8]>) -> String {
    let raw = truncate(&sanitize(name), 40);
    let name = esc(&raw);
    let (x, y, size, r) = (CONTENT_X, STRIP_Y, STRIP_AV, STRIP_AV / 2.0);
    let avatar = match avatar_png {
        Some(png) => embed_png("strip-av", x, y, size, size, r, png),
        None => format!(
            r##"<rect x="{x}" y="{y}" width="{size}" height="{size}" rx="{r}" fill="{PRIMARY}" fill-opacity="0.18"/>
<text x="{cx}" y="{cy}" font-family="{family}" font-size="30" font-weight="700" fill="{LINK}" text-anchor="middle" dominant-baseline="central">{initial}</text>"##,
            cx = x + r,
            cy = y + r,
            initial = esc(&first_initial(&raw)),
        ),
    };
    format!(
        r##"{avatar}
<text x="{tx}" y="{ty}" font-family="{family}" font-size="30" font-weight="600" fill="{INK}">{name}</text>"##,
        tx = x + size + 20.0,
        ty = y + r + 10.0,
    )
}

/// The tagline slot.
///
/// Deliberately a single seam: the copy decision (one line vs. a list keyed by
/// pubkey) is still open, and whatever it lands on, the
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
            let image = embed_png("av", AV_X, AV_Y, AV_SIZE, AV_SIZE, AV_RADIUS, png);
            format!(
                r##"{image}
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
/// name can visually reorder the rest of the card — and flag emoji, which
/// resvg can't shape (tofu, and it breaks fallback for the rest of the line).
pub fn sanitize(s: &str) -> String {
    s.chars()
        .filter(|c| {
            !c.is_control()
                && !matches!(
                    *c,
                    '\u{202A}'..='\u{202E}'
                        | '\u{2066}'..='\u{2069}'
                        | '\u{200E}'
                        | '\u{200F}'
                        | '\u{1F1E6}'..='\u{1F1FF}'
                        | '\u{E0020}'..='\u{E007F}'
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
pub fn esc(s: &str) -> String {
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
        // Flags (regional-indicator pairs, tag sequences) go; the rest of the line stays.
        assert_eq!(sanitize("Tokyo 🇯🇵JP/jp 🇧🇷 x"), "Tokyo JP/jp x");
        assert_eq!(
            sanitize("a 🏴\u{E0067}\u{E0062}\u{E0065}\u{E006E}\u{E0067}\u{E007F} b"),
            "a 🏴 b"
        );
    }

    #[test]
    fn esc_covers_the_xml_five() {
        assert_eq!(esc(r#"<&>"'"#), "&lt;&amp;&gt;&quot;&apos;");
    }

    fn fonts() -> usvg::fontdb::Database {
        let mut db = usvg::fontdb::Database::new();
        load_fonts(&mut db, "assets");
        db
    }

    #[test]
    fn bundled_fonts_cover_cyrillic_and_greek() {
        let db = fonts();
        for c in "Їжак ґанок є і ў ђ љ ӂ қ ә Ελληνικά ΐ"
            .chars()
            .filter(|c| !c.is_whitespace())
        {
            let has = |id| {
                db.with_face_data(id, |d, i| {
                    ttf_parser::Face::parse(d, i).is_ok_and(|f| f.glyph_index(c).is_some())
                })
            };
            assert!(
                db.faces().any(|f| has(f.id) == Some(true)),
                "no face has {c}"
            );
        }
    }

    /// Ink width as resvg lays the line out. Long lines make side bearings negligible.
    fn laid_out(db: &usvg::fontdb::Database, text: &str, size: f32, weight: u16) -> f32 {
        let svg = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="20000" height="400"><text x="0" y="200" font-family="Figtree" font-size="{size}" font-weight="{weight}">{}</text></svg>"#,
            esc(text)
        );
        let opt = usvg::Options {
            font_family: "Figtree".into(),
            fontdb: std::sync::Arc::new(db.clone()),
            ..Default::default()
        };
        let tree = usvg::Tree::from_str(&svg, &opt).unwrap();
        tree.root().abs_bounding_box().width()
    }

    #[test]
    fn measuring_matches_the_rendered_width_across_scripts_and_weights() {
        let mut db = fonts();
        let m = Metrics::load(&mut db, "Figtree").unwrap();
        for text in [
            "The quick brown fox jumps over the lazy dog",
            "Съешь же ещё этих мягких французских булок",
            "いろはにほへと ちりぬるを わかよたれそ",
            "Café 東京 Москва",
            "Їжак ґанок є і Київ Ελληνικά",
            "東京 Їжак ґанок є і Київ",
        ] {
            for weight in [400, 600, 700] {
                let line = [text; 4].join(" ");
                let want = laid_out(&db, &line, 40.0, weight);
                let got = m.measure(&line, 40.0, weight);
                // Never narrower than drawn (that overflows); a little wider just wraps early.
                let ratio = got / want;
                assert!(
                    (0.98..1.05).contains(&ratio),
                    "{text} @{weight}: measured {got}, drawn {want}"
                );
            }
        }
    }

    #[test]
    fn first_initial_handles_empty_and_unicode() {
        assert_eq!(first_initial("alice"), "A");
        assert_eq!(first_initial(""), "?");
        assert_eq!(first_initial("さとう"), "さ");
    }
}
