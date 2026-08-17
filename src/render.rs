use anyhow::{anyhow, Result};
use base64::Engine as _;
use resvg::{tiny_skia, usvg};
use std::time::Duration;

use crate::data::{Card, Overview};
use crate::state::AppState;

pub const WIDTH: u32 = 1200;
pub const HEIGHT: u32 = 630;

const BG: &str = "#0f0d2e";
const ACCENT: &str = "#7c6cff";
const FG: &str = "#ffffff";
const MUTED: &str = "#8b8fae";
const SOFT: &str = "#c7cae0";

/// Render a profile card to PNG. Missing avatar / metadata degrade gracefully.
pub async fn render_card(state: &AppState, card: &Card) -> Result<Vec<u8>> {
    let avatar = match &card.meta.picture {
        Some(url) => fetch_avatar(state, url).await.ok(),
        None => None,
    };
    let svg = build_svg(state, card, avatar.as_deref());
    rasterize(state, &svg)
}

/// Last-resort branded card when even normal rendering fails.
pub fn render_fallback(state: &AppState) -> Vec<u8> {
    let card = Card {
        meta: Default::default(),
        overview: None,
    };
    let svg = build_svg(state, &card, None);
    rasterize(state, &svg).unwrap_or_default()
}

async fn fetch_avatar(state: &AppState, url: &str) -> Result<Vec<u8>> {
    let resp = state
        .http
        .get(url)
        .timeout(Duration::from_secs(state.config.avatar_timeout_secs))
        .send()
        .await?
        .error_for_status()?;
    let bytes = resp.bytes().await?;
    // Re-encode to PNG (handles webp/jpeg/gif) and cover-crop to a square so usvg
    // can always embed it and the circle clip looks right.
    let img = image::load_from_memory(&bytes)?
        .resize_to_fill(240, 240, image::imageops::FilterType::Lanczos3);
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png)?;
    Ok(out.into_inner())
}

fn rasterize(state: &AppState, svg: &str) -> Result<Vec<u8>> {
    let opt = usvg::Options {
        font_family: state.font_family.clone(),
        fontdb: state.fontdb.clone(),
        ..Default::default()
    };

    let tree = usvg::Tree::from_str(svg, &opt)?;
    let mut pixmap = tiny_skia::Pixmap::new(WIDTH, HEIGHT).ok_or_else(|| anyhow!("pixmap alloc failed"))?;
    resvg::render(&tree, tiny_skia::Transform::identity(), &mut pixmap.as_mut());
    Ok(pixmap.encode_png()?)
}

fn build_svg(state: &AppState, card: &Card, avatar_png: Option<&[u8]>) -> String {
    let family = &state.font_family;
    let name = card.meta.best_name().unwrap_or_else(|| "Nostr profile".to_string());
    let name = esc(&truncate(&name, 22));
    let nip05 = card.meta.nip05.as_deref().map(|s| esc(&truncate(s, 38)));
    let about = card
        .meta
        .about
        .as_deref()
        .map(|s| esc(&truncate(&one_line(s), 64)));

    let influence = fmt_influence(&card.overview);
    let followers = humanize(card.overview.as_ref().map(|o| o.followers).unwrap_or(0));
    let following = humanize(card.overview.as_ref().map(|o| o.following).unwrap_or(0));

    let avatar_svg = match avatar_png {
        Some(png) => {
            let b64 = base64::engine::general_purpose::STANDARD.encode(png);
            format!(
                r#"<clipPath id="av"><circle cx="150" cy="200" r="110"/></clipPath>
<image x="40" y="90" width="220" height="220" href="data:image/png;base64,{b64}" clip-path="url(#av)" preserveAspectRatio="xMidYMid slice"/>
<circle cx="150" cy="200" r="110" fill="none" stroke="{ACCENT}" stroke-width="4"/>"#
            )
        }
        None => {
            let initial = esc(&first_initial(&name));
            format!(
                r##"<circle cx="150" cy="200" r="110" fill="#1b1840" stroke="{ACCENT}" stroke-width="4"/>
<text x="150" y="200" font-family="{family}" font-size="110" font-weight="700" fill="{ACCENT}" text-anchor="middle" dominant-baseline="central">{initial}</text>"##
            )
        }
    };

    let nip05_svg = nip05
        .map(|s| format!(r#"<text x="300" y="218" font-family="{family}" font-size="28" fill="{ACCENT}">{s}</text>"#))
        .unwrap_or_default();

    let about_svg = about
        .map(|s| format!(r#"<text x="300" y="278" font-family="{family}" font-size="30" fill="{SOFT}">{s}</text>"#))
        .unwrap_or_default();

    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{WIDTH}" height="{HEIGHT}" viewBox="0 0 {WIDTH} {HEIGHT}">
<rect width="{WIDTH}" height="{HEIGHT}" fill="{BG}"/>
<rect width="{WIDTH}" height="8" fill="{ACCENT}"/>
{avatar_svg}
<text x="300" y="170" font-family="{family}" font-size="60" font-weight="700" fill="{FG}">{name}</text>
{nip05_svg}
{about_svg}
<rect x="40" y="380" width="1120" height="170" rx="20" fill="#16133a"/>
{stat_0}
{stat_1}
{stat_2}
<text x="1160" y="600" font-family="{family}" font-size="32" font-weight="700" fill="{ACCENT}" text-anchor="end">brainstorm</text>
</svg>"##,
        stat_0 = stat_block(family, 110, &influence, "Influence"),
        stat_1 = stat_block(family, 470, &followers, "Followers"),
        stat_2 = stat_block(family, 830, &following, "Following"),
    )
}

fn stat_block(family: &str, x: u32, value: &str, label: &str) -> String {
    format!(
        r#"<text x="{x}" y="475" font-family="{family}" font-size="64" font-weight="700" fill="{FG}">{value}</text>
<text x="{x}" y="520" font-family="{family}" font-size="28" fill="{MUTED}">{label}</text>"#,
        value = esc(value),
        label = esc(label),
    )
}

fn fmt_influence(overview: &Option<Overview>) -> String {
    match overview.as_ref().and_then(|o| o.influence) {
        // Influence is a 0..1 trust score; show as a 0..100 figure.
        Some(v) => {
            let scaled = if v <= 1.0 { v * 100.0 } else { v };
            format!("{:.0}", scaled.round())
        }
        None => "—".to_string(),
    }
}

fn humanize(n: i64) -> String {
    if n.abs() >= 1000 {
        let k = n as f64 / 1000.0;
        let s = format!("{:.1}", k);
        format!("{}k", s.trim_end_matches(".0"))
    } else {
        n.to_string()
    }
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
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
    name.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_else(|| "?".to_string())
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
