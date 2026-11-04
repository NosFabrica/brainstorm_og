//! Place card: a DList item (39999 / 9999) under the place concept. Name, kind, address, one panel; no author.

use std::collections::hash_map::DefaultHasher;
use std::hash::Hash;
use std::sync::Arc;

use super::{fit, Body, Built, DrawCx, ImageRequest, Meta, Skip, Visual};
use crate::event::Event;
use crate::render::{self, ACCENT, INK, MUTED};
use crate::state::AppState;

const NAME_SIZES: [f32; 6] = [72.0, 64.0, 58.0, 52.0, 48.0, 44.0];
const NAME_LH: f32 = 1.15;
const NAME_BOX: f32 = 170.0;
const KIND_SIZE: f32 = 34.0;
const ADDR_SIZE: f32 = 30.0;
const ADDR_LH: f32 = 1.35;
const ADDR_MAX_LINES: usize = 4;

const GAP: f32 = 48.0;
pub const PANEL_W: f32 = 416.0;
pub const PANEL_H: f32 = render::CONTENT_BOTTOM_NO_AUTHOR - render::CONTENT_TOP;
pub const PANEL_X: f32 = render::CONTENT_X + render::CONTENT_W - PANEL_W;
const PANEL_R: f32 = 24.0;

/// The card's one visual: the item's image.
#[derive(Debug, Clone, Hash, PartialEq)]
pub enum Panel {
    Image(String),
}

#[derive(Debug, Clone, Hash, PartialEq)]
pub struct Place {
    pub name: String,
    /// "Restaurant · Argentinian".
    pub kind: Option<String>,
    pub address: Vec<String>,
    pub panel: Option<Panel>,
}

impl Body for Place {
    fn fingerprint(&self, h: &mut DefaultHasher) {
        self.hash(h);
    }

    fn visual(&self) -> Option<Visual> {
        let (width, height) = (PANEL_W as u32, PANEL_H as u32);
        Some(match self.panel.as_ref()? {
            Panel::Image(url) => Visual::Image(ImageRequest {
                url: url.clone(),
                width,
                height,
            }),
        })
    }

    fn draw(&self, cx: &DrawCx) -> String {
        let panel = cx.image.map(panel_svg);
        let width = match panel {
            Some(_) => PANEL_X - GAP - render::CONTENT_X,
            None => render::CONTENT_W,
        };
        let mut out = self.text_svg(cx, width);
        if let Some(p) = panel {
            out.push_str(&p);
        }
        out
    }
}

