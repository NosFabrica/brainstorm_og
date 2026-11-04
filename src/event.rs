//! A nostr event as read from the search relay, and the skip rules every card shares.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Event {
    pub id: String,
    pub pubkey: String,
    pub kind: u32,
    #[serde(default)]
    pub created_at: u64,
    #[serde(default)]
    pub tags: Vec<Vec<String>>,
    #[serde(default)]
    pub content: String,
}

impl Event {
    /// Tags named `name`, as their remaining values.
    pub fn tags_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a [String]> + 'a {
        self.tags
            .iter()
            .filter(move |t| t.first().map(String::as_str) == Some(name))
            .map(|t| &t[1..])
    }

    /// First non-blank value of tag `name`.
    pub fn tag<'a>(&'a self, name: &'a str) -> Option<&'a str> {
        self.tags_named(name)
            .filter_map(|v| v.first())
            .map(|v| v.trim())
            .find(|v| !v.is_empty())
    }

    /// `kind:pubkey:d` for addressable kinds.
    pub fn address(&self) -> Option<String> {
        (30000..40000).contains(&self.kind).then(|| {
            format!(
                "{}:{}:{}",
                self.kind,
                self.pubkey,
                self.tag("d").unwrap_or("")
            )
        })
    }

    /// NIP-36 `content-warning`, or a `t` tag `nsfw`.
    pub fn is_nsfw(&self) -> bool {
        self.tags_named("content-warning").next().is_some()
            || self.tags_named("t").any(|v| {
                v.first()
                    .is_some_and(|t| t.trim().eq_ignore_ascii_case("nsfw"))
            })
    }
}

/// Test fixture: an event of `kind` with `tags`, empty content.
#[cfg(test)]
pub fn ev(kind: u32, tags: &[&[&str]]) -> Event {
    Event {
        id: "a".repeat(64),
        pubkey: "3bf0c63fcb93463407af97a5e5ee64fa883d107ef9e558472c4eb9aaaefa459d".into(),
        kind,
        created_at: 0,
        tags: tags
            .iter()
            .map(|t| t.iter().map(|s| s.to_string()).collect())
            .collect(),
        content: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(tags: &[&[&str]]) -> Event {
        super::ev(1, tags)
    }

    #[test]
    fn nsfw_is_a_content_warning_or_an_nsfw_hashtag() {
        assert!(ev(&[&["content-warning"]]).is_nsfw());
        assert!(ev(&[&["content-warning", "gore"]]).is_nsfw());
        assert!(ev(&[&["t", "NSFW"]]).is_nsfw());
        assert!(!ev(&[&["t", "nostr"], &["L", "content-warning"]]).is_nsfw());
        assert!(!ev(&[]).is_nsfw());
    }

    #[test]
    fn tag_skips_blank_values() {
        assert_eq!(
            ev(&[&["title", " "], &["title", "Hi"]]).tag("title"),
            Some("Hi")
        );
        assert_eq!(ev(&[&["title"]]).tag("title"), None);
    }
}
