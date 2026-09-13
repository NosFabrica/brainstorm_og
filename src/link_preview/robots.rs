//! robots.txt, per RFC 9309.
//!
//! An earlier version of this service did not consult robots.txt at all, on
//! Slack's documented reasoning: a preview fetched because a human opened a
//! note is not crawling, and robots exclusion addresses crawlers. That argument
//! still holds on its own terms — we do not traverse links, do not schedule
//! ourselves, and keep nothing but four fields.
//!
//! It was dropped anyway, for two reasons a future reader should have:
//!
//! * Cloudflare's Verified Bots programme turned out to be a *systemic* unlock
//!   rather than a per-site one — Medium answers 403 to every user-agent we
//!   tried, a real browser included, and only verification addresses that whole
//!   class. Compliance is its entry ticket.
//! * The choice is one-way. Complying can be relaxed later; declining cannot be
//!   undone once a site operator notices, and this product is sold on being
//!   trustworthy.
//!
//! Measured cost: nothing across 28 Nostr/Bitcoin URLs; in the mainstream set,
//! X, Instagram, Facebook, LinkedIn and lobste.rs. Realistically X.
//!
//! `Crawl-delay` is deliberately ignored. It is not in RFC 9309, and the values
//! real sites publish — arxiv 15s, Hacker News 30s — are unworkable for a fetch
//! a reader is waiting on. The `/bot` page says so rather than staying quiet.

use std::sync::Arc;
use std::time::Duration;

use axum::http::header;
use moka::future::Cache;
use url::Url;

use crate::net;
use crate::state::AppState;

/// Our product token. RFC 9309 §2.2.1 allows only letters, `_` and `-`, and
/// requires it to be a substring of the User-Agent we send — it is.
pub const TOKEN: &str = "brainstormbot";

/// A robots.txt we could not read disallows everything, per RFC 9309 §2.3.1.4
/// — but for minutes, not the full day. Taking this branch is the difference
/// between complying and saying we comply; capping it short is what stops one
/// upstream blip from hiding a domain until tomorrow.
pub const UNREADABLE_TTL: Duration = Duration::from_secs(300);

/// RFC 9309 §2.5 asks for at least 500 KiB to be parsed.
const MAX_ROBOTS_BYTES: usize = 512 * 1024;

/// Same cap as a page fetch. A robots.txt behind four redirects is already odd.
const MAX_HOPS: u8 = 3;

pub type RobotsCache = Cache<String, Arc<Rules>>;

/// The rules from the group that applies to us, in file order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rules {
    /// `(allow, pattern)`. Longest pattern wins; allow breaks a tie.
    rules: Vec<(bool, String)>,
    /// Came from a failure rather than a file, so it expires sooner.
    pub unreadable: bool,
}

impl Rules {
    pub fn allow_all() -> Self {
        Self::default()
    }

    /// Everything refused, and marked so the cache expires it in minutes.
    pub fn unreadable() -> Self {
        Self {
            rules: vec![(false, "/".to_string())],
            unreadable: true,
        }
    }

    /// RFC 9309 §2.2.2: the most specific match wins, measured by pattern
    /// length, and `Allow` wins a tie. No matching rule means allowed.
    pub fn allows(&self, path: &str) -> bool {
        let mut best: Option<(usize, bool)> = None;
        for (allow, pattern) in &self.rules {
            if !matches(pattern, path) {
                continue;
            }
            let len = pattern.len();
            best = Some(match best {
                Some((blen, ballow)) if blen > len => (blen, ballow),
                Some((blen, ballow)) if blen == len => (blen, ballow || *allow),
                _ => (len, *allow),
            });
        }
        best.is_none_or(|(_, allow)| allow)
    }
}

/// Glob per RFC 9309 §2.2.3: `*` is any run of characters, a trailing `$`
/// anchors the end. Indexing is done with `get`, never slicing: a path may
/// carry multi-byte characters and a byte offset can land mid-character.
fn matches(pattern: &str, path: &str) -> bool {
    let (pat, anchored) = pattern
        .strip_suffix('$')
        .map_or((pattern, false), |p| (p, true));
    if pat.is_empty() {
        // `Disallow:` with nothing after it is not a rule; it is the absence
        // of one. Handled at parse time too, belt and braces.
        return false;
    }

    let segments: Vec<&str> = pat.split('*').collect();
    let last = segments.len() - 1;
    let mut pos = 0usize;

    for (i, segment) in segments.iter().enumerate() {
        if segment.is_empty() {
            // A trailing `*` (or `*$`) leaves the rest free.
            if i == last {
                return true;
            }
            continue;
        }
        let Some(rest) = path.get(pos..) else {
            return false;
        };
        if i == 0 {
            if !rest.starts_with(segment) {
                return false;
            }
            pos += segment.len();
        } else {
            match rest.find(segment) {
                Some(offset) => pos += offset + segment.len(),
                None => return false,
            }
        }
    }

    // `$` only means something when the pattern did not end in a wildcard.
    if anchored {
        return pos == path.len();
    }
    true
}