impl Place {
    fn text_svg(&self, cx: &DrawCx, width: f32) -> String {
        let x = render::CONTENT_X;
        let mut out = String::new();
        let style = |weight: u16, fill: &str| format!(r#"font-weight="{weight}" fill="{fill}""#);

        let name = fit::fit(
            &self.name,
            width,
            NAME_BOX,
            &NAME_SIZES,
            NAME_LH,
            cx.at(700),
        );
        let top = render::CONTENT_TOP;
        let name_h = cx.lines(&mut out, x, top, &name, NAME_LH, &style(700, INK));
        // Baseline of the name's last line.
        let mut y = top + name.size + name_h - name.size * NAME_LH;

        if let Some(kind) = &self.kind {
            y += 24.0 + KIND_SIZE;
            let l = fit::fit(kind, width, KIND_SIZE, &[KIND_SIZE], 1.0, cx.at(600));
            cx.lines(&mut out, x, y - KIND_SIZE, &l, 1.0, &style(600, ACCENT));
        }

        y += 16.0;
        let step = ADDR_SIZE * ADDR_LH;
        let mut left = ADDR_MAX_LINES;
        for part in &self.address {
            if left == 0 {
                break;
            }
            let room = left as f32 * step;
            let f = fit::fit(part, width, room, &[ADDR_SIZE], ADDR_LH, cx.at(400));
            y += cx.lines(
                &mut out,
                x,
                y + step - ADDR_SIZE,
                &f,
                ADDR_LH,
                &style(400, MUTED),
            );
            left = left.saturating_sub(f.lines.len());
        }
        out
    }
}

fn panel_svg(png: &[u8]) -> String {
    let (x, y) = (PANEL_X, render::CONTENT_TOP);
    render::embed_png("place-panel", x, y, PANEL_W, PANEL_H, PANEL_R, png)
}

pub async fn build(st: &AppState, ev: &Event) -> Result<Built, Skip> {
    let (place, meta) = read(ev, &st.config.place_concept)?;
    Ok(Built {
        author: None,
        meta,
        body: Arc::new(place),
    })
}

/// `ev` as a place, if it is one. Everything comes from the item's tags.
pub fn read(ev: &Event, concept: &str) -> Result<(Place, Meta), Skip> {
    if ev.tag("z") != Some(concept.trim()) {
        return Err(Skip::Unsupported);
    }
    let field = |n: &str| ev.tag(n).map(render::sanitize).filter(|s| !s.is_empty());
    let name = field("name").ok_or(Skip::Empty)?;
    let category = field("category").map(|s| category_words(&s));
    let cuisine = field("cuisine")
        .map(|s| category_words(&s))
        .filter(|c| Some(c) != category.as_ref());
    let locality = field("locality");
    let kind = join(&[category.clone(), cuisine.clone()], " · ");

    let description = join(&[category, cuisine, locality.clone()], " · ")
        .or_else(|| join(&[field("region"), field("country")], ", "))
        .unwrap_or_else(|| "A place on Brainstorm".into());

    let address = address_lines(&AddressParts {
        street: field("address"),
        locality,
        region: field("region"),
        postal: field("postal-code"),
        country: field("country"),
    });

    let panel = field("image")
        .and_then(|u| super::web_url(&u))
        .map(Panel::Image);

    let meta = Meta {
        title: name.clone(),
        description,
        og_type: "website",
        extra: vec![],
    };
    Ok((
        Place {
            name,
            kind,
            address,
            panel,
        },
        meta,
    ))
}

fn join(parts: &[Option<String>], sep: &str) -> Option<String> {
    let s = parts
        .iter()
        .flatten()
        .cloned()
        .collect::<Vec<_>>()
        .join(sep);
    (!s.is_empty()).then_some(s)
}

/// `coffee_shop` → `Coffee shop`, as the UI's `categoryWords`.
pub fn category_words(v: &str) -> String {
    let s = v.trim().replace('_', " ");
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => s,
    }
}

pub struct AddressParts {
    pub street: Option<String>,
    pub locality: Option<String>,
    pub region: Option<String>,
    pub postal: Option<String>,
    pub country: Option<String>,
}

/// Street, then "Locality, Region Postcode, Country" as the UI's `addressLines`; a locality or postcode the street repeats is said once.
pub fn address_lines(p: &AddressParts) -> Vec<String> {
    let mut postal_seen: Option<String> = None;
    let street = p.street.as_deref().and_then(|street| {
        let mut words: Vec<&str> = street.split_whitespace().collect();
        let lw: Vec<&str> = p
            .locality
            .as_deref()
            .map(|l| l.split_whitespace().collect())
            .unwrap_or_default();
        loop {
            if words.len() <= 1 {
                break;
            }
            let last = words[words.len() - 1].trim_end_matches([',', ';']);
            if let Some(code) = p.postal.as_deref() {
                if last == code || last.starts_with(&format!("{code}-")) {
                    postal_seen = Some(last.to_string());
                    words.pop();
                    continue;
                }
            }
            if !lw.is_empty() && words.len() > lw.len() {
                let tail = words[words.len() - lw.len()..].join(" ");
                if tail.eq_ignore_ascii_case(&lw.join(" ")) {
                    words.truncate(words.len() - lw.len());
                    continue;
                }
            }
            break;
        }
        let s = words.join(" ");
        let s = s.trim_end_matches(',').trim().to_string();
        let same_as_locality = p
            .locality
            .as_deref()
            .is_some_and(|l| l.eq_ignore_ascii_case(&s));
        (!s.is_empty() && !same_as_locality).then_some(s)
    });
    let code = postal_seen.or_else(|| p.postal.clone()).map(|c| zip5(&c));
    let region = join(&[p.region.clone(), code], " ");
    let city = join(&[p.locality.clone(), region, p.country.clone()], ", ");
    [street, city].into_iter().flatten().collect()
}

