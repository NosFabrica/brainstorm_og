//! Turn a fetched page into the four fields a preview card needs.
//!
//! Everything here reads attacker-supplied markup, so the shape is: collect
//! raw candidates, then decide. Precedence is load-bearing rather than
//! decorative — X/Twitter serves **no `og:` tags at all**, only a complete
//! `twitter:*` set, and the Guardian, Hacker News and sourcehut have neither
//! and only a `<title>`. Measured over 48 sites, the full chain is the
//! difference between ~60% and ~80% of links previewing at all.
//!
//! A real parser, not a regex: CNN emits `property='og:title'` and
//! `content='…'` on separate lines with single quotes, which any naive
//! single-line double-quote pattern misses.

use encoding_rs::Encoding;
use lol_html::html_content::{Element, TextChunk};
use lol_html::{element, text, HtmlRewriter, Settings};
use serde::Serialize;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use url::Url;

use super::{find_head_end, is_image, Page, HEAD_FLOOR, SCHEMES};
use crate::net::{self, Reserved};

/// Caps, in characters. Long enough for any honest page — the longest
/// `og:description` in the 48-site sample is 297 — and short enough that a
/// hostile one cannot make our JSON the payload.
const TITLE_CAP: usize = 300;
const DESCRIPTION_CAP: usize = 1000;
const SITE_NAME_CAP: usize = 100;

/// How far in we look for `<meta charset>` when the header did not say. The
/// HTML spec's own prescan is 1024 bytes; matching it costs nothing.
const CHARSET_PRESCAN: usize = 1024;

/// `<title>`'s text, held under a key no `meta` can claim — `<meta name="title">`
/// is a real and different thing.
const DOC_TITLE: &str = "<title>";

/// Precedence, one list per field, read left to right — the ticket's table.
/// `scan` keeps exactly the keys named here and drops everything else as it
/// streams, so adding a source is one edit rather than two.
const TITLE: &[&str] = &["og:title", "twitter:title", DOC_TITLE];
const DESCRIPTION: &[&str] = &["og:description", "twitter:description", "description"];
const IMAGE: &[&str] = &["og:image", "og:image:url", "twitter:image"];
const SITE_NAME: &[&str] = &["og:site_name"];

fn wanted(key: &str) -> bool {
    [TITLE, DESCRIPTION, IMAGE, SITE_NAME]
        .iter()
        .any(|list| list.contains(&key))
}

/// What `/link-preview` returns in `data`. Every field is nullable: a page
/// with no usable markup is a 200 with nulls, so the card degrades rather
/// than disappearing.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Preview {
    pub kind: Kind,
    pub title: Option<String>,
    pub description: Option<String>,
    pub image: Option<String>,
    pub site_name: Option<String>,
    /// The final URL, after redirects — not the one that was asked for.
    pub url: String,
}

/// What the link points at. An image link carries no markup to read, but the
/// UI can still show the picture rather than a card naming its path.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    #[default]
    Page,
    Image,
}

impl Preview {
    /// Heap bytes this holds, for the cache's weigher. Field caps are in
    /// *characters*, not bytes, so a CJK-heavy preview is roughly three times
    /// a Latin one at the same cap — which is exactly why this counts the
    /// strings rather than assuming a per-entry average.
    pub(crate) fn byte_size(&self) -> usize {
        [&self.title, &self.description, &self.image, &self.site_name]
            .into_iter()
            .flatten()
            .map(String::len)
            .sum::<usize>()
            + self.url.len()
    }
}

/// Raw candidates, keyed as the markup named them.
#[derive(Debug, Default)]
struct Tags {
    found: HashMap<String, String>,
    /// `<title>`'s text, accumulated across chunks before it joins `found`.
    doc_title: String,
    /// `<title>` is closed, so a later one (an inline SVG's, say) is not ours.
    title_closed: bool,
}

impl Tags {
    /// First non-empty occurrence wins: duplicated `og:` tags are common and
    /// the first is the canonical one. An empty `content` is not a value — it
    /// must fall through to the next source rather than win by being present.
    /// Unwanted keys are dropped here, so hostile markup cannot grow the map.
    fn record(&mut self, key: &str, value: Option<String>) {
        let Some(value) = value.filter(|v| !v.trim().is_empty()) else {
            return;
        };
        if wanted(key) {
            self.found.entry(key.to_owned()).or_insert(value);
        }
    }

