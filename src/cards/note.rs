//! Note card (kind 1): the note's own text, cut; author strip.
//!
//! A note's first image is what its page advertises as `og:image`; the card is
//! the fallback for notes without one. A note that is only an image draws it.

use std::collections::hash_map::DefaultHasher;
use std::hash::Hash;
use std::sync::Arc;

use super::{fit, Body, Built, DrawCx, ImageRequest, Meta, Skip, Visual};
use crate::event::Event;
use crate::nip19::{self, Entity};
use crate::render::{self, INK, MUTED};
use crate::state::AppState;

/// Longer than any box can show; keeps wrapping cheap on huge notes.
const MAX_CHARS: usize = 800;
const SIZES: [f32; 7] = [44.0, 42.0, 40.0, 38.0, 36.0, 34.0, 32.0];
const LINE_HEIGHT: f32 = 1.3;
const REPLY_SIZE: f32 = 28.0;

/// Rounded corners of a drawn image, as on the article card.
const IMAGE_RADIUS: f32 = 24.0;

#[derive(Debug, Hash)]
struct Note {
    replying_to: Option<String>,
    /// Empty for a note that is only an image.
    text: String,
    /// Drawn only when there is no text: then it is the whole note.
    image: Option<String>,
}

impl Note {
    fn image_top(&self) -> f32 {
        render::CONTENT_TOP
            + if self.replying_to.is_some() {
                REPLY_SIZE * 2.0
            } else {
                0.0
            }
    }
}

impl Body for Note {
    fn fingerprint(&self, h: &mut DefaultHasher) {
        self.hash(h);
    }

    fn visual(&self) -> Option<Visual> {
        let url = self.image.as_ref().filter(|_| self.text.is_empty())?;
        Some(Visual::Image(ImageRequest {
            url: url.clone(),
            width: render::CONTENT_W as u32,
            height: (render::CONTENT_BOTTOM - self.image_top()) as u32,
        }))
    }

