//! Share cards for `/e/` entities. A new kind is a module with `build` returning a [`Body`], plus one arm in [`build`].

pub mod article;
pub mod calendar;
pub mod fit;
pub mod note;
pub mod place;

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::data::ProfileMeta;
use crate::event::Event;
use crate::nip19::{Entity, Pointer};
use crate::state::AppState;

/// Why no card is drawn. Every skip is a 404, so the crawler falls back to the SPA's card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    /// Not on the relay (after one retry). Short-lived: it may just be new.
    NotFound,
    Nsfw,
    Deleted,
    /// Found, but nothing to draw.
    Empty,
    Unsupported,
}

pub type Outcome = Result<EntityCard, Skip>;

/// The small avatar + name strip at the bottom of note, article and calendar cards.
#[derive(Debug, Clone)]
pub struct Author {
    pub name: String,
    pub picture: Option<String>,
    /// No kind-0 found.
    pub provisional: bool,
}

/// What the meta page says about the card.
#[derive(Debug, Clone)]
pub struct Meta {
    pub title: String,
    pub description: String,
    pub og_type: &'static str,
    /// Extra `<meta property=…>` pairs, e.g. `article:author`.
    pub extra: Vec<(&'static str, String)>,
}

/// An image a body wants fetched (through the avatar pipeline) before drawing.
#[derive(Debug, Clone)]
pub struct ImageRequest {
    pub url: String,
    pub width: u32,
    pub height: u32,
}

/// A map centred on a point, drawn from vector tiles instead of fetched as an image.
#[derive(Debug, Clone, Copy)]
pub struct MapRequest {
    pub lat: f64,
    pub lon: f64,
    pub width: u32,
    pub height: u32,
}

/// A body's one picture: fetched, or drawn from vector tiles.
#[derive(Debug, Clone)]
pub enum Visual {
    Image(ImageRequest),
    Map(MapRequest),
}

/// Everything a body needs to draw its content area.
pub struct DrawCx<'a> {
    pub family: &'a str,
    /// Advance width of `text` at `size` px and font weight, as resvg will draw it.
    pub measure: &'a dyn Fn(&str, f32, u16) -> f32,
    /// The [`Body::visual`] as PNG. `None` if absent or it failed.
    pub image: Option<&'a [u8]>,
}

impl DrawCx<'_> {
    /// `measure` at one weight, the shape `fit` takes.
    pub fn at(&self, weight: u16) -> impl Fn(&str, f32) -> f32 + '_ {
        move |t, size| (self.measure)(t, size, weight)
    }

    /// `f`'s lines as `<text>`, the first baseline at `top + f.size`; returns the height used.
    pub fn lines(
        &self,
        out: &mut String,
        x: f32,
        top: f32,
        f: &fit::Fit,
        lh: f32,
        attrs: &str,
    ) -> f32 {
        let family = crate::render::esc(self.family);
        let step = f.size * lh;
        for (i, line) in f.lines.iter().enumerate() {
            out.push_str(&format!(
                r#"<text x="{x}" y="{y}" font-family="{family}" font-size="{size}" {attrs}>{}</text>"#,
                crate::render::esc(line),
                y = top + f.size + step * i as f32,
                size = f.size,
            ));
        }
        step * f.lines.len() as f32
    }
}

/// The per-kind part of a card.
pub trait Body: Send + Sync + std::fmt::Debug {
    /// Feed exactly what `draw` draws into the `?v=` hash.
    fn fingerprint(&self, h: &mut DefaultHasher);
    /// SVG for the content area: inside the frame, above the author strip.
    fn draw(&self, cx: &DrawCx) -> String;
    fn visual(&self) -> Option<Visual> {
        None
    }
}

/// What a kind's `build` returns.
#[derive(Debug, Clone)]
pub struct Built {
    /// `None` draws no author strip (place cards).
    pub author: Option<Author>,
    pub meta: Meta,
    pub body: Arc<dyn Body>,
}