    fn candidates<'a>(&'a self, keys: &'a [&str]) -> impl Iterator<Item = &'a str> {
        keys.iter()
            .filter_map(|k| self.found.get(*k).map(String::as_str))
    }
}

/// Read a fetched page into a `Preview`.
///
/// `reserved` is the deployment's address policy: `og:image` is dialled by the
/// *browser* on our say-so, so it is held to the same ranges the fetch refuses.
///
/// The body is already capped by `fetch`; this additionally stops at the same
/// place the read did, so parsing stays bounded even if a caller hands over
/// something larger.
pub fn preview(page: &Page, reserved: Reserved) -> Preview {
    if is_image(&page.content_type) {
        return Preview {
            kind: Kind::Image,
            image: net::validate_url_with(page.final_url.as_str(), SCHEMES, reserved)
                .ok()
                .map(|u| u.to_string()),
            url: page.final_url.to_string(),
            ..Default::default()
        };
    }
    let html = decode(&page.content_type, &page.body[..head_cut(&page.body)]);
    let tags = scan(&html);

    // Bound rather than returned directly: the candidate iterators borrow
    // `tags`, and a tail expression's temporaries outlive the block's locals.
    let preview = Preview {
        title: pick(tags.candidates(TITLE), TITLE_CAP),
        description: pick(tags.candidates(DESCRIPTION), DESCRIPTION_CAP),
        // A candidate that fails validation falls through to the next, the way
        // an empty one does. The alternative — nulling the whole field because
        // the *first* source was unusable — loses previews for no gain.
        image: tags
            .candidates(IMAGE)
            .find_map(|raw| resolve_image(raw, &page.final_url, reserved)),
        site_name: pick(tags.candidates(SITE_NAME), SITE_NAME_CAP)
            .or_else(|| host_label(&page.final_url)),
        url: page.final_url.to_string(),
        kind: Kind::Page,
    };
    preview
}

/// Where to stop parsing: just past `</head>`, but never before the floor —
/// a few real sites (blockstream.com) close `<head>` early and put their tags
/// behind it. Deliberately the same rule the read uses, so the parser sees
/// exactly the bytes the fetch decided were worth having.
fn head_cut(body: &[u8]) -> usize {
    match find_head_end(body, 0) {
        Some(end) => end.max(HEAD_FLOOR).min(body.len()),
        None => body.len(),
    }
}

/// First candidate with something in it, cleaned and capped.
fn pick<'a>(candidates: impl Iterator<Item = &'a str>, cap: usize) -> Option<String> {
    candidates.into_iter().find_map(|raw| clean(raw, cap))
}

/// Entity-decode, collapse whitespace, cap. Titles routinely arrive wrapped
/// across lines with the indentation still in them, and nothing here is
/// re-parsed as HTML, so `&amp;` must become `&` rather than stay escaped.
fn clean(raw: &str, cap: usize) -> Option<String> {
    let decoded = html_escape::decode_html_entities(raw);
    let collapsed = decoded.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return None;
    }
    Some(truncate_chars(collapsed, cap))
}

fn truncate_chars(mut s: String, max: usize) -> String {
    if let Some((end, _)) = s.char_indices().nth(max) {
        s.truncate(end);
    }
    s
}

/// Resolve `og:image` against the **final** URL — relative values are common —
/// then judge the address, so we never hand the browser a `file://` or an
/// internal one to load.
///
/// This is the *syntactic* check, not the fetch's resolving one: an image at a
/// hostname that resolves into reserved space still passes. Closing that means
/// resolving attacker-named hosts on their behalf, which is ticket 06's
/// resolver, not a second lookup here.
fn resolve_image(raw: &str, final_url: &Url, reserved: Reserved) -> Option<String> {
    let decoded = html_escape::decode_html_entities(raw);
    let absolute = final_url.join(decoded.trim()).ok()?;
    net::validate_url_with(absolute.as_str(), SCHEMES, reserved)
        .ok()
        .map(|u| u.to_string())
}