    fn draw(&self, cx: &DrawCx) -> String {
        let (x, mut top, width, bottom) = (
            render::CONTENT_X,
            render::CONTENT_TOP,
            render::CONTENT_W,
            render::CONTENT_BOTTOM,
        );
        let mut out = String::new();
        if let Some(name) = &self.replying_to {
            let line = fit::Fit {
                size: REPLY_SIZE,
                lines: vec![fit::clip_words(&format!("Replying to {name}"), 60)],
            };
            cx.lines(&mut out, x, top, &line, 1.0, &format!(r#"fill="{MUTED}""#));
            top += REPLY_SIZE * 2.0;
        }
        if self.text.is_empty() {
            if let Some(png) = cx.image {
                let id = "note-img";
                out.push_str(&render::embed_png(
                    id,
                    x,
                    top,
                    width,
                    bottom - top,
                    IMAGE_RADIUS,
                    png,
                ));
            }
            return out;
        }
        let f = fit::fit(
            &self.text,
            width,
            bottom - top,
            &SIZES,
            LINE_HEIGHT,
            cx.at(400),
        );
        // Centred in the box, so a one-liner sits like a pull quote.
        let used = f.size * LINE_HEIGHT * f.lines.len() as f32;
        let first = top + ((bottom - top - used) / 2.0).max(0.0);
        cx.lines(
            &mut out,
            x,
            first,
            &f,
            LINE_HEIGHT,
            &format!(r#"fill="{INK}""#),
        );
        out
    }
}

pub async fn build(st: &AppState, ev: &Event) -> Result<Built, Skip> {
    let parent = reply_parent(ev);
    let mut wanted = mentioned_pubkeys(&ev.content);
    if let Some(Reply::To(p)) = &parent {
        if !wanted.contains(p) {
            wanted.push(p.clone());
        }
    }
    wanted.truncate(16);
    let (author, names) = tokio::join!(super::author(st, &ev.pubkey), super::names(st, &wanted));
    let name_of = |pk: &str| names.get(pk).cloned();

    let image = first_image(ev);
    let text = text(&ev.content, name_of)
        .map(|t| render::sanitize(&t))
        .filter(|t| !t.is_empty());
    // A picture with no words is still a note worth a preview; nothing at all is not.
    if text.is_none() && image.is_none() {
        return Err(Skip::Empty);
    }
    let text = text.unwrap_or_default();
    let replying_to = parent.map(|r| match r {
        Reply::To(pk) => render::sanitize(&name_of(&pk).unwrap_or_else(|| short_npub(&pk))),
        Reply::ToNote => "a note".into(),
    });

    Ok(Built {
        meta: Meta {
            title: format!("{} on Brainstorm", author.name),
            description: fit::clip_words(&text, 200),
            og_type: "article",
            extra: vec![],
            image: image.clone(),
        },
        author: Some(author),
        body: Arc::new(Note {
            replying_to,
            text: fit::clip_words(&text, MAX_CHARS),
            image,
        }),
    })
}

/// File extensions that name a picture. Matched on the URL's path, so a query string doesn't hide one.
const IMAGE_EXTENSIONS: [&str; 7] = ["jpg", "jpeg", "png", "gif", "webp", "avif", "jfif"];

/// The first image in the note's text: a URL whose path ends in a picture's
/// extension, or one a NIP-92 `imeta` tag says is an image (`m image/…`).
pub fn first_image(ev: &Event) -> Option<String> {
    let imeta_image = |url: &str| {
        ev.tags_named("imeta").any(|fields| {
            let has = |key: &str, ok: &dyn Fn(&str) -> bool| {
                fields
                    .iter()
                    .any(|f| f.strip_prefix(key).is_some_and(|v| ok(v.trim())))
            };
            has("url ", &|v| v == url)
                && has("m ", &|v| v.to_ascii_lowercase().starts_with("image/"))
        })
    };
    ev.content
        .split_whitespace()
        .filter_map(|chunk| {
            // A URL at the end of a sentence or in brackets: the punctuation isn't part of it.
            let url =
                chunk.trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '>', '"', '\'']);
            super::web_url(url)
        })
        .find(|url| has_image_extension(url) || imeta_image(url))
}

fn has_image_extension(url: &str) -> bool {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let last = path.rsplit('/').next().unwrap_or("");
    last.rsplit_once('.')
        .is_some_and(|(_, ext)| IMAGE_EXTENSIONS.iter().any(|e| ext.eq_ignore_ascii_case(e)))
}

enum Token<'a> {
    Word(&'a str),
    Url,
    Profile(String),
    Quote(Entity),
}

/// One whitespace-free chunk, split around any `nostr:` refs inside it.
fn split_refs(chunk: &str) -> Vec<Token<'_>> {
    let lower = chunk.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return vec![Token::Url];
    }
    let mut out = Vec::new();
    let mut rest = chunk;
    while let Some(at) = rest.find("nostr:") {
        let tail = &rest[at + 6..];
        let len = tail
            .find(|c: char| !c.is_ascii_alphanumeric())
            .unwrap_or(tail.len());
        let bech = &tail[..len];
        let is_profile = bech.starts_with("npub1") || bech.starts_with("nprofile1");
        let token = if is_profile {
            nip19::decode(bech).ok().map(|p| Token::Profile(p.hex))
        } else {
            nip19::decode_entity(bech).ok().map(Token::Quote)
        };
        match token {
            Some(t) => {
                if at > 0 {
                    out.push(Token::Word(&rest[..at]));
                }
                out.push(t);
                rest = &tail[len..];
            }
            None => break,
        }
    }
    if !rest.is_empty() {
        out.push(Token::Word(rest));
    }
    out
}

/// Pubkeys referenced as `nostr:npub…`/`nprofile…`, deduplicated, in order.
pub fn mentioned_pubkeys(content: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in content.split_whitespace().flat_map(split_refs) {
        if let Token::Profile(pk) = t {
            if !out.contains(&pk) {
                out.push(pk);
            }
        }
    }
    out
}

/// The drawable text: URLs dropped, refs shortened; `None` if only media and quotes remain.
pub fn text(content: &str, names: impl Fn(&str) -> Option<String>) -> Option<String> {
    let mut chunks: Vec<String> = Vec::new();
    let mut substance = false;
    for chunk in content.split_whitespace() {
        let mut out = String::new();
        for t in split_refs(chunk) {
            match t {
                Token::Url => {}
                Token::Word(w) => {
                    substance = true;
                    out.push_str(w);
                }
                Token::Profile(pk) => {
                    substance = true;
                    if !out.ends_with('@') {
                        out.push('@');
                    }
                    out.push_str(&names(&pk).unwrap_or_else(|| short_npub(&pk)));
                }
                Token::Quote(e) => out.push_str(&short_ref(&e)),
            }
        }
        if !out.is_empty() {
            chunks.push(out);
        }
    }
    substance.then(|| chunks.join(" "))
}

pub fn short_npub(pk: &str) -> String {
    let npub = nip19::npub_from_hex(pk).unwrap_or_else(|| pk.to_string());
    format!("{}…", &npub[..npub.len().min(12)])
}

fn short_ref(e: &Entity) -> String {
    let full = match e {
        Entity::Event { id } => nip19::note_from_hex(id).unwrap_or_default(),
        Entity::Address { .. } => e.canonical(),
    };
    let prefix = if full.starts_with("note1") { 9 } else { 10 };
    format!("{}…", &full[..full.len().min(prefix)])
}

