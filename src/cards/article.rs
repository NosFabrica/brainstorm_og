//! Article card (kind 30023): title, summary, cover image; author strip.

use std::collections::hash_map::DefaultHasher;
use std::hash::Hash;
use std::sync::Arc;

use super::{fit, note, Body, Built, DrawCx, ImageRequest, Meta, Skip, Visual};
use crate::event::Event;
use crate::nip19;
use crate::render::{self, INK, MUTED};
use crate::state::AppState;

const TITLE_SIZES: [f32; 6] = [56.0, 52.0, 48.0, 44.0, 40.0, 36.0];
const TITLE_LH: f32 = 1.15;
const MAX_TITLE_LINES: usize = 3;
const SUMMARY_SIZE: f32 = 28.0;
const SUMMARY_LH: f32 = 1.35;
const GAP: f32 = 24.0;
const IMAGE_W: u32 = 400;
const IMAGE_H: u32 = 330;
const IMAGE_GAP: f32 = 40.0;

#[derive(Debug, Hash)]
struct Article {
    title: String,
    summary: Option<String>,
    image: Option<String>,
}

impl Body for Article {
    fn fingerprint(&self, h: &mut DefaultHasher) {
        self.hash(h);
    }

    fn visual(&self) -> Option<Visual> {
        self.image.as_ref().map(|url| {
            Visual::Image(ImageRequest {
                url: url.clone(),
                width: IMAGE_W,
                height: IMAGE_H,
            })
        })
    }