/// The `og:site_name` fallback: the host, minus a leading `www.`.
fn host_label(url: &Url) -> Option<String> {
    let host = url.host_str()?;
    let label = host.strip_prefix("www.").unwrap_or(host);
    clean(label, SITE_NAME_CAP)
}

/// Stream the markup through lol_html, collecting the tags we care about. No
/// DOM is built from attacker HTML — handlers fire and the bytes are dropped.
fn scan(html: &str) -> Tags {
    let tags = Rc::new(RefCell::new(Tags::default()));

    let meta = tags.clone();
    let title = tags.clone();
    let settings = Settings::new()
        .append_element_content_handler(element!("meta", move |el: &mut Element| {
            record_meta(&mut meta.borrow_mut(), el);
            Ok(())
        }))
        .append_element_content_handler(text!("title", move |t: &mut TextChunk| {
            let mut tags = title.borrow_mut();
            if !tags.title_closed {
                tags.doc_title.push_str(t.as_str());
                tags.title_closed = t.last_in_text_node();
            }
            Ok(())
        }))
        // Reading, not rewriting. Strict mode exists to refuse rewrites lol_html
        // cannot guarantee; here, keeping what we collected before a malformed
        // tail beats an error that discards a perfectly good title.
        .with_strict(false);

    let mut rewriter = HtmlRewriter::new(settings, |_: &[u8]| {});
    if rewriter.write(html.as_bytes()).is_ok() {
        let _ = rewriter.end();
    } else {
        drop(rewriter);
    }

    let mut tags = Rc::try_unwrap(tags)
        .expect("handlers are dropped with the rewriter")
        .into_inner();
    let title = std::mem::take(&mut tags.doc_title);
    tags.record(DOC_TITLE, Some(title));
    tags
}

/// `property` first, then `name`: `og:` is specified on `property` and
/// `twitter:` on `name`, but plenty of sites swap them, so both are read for
/// both.
fn record_meta(tags: &mut Tags, el: &Element<'_, '_>) {
    let Some(key) = el
        .get_attribute("property")
        .or_else(|| el.get_attribute("name"))
    else {
        return;
    };
    tags.record(
        &key.trim().to_ascii_lowercase(),
        el.get_attribute("content"),
    );
}

/// Bytes to text. `Encoding::decode` sniffs a BOM first, which outranks both
/// of these, and replaces malformed sequences rather than failing.
fn decode(content_type: &str, body: &[u8]) -> String {
    let encoding = charset_of(content_type)
        .or_else(|| charset_from_meta(body))
        .unwrap_or(encoding_rs::UTF_8);
    encoding.decode(body).0.into_owned()
}

/// The `charset` parameter of a `Content-Type`, if it names an encoding we know.
fn charset_of(content_type: &str) -> Option<&'static Encoding> {
    content_type
        .split(';')
        .skip(1)
        .filter_map(|param| param.split_once('='))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("charset"))
        .and_then(|(_, v)| Encoding::for_label(v.trim().trim_matches(['"', '\'']).as_bytes()))
}

