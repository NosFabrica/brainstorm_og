//! The place card's map panel, drawn from the tiles server's vector tiles.

pub mod mvt;

use std::io::Read;
use std::time::Duration;

use anyhow::{anyhow, bail};
use bytes::Bytes;
use futures_util::StreamExt;
use moka::future::Cache;
use resvg::tiny_skia::{
    Color, FillRule, LineCap, LineJoin, Paint, PathBuilder, Pixmap, Stroke, StrokeDash, Transform,
};

use crate::cards::MapRequest;
use crate::state::AppState;
use mvt::GeomType;

/// A panel up to 512 px square never needs more.
const MAX_TILES: usize = 4;
/// A dense city tile is ~250 KB gzipped (~860 KB raw, Manhattan).
const MAX_TILE_WIRE_BYTES: u64 = 1024 * 1024;
const MAX_TILE_RAW_BYTES: u64 = 4 * 1024 * 1024;
const TILE_CACHE_BYTES: u64 = 16 * 1024 * 1024;
const TILE_CACHE_TTL: Duration = Duration::from_secs(24 * 3600);

pub fn tile_cache() -> Cache<TileId, Bytes> {
    Cache::builder()
        .weigher(|_k: &TileId, v: &Bytes| v.len().min(u32::MAX as usize) as u32)
        .max_capacity(TILE_CACHE_BYTES)
        .time_to_live(TILE_CACHE_TTL)
        .build()
}

/// The map panel as PNG, or `None` (no map, text widens) if any tile fails or the budget runs out.
pub async fn draw(st: &AppState, req: MapRequest) -> Option<Vec<u8>> {
    let ids = covering(&req);
    if ids.is_empty() || ids.len() > MAX_TILES {
        return None;
    }
    let budget = Duration::from_secs(st.config.tiles_timeout_secs);
    let fetched = tokio::time::timeout(
        budget,
        futures_util::future::join_all(ids.iter().map(|&id| tile(st, id))),
    )
    .await
    .map_err(|_| tracing::warn!("map tiles timed out for {},{}", req.lat, req.lon))
    .ok()?;
    let tiles = ids
        .into_iter()
        .zip(fetched)
        .map(|(id, t)| Some((id, t?)))
        .collect::<Option<Vec<_>>>()?;
    tokio::task::spawn_blocking(move || {
        let raw = tiles
            .iter()
            .map(|(id, b)| Some((*id, inflate(b)?)))
            .collect::<Option<Vec<_>>>()?;
        let refs: Vec<_> = raw.iter().map(|(id, b)| (*id, b.as_slice())).collect();
        paint(&req, &refs)
    })
    .await
    .ok()?
}

async fn tile(st: &AppState, id: TileId) -> Option<Bytes> {
    st.tile_cache
        .try_get_with(id, fetch_tile(st, id))
        .await
        .map_err(|e| tracing::warn!("tile {}/{}/{} failed: {e:#}", DATA_ZOOM, id.x, id.y))
        .ok()
}

