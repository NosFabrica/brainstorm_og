//! Calendar event card (31922 date, 31923 time): title, when, location, image; author strip.

use std::collections::hash_map::DefaultHasher;
use std::hash::Hash;
use std::sync::Arc;

use chrono::{DateTime, Datelike, NaiveDate, TimeZone};
use chrono_tz::Tz;

use super::{fit, Body, Built, DrawCx, ImageRequest, Meta, Skip, Visual};
use crate::event::Event;
use crate::render::{self, ACCENT, INK, MUTED};
use crate::state::AppState;

const IMG_W: f32 = 440.0;
const IMG_H: f32 = 330.0;
const GAP: f32 = 48.0;
pub const DATE_KIND: u32 = 31922;
pub const TIME_KIND: u32 = 31923;

const WHEN_SIZES: [f32; 4] = [30.0, 28.0, 26.0, 24.0];
/// Two lines at the largest size.
const WHEN_BOX: f32 = 2.0 * WHEN_SIZES[0] * LINE_HEIGHT;
const WHEN_GAP: f32 = 20.0;
/// Between the title and the location.
const TITLE_GAP: f32 = 16.0;
const TITLE_SIZES: [f32; 5] = [56.0, 50.0, 44.0, 40.0, 36.0];
const TITLE_LH: f32 = 1.15;
const LOC_SIZE: f32 = 28.0;
const LINE_HEIGHT: f32 = 1.3;

#[derive(Debug, Hash)]
struct Calendar {
    title: String,
    when: Option<String>,
    location: Option<String>,
    image: Option<String>,
}

impl Calendar {
    fn meta(&self) -> Meta {
        let parts: Vec<&str> = [self.when.as_deref(), self.location.as_deref()]
            .into_iter()
            .flatten()
            .collect();
        Meta {
            title: self.title.clone(),
            description: parts.join(" · "),
            og_type: "website",
            extra: vec![],
        }
    }
}

impl Body for Calendar {
    fn fingerprint(&self, h: &mut DefaultHasher) {
        self.hash(h);
    }