/// `<meta charset>` / `<meta http-equiv="content-type">`, for the many pages
/// that declare an encoding in the markup and nowhere else.
fn charset_from_meta(body: &[u8]) -> Option<&'static Encoding> {
    let prescan = String::from_utf8_lossy(&body[..body.len().min(CHARSET_PRESCAN)]);
    let prescan = prescan.to_ascii_lowercase();
    // Scoped to the inside of a `<meta>`, not the first `charset` anywhere: a
    // stylesheet href with `?charset=` in it would otherwise pick the encoding.
    prescan.split("<meta").skip(1).find_map(|tag| {
        let tag = tag.split_once('>').map_or(tag, |(inside, _)| inside);
        let value = tag
            .split_once("charset")?
            .1
            .trim_start()
            .strip_prefix('=')?;
        let label: String = value
            .trim_start()
            .trim_start_matches(['"', '\''])
            .chars()
            .take_while(|c| !matches!(c, '"' | '\'' | ' ' | ';' | '>' | '/'))
            .collect();
        Encoding::for_label(label.as_bytes())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(html: &str) -> Page {
        page_at("https://example.com/article", html)
    }

    fn page_at(url: &str, html: &str) -> Page {
        page_bytes(url, "text/html; charset=utf-8", html.as_bytes().to_vec())
    }

    fn page_bytes(url: &str, content_type: &str, body: Vec<u8>) -> Page {
        Page {
            final_url: Url::parse(url).unwrap(),
            content_type: content_type.into(),
            body,
            truncated: false,
        }
    }

    fn parse(html: &str) -> Preview {
        preview(&page(html), Reserved::Refuse)
    }

    #[test]
    fn a_full_opengraph_page_yields_all_four_fields() {
        let got = parse(
            r#"<html><head>
                <title>ignored, og wins</title>
                <meta property="og:title" content="The Title">
                <meta property="og:description" content="The description.">
                <meta property="og:image" content="https://cdn.example/card.png">
                <meta property="og:site_name" content="Example News">
            </head><body>x</body></html>"#,
        );

        assert_eq!(got.title.as_deref(), Some("The Title"));
        assert_eq!(got.description.as_deref(), Some("The description."));
        assert_eq!(got.image.as_deref(), Some("https://cdn.example/card.png"));
        assert_eq!(got.site_name.as_deref(), Some("Example News"));
        assert_eq!(got.url, "https://example.com/article");
    }

    /// X serves no `og:` tags at all. This is the case the whole precedence
    /// chain exists for.
    #[test]
    fn a_twitter_only_page_yields_title_and_description() {
        let got = parse(
            r#"<html><head>
                <meta name="twitter:card" content="summary">
                <meta name="twitter:title" content="A post on X">
                <meta name="twitter:description" content="what it said">
                <meta name="twitter:image" content="https://pbs.example/img.jpg">
            </head></html>"#,
        );

        assert_eq!(got.title.as_deref(), Some("A post on X"));
        assert_eq!(got.description.as_deref(), Some("what it said"));
        assert_eq!(got.image.as_deref(), Some("https://pbs.example/img.jpg"));
        // No `og:site_name`, so the host stands in.
        assert_eq!(got.site_name.as_deref(), Some("example.com"));
    }

    /// The Guardian, Hacker News and sourcehut: no `og:`, no `twitter:`.
    #[test]
    fn a_title_only_page_still_yields_a_title() {
        let got = parse("<html><head><title>Hacker News</title></head><body>x</body></html>");

        assert_eq!(got.title.as_deref(), Some("Hacker News"));
        assert_eq!(got.description, None);
        assert_eq!(got.image, None);
    }

    /// CNN's real shape: single quotes, attributes on separate lines. A
    /// single-line double-quote regex sees none of this.
    #[test]
    fn multi_line_single_quoted_attributes_parse() {
        let got = parse(
            "<html><head><meta\n  property='og:title'\n  content='CNN — breaking'\n>\
             <meta\n property='og:description'\n content='a story'\n></head>",
        );

        assert_eq!(got.title.as_deref(), Some("CNN — breaking"));
        assert_eq!(got.description.as_deref(), Some("a story"));
    }

    #[test]
    fn a_relative_image_resolves_against_the_final_url() {
        let got = preview(
            &page_at(
                "https://example.com/news/2026/story",
                r#"<head><meta property="og:image" content="/static/card.png"></head>"#,
            ),
            Reserved::Refuse,
        );
        assert_eq!(
            got.image.as_deref(),
            Some("https://example.com/static/card.png")
        );

        let relative = preview(
            &page_at(
                "https://example.com/news/2026/story",
                r#"<head><meta property="og:image" content="card.png"></head>"#,
            ),
            Reserved::Refuse,
        );
        assert_eq!(
            relative.image.as_deref(),
            Some("https://example.com/news/2026/card.png")
        );
    }

    /// The browser loads `og:image` on our say-so, so it clears the same bar
    /// the fetch does.
    #[test]
    fn an_image_at_a_reserved_address_or_foreign_scheme_is_nulled() {
        for hostile in [
            "http://169.254.169.254/latest/meta-data/",
            "http://127.0.0.1:8000/x.png",
            "http://[::1]/x.png",
            "http://10.0.0.5/x.png",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "data:image/png;base64,AAAA",
        ] {
            let got = parse(&format!(
                r#"<head><meta property="og:image" content="{hostile}"></head>"#
            ));
            assert_eq!(got.image, None, "handed the browser {hostile}");
        }
    }

    #[test]
    fn a_page_with_no_usable_markup_is_all_nulls_but_the_host() {
        let got = parse("<html><body><p>just words</p></body></html>");

        assert_eq!(got.title, None);
        assert_eq!(got.description, None);
        assert_eq!(got.image, None);
        assert_eq!(got.site_name.as_deref(), Some("example.com"));
        assert_eq!(got.url, "https://example.com/article");
    }

    #[test]
    fn precedence_runs_og_then_twitter_then_the_document() {
        let all = parse(
            r#"<head><title>doc</title>
               <meta name="twitter:title" content="tw">
               <meta property="og:title" content="og">
               <meta name="description" content="meta-desc">
               <meta name="twitter:description" content="tw-desc"></head>"#,
        );
        assert_eq!(all.title.as_deref(), Some("og"));
        assert_eq!(all.description.as_deref(), Some("tw-desc"));

        // og wins over twitter for the description too, not just the title.
        let og_desc = parse(
            r#"<head><meta property="og:description" content="og-desc">
               <meta name="twitter:description" content="tw-desc"></head>"#,
        );
        assert_eq!(og_desc.description.as_deref(), Some("og-desc"));

        let no_og = parse(
            r#"<head><title>doc</title>
               <meta name="twitter:title" content="tw">
               <meta name="description" content="meta-desc"></head>"#,
        );
        assert_eq!(no_og.title.as_deref(), Some("tw"));
        assert_eq!(no_og.description.as_deref(), Some("meta-desc"));

        let bare = parse(r#"<head><title>doc</title></head>"#);
        assert_eq!(bare.title.as_deref(), Some("doc"));
    }

    #[test]
    fn og_image_url_stands_in_for_og_image() {
        let all_three = parse(
            r#"<head><meta property="og:image" content="https://cdn.example/a.png">
               <meta property="og:image:url" content="https://cdn.example/b.png">
               <meta name="twitter:image" content="https://cdn.example/c.png"></head>"#,
        );
        assert_eq!(
            all_three.image.as_deref(),
            Some("https://cdn.example/a.png")
        );

        let no_og_image = parse(
            r#"<head><meta property="og:image:url" content="https://cdn.example/b.png">
               <meta name="twitter:image" content="https://cdn.example/c.png"></head>"#,
        );
        assert_eq!(
            no_og_image.image.as_deref(),
            Some("https://cdn.example/b.png")
        );
    }

    /// A refused candidate falls through, the way an empty one does. Pinned
    /// because the opposite reading — null the field outright — is defensible
    /// and would be a silent behaviour change.
    #[test]
    fn a_refused_image_falls_through_to_the_next_source() {
        let got = parse(
            r#"<head><meta property="og:image" content="http://169.254.169.254/a.png">
               <meta name="twitter:image" content="https://cdn.example/ok.png"></head>"#,
        );
        assert_eq!(got.image.as_deref(), Some("https://cdn.example/ok.png"));
    }

    /// An empty tag is not a value — it must fall through, not win.
    #[test]
    fn empty_and_whitespace_only_values_fall_through() {
        let got = parse(
            r#"<head><title>real title</title>
               <meta property="og:title" content="">
               <meta name="twitter:title" content="   "></head>"#,
        );
        assert_eq!(got.title.as_deref(), Some("real title"));
    }

    #[test]
    fn entities_are_decoded_and_whitespace_collapsed() {
        let got =
            parse("<head><title>\n   Bells &amp; Whistles\n   &#8212; part 2\n  </title></head>");
        assert_eq!(got.title.as_deref(), Some("Bells & Whistles — part 2"));
    }

    #[test]
    fn fields_are_capped_at_their_documented_lengths() {
        let long = "é".repeat(2000);
        let got = parse(&format!(
            r#"<head><meta property="og:title" content="{long}">
               <meta property="og:description" content="{long}">
               <meta property="og:site_name" content="{long}"></head>"#
        ));

        assert_eq!(got.title.unwrap().chars().count(), TITLE_CAP);
        assert_eq!(got.description.unwrap().chars().count(), DESCRIPTION_CAP);
        assert_eq!(got.site_name.unwrap().chars().count(), SITE_NAME_CAP);
    }

    #[test]
    fn the_site_name_fallback_drops_a_leading_www() {
        let got = preview(
            &page_at(
                "https://www.theguardian.com/x",
                "<head><title>t</title></head>",
            ),
            Reserved::Refuse,
        );
        assert_eq!(got.site_name.as_deref(), Some("theguardian.com"));
    }

    /// A `<title>` in inline SVG further down the page is not the document's.
    #[test]
    fn only_the_first_title_counts() {
        let got =
            parse("<head><title>Real</title></head><body><svg><title>icon</title></svg></body>");
        assert_eq!(got.title.as_deref(), Some("Real"));
    }

    /// The read stops just past `</head>`, and so does this — but never before
    /// the floor, because a few sites close `<head>` early and put their tags
    /// behind it (blockstream.com).
    #[test]
    fn parsing_stops_at_the_head_but_not_before_the_floor() {
        let late = format!(
            "<html><head><link rel=stylesheet href=/a.css></head><body>{}\
             <meta property=\"og:title\" content=\"Late\">",
            " ".repeat(3000)
        );
        assert_eq!(parse(&late).title.as_deref(), Some("Late"));

        // Past the floor, it is not ours to find — and that is the bound: a
        // huge body does not mean a huge parse.
        let past = format!(
            "<html><head></head><body>{}<meta property=\"og:title\" content=\"Never\">{}",
            " ".repeat(HEAD_FLOOR),
            "y".repeat(4 * 1024 * 1024)
        );
        assert_eq!(head_cut(past.as_bytes()), HEAD_FLOOR);
        assert_eq!(parse(&past).title, None);
    }

    #[test]
    fn a_declared_charset_is_honoured() {
        // "Ærø — café" in windows-1252, which as UTF-8 is mojibake.
        let (bytes, _, _) = encoding_rs::WINDOWS_1252.encode("<head><title>café</title></head>");
        let from_header = preview(
            &page_bytes(
                "https://example.com/",
                "text/html; charset=windows-1252",
                bytes.to_vec(),
            ),
            Reserved::Refuse,
        );
        assert_eq!(from_header.title.as_deref(), Some("café"));

        // A `charset=` outside a `<meta>` is not a declaration.
        let (bytes, _, _) = encoding_rs::WINDOWS_1252.encode(
            "<head><link rel=stylesheet href=\"/a.css?charset=windows-1252\">\
             <meta charset=\"utf-8\"><title>caf\u{e9}</title></head>",
        );
        let decoy = preview(
            &page_bytes("https://example.com/", "text/html", bytes.to_vec()),
            Reserved::Refuse,
        );
        assert_ne!(
            decoy.title.as_deref(),
            Some("café"),
            "an href steered the decoding"
        );

        // ...and from the markup when the header says nothing.
        let (bytes, _, _) = encoding_rs::WINDOWS_1252
            .encode("<head><meta charset=\"windows-1252\"><title>café</title></head>");
        let from_meta = preview(
            &page_bytes("https://example.com/", "text/html", bytes.to_vec()),
            Reserved::Refuse,
        );
        assert_eq!(from_meta.title.as_deref(), Some("café"));
    }

    /// The test seam that lets integration tests reach a stub server has to
    /// reach it for images too, or the relative-image test proves nothing.
    #[test]
    fn the_loopback_seam_applies_to_images_and_nothing_wider() {
        let html = r#"<head><meta property="og:image" content="http://127.0.0.1:9/a.png"></head>"#;
        assert!(preview(&page(html), Reserved::AllowLoopback)
            .image
            .is_some());
        assert!(preview(&page(html), Reserved::Refuse).image.is_none());

        let metadata =
            r#"<head><meta property="og:image" content="http://169.254.169.254/a.png"></head>"#;
        assert!(preview(&page(metadata), Reserved::AllowLoopback)
            .image
            .is_none());
    }

    #[test]
    fn the_json_shape_is_camel_case_with_nulls_not_omissions() {
        let json = serde_json::to_value(parse("<head><title>t</title></head>")).unwrap();
        assert_eq!(json["title"], "t");
        assert!(json["description"].is_null());
        assert!(json["image"].is_null());
        assert_eq!(json["siteName"], "example.com");
        assert_eq!(json["url"], "https://example.com/article");
        assert_eq!(json["kind"], "page");
    }
}