#[derive(Debug, Clone)]
pub struct EntityCard {
    pub entity: Entity,
    pub built: Built,
    /// The delete check failed, so a delete may yet show up.
    pub delete_unknown: bool,
}

impl EntityCard {
    pub fn provisional(&self) -> bool {
        self.delete_unknown || self.built.author.as_ref().is_some_and(|a| a.provisional)
    }

    /// `?v=`: what the card draws plus the render epoch. CONTEXT.md.
    pub fn version(&self, render_epoch: &str) -> String {
        let mut h = DefaultHasher::new();
        if let Some(a) = &self.built.author {
            a.name.hash(&mut h);
            a.picture.hash(&mut h);
        }
        self.built.body.fingerprint(&mut h);
        render_epoch.hash(&mut h);
        format!("{:x}", h.finish())
    }
}

/// An `http(s)` URL, the only kind we fetch for a card.
pub fn web_url(s: &str) -> Option<String> {
    let lower = s.get(..8).unwrap_or(s).to_ascii_lowercase();
    (lower.starts_with("https://") || lower.starts_with("http://")).then(|| s.to_string())
}

/// Per-kind dispatch.
async fn build(st: &AppState, ev: &Event) -> Result<Built, Skip> {
    match ev.kind {
        1 => Box::pin(note::build(st, ev)).await,
        30023 => Box::pin(article::build(st, ev)).await,
        calendar::DATE_KIND | calendar::TIME_KIND => Box::pin(calendar::build(st, ev)).await,
        9999 | 39999 => Box::pin(place::build(st, ev)).await,
        _ => Err(Skip::Unsupported),
    }
}

/// Per-entry TTL: provisional cards die young, settled cards and skips cache normally.
pub struct OutcomeExpiry {
    pub provisional: Duration,
    pub normal: Duration,
}

impl moka::Expiry<String, Outcome> for OutcomeExpiry {
    fn expire_after_create(
        &self,
        _k: &String,
        v: &Outcome,
        _now: std::time::Instant,
    ) -> Option<Duration> {
        Some(match v {
            Ok(card) if card.provisional() => self.provisional,
            _ => self.normal,
        })
    }
}

/// Fetch, skip-check and build (cached, coalesced). A not-found is never cached.
pub async fn get(st: &AppState, entity: &Entity) -> Outcome {
    let (s, e) = (st.clone(), entity.clone());
    st.entity_cache
        .try_get_with(entity.key(), async move {
            let outcome = tokio::time::timeout(s.config.entity_deadline(), Box::pin(load(&s, &e)))
                .await
                .unwrap_or_else(|_| {
                    tracing::warn!("entity {} hit the deadline", e.key());
                    Err(Skip::NotFound)
                });
            match outcome {
                Err(Skip::NotFound) => Err(()),
                other => Ok(other),
            }
        })
        .await
        .unwrap_or(Err(Skip::NotFound))
}

async fn load(st: &AppState, entity: &Entity) -> Outcome {
    let ev = match fetch_entity(st, entity).await {
        Some(ev) => ev,
        None => {
            tokio::time::sleep(Duration::from_millis(st.config.entity_retry_delay_ms)).await;
            fetch_entity(st, entity).await.ok_or(Skip::NotFound)?
        }
    };
    if ev.is_nsfw() {
        return Err(Skip::Nsfw);
    }
    let (deleted, card) = tokio::join!(is_deleted(st, &ev), build(st, &ev));
    if deleted == Some(true) {
        return Err(Skip::Deleted);
    }
    card.map(|built| EntityCard {
        entity: entity.clone(),
        built,
        delete_unknown: deleted.is_none(),
    })
}

fn filter_for(entity: &Entity) -> Value {
    match entity {
        Entity::Event { id } => json!({ "ids": [id], "limit": 1 }),
        Entity::Address { kind, pubkey, d } => {
            json!({ "authors": [pubkey], "kinds": [kind], "#d": [d], "limit": 1 })
        }
    }
}