    fn visual(&self) -> Option<Visual> {
        self.image.as_ref().map(|url| {
            Visual::Image(ImageRequest {
                url: url.clone(),
                width: IMG_W as u32,
                height: IMG_H as u32,
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
            let ix = x + width - IMG_W;
            out.push_str(&render::embed_png(
                "cal-img", ix, top, IMG_W, IMG_H, 24.0, png,
            ));
            width -= IMG_W + GAP;
        }
        let mut y = top;
        if let Some(when) = &self.when {
            let f = fit::fit(when, width, WHEN_BOX, &WHEN_SIZES, LINE_HEIGHT, cx.at(600));
            y += cx.lines(
                &mut out,
                x,
                y,
                &f,
                LINE_HEIGHT,
                &format!(r#"font-weight="600" fill="{ACCENT}""#),
            ) + WHEN_GAP;
        }
        let loc_h = LOC_SIZE * LINE_HEIGHT;
        let title_bottom = if self.location.is_some() {
            bottom - loc_h - TITLE_GAP
        } else {
            bottom
        };
        let f = fit::fit(
            &self.title,
            width,
            title_bottom - y,
            &TITLE_SIZES,
            TITLE_LH,
            cx.at(700),
        );
        y += cx.lines(
            &mut out,
            x,
            y,
            &f,
            TITLE_LH,
            &format!(r#"font-weight="700" fill="{INK}""#),
        ) + TITLE_GAP;
        if let Some(loc) = &self.location {
            let f = fit::fit(loc, width, loc_h, &[LOC_SIZE], LINE_HEIGHT, cx.at(400));
            cx.lines(
                &mut out,
                x,
                y,
                &f,
                LINE_HEIGHT,
                &format!(r#"fill="{MUTED}""#),
            );
        }
        out
    }
}

/// The card's own parts, read from the event.
fn event(ev: &Event) -> Result<Calendar, Skip> {
    let text = |t| ev.tag(t).map(render::sanitize).filter(|s| !s.is_empty());
    Ok(Calendar {
        title: text("title").ok_or(Skip::Empty)?,
        when: when(ev),
        location: text("location"),
        image: ev.tag("image").and_then(super::web_url),
    })
}

pub async fn build(st: &AppState, ev: &Event) -> Result<Built, Skip> {
    let card = event(ev)?;
    Ok(Built {
        meta: card.meta(),
        author: Some(super::author(st, &ev.pubkey).await),
        body: Arc::new(card),
    })
}

/// When the event happens, as drawn. `None` without a readable `start`.
pub fn when(ev: &Event) -> Option<String> {
    match ev.kind {
        DATE_KIND => dates(ev),
        TIME_KIND => times(ev),
        _ => None,
    }
}

/// NIP-52 time event: unix seconds, each end in its own IANA zone; bad or missing zone → UTC.
fn times(ev: &Event) -> Option<String> {
    let secs = |t| ev.tag(t)?.parse::<i64>().ok();
    let zone = |t| ev.tag(t).and_then(|z| z.parse::<Tz>().ok());
    let start_tz = zone("start_tzid").unwrap_or(Tz::UTC);
    let start = start_tz.timestamp_opt(secs("start")?, 0).single()?;
    let end_tz = zone("end_tzid").unwrap_or(start_tz);
    let end = secs("end")
        .and_then(|e| end_tz.timestamp_opt(e, 0).single())
        .filter(|e| *e > start);
    let day = |d: &DateTime<Tz>, year: bool| {
        d.format(if year { "%a, %-d %b %Y" } else { "%a, %-d %b" })
            .to_string()
    };
    let clock = |d: &DateTime<Tz>| d.format("%H:%M").to_string();
    let Some(end) = end else {
        return Some(format!(
            "{} · {} {}",
            day(&start, true),
            clock(&start),
            label(&start)
        ));
    };
    let same_zone = label(&start) == label(&end);
    if same_zone && start.date_naive() == end.date_naive() {
        return Some(format!(
            "{} · {} – {} {}",
            day(&start, true),
            clock(&start),
            clock(&end),
            label(&end)
        ));
    }
    let other_year = start.year() != end.year();
    let start_zone = if same_zone {
        String::new()
    } else {
        format!(" {}", label(&start))
    };
    Some(format!(
        "{}, {}{start_zone} – {}, {} {}",
        day(&start, true),
        clock(&start),
        day(&end, other_year),
        clock(&end),
        label(&end)
    ))
}

/// The zone abbreviation; numeric ones (`-03`) as `UTC-03`.
fn label(d: &DateTime<Tz>) -> String {
    let abbr = d.format("%Z").to_string();
    if abbr.starts_with(['+', '-']) {
        format!("UTC{abbr}")
    } else {
        abbr
    }
}

/// NIP-52 date event: ISO dates, `end` exclusive.
fn dates(ev: &Event) -> Option<String> {
    let parse = |t| NaiveDate::parse_from_str(ev.tag(t)?, "%Y-%m-%d").ok();
    let start = parse("start")?;
    let last = parse("end")
        .and_then(|e| e.pred_opt())
        .filter(|l| *l > start);
    Some(match last {
        None => start.format("%a, %-d %b %Y").to_string(),
        Some(l) if l.year() != start.year() => {
            format!("{} – {}", start.format("%-d %b %Y"), l.format("%-d %b %Y"))
        }
        Some(l) if l.month() != start.month() => {
            format!("{} – {}", start.format("%-d %b"), l.format("%-d %b %Y"))
        }
        Some(l) => format!("{} – {}", start.format("%-d"), l.format("%-d %b %Y")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cards::{EntityCard, Meta};
    use crate::nip19::Entity;

    const PK: &str = "3bf0c63fcb93463407af97a5e5ee64fa883d107ef9e558472c4eb9aaaefa459d";

    use crate::event::ev;

    #[test]
    fn a_date_event_shows_its_day() {
        let e = ev(31922, &[&["start", "2026-11-05"]]);
        assert_eq!(when(&e).as_deref(), Some("Thu, 5 Nov 2026"));
    }

    fn dates(start: &str, end: &str) -> Option<String> {
        when(&ev(31922, &[&["start", start], &["end", end]]))
    }

    #[test]
    fn date_end_is_exclusive() {
        assert_eq!(
            dates("2026-11-05", "2026-11-06").as_deref(),
            Some("Thu, 5 Nov 2026")
        );
        assert_eq!(
            dates("2026-11-05", "2026-11-08").as_deref(),
            Some("5 – 7 Nov 2026")
        );
    }

    #[test]
    fn date_ranges_name_only_what_changes() {
        assert_eq!(
            dates("2026-11-30", "2026-12-03").as_deref(),
            Some("30 Nov – 2 Dec 2026")
        );
        assert_eq!(
            dates("2026-12-30", "2027-01-03").as_deref(),
            Some("30 Dec 2026 – 2 Jan 2027")
        );
    }

    #[test]
    fn a_bad_or_backwards_date_end_is_ignored() {
        assert_eq!(
            dates("2026-11-05", "soon").as_deref(),
            Some("Thu, 5 Nov 2026")
        );
        assert_eq!(
            dates("2026-11-05", "2026-11-01").as_deref(),
            Some("Thu, 5 Nov 2026")
        );
    }

    #[test]
    fn no_readable_start_has_no_when() {
        assert_eq!(when(&ev(31922, &[])), None);
        assert_eq!(when(&ev(31922, &[&["start", "1793818800"]])), None);
        assert_eq!(when(&ev(31923, &[&["start", "2026-11-05"]])), None);
    }

    fn timed(tags: &[&[&str]]) -> Option<String> {
        when(&ev(31923, tags))
    }

    #[test]
    fn a_timed_event_shows_its_local_time_and_zone() {
        let t = timed(&[&["start", "1793818800"], &["start_tzid", "Europe/Rome"]]);
        assert_eq!(t.as_deref(), Some("Wed, 4 Nov 2026 · 20:00 CET"));
    }

    #[test]
    fn same_day_range_shows_one_date() {
        let t = timed(&[
            &["start", "1793818800"],
            &["end", "1793826000"],
            &["start_tzid", "Europe/Rome"],
        ]);
        assert_eq!(t.as_deref(), Some("Wed, 4 Nov 2026 · 20:00 – 22:00 CET"));
    }

    #[test]
    fn multi_day_range_shows_both_dates() {
        let t = timed(&[
            &["start", "1793574060"],
            &["end", "1794178800"],
            &["start_tzid", "Europe/Berlin"],
        ]);
        assert_eq!(
            t.as_deref(),
            Some("Mon, 2 Nov 2026, 00:01 – Mon, 9 Nov, 00:00 CET")
        );
    }

    #[test]
    fn no_or_bad_tzid_is_utc() {
        let want = Some("Wed, 4 Nov 2026 · 19:00 UTC");
        assert_eq!(timed(&[&["start", "1793818800"]]).as_deref(), want);
        let bad = timed(&[&["start", "1793818800"], &["start_tzid", "Mars/Olympus"]]);
        assert_eq!(bad.as_deref(), want);
    }

    #[test]
    fn numeric_zone_abbreviations_read_as_utc_offsets() {
        let t = timed(&[
            &["start", "1796212800"],
            &["end", "1796223600"],
            &["start_tzid", "America/Sao_Paulo"],
        ]);
        assert_eq!(t.as_deref(), Some("Wed, 2 Dec 2026 · 09:00 – 12:00 UTC-03"));
    }

    #[test]
    fn a_different_end_zone_labels_each_end() {
        let t = timed(&[
            &["start", "1793818800"],
            &["end", "1793826000"],
            &["start_tzid", "Europe/Rome"],
            &["end_tzid", "Europe/London"],
        ]);
        assert_eq!(
            t.as_deref(),
            Some("Wed, 4 Nov 2026, 20:00 CET – Wed, 4 Nov, 21:00 GMT")
        );
    }

    #[test]
    fn a_bad_or_backwards_time_end_is_ignored() {
        let want = Some("Wed, 4 Nov 2026 · 19:00 UTC");
        assert_eq!(
            timed(&[&["start", "1793818800"], &["end", "x"]]).as_deref(),
            want
        );
        let back = timed(&[&["start", "1793818800"], &["end", "1793818000"]]);
        assert_eq!(back.as_deref(), want);
    }

    fn full() -> Event {
        ev(
            31923,
            &[
                &["d", "x"],
                &["title", "Meetup bitcoiner Vignola"],
                &["start", "1793818800"],
                &["start_tzid", "Europe/Rome"],
                &["location", "Circolo Paradosso, Spilamberto"],
                &["image", "https://x.org/a.jpg"],
            ],
        )
    }

    #[test]
    fn no_title_is_skipped() {
        let mut e = full();
        e.tags.retain(|t| t[0] != "title");
        assert_eq!(event(&e).unwrap_err(), Skip::Empty);
        e.tags.push(vec!["title".into(), "  ".into()]);
        assert_eq!(event(&e).unwrap_err(), Skip::Empty);
    }

    #[test]
    fn meta_is_title_and_when_with_location() {
        let m = event(&full()).unwrap().meta();
        assert_eq!(m.title, "Meetup bitcoiner Vignola");
        assert_eq!(
            m.description,
            "Wed, 4 Nov 2026 · 20:00 CET · Circolo Paradosso, Spilamberto"
        );
        assert_eq!(m.og_type, "website");
    }

    #[test]
    fn meta_description_drops_missing_parts() {
        let mut e = full();
        e.tags.retain(|t| t[0] != "location");
        assert_eq!(
            event(&e).unwrap().meta().description,
            "Wed, 4 Nov 2026 · 20:00 CET"
        );
    }

    #[test]
    fn the_image_is_requested_and_drawn_only_when_present() {
        let measure = |t: &str, px: f32, _w: u16| t.chars().count() as f32 * px * 0.5;
        let cx = |image| DrawCx {
            family: "Figtree",
            measure: &measure,
            image,
        };
        let with = event(&full()).unwrap();
        assert!(matches!(with.visual(), Some(Visual::Image(r)) if r.url == "https://x.org/a.jpg"));
        assert!(with.draw(&cx(Some(b"png"))).contains("<image"));
        assert!(
            !with.draw(&cx(None)).contains("<image"),
            "failed fetch draws no image"
        );

        let mut e = full();
        e.tags.retain(|t| t[0] != "image" && t[0] != "location");
        let bare = event(&e).unwrap();
        assert!(bare.visual().is_none());
        let svg = bare.draw(&cx(None));
        assert!(
            svg.contains("Meetup bitcoiner Vignola") && svg.contains("20:00 CET"),
            "{svg}"
        );
    }

    #[test]
    fn only_web_images_are_requested() {
        let mut e = full();
        e.tags.retain(|t| t[0] != "image");
        e.tags
            .push(vec!["image".into(), "javascript:alert(1)".into()]);
        assert!(event(&e).unwrap().visual().is_none());
    }

    fn version(e: &Event) -> String {
        EntityCard {
            entity: Entity::Address {
                kind: e.kind,
                pubkey: PK.into(),
                d: "x".into(),
            },
            built: crate::cards::Built {
                author: None,
                meta: Meta {
                    title: String::new(),
                    description: String::new(),
                    og_type: "website",
                    extra: vec![],
                },
                body: Arc::new(event(e).unwrap()),
            },
            delete_unknown: false,
        }
        .version("e1")
    }

    #[test]
    fn version_covers_everything_drawn() {
        let base = version(&full());
        assert_eq!(base, version(&full()));
        for (tag, value) in [
            ("title", "Other"),
            ("location", "Elsewhere"),
            ("image", "https://x.org/b.jpg"),
            ("start", "1793822400"),
            ("start_tzid", "Europe/London"),
        ] {
            let mut e = full();
            e.tags.retain(|t| t[0] != tag);
            e.tags.push(vec![tag.into(), value.into()]);
            assert_ne!(base, version(&e), "{tag}");
        }
    }
}