async fn fetch_tile(st: &AppState, id: TileId) -> anyhow::Result<Bytes> {
    let c = &st.config;
    let url = format!("{}/planet/{DATA_ZOOM}/{}/{}", c.tiles_base_url, id.x, id.y);
    let rq = st
        .tiles_http
        .get(url)
        .header(reqwest::header::ACCEPT_ENCODING, "gzip")
        .timeout(Duration::from_secs(c.tiles_timeout_secs));
    let rq = match &c.tiles_key {
        Some(k) => rq.header("X-NF-Tiles-Key", k),
        None => rq.header(reqwest::header::ORIGIN, &c.app_base_url),
    };
    let resp = rq.send().await?;
    if !resp.status().is_success() {
        bail!("status {}", resp.status());
    }
    let mut body = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if (body.len() + chunk.len()) as u64 > MAX_TILE_WIRE_BYTES {
            return Err(anyhow!("over the {MAX_TILE_WIRE_BYTES} byte cap"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body.into())
}

/// Gzip as the server sends it, or plain; bounded either way.
fn inflate(body: &[u8]) -> Option<Vec<u8>> {
    if !body.starts_with(&[0x1f, 0x8b]) {
        return Some(body.to_vec());
    }
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(body)
        .take(MAX_TILE_RAW_BYTES + 1)
        .read_to_end(&mut out)
        .ok()?;
    (out.len() as u64 <= MAX_TILE_RAW_BYTES).then_some(out)
}

/// Web Mercator's latitude limit.
pub const MAX_LAT: f64 = 85.051_128;

/// A z14 tile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TileId {
    pub x: u32,
    pub y: u32,
}

/// Data zoom. The card draws at Leaflet's zoom 15 (as the UI's `PlaceMap` opens), overzoomed.
pub const DATA_ZOOM: u32 = 14;
/// A z14 tile's size in panel pixels at zoom 15 (256-px tiles).
const TILE_PX: f64 = 512.0;

/// The panel's top-left corner in zoom-15 world pixels.
fn origin(req: &MapRequest) -> (f64, f64) {
    let world = TILE_PX * f64::from(1u32 << DATA_ZOOM);
    let x = (req.lon + 180.0) / 360.0 * world;
    let lat = req.lat.to_radians();
    let y = (1.0 - (lat.tan() + 1.0 / lat.cos()).ln() / std::f64::consts::PI) / 2.0 * world;
    (
        x - f64::from(req.width) / 2.0,
        y - f64::from(req.height) / 2.0,
    )
}

/// The z14 tiles under the panel, row by row. At most 2×2 for a panel up to 512 px.
pub fn covering(req: &MapRequest) -> Vec<TileId> {
    let n = 1i64 << DATA_ZOOM;
    let (ox, oy) = origin(req);
    let span = |o: f64, len: u32| {
        let a = (o / TILE_PX).floor() as i64;
        let b = ((o + f64::from(len) - 1.0) / TILE_PX).floor() as i64;
        a..=b
    };
    let mut out = Vec::new();
    for y in span(oy, req.height).filter(|y| (0..n).contains(y)) {
        for x in span(ox, req.width) {
            out.push(TileId {
                x: x.rem_euclid(n) as u32,
                y: y as u32,
            });
        }
    }
    out
}

// Positron (OpenFreeMap's `styles/positron`), widths at its zoom 14 = Leaflet's 15.
const BACKGROUND: [u8; 3] = [242, 243, 240];
const PARK: [u8; 3] = [230, 233, 229];
const WATER: [u8; 3] = [194, 200, 202];
const RESIDENTIAL: [u8; 3] = [234, 234, 230];
const WOOD: [u8; 3] = [220, 224, 220];
const WATERWAY: [u8; 3] = [189, 204, 208];
const BUILDING: [u8; 3] = [234, 234, 229];
const BUILDING_EDGE: [u8; 3] = [219, 219, 218];
const CASING: [u8; 3] = [213, 213, 213];
const MINOR: [u8; 3] = [224, 224, 224];
const PATH: [u8; 3] = [234, 234, 234];
const ROAD: [u8; 3] = [255, 255, 255];
const RAIL: [u8; 3] = [221, 221, 221];
const RAIL_DASH: [u8; 3] = [250, 250, 250];

#[derive(Clone, Copy)]
enum Draw {
    Fill([u8; 3], f32),
    Line([u8; 3], f32, Option<f32>),
}

/// One style layer: which features of which source layer, drawn how. In Positron's order.
struct Pass {
    layer: &'static str,
    when: fn(&mvt::Feature) -> bool,
    draw: Draw,
}

fn class_in(f: &mvt::Feature, classes: &[&str]) -> bool {
    f.str("class").is_some_and(|c| classes.contains(&c))
}

fn road(f: &mvt::Feature, classes: &[&str]) -> bool {
    f.kind == GeomType::Line && f.str("brunnel") != Some("tunnel") && class_in(f, classes)
}

const MAJOR: &[&str] = &["primary", "secondary", "tertiary", "trunk"];

const PASSES: &[Pass] = &[
    Pass {
        layer: "park",
        when: |_| true,
        draw: Draw::Fill(PARK, 1.0),
    },
    Pass {
        layer: "water",
        when: |f| f.str("brunnel") != Some("tunnel"),
        draw: Draw::Fill(WATER, 1.0),
    },
    Pass {
        layer: "landuse",
        when: |f| class_in(f, &["residential"]),
        draw: Draw::Fill(RESIDENTIAL, 0.6),
    },
    Pass {
        layer: "landcover",
        when: |f| class_in(f, &["wood"]),
        draw: Draw::Fill(WOOD, 1.0),
    },
    Pass {
        layer: "waterway",
        when: |f| !class_in(f, &["river", "canal"]),
        draw: Draw::Line(WATERWAY, 1.0, None),
    },
    Pass {
        layer: "waterway",
        when: |f| class_in(f, &["river", "canal"]),
        draw: Draw::Line(WATERWAY, 2.0, None),
    },
    Pass {
        layer: "building",
        when: |_| true,
        draw: Draw::Fill(BUILDING, 1.0),
    },
    Pass {
        layer: "building",
        when: |_| true,
        draw: Draw::Line(BUILDING_EDGE, 1.0, None),
    },
    Pass {
        layer: "aeroway",
        when: |f| class_in(f, &["runway", "taxiway"]),
        draw: Draw::Fill(ROAD, 1.0),
    },
    Pass {
        layer: "transportation",
        when: |f| road(f, &["path"]),
        draw: Draw::Line(PATH, 1.7, None),
    },
    Pass {
        layer: "transportation",
        when: |f| road(f, &["minor", "service", "track"]),
        draw: Draw::Line(MINOR, 2.3, None),
    },
    Pass {
        layer: "transportation",
        when: |f| road(f, MAJOR),
        draw: Draw::Line(CASING, 5.9, None),
    },
    Pass {
        layer: "transportation",
        when: |f| road(f, MAJOR),
        draw: Draw::Line(ROAD, 4.6, None),
    },
    Pass {
        layer: "transportation",
        when: |f| road(f, &["motorway"]),
        draw: Draw::Line(CASING, 7.6, None),
    },
    Pass {
        layer: "transportation",
        when: |f| road(f, &["motorway"]),
        draw: Draw::Line(ROAD, 4.9, None),
    },
    Pass {
        layer: "transportation",
        when: |f| road(f, &["rail", "transit"]),
        draw: Draw::Line(RAIL, 3.0, None),
    },
    Pass {
        layer: "transportation",
        when: |f| road(f, &["rail", "transit"]),
        draw: Draw::Line(RAIL_DASH, 2.0, Some(3.0)),
    },
];

fn color([r, g, b]: [u8; 3], alpha: f32) -> Color {
    let mut c = Color::from_rgba8(r, g, b, 255);
    c.set_alpha(alpha);
    c
}

/// Draw the map for `req` from its decoded tiles, as PNG. Any tile that does not decode → `None`.
pub fn paint(req: &MapRequest, tiles: &[(TileId, &[u8])]) -> Option<Vec<u8>> {
    let decoded = tiles
        .iter()
        .map(|(id, b)| Some((*id, mvt::decode(b)?)))
        .collect::<Option<Vec<_>>>()?;
    let mut px = Pixmap::new(req.width, req.height)?;
    px.fill(color(BACKGROUND, 1.0));
    let (ox, oy) = origin(req);
    let world = TILE_PX * f64::from(1u32 << DATA_ZOOM);

    for pass in PASSES {
        let mut pb = PathBuilder::new();
        for (id, tile) in &decoded {
            let mut dx = f64::from(id.x) * TILE_PX - ox;
            if dx > world / 2.0 {
                dx -= world;
            } else if dx < -world / 2.0 {
                dx += world;
            }
            let dy = f64::from(id.y) * TILE_PX - oy;
            for layer in tile.layers.iter().filter(|l| l.name == pass.layer) {
                let scale = TILE_PX / f64::from(layer.extent);
                let at = |(x, y): (i32, i32)| {
                    (
                        (dx + f64::from(x) * scale) as f32,
                        (dy + f64::from(y) * scale) as f32,
                    )
                };
                for f in layer.features() {
                    let f = f?;
                    let want = match pass.draw {
                        Draw::Fill(..) => GeomType::Polygon,
                        Draw::Line(..) if pass.layer == "building" => GeomType::Polygon,
                        Draw::Line(..) => GeomType::Line,
                    };
                    if f.kind != want || !(pass.when)(&f) {
                        continue;
                    }
                    for part in f.parts()? {
                        let mut pts = part.into_iter().map(at);
                        let Some((x, y)) = pts.next() else { continue };
                        pb.move_to(x, y);
                        for (x, y) in pts {
                            pb.line_to(x, y);
                        }
                        if want == GeomType::Polygon {
                            pb.close();
                        }
                    }
                }
            }
        }
        let Some(path) = pb.finish() else { continue };
        let mut paint = Paint {
            anti_alias: true,
            ..Default::default()
        };
        match pass.draw {
            Draw::Fill(c, a) => {
                paint.set_color(color(c, a));
                px.fill_path(
                    &path,
                    &paint,
                    FillRule::Winding,
                    Transform::identity(),
                    None,
                );
            }
            Draw::Line(c, width, dash) => {
                paint.set_color(color(c, 1.0));
                let stroke = Stroke {
                    width,
                    line_cap: LineCap::Round,
                    line_join: LineJoin::Round,
                    dash: dash.and_then(|d| StrokeDash::new(vec![d, d], 0.0)),
                    ..Default::default()
                };
                px.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
            }
        }
    }
    px.encode_png().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cape Oysters, Sedgefield: a lagoon-side place, from the search relay 2026-11-02.
    const SEDGEFIELD: MapRequest = MapRequest {
        lat: -34.0097487,
        lon: 22.7773603,
        width: 416,
        height: 414,
    };

    #[test]
    fn the_panel_needs_the_tiles_it_overlaps() {
        let ids = covering(&SEDGEFIELD);
        let t = |x, y| TileId { x, y };
        assert_eq!(
            ids,
            vec![t(9228, 9839), t(9229, 9839), t(9228, 9840), t(9229, 9840)]
        );
        let mid = MapRequest {
            width: 100,
            height: 100,
            ..SEDGEFIELD
        };
        assert_eq!(covering(&mid), vec![t(9228, 9839)]);
    }

    const FIXTURE: &[u8] = include_bytes!("testdata/14-9228-9839.mvt");
    const IN_FIXTURE: MapRequest = MapRequest {
        lat: -34.007135,
        lon: 22.774658,
        width: 416,
        height: 414,
    };
    const ID: TileId = TileId { x: 9228, y: 9839 };

    fn pixel(png: &[u8], x: u32, y: u32) -> [u8; 3] {
        let img = image::load_from_memory(png).unwrap().to_rgb8();
        img.get_pixel(x, y).0
    }

    #[test]
    fn a_tile_that_does_not_decode_draws_no_map() {
        assert!(paint(&IN_FIXTURE, &[(ID, b"\xff\xff\xff".as_slice())]).is_none());
    }

    #[test]
    fn the_map_is_drawn_from_the_tile() {
        let png = paint(&IN_FIXTURE, &[(ID, FIXTURE)]).unwrap();
        let img = image::load_from_memory(&png).unwrap();
        assert_eq!((img.width(), img.height()), (416, 414));
        // Swartvlei top left, the reserve right, the N2 (trunk) across at tile y 2541.
        assert_eq!(pixel(&png, 20, 20), WATER);
        assert_eq!(pixel(&png, 300, 100), PARK);
        assert_eq!(pixel(&png, 300, 268), ROAD);
    }
}