/// RFC 9309 §2.2.1 group selection: a group naming our token wins outright,
/// and `*` applies only when no group names us. A group that names us but
/// carries no rules still wins — it means "you, specifically, may go anywhere".
pub fn parse(text: &str, token: &str) -> Rules {
    let token = token.to_ascii_lowercase();
    let mut specific: Vec<(bool, String)> = Vec::new();
    let mut wildcard: Vec<(bool, String)> = Vec::new();
    let mut names_us = false;

    let mut agents: Vec<String> = Vec::new();
    // A rule closes the run of user-agent lines above it; the next user-agent
    // line starts a new group.
    let mut seen_rule = false;

    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();

        match key.as_str() {
            "user-agent" => {
                if seen_rule {
                    agents.clear();
                    seen_rule = false;
                }
                let agent = value.to_ascii_lowercase();
                if agent == token {
                    names_us = true;
                }
                agents.push(agent);
            }
            "allow" | "disallow" => {
                seen_rule = true;
                if value.is_empty() {
                    continue;
                }
                let allow = key == "allow";
                for agent in &agents {
                    if *agent == token {
                        specific.push((allow, value.to_string()));
                    } else if agent == "*" {
                        wildcard.push((allow, value.to_string()));
                    }
                }
            }
            // Sitemap, Crawl-delay, and anything unrecognised.
            _ => {}
        }
    }

    Rules {
        rules: if names_us { specific } else { wildcard },
        unreadable: false,
    }
}

/// The origin's rules, fetched once and then cached. Concurrent callers for the
/// same origin collapse onto one request.
pub async fn rules_for(st: &AppState, url: &Url) -> Arc<Rules> {
    let origin = url.origin().ascii_serialization();
    // An opaque origin serialises to "null" and has no robots.txt to read.
    if origin == "null" {
        return Arc::new(Rules::allow_all());
    }
    let loader = st.clone();
    let key = origin.clone();
    st.robots_cache
        .get_with(origin, async move { Arc::new(fetch(&loader, &key).await) })
        .await
}

/// What robots.txt says about this URL.
///
/// `Unreadable` is kept apart from `Disallowed` on purpose, and the difference
/// is about *caching*, not politeness. A `Disallow` is a stable fact about that
/// URL: answer it once, cache it for a day, move on. A robots.txt we could not
/// read is a fact about right now — a host that is down would otherwise be
/// cached as "no preview" for a day, and would still look that way long after
/// it came back. Both stop the page fetch; only one of them is an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Allowed,
    Disallowed,
    Unreadable,
}

/// The query counts: RFC 9309 matches against the path *and* query, and sites
/// do disallow things like `/search?q=`.
pub async fn verdict(st: &AppState, url: &Url) -> Verdict {
    let rules = rules_for(st, url).await;
    if rules.unreadable {
        return Verdict::Unreadable;
    }
    let mut path = url.path().to_string();
    if let Some(query) = url.query() {
        path.push('?');
        path.push_str(query);
    }
    if rules.allows(&path) {
        Verdict::Allowed
    } else {
        Verdict::Disallowed
    }
}

async fn fetch(st: &AppState, origin: &str) -> Rules {
    let Ok(url) = Url::parse(&format!("{origin}/robots.txt")) else {
        return Rules::unreadable();
    };
    match read(st, url).await {
        // A file we understood.
        Ok(Some(text)) => parse(&text, TOKEN),
        // 4xx is "no robots.txt here", which RFC 9309 §2.3.1.3 reads as full
        // allowance — the common case for most of the web.
        Ok(None) => Rules::allow_all(),
        // 5xx, a refused address, a timeout: §2.3.1.4 says assume disallow.
        Err(()) => Rules::unreadable(),
    }
}

/// Redirects are followed by hand for the same reason the page fetch does it:
/// the client cannot re-check an address it has already dialled.
async fn read(st: &AppState, url: Url) -> Result<Option<String>, ()> {
    let timeout = Duration::from_secs(st.config.robots_timeout_secs);
    let policy = st.config.reserved_policy();
    let mut target = net::validate_and_resolve_with(url.as_str(), super::SCHEMES, policy)
        .await
        .map_err(|_| ())?;

    let mut hops = 0u8;
    let resp = loop {
        let resp = st
            .preview_http
            .get(target.clone())
            .header(header::ACCEPT, "text/plain")
            .timeout(timeout)
            .send()
            .await
            .map_err(|_| ())?;

        if !resp.status().is_redirection() {
            break resp;
        }
        hops += 1;
        if hops > MAX_HOPS {
            return Err(());
        }
        let location = resp
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .ok_or(())?;
        let next = target.join(location).map_err(|_| ())?;
        target = net::validate_and_resolve_with(next.as_str(), super::SCHEMES, policy)
            .await
            .map_err(|_| ())?;
    };

    let status = resp.status();
    if status.is_client_error() {
        return Ok(None);
    }
    if !status.is_success() {
        return Err(());
    }

    // Capped while streaming, like the page body: Content-Length is a claim.
    use futures_util::StreamExt as _;
    let mut body: Vec<u8> = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| ())?;
        body.extend_from_slice(&chunk);
        if body.len() >= MAX_ROBOTS_BYTES {
            body.truncate(MAX_ROBOTS_BYTES);
            break;
        }
    }
    Ok(Some(String::from_utf8_lossy(&body).into_owned()))
}