#[derive(Debug, PartialEq)]
pub enum Reply {
    To(String),
    /// The parent `e` tag names no author.
    ToNote,
}

/// Whom a NIP-10 reply answers: only the parent `e` tag's own author, never a guess from `p`.
pub fn reply_parent(ev: &Event) -> Option<Reply> {
    let es: Vec<&[String]> = ev.tags_named("e").collect();
    fn marker(v: &[String]) -> &str {
        v.get(2).map(String::as_str).unwrap_or("")
    }
    let parent = es
        .iter()
        .find(|v| marker(v) == "reply")
        .or_else(|| es.iter().find(|v| marker(v) == "root"))
        .or_else(|| es.iter().rev().find(|v| marker(v).is_empty()))?;
    Some(match parent.get(3).filter(|pk| nip19::is_hex64(pk)) {
        Some(pk) => Reply::To(pk.to_lowercase()),
        None => Reply::ToNote,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nip19::Entity;

    const PK: &str = "3bf0c63fcb93463407af97a5e5ee64fa883d107ef9e558472c4eb9aaaefa459d";
    const ID: &str = "b9f5441e45ca39179320e0031cfb18e34078673dcc3d3e3a3b3a981760aa5696";

    fn names(pk: &str) -> Option<String> {
        (pk == PK).then(|| "fiatjaf".to_string())
    }

    fn npub() -> String {
        crate::nip19::npub_from_hex(PK).unwrap()
    }

    #[test]
    fn plain_text_is_kept_with_whitespace_collapsed() {
        assert_eq!(text("  gm\n\n nostr  ", names).as_deref(), Some("gm nostr"));
    }

    #[test]
    fn urls_are_dropped() {
        let t = text("look https://x.com/a.png?b=1 and HTTP://y.org\nnice", names);
        assert_eq!(t.as_deref(), Some("look and nice"));
    }

    #[test]
    fn profile_refs_become_names() {
        let t = text(&format!("hi nostr:{}, and you", npub()), names);
        assert_eq!(t.as_deref(), Some("hi @fiatjaf, and you"));
    }

    #[test]
    fn a_ref_after_an_at_sign_gets_no_second_one() {
        let t = text(&format!("/cmd@nostr:{}", npub()), names);
        assert_eq!(t.as_deref(), Some("/cmd@fiatjaf"));
    }

    #[test]
    fn unknown_profiles_fall_back_to_a_short_npub() {
        let t = text(&format!("hi nostr:{}", npub()), |_| None).unwrap();
        assert!(t.starts_with("hi @npub1"), "got {t}");
        assert!(t.ends_with('…'), "got {t}");
    }

    #[test]
    fn event_refs_become_short_note_mentions() {
        let nevent = Entity::Event { id: ID.into() }.canonical();
        let t = text(&format!("see nostr:{nevent} wow"), names).unwrap();
        assert!(t.starts_with("see note1"), "got {t}");
        assert!(t.contains("… wow"), "got {t}");
        assert!(!t.contains(&nevent), "full ref leaked: {t}");
    }

    #[test]
    fn nothing_left_after_stripping_is_none() {
        assert_eq!(text("https://x.com/a.jpg https://x.com/b.mp4", names), None);
        let nevent = Entity::Event { id: ID.into() }.canonical();
        assert_eq!(text(&format!("nostr:{nevent}"), names), None);
        assert_eq!(text("   \n ", names), None);
    }

    #[test]
    fn a_bare_mention_still_draws() {
        assert_eq!(
            text(&format!("nostr:{}", npub()), names).as_deref(),
            Some("@fiatjaf")
        );
    }

    #[test]
    fn mentioned_pubkeys_are_listed_once() {
        let n = npub();
        assert_eq!(
            mentioned_pubkeys(&format!("nostr:{n} nostr:{n} x")),
            vec![PK.to_string()]
        );
    }

    fn ev(tags: &[&[&str]]) -> Event {
        crate::event::ev(1, tags)
    }

    fn card(
        text: &str,
        replying: Option<&str>,
        name: &str,
        picture: Option<&str>,
    ) -> super::super::EntityCard {
        super::super::EntityCard {
            entity: Entity::Event { id: ID.into() },
            built: super::super::Built {
                author: Some(super::super::Author {
                    name: name.into(),
                    picture: picture.map(str::to_string),
                    provisional: false,
                }),
                meta: Meta {
                    title: "t".into(),
                    description: "unhashed".into(),
                    og_type: "article",
                    extra: vec![],
                    image: None,
                },
                body: Arc::new(Note {
                    replying_to: replying.map(str::to_string),
                    text: text.into(),
                    image: None,
                }),
            },
            delete_unknown: false,
        }
    }

    #[test]
    fn version_covers_what_is_drawn() {
        let base = card("hello", None, "alice", None).version("e1");
        assert_eq!(base, card("hello", None, "alice", None).version("e1"));
        assert_ne!(base, card("hello!", None, "alice", None).version("e1"));
        assert_ne!(
            base,
            card("hello", Some("bob"), "alice", None).version("e1")
        );
        assert_ne!(base, card("hello", None, "alicia", None).version("e1"));
        assert_ne!(
            base,
            card("hello", None, "alice", Some("https://x/a.png")).version("e1")
        );
        assert_ne!(base, card("hello", None, "alice", None).version("e2"));
        let mut other_desc = card("hello", None, "alice", None);
        other_desc.built.meta.description = "changed".into();
        assert_eq!(base, other_desc.version("e1"));
    }

    fn with_content(content: &str, tags: &[&[&str]]) -> Event {
        Event {
            content: content.into(),
            ..ev(tags)
        }
    }

    #[test]
    fn first_image_is_the_first_picture_url() {
        let e = with_content(
            "see https://x.com/page and https://cdn.x/a.PNG?w=1, then https://cdn.x/b.jpg",
            &[],
        );
        assert_eq!(first_image(&e).as_deref(), Some("https://cdn.x/a.PNG?w=1"));
    }

    #[test]
    fn first_image_drops_trailing_punctuation() {
        let e = with_content("(look: https://cdn.x/a.webp).", &[]);
        assert_eq!(first_image(&e).as_deref(), Some("https://cdn.x/a.webp"));
    }

    #[test]
    fn first_image_takes_an_extensionless_url_imeta_calls_an_image() {
        let e = with_content(
            "https://blossom.x/abc https://blossom.x/def",
            &[
                &["imeta", "url https://blossom.x/abc", "m video/mp4"],
                &[
                    "imeta",
                    "url https://blossom.x/def",
                    "m image/jpeg",
                    "dim 10x10",
                ],
            ],
        );
        assert_eq!(first_image(&e).as_deref(), Some("https://blossom.x/def"));
    }

    #[test]
    fn video_and_pages_are_not_images() {
        let e = with_content(
            "https://x.com/v.mp4 https://x.com/a.png.html nostr:abc",
            &[],
        );
        assert_eq!(first_image(&e), None);
    }

    #[test]
    fn an_image_only_note_draws_its_image_and_nothing_else() {
        let note = Note {
            replying_to: None,
            text: String::new(),
            image: Some("https://cdn.x/a.png".into()),
        };
        let Some(Visual::Image(req)) = note.visual() else {
            panic!("an image-only note must ask for its image");
        };
        assert_eq!(req.url, "https://cdn.x/a.png");
        let svg = note.draw(&DrawCx {
            family: "Figtree",
            measure: &|t, px, _| t.chars().count() as f32 * px * 0.5,
            image: Some(b"png"),
        });
        assert!(svg.contains("<image "), "got {svg}");
        assert!(!svg.contains("<text "), "got {svg}");
    }

    #[test]
    fn a_note_with_text_draws_text_not_its_image() {
        let note = Note {
            replying_to: None,
            text: "gm".into(),
            image: Some("https://cdn.x/a.png".into()),
        };
        assert!(note.visual().is_none());
    }

    #[test]
    fn reply_parent_follows_nip10() {
        let other = "a".repeat(64);
        assert_eq!(
            reply_parent(&ev(&[
                &["e", ID, "", "root"],
                &["e", ID, "", "reply", &other]
            ])),
            Some(Reply::To(other.clone()))
        );
        assert_eq!(
            reply_parent(&ev(&[&["e", ID, "", "root", PK]])),
            Some(Reply::To(PK.to_string()))
        );
        assert_eq!(
            reply_parent(&ev(&[&["e", ID, "", "mention"], &["p", PK]])),
            None
        );
        assert_eq!(reply_parent(&ev(&[&["q", ID], &["p", PK]])), None);
        assert_eq!(reply_parent(&ev(&[])), None);
    }

    #[test]
    fn a_parent_without_an_author_is_not_guessed_from_p_tags() {
        // The first `p` is often the thread root's author, not the parent's.
        assert_eq!(
            reply_parent(&ev(&[&["e", ID, "", "reply"], &["p", PK]])),
            Some(Reply::ToNote)
        );
        assert_eq!(
            reply_parent(&ev(&[&["e", ID], &["p", PK]])),
            Some(Reply::ToNote)
        );
    }
}