/// `34231-5608` → `34231`.
fn zip5(code: &str) -> String {
    match code.split_once('-') {
        Some((a, b))
            if a.len() == 5
                && b.len() == 4
                && a.chars().chain(b.chars()).all(|c| c.is_ascii_digit()) =>
        {
            a.to_string()
        }
        _ => code.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cards::EntityCard;
    use crate::nip19::Entity;

    const PLACES: &str = crate::config::DEFAULT_PLACE_CONCEPT;

    fn item(tags: &[&[&str]]) -> Event {
        crate::event::ev(39999, tags)
    }

    /// btcmap item "FanShop OrangeCampus" from the search relay, 2026-11-02.
    fn fanshop() -> Event {
        item(&[
            &["d", "osm-node-10011965405"],
            &["z", PLACES],
            &["name", "FanShop OrangeCampus"],
            &["category", "cafe"],
            &["address", "37 Wiblinger Straße Neu-Ulm 89231"],
            &["locality", "Neu-Ulm"],
            &["postal-code", "89231"],
            &["country", "DE"],
            &["cuisine", "coffee_shop"],
            &["lat", "48.388067"],
            &["lon", "9.9871421"],
            &[
                "image",
                "https://ratiopharmulm.com/wp-content/uploads/2022/05/Bild3.jpg",
            ],
            &["cuisine", "bakery"],
        ])
    }

    #[test]
    fn reads_a_real_item() {
        let (p, meta) = read(&fanshop(), PLACES).unwrap();
        assert_eq!(meta.title, "FanShop OrangeCampus");
        assert_eq!(meta.description, "Cafe · Coffee shop · Neu-Ulm");
        assert_eq!(meta.og_type, "website");
        assert_eq!(
            p,
            Place {
                name: "FanShop OrangeCampus".into(),
                kind: Some("Cafe · Coffee shop".into()),
                address: vec!["37 Wiblinger Straße".into(), "Neu-Ulm, 89231, DE".into()],
                panel: Some(Panel::Image(
                    "https://ratiopharmulm.com/wp-content/uploads/2022/05/Bild3.jpg".into()
                )),
            }
        );
    }

    #[test]
    fn other_concepts_and_nameless_items_are_skipped() {
        let mut ev = fanshop();
        ev.tags[1][1] = "39998:aa:books".into();
        assert_eq!(read(&ev, PLACES).unwrap_err(), Skip::Unsupported);
        let ev = item(&[&["z", PLACES], &["name", "  "], &["category", "cafe"]]);
        assert_eq!(read(&ev, PLACES).unwrap_err(), Skip::Empty);
        assert_eq!(
            read(&item(&[&["name", "X"]]), PLACES).unwrap_err(),
            Skip::Unsupported
        );
    }

    #[test]
    fn a_street_that_is_just_the_locality_is_said_once() {
        // btcmap "Coque": address == locality.
        let ev = item(&[
            &["z", PLACES],
            &["name", "Coque"],
            &["category", "restaurant"],
            &["address", "San Salvador de Jujuy"],
            &["locality", "San Salvador de Jujuy"],
            &["country", "AR"],
            &["cuisine", "argentinian"],
        ]);
        let (p, _) = read(&ev, PLACES).unwrap();
        assert_eq!(p.address, vec!["San Salvador de Jujuy, AR"]);
        assert_eq!(p.kind.as_deref(), Some("Restaurant · Argentinian"));
        assert_eq!(p.panel, None);
    }

    #[test]
    fn us_addresses_read_as_a_reader_expects() {
        let lines = address_lines(&AddressParts {
            street: Some("1234 Main St Sarasota 34231-5608".into()),
            locality: Some("Sarasota".into()),
            region: Some("FL".into()),
            postal: Some("34231".into()),
            country: Some("US".into()),
        });
        assert_eq!(lines, vec!["1234 Main St", "Sarasota, FL 34231, US"]);
    }

    #[test]
    fn a_postcode_cut_from_the_street_keeps_no_punctuation() {
        let lines = address_lines(&AddressParts {
            street: Some("Estrada Municipal do Bairro 12450-000,".into()),
            locality: None,
            region: None,
            postal: Some("12450".into()),
            country: Some("BR".into()),
        });
        assert_eq!(lines, vec!["Estrada Municipal do Bairro", "12450-000, BR"]);
    }

    #[test]
    fn same_category_and_cuisine_is_said_once() {
        let ev = item(&[
            &["z", PLACES],
            &["name", "B"],
            &["category", "bakery"],
            &["cuisine", "bakery"],
        ]);
        let (p, meta) = read(&ev, PLACES).unwrap();
        assert_eq!(p.kind.as_deref(), Some("Bakery"));
        assert_eq!(meta.description, "Bakery");
        assert_eq!(p.address, Vec::<String>::new());
    }

    #[test]
    fn only_web_images_become_the_panel() {
        let ev = item(&[
            &["z", PLACES],
            &["name", "B"],
            &["image", "javascript:alert(1)"],
        ]);
        assert_eq!(read(&ev, PLACES).unwrap().0.panel, None);
    }

    fn version(p: Place) -> String {
        EntityCard {
            entity: Entity::Event { id: "a".repeat(64) },
            built: crate::cards::Built {
                author: None,
                meta: read(&fanshop(), PLACES).unwrap().1,
                body: Arc::new(p),
            },
            delete_unknown: false,
        }
        .version("e1")
    }

    #[test]
    fn version_covers_what_is_drawn() {
        let base = read(&fanshop(), PLACES).unwrap().0;
        let v = version(base.clone());
        assert_eq!(v, version(base.clone()));
        let change = |f: &dyn Fn(&mut Place)| {
            let mut p = base.clone();
            f(&mut p);
            version(p)
        };
        assert_ne!(v, change(&|p| p.name.push('!')));
        assert_ne!(v, change(&|p| p.kind = None));
        assert_ne!(v, change(&|p| p.address.pop().map(drop).unwrap_or(())));
        assert_ne!(v, change(&|p| p.panel = None));
    }

    fn draw(p: &Place, image: Option<&[u8]>) -> String {
        let measure = |t: &str, size: f32, _w: u16| t.chars().count() as f32 * size * 0.55;
        p.draw(&DrawCx {
            family: "Figtree",
            measure: &measure,
            image,
        })
    }

    #[test]
    fn the_panel_is_drawn_only_when_its_image_arrived() {
        let (p, _) = read(&fanshop(), PLACES).unwrap();
        assert!(matches!(p.visual(), Some(Visual::Image(_))));
        let with = draw(&p, Some(b"png"));
        assert!(with.contains("place-panel"));
        assert!(with.contains("FanShop") && with.contains("OrangeCampus"));
        assert!(with.contains("Cafe · Coffee shop"));
        assert!(with.contains("37 Wiblinger Straße"));
        assert!(!draw(&p, None).contains("place-panel"));
    }

    #[test]
    fn text_widens_without_a_panel() {
        let p = Place {
            name: "The Very Long Name Of A Neighbourhood Coffee Roastery".into(),
            kind: None,
            address: vec![],
            panel: None,
        };
        let lines = |svg: String| svg.matches("<text").count();
        assert!(lines(draw(&p, None)) < lines(draw(&p, Some(b"png"))));
    }
}