    fn draw(&self, cx: &DrawCx) -> String {
        let (x, top, bottom) = (
            render::CONTENT_X,
            render::CONTENT_TOP,
            render::CONTENT_BOTTOM,
        );
        let mut out = String::new();
        let mut width = render::CONTENT_W;
        if let Some(png) = cx.image {
            let (w, h) = (IMAGE_W as f32, IMAGE_H as f32);
            let ix = x + width - w;
            out.push_str(&render::embed_png("article-img", ix, top, w, h, 24.0, png));
            width -= w + IMAGE_GAP;
        }

        let bold = cx.at(700);
        let t = match self.summary {
            Some(_) => fit_title(&self.title, width, bold),
            None => fit::fit(
                &self.title,
                width,
                bottom - top,
                &TITLE_SIZES,
                TITLE_LH,
                bold,
            ),
        };
        let title_h = cx.lines(
            &mut out,
            x,
            top,
            &t,
            TITLE_LH,
            &format!(r#"font-weight="700" fill="{INK}""#),
        );

        if let Some(summary) = &self.summary {
            let s_top = top + title_h + GAP;
            if bottom - s_top >= SUMMARY_SIZE * SUMMARY_LH {
                let s = fit::fit(
                    summary,
                    width,
                    bottom - s_top,
                    &[SUMMARY_SIZE],
                    SUMMARY_LH,
                    cx.at(400),
                );
                cx.lines(
                    &mut out,
                    x,
                    s_top,
                    &s,
                    SUMMARY_LH,
                    &format!(r#"fill="{MUTED}""#),
                );
            }
        }
        out
    }
}

/// Leaves the summary room: largest size within [`MAX_TITLE_LINES`]; else cut at the smallest.
fn fit_title(title: &str, width: f32, measure: impl Fn(&str, f32) -> f32) -> fit::Fit {
    for &size in &TITLE_SIZES {
        let lines = fit::wrap(title, width, size, &measure);
        if lines.len() <= MAX_TITLE_LINES {
            return fit::Fit { size, lines };
        }
    }
    let smallest = &TITLE_SIZES[TITLE_SIZES.len() - 1..];
    let height = smallest[0] * TITLE_LH * MAX_TITLE_LINES as f32;
    fit::fit(title, width, height, smallest, TITLE_LH, measure)
}

pub async fn build(st: &AppState, ev: &Event) -> Result<Built, Skip> {
    let clean = |s: &str| Some(render::sanitize(s)).filter(|s| !s.trim().is_empty());
    let title = ev.tag("title").and_then(clean).ok_or(Skip::Empty)?;
    let summary = ev.tag("summary").and_then(clean);
    let image = ev.tag("image").and_then(super::web_url);
    let author = super::author(st, &ev.pubkey).await;

    let description = summary
        .clone()
        .or_else(|| note::text(&ev.content, |_| None).map(|t| render::sanitize(&t)))
        .map(|d| fit::clip_words(&d, 200))
        .unwrap_or_default();
    let mut extra = vec![];
    if let Some(npub) = nip19::npub_from_hex(&ev.pubkey) {
        extra.push((
            "article:author",
            format!("{}/p/{npub}", st.config.app_base_url),
        ));
    }
    if let Some(at) = ev
        .tag("published_at")
        .and_then(|s| iso8601(s.parse().ok()?))
    {
        extra.push(("article:published_time", at));
    }

    Ok(Built {
        meta: Meta {
            title: fit::clip_words(&title, 200),
            description,
            og_type: "article",
            extra,
        },
        author: Some(author),
        body: Arc::new(Article {
            title: fit::clip_words(&title, 300),
            summary: summary.map(|s| fit::clip_words(&s, 600)),
            image,
        }),
    })
}

/// Unix seconds → `YYYY-MM-DDTHH:MM:SSZ`.
fn iso8601(secs: i64) -> Option<String> {
    let at = chrono::DateTime::from_timestamp(secs, 0)?;
    Some(at.format("%Y-%m-%dT%H:%M:%SZ").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cards::{Author, EntityCard};
    use crate::nip19::Entity;

    fn measure(t: &str, px: f32, _weight: u16) -> f32 {
        t.chars().count() as f32 * px * 0.5
    }

    fn draw(a: &Article, image: Option<&[u8]>) -> String {
        a.draw(&DrawCx {
            family: "Figtree",
            measure: &measure,
            image,
        })
    }

    fn article(title: &str, summary: Option<&str>, image: Option<&str>) -> Article {
        Article {
            title: title.into(),
            summary: summary.map(str::to_string),
            image: image.map(str::to_string),
        }
    }

    fn texts(svg: &str, attr: &str) -> Vec<String> {
        svg.split("<text ")
            .skip(1)
            .filter(|t| t.contains(attr))
            .map(|t| t[t.find('>').unwrap() + 1..t.find("</text>").unwrap()].to_string())
            .collect()
    }

    #[test]
    fn a_two_line_title_stays_at_the_largest_size() {
        // 56px * 0.5 = 28px/char; 1056px fits 37 chars a line.
        let svg = draw(&article(&"word ".repeat(12), None, None), None);
        let lines = texts(&svg, r#"font-weight="700""#);
        assert_eq!(lines.len(), 2, "{svg}");
        assert!(svg.contains(r#"font-size="56""#), "{svg}");
    }

    #[test]
    fn with_a_summary_a_long_title_is_cut_at_three_lines() {
        let svg = draw(&article(&"word ".repeat(200), Some("s"), None), None);
        let lines = texts(&svg, r#"font-weight="700""#);
        assert_eq!(lines.len(), MAX_TITLE_LINES);
        assert!(lines.last().unwrap().ends_with('…'));
    }

    #[test]
    fn without_a_summary_a_long_title_fills_the_box() {
        let svg = draw(&article(&"word ".repeat(200), None, None), None);
        let lines = texts(&svg, r#"font-weight="700""#);
        assert!(lines.len() > MAX_TITLE_LINES, "{}", lines.len());
        assert!(lines.last().unwrap().ends_with('…'));
    }

    #[test]
    fn a_long_summary_is_cut_inside_the_box() {
        let svg = draw(&article("Hi", Some(&"more ".repeat(500)), None), None);
        let summary = texts(&svg, MUTED);
        assert!(!summary.is_empty());
        assert!(summary.last().unwrap().ends_with('…'));
        let last_y: f32 = svg
            .rsplit("<text ")
            .next()
            .and_then(|t| t.split("y=\"").nth(1))
            .and_then(|t| t.split('"').next())
            .unwrap()
            .parse()
            .unwrap();
        assert!(last_y <= render::CONTENT_BOTTOM, "{last_y}");
    }

    #[test]
    fn no_summary_and_no_image_draw_the_title_alone() {
        let svg = draw(&article("Hi", None, Some("https://x/a.png")), None);
        assert_eq!(texts(&svg, "font-size").len(), 1);
        assert!(!svg.contains("<image"));
    }

    #[test]
    fn the_image_takes_the_right_panel_and_narrows_the_text() {
        let wide = draw(&article(&"word ".repeat(12), None, None), None);
        let narrow = draw(&article(&"word ".repeat(12), None, None), Some(b"png"));
        assert!(narrow.contains("<image"));
        assert!(
            texts(&narrow, r#"font-weight="700""#).len()
                > texts(&wide, r#"font-weight="700""#).len()
        );
    }

    #[test]
    fn titles_and_summaries_are_escaped() {
        let svg = draw(&article("a<b", Some("c&d"), None), None);
        assert!(svg.contains("a&lt;b") && svg.contains("c&amp;d"), "{svg}");
    }

    fn card(a: Article, name: &str, picture: Option<&str>) -> EntityCard {
        EntityCard {
            entity: Entity::Event { id: "a".repeat(64) },
            built: Built {
                author: Some(Author {
                    name: name.into(),
                    picture: picture.map(str::to_string),
                    provisional: false,
                }),
                meta: Meta {
                    title: "t".into(),
                    description: "d".into(),
                    og_type: "article",
                    extra: vec![],
                },
                body: Arc::new(a),
            },
            delete_unknown: false,
        }
    }

    #[test]
    fn version_covers_title_summary_image_and_author() {
        let v = |t: &str, s: Option<&str>, i: Option<&str>, n: &str, p: Option<&str>| {
            card(article(t, s, i), n, p).version("e")
        };
        let base = v("T", Some("S"), Some("https://x/i.png"), "alice", None);
        assert_eq!(
            base,
            v("T", Some("S"), Some("https://x/i.png"), "alice", None)
        );
        assert_ne!(
            base,
            v("T2", Some("S"), Some("https://x/i.png"), "alice", None)
        );
        assert_ne!(base, v("T", None, Some("https://x/i.png"), "alice", None));
        assert_ne!(
            base,
            v("T", Some("S"), Some("https://x/j.png"), "alice", None)
        );
        assert_ne!(base, v("T", Some("S"), None, "alice", None));
        assert_ne!(
            base,
            v("T", Some("S"), Some("https://x/i.png"), "bob", None)
        );
        assert_ne!(
            base,
            v(
                "T",
                Some("S"),
                Some("https://x/i.png"),
                "alice",
                Some("https://x/a.png")
            )
        );
    }

    #[test]
    fn iso8601_formats_utc() {
        assert_eq!(iso8601(0).as_deref(), Some("1970-01-01T00:00:00Z"));
        assert_eq!(
            iso8601(951_825_600).as_deref(),
            Some("2000-02-29T12:00:00Z")
        );
        assert_eq!(
            iso8601(1_700_000_000).as_deref(),
            Some("2023-11-14T22:13:20Z")
        );
        assert_eq!(iso8601(i64::MAX), None);
    }
}