fn answers(entity: &Entity, ev: &Event) -> bool {
    match entity {
        Entity::Event { id } => ev.id.eq_ignore_ascii_case(id),
        Entity::Address { kind, pubkey, d } => {
            ev.kind == *kind
                && ev.pubkey.eq_ignore_ascii_case(pubkey)
                && ev.tag("d").unwrap_or("") == d.trim()
        }
    }
}

async fn fetch_entity(st: &AppState, entity: &Entity) -> Option<Event> {
    let relay = &st.config.local_relay_url;
    let events =
        crate::relay::fetch_events(relay, filter_for(entity), st.config.fetch_timeout_secs)
            .await
            .map_err(|e| tracing::warn!("entity read {} on {relay} failed: {e}", entity.key()))
            .ok()?;
    events
        .into_iter()
        .filter_map(|v| serde_json::from_value::<Event>(v).ok())
        .filter(|ev| answers(entity, ev))
        .max_by_key(|ev| ev.created_at)
}

/// Whether the author's kind-5 names the event (`e`) or its address (`a`); `None` if a read failed.
async fn is_deleted(st: &AppState, ev: &Event) -> Option<bool> {
    let relay = &st.config.local_relay_url;
    let timeout = st.config.fetch_timeout_secs;
    let by_id = json!({ "kinds": [5], "authors": [ev.pubkey], "#e": [ev.id] });
    let by_addr = ev
        .address()
        .map(|a| json!({ "kinds": [5], "authors": [ev.pubkey], "#a": [a] }));
    let (a, b) = tokio::join!(crate::relay::fetch_events(relay, by_id, timeout), async {
        match by_addr {
            Some(f) => crate::relay::fetch_events(relay, f, timeout).await,
            None => Ok(vec![]),
        }
    });
    let mut failed = false;
    for r in [a, b] {
        match r {
            Ok(events) => {
                let by_author = events.iter().any(|e| {
                    e.get("pubkey")
                        .and_then(Value::as_str)
                        .is_some_and(|pk| pk.eq_ignore_ascii_case(&ev.pubkey))
                });
                if by_author {
                    return Some(true);
                }
            }
            Err(err) => {
                tracing::warn!("delete check for {} failed: {err}", ev.id);
                failed = true;
            }
        }
    }
    (!failed).then_some(false)
}

/// The author strip, through the profile card path and its cache.
pub async fn author(st: &AppState, pubkey: &str) -> Author {
    let card = crate::data::get_card(
        st,
        &Pointer {
            hex: pubkey.to_lowercase(),
        },
    )
    .await;
    Author {
        name: card.display_name(),
        picture: card.meta.picture.clone(),
        provisional: card.provisional,
    }
}

/// Display names for referenced pubkeys in one read. Missing ones are absent.
pub async fn names(st: &AppState, pubkeys: &[String]) -> HashMap<String, String> {
    if pubkeys.is_empty() {
        return HashMap::new();
    }
    let relay = &st.config.local_relay_url;
    let filter = json!({ "authors": pubkeys, "kinds": [0] });
    let events = crate::relay::fetch_events(relay, filter, st.config.fetch_timeout_secs)
        .await
        .map_err(|e| tracing::warn!("kind-0 batch on {relay} failed: {e}"))
        .unwrap_or_default();
    let mut newest: HashMap<String, (u64, String)> = HashMap::new();
    for e in events
        .into_iter()
        .filter_map(|v| serde_json::from_value::<Event>(v).ok())
    {
        if e.kind != 0 {
            continue;
        }
        let Some(name) = ProfileMeta::from_kind0_content(&e.content)
            .ok()
            .and_then(|m| m.best_name())
        else {
            continue;
        };
        let pk = e.pubkey.to_lowercase();
        if newest.get(&pk).is_none_or(|(at, _)| e.created_at > *at) {
            newest.insert(pk, (e.created_at, name));
        }
    }
    newest
        .into_iter()
        .map(|(pk, (_, name))| (pk, name))
        .collect()
}