/// A file we read lasts a day; one we could not read, minutes.
pub struct RobotsExpiry {
    pub read: Duration,
    pub unreadable: Duration,
}

impl moka::Expiry<String, Arc<Rules>> for RobotsExpiry {
    fn expire_after_create(
        &self,
        _key: &String,
        value: &Arc<Rules>,
        _now: std::time::Instant,
    ) -> Option<Duration> {
        Some(if value.unreadable {
            self.unreadable
        } else {
            self.read
        })
    }
}

pub fn store(capacity: u64, ttl: Duration) -> RobotsCache {
    Cache::builder()
        .max_capacity(capacity)
        .expire_after(RobotsExpiry {
            read: ttl,
            unreadable: UNREADABLE_TTL,
        })
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(text: &str) -> Rules {
        parse(text, TOKEN)
    }

    #[test]
    fn no_rules_means_everything_is_allowed() {
        assert!(rules("").allows("/anything"));
        assert!(Rules::allow_all().allows("/anything"));
    }

    #[test]
    fn an_empty_disallow_is_the_absence_of_a_rule_not_a_ban() {
        // "Disallow:" with nothing after it is how a site says "go anywhere".
        // Read as a pattern it would match every path and ban the site.
        assert!(rules("User-agent: *\nDisallow:\n").allows("/anything"));
    }

    #[test]
    fn the_longest_matching_pattern_wins() {
        // RFC 9309 §2.2.2. Order in the file is irrelevant, which is why this
        // states them in the order that would give the wrong answer naively.
        let r = rules("User-agent: *\nDisallow: /a/\nAllow: /a/b/\n");
        assert!(!r.allows("/a/x"));
        assert!(r.allows("/a/b/x"), "the more specific Allow governs");
    }

    #[test]
    fn allow_wins_a_tie() {
        let r = rules("User-agent: *\nDisallow: /p\nAllow: /p\n");
        assert!(r.allows("/p"));
    }

    #[test]
    fn a_star_matches_any_run_and_a_dollar_anchors_the_end() {
        let r = rules("User-agent: *\nDisallow: /*.pdf$\n");
        assert!(!r.allows("/docs/manual.pdf"));
        assert!(
            r.allows("/docs/manual.pdf.html"),
            "$ anchors, so a suffix past .pdf is a different path"
        );
        assert!(r.allows("/docs/manual.txt"));
    }

    #[test]
    fn a_trailing_star_leaves_the_rest_free() {
        let r = rules("User-agent: *\nDisallow: /search*\n");
        assert!(!r.allows("/search"));
        assert!(!r.allows("/search?q=x"));
        assert!(r.allows("/other"));
    }

    #[test]
    fn the_query_is_part_of_what_is_matched() {
        // Sites really do disallow /search?q= while allowing /search.
        let r = rules("User-agent: *\nDisallow: /search?q=\n");
        assert!(!r.allows("/search?q=nostr"));
        assert!(r.allows("/search"));
    }

    #[test]
    fn a_group_naming_us_wins_outright_even_when_it_carries_no_rules() {
        // "You specifically may go anywhere", while everyone else is banned.
        // Falling through to `*` here would ban us against the site's wishes.
        let r = rules("User-agent: *\nDisallow: /\n\nUser-agent: BrainstormBot\nDisallow:\n");
        assert!(r.allows("/anything"));
    }

    #[test]
    fn agent_names_are_matched_without_case() {
        let r = rules("User-agent: BRAINSTORMBOT\nDisallow: /x\n");
        assert!(!r.allows("/x"));
    }

    #[test]
    fn comments_and_unknown_directives_are_skipped() {
        let r = rules(
            "# a comment\nUser-agent: *   # trailing\nCrawl-delay: 30\nSitemap: /s.xml\nDisallow: /x\n",
        );
        assert!(!r.allows("/x"));
        assert!(r.allows("/y"), "Crawl-delay and Sitemap are not rules");
    }

    #[test]
    fn a_user_agent_line_after_a_rule_starts_a_new_group() {
        // Otherwise the second group's agents pile onto the first, and a ban
        // meant for someone else lands on us.
        let r = rules("User-agent: OtherBot\nDisallow: /\n\nUser-agent: *\nAllow: /\n");
        assert!(r.allows("/anything"));
    }

    #[test]
    fn an_unreadable_file_refuses_everything_and_says_so() {
        let r = Rules::unreadable();
        assert!(!r.allows("/anything"));
        assert!(
            r.unreadable,
            "the flag is what keeps this out of the day-long cache"
        );
    }

    #[test]
    fn a_multi_byte_path_does_not_panic_the_matcher() {
        // Byte offsets walk the path; a naive slice would split a character.
        let r = rules("User-agent: *\nDisallow: /a*z\n");
        assert!(r.allows("/日本語/ページ"));
    }
}
