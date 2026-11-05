use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::Duration;

use crate::nip19::Pointer;
use crate::state::AppState;

/// Kind-0 profile metadata, from the in-cluster relay. See CONTEXT.md.
#[derive(Clone, Default, Debug)]
pub struct ProfileMeta {
    pub name: Option<String>,
    pub display_name: Option<String>,
    pub picture: Option<String>,
    pub about: Option<String>,
    pub nip05: Option<String>,
}

/// Empty and whitespace-only both mean absent — clients write `""` about as
/// often as they omit the key.
fn field(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|x| !x.trim().is_empty())
}

impl ProfileMeta {
    fn from_doc(doc: &Value) -> Self {
        ProfileMeta {
            name: field(doc, "name"),
            // Both spellings are in the wild; NIP-24 says `display_name`.
            display_name: field(doc, "display_name").or_else(|| field(doc, "displayName")),
            picture: field(doc, "picture"),
            about: field(doc, "about"),
            nip05: field(doc, "nip05"),
        }
    }

    pub fn from_kind0_content(content: &str) -> Result<Self> {
        Ok(Self::from_doc(&serde_json::from_str::<Value>(content)?))
    }

    pub fn best_name(&self) -> Option<String> {
        self.display_name.clone().or_else(|| self.name.clone())
    }

    fn is_useful(&self) -> bool {
        self.best_name().is_some() || self.picture.is_some() || self.about.is_some()
    }
}

/// Trust stats from brainstorm-server.
#[derive(Clone, Default, Debug)]
pub struct Overview {
    pub followers: i64,
    pub following: i64,
}

#[derive(Deserialize)]
struct OverviewEnvelope {
    data: OverviewData,
}
#[derive(Deserialize)]
struct OverviewData {
    counts: OverviewCounts,
}
#[derive(Deserialize)]
struct OverviewCounts {
    followed_by: i64,
    following: i64,
}

/// Everything needed to render a card / meta tags for one pubkey.
#[derive(Clone, Debug)]
pub struct Card {
    /// Hex pubkey — the identity fallback when there is no name.
    pub hex: String,
    pub meta: ProfileMeta,
    pub overview: Option<Overview>,
    /// No kind-0 found. Expires fast so the card self-corrects.
    pub provisional: bool,
}

impl Card {
    /// Falls back to a truncated npub, matching `SharePage.tsx:413`.
    pub fn display_name(&self) -> String {
        self.meta.best_name().unwrap_or_else(|| {
            crate::nip19::npub_from_hex(&self.hex)
                .map(|npub| format!("{}…", &npub[..npub.len().min(12)]))
                .unwrap_or_else(|| format!("{}…", &self.hex[..self.hex.len().min(12)]))
        })
    }

    /// Content fingerprint for the `?v=` on `og:image` — what makes a changed
    /// name or avatar reach a crawler.
    ///
    /// Covers exactly what the card renders and nothing else. Score and
    /// relationship counts are excluded: they move constantly, and hashing
    /// them would mint a new image URL for an image identical to the last one.
    /// `about` is out for the same reason — it only feeds the meta
    /// description, which is served with a short max-age anyway.
    pub fn version(&self, render_epoch: &str) -> String {
        let mut h = DefaultHasher::new();
        self.display_name().hash(&mut h);
        self.meta.picture.hash(&mut h);
        self.meta.nip05.hash(&mut h);
        // Inputs alone cannot express "the renderer changed", so a redraw would
        // otherwise never reach anything holding an immutable URL.
        render_epoch.hash(&mut h);
        format!("{:x}", h.finish())
    }
}

/// Per-entry TTL so provisional cards die young.
pub struct CardExpiry {
    pub provisional: Duration,
    pub normal: Duration,
}

impl moka::Expiry<String, Card> for CardExpiry {
    fn expire_after_create(
        &self,
        _key: &String,
        value: &Card,
        _now: std::time::Instant,
    ) -> Option<Duration> {
        Some(if value.provisional {
            self.provisional
        } else {
            self.normal
        })
    }
}

