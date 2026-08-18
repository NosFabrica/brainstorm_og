//! Maps the server's tier bucket to the coin's fill and text colour.
//! Mirrors `TRUST_TIER_COLORS` / `DARK_TEXT_TIERS` in Brainstorm-UI. See CONTEXT.md.

use crate::data::Card;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tier {
    High,
    Trusted,
    Neutral,
    Low,
    Unverified,
    /// No score at all — not the same as scoring zero.
    Unrated,
}

impl Tier {
    /// `flagged` collapses to `Unverified` on purpose — see CONTEXT.md.
    pub fn from_card(card: &Card) -> Tier {
        let Some(ov) = card.overview.as_ref() else {
            return Tier::Unrated;
        };
        if card.rank().is_none() {
            return Tier::Unrated;
        }
        match ov.tier.as_deref() {
            Some("high") => Tier::High,
            Some("medium_high") => Tier::Trusted,
            Some("medium") => Tier::Neutral,
            Some("medium_low") | Some("low") => Tier::Low,
            // Unknown buckets, flagged included, fall to the floor.
            Some(_) => Tier::Unverified,
            None => Tier::Unverified,
        }
    }

    /// Stable identifier, used in the card's content hash.
    pub fn key(self) -> &'static str {
        match self {
            Tier::High => "high",
            Tier::Trusted => "trusted",
            Tier::Neutral => "neutral",
            Tier::Low => "low",
            Tier::Unverified => "unverified",
            Tier::Unrated => "unrated",
        }
    }

    // No `label()`: the coin is deliberately label-less.

    /// `Unrated` has no fill — absence of a number is drawn as an outline, not
    /// a second grey a shade off `Unverified`.
    pub fn fill(self) -> Option<&'static str> {
        match self {
            Tier::High => Some("#7237ff"),       // Aurora Purple
            Tier::Trusted => Some("#13d2e5"),    // Aurora Cyan
            Tier::Neutral => Some("#665487"),    // Muted Violet
            Tier::Low => Some("#f59e0b"),        // Amber
            Tier::Unverified => Some("#8c929e"), // Neutral Grey
            Tier::Unrated => None,
        }
    }

    /// From `DARK_TEXT_TIERS`, which derives these from measured contrast.
    /// White on Aurora Cyan is 1.85:1 — do not simplify to white everywhere.
    pub fn text_color(self) -> &'static str {
        match self {
            // white on purple 5.67:1, on violet 6.60:1
            Tier::High | Tier::Neutral => "#ffffff",
            // dark on cyan 7.93:1, on amber 6.81:1, on grey 4.68:1
            Tier::Trusted | Tier::Low | Tier::Unverified => "#1e293b",
            Tier::Unrated => "#94a3b8",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{Overview, ProfileMeta};

    const ALL: [Tier; 6] = [
        Tier::High,
        Tier::Trusted,
        Tier::Neutral,
        Tier::Low,
        Tier::Unverified,
        Tier::Unrated,
    ];

    fn card_with(tier: Option<&str>, influence: Option<f64>) -> Card {
        Card {
            hex: "a".repeat(64),
            meta: ProfileMeta::default(),
            overview: Some(Overview {
                influence,
                followers: 0,
                following: 0,
                tier: tier.map(str::to_string),
            }),
            provisional: false,
        }
    }

    #[test]
    fn maps_every_server_bucket() {
        assert_eq!(card_with(Some("high"), Some(0.9)).tier(), Tier::High);
        assert_eq!(
            card_with(Some("medium_high"), Some(0.3)).tier(),
            Tier::Trusted
        );
        assert_eq!(card_with(Some("medium"), Some(0.1)).tier(), Tier::Neutral);
        assert_eq!(card_with(Some("medium_low"), Some(0.05)).tier(), Tier::Low);
        assert_eq!(card_with(Some("low"), Some(0.03)).tier(), Tier::Low);
    }

    #[test]
    fn flagged_never_renders_red() {
        let flagged = card_with(
            Some("low_and_reported_by_2_or_more_trusted_pubkeys"),
            Some(0.02),
        );
        assert_eq!(flagged.tier(), Tier::Unverified);
        assert_eq!(flagged.tier().fill(), Some("#8c929e"));
        // The red in TRUST_TIER_COLORS must not be reachable from any input.
        for t in ALL {
            assert_ne!(t.fill(), Some("#ef4444"));
        }
    }

    /// `Unrated` must be an outline, not a second grey fill.
    #[test]
    fn unrated_has_no_fill() {
        assert_eq!(Tier::Unrated.fill(), None);
        for t in ALL.iter().filter(|t| **t != Tier::Unrated) {
            assert!(t.fill().is_some(), "{t:?} must have a fill");
        }
    }

    /// Guards the contrast pairs against being "simplified" to white.
    #[test]
    fn text_colour_follows_measured_contrast() {
        assert_eq!(Tier::High.text_color(), "#ffffff");
        assert_eq!(Tier::Neutral.text_color(), "#ffffff");
        assert_eq!(Tier::Trusted.text_color(), "#1e293b");
        assert_eq!(Tier::Low.text_color(), "#1e293b");
        assert_eq!(Tier::Unverified.text_color(), "#1e293b");
    }

    #[test]
    fn unknown_buckets_fall_to_the_floor() {
        assert_eq!(
            card_with(Some("something_new"), Some(0.5)).tier(),
            Tier::Unverified
        );
        assert_eq!(card_with(None, Some(0.5)).tier(), Tier::Unverified);
    }

    #[test]
    fn no_score_is_unrated_not_zero() {
        assert_eq!(card_with(Some("high"), None).tier(), Tier::Unrated);
        let no_overview = Card {
            hex: "a".repeat(64),
            meta: ProfileMeta::default(),
            overview: None,
            provisional: true,
        };
        assert_eq!(no_overview.tier(), Tier::Unrated);
    }

    #[test]
    fn keys_are_distinct() {
        let mut keys: Vec<_> = ALL.iter().map(|t| t.key()).collect();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), ALL.len());
    }
}