/// Assemble (and cache) a profile card. `get_with` coalesces concurrent loads
/// of the same key, so a crawler burst on one profile does one fetch.
pub async fn get_card(state: &AppState, pointer: &Pointer) -> Card {
    let hex = pointer.hex.clone();
    let st = state.clone();

    state
        .card_cache
        .get_with(hex.clone(), async move {
            let deadline = Duration::from_secs(st.config.request_deadline_secs);
            // Whole-request budget: partial data beats a crawler timeout.
            let fetched = tokio::time::timeout(deadline, async {
                tokio::join!(fetch_overview(&st, &hex), fetch_meta(&st, &hex))
            })
            .await;

            let (overview, meta) = match fetched {
                Ok(pair) => pair,
                Err(_) => {
                    tracing::warn!("card assembly for {hex} hit the {deadline:?} deadline");
                    (None, None)
                }
            };

            Card {
                hex,
                provisional: meta.is_none(),
                meta: meta.unwrap_or_default(),
                overview,
            }
        })
        .await
}

async fn fetch_overview(state: &AppState, hex: &str) -> Option<Overview> {
    let url = format!("{}/user/{}/overview", state.config.api_base_url, hex);
    let resp = state
        .http
        .get(&url)
        .timeout(Duration::from_secs(state.config.fetch_timeout_secs))
        .send()
        .await
        .map_err(|e| tracing::warn!("overview request failed for {hex}: {e}"))
        .ok()?;

    if !resp.status().is_success() {
        tracing::warn!("overview {hex} -> HTTP {}", resp.status());
        return None;
    }

    let env: OverviewEnvelope = resp
        .json()
        .await
        .map_err(|e| tracing::warn!("overview decode failed for {hex}: {e}"))
        .ok()?;

    Some(Overview {
        followers: env.data.counts.followed_by,
        following: env.data.counts.following,
    })
}

async fn fetch_meta(state: &AppState, hex: &str) -> Option<ProfileMeta> {
    let relay = &state.config.local_relay_url;
    let meta = crate::relay::fetch_kind0(relay, hex, state.config.fetch_timeout_secs)
        .await
        .map_err(|e| tracing::warn!("kind-0 lookup failed for {hex} on {relay}: {e}"))
        .ok()?;
    meta.is_useful().then_some(meta)
}

#[derive(Deserialize)]
struct ShortUrlEnvelope {
    data: ShortUrlData,
}
#[derive(Deserialize)]
struct ShortUrlData {
    kind: String,
    key: String,
}

/// What a short code names, as far as og cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShortTarget {
    Profile(String),
    Entity(crate::nip19::Entity),
    /// A hashtag or tag code: no card.
    Other,
}

/// Resolve a short code, or `None` if it doesn't resolve.
///
/// `None` covers "no such code", "the API is unreachable" and "the API
/// answered with a key that doesn't fit its kind". The caller must not invent
/// a card in any of them: a preview claiming something that doesn't exist is
/// worse than no preview.
///
/// Cached without a TTL — a code's mapping is immutable once minted. The
/// accepted consequence is that a code deleted upstream stays resolvable here
/// until capacity evicts it. Failures are not cached, so a code minted after a
/// crawler first tried it still resolves.
pub async fn resolve_short_code(state: &AppState, code: &str) -> Option<ShortTarget> {
    let key = normalize_short_code(code);
    if key.is_empty() {
        return None;
    }

    // try_get_with, not get + insert: crawlers arrive in bursts for one link,
    // and this coalesces them onto a single in-flight lookup — the same
    // stampede protection get_card relies on. Errors are not cached.
    state
        .short_code_cache
        .try_get_with(key.clone(), fetch_short_code(state, &key))
        .await
        .ok()
}

/// Match the server's folding (`normalize_short_code` in `shorturl_service.py`)
/// so one mapping is one cache entry: Crockford excludes I, L, O and U, and
/// resolution folds the first three back to digits.
fn normalize_short_code(code: &str) -> String {
    code.trim()
        .to_uppercase()
        .replace(['I', 'L'], "1")
        .replace('O', "0")
}

async fn fetch_short_code(state: &AppState, key: &str) -> Result<ShortTarget> {
    let url = format!("{}/shorturl/{}", state.config.api_base_url, key);
    let resp = state
        .http
        .get(&url)
        .timeout(Duration::from_secs(state.config.fetch_timeout_secs))
        .send()
        .await?;

    // A 404 is routine — someone pasted a dead link. A 5xx is not.
    if !resp.status().is_success() {
        if resp.status().is_server_error() {
            tracing::warn!("short code {key} -> HTTP {}", resp.status());
        } else {
            tracing::debug!("short code {key} -> HTTP {}", resp.status());
        }
        anyhow::bail!("short code {key} did not resolve");
    }

    let env: ShortUrlEnvelope = resp.json().await?;
    let target = short_target(&env.data.kind, &env.data.key);
    if target.is_none() {
        tracing::warn!(
            "short code {key} resolved to a malformed {} key",
            env.data.kind
        );
    }
    target.ok_or_else(|| anyhow::anyhow!("short code {key} resolved to a malformed key"))
}

/// The server's stored key forms: hex pubkey, hex event id, `kind:pubkey:d`.
fn short_target(kind: &str, key: &str) -> Option<ShortTarget> {
    use crate::nip19::Entity;
    let key = key.trim();
    match kind {
        "profile" => crate::nip19::is_hex64(key).then(|| ShortTarget::Profile(key.to_lowercase())),
        "event" => crate::nip19::is_hex64(key).then(|| {
            ShortTarget::Entity(Entity::Event {
                id: key.to_lowercase(),
            })
        }),
        "address" => {
            let mut parts = key.splitn(3, ':');
            let kind = parts.next()?.parse().ok()?;
            let pubkey = parts
                .next()
                .filter(|p| crate::nip19::is_hex64(p))?
                .to_lowercase();
            let d = parts.next()?.to_string();
            Some(ShortTarget::Entity(Entity::Address { kind, pubkey, d }))
        }
        _ => Some(ShortTarget::Other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPOCH: &str = "test-epoch";

    fn card(name: Option<&str>) -> Card {
        Card {
            hex: "a".repeat(64),
            meta: ProfileMeta {
                display_name: name.map(str::to_string),
                ..Default::default()
            },
            overview: Some(Overview {
                followers: 0,
                following: 0,
            }),
            provisional: false,
        }
    }

    fn doc(json: &str) -> ProfileMeta {
        ProfileMeta::from_doc(&serde_json::from_str::<Value>(json).unwrap())
    }

    #[test]
    fn parses_vespa_doc_shapes() {
        assert_eq!(
            doc(r#"{"name":"a","display_name":"B"}"#)
                .best_name()
                .as_deref(),
            Some("B")
        );

        // Vespa clears absent fields to "" rather than null, so empty and
        // whitespace-only both have to mean absent.
        let m = doc(r#"{"name":"","picture":"x"}"#);
        assert!(m.name.is_none());
        assert_eq!(m.picture.as_deref(), Some("x"));
        assert!(doc(r#"{"name":"  "}"#).name.is_none());

        // Non-string values must not panic or coerce.
        let m = doc(r#"{"name":42,"picture":null}"#);
        assert!(m.name.is_none() && m.picture.is_none());

        // Shapes that are valid JSON but not an object.
        assert!(doc("{}").best_name().is_none());
        assert!(doc("[]").best_name().is_none());
        assert!(doc("null").best_name().is_none());
    }

    #[test]
    fn falls_back_to_truncated_npub() {
        let c = card(None);
        let shown = c.display_name();
        // Never blank, and recognisably an npub rather than raw hex.
        assert!(shown.starts_with("npub1"), "got {shown}");
        assert!(shown.ends_with('…'));
    }

    #[test]
    fn version_tracks_rendered_content() {
        let a = card(Some("Alice"));
        assert_eq!(a.version(EPOCH), card(Some("Alice")).version(EPOCH));

        // A changed name must mint a new image URL — it is drawn.
        assert_ne!(a.version(EPOCH), card(Some("Alice B")).version(EPOCH));

        // Changed counts must NOT: they aren't drawn and move constantly, so a
        // new URL would only force a re-fetch of a byte-identical image.
        let mut moved = card(Some("Alice"));
        moved.overview = Some(Overview {
            followers: 99,
            following: 7,
        });
        assert_eq!(a.version(EPOCH), moved.version(EPOCH));

        // `about` only feeds the short-lived meta description.
        let mut b = card(Some("Alice"));
        b.meta.about = Some("changed".into());
        assert_eq!(a.version(EPOCH), b.version(EPOCH));

        // A new render epoch must move every hash, so a redraw reaches
        // anything holding an immutable URL.
        assert_ne!(a.version(EPOCH), a.version("other-epoch"));

        // ...but the avatar and nip05 are drawn.
        let mut c = card(Some("Alice"));
        c.meta.picture = Some("https://example/new.png".into());
        assert_ne!(a.version(EPOCH), c.version(EPOCH));
        let mut d = card(Some("Alice"));
        d.meta.nip05 = Some("alice@example.com".into());
        assert_ne!(a.version(EPOCH), d.version(EPOCH));
    }
}
