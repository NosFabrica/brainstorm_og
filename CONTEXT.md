# brainstorm-og

Renders the share card and crawler-visible meta tags for `/p/{id}`. Replaces
`Brainstorm-UI`'s `useShareMeta.ts`, a client-side stopgap invisible to crawlers.

## Language

**Share card**: the rendered 1200×630 image. _Avoid_: OG card, link preview,
preview image. "OG" survives only in protocol names (`og:image`, `/og/`).

**Unfurl**: what a third-party crawler does with a share link. _Avoid_: preview,
embed.

**Rank**: the published 0–100 integer, `round(Influence × 100)`. Same meaning as
the server glossary. Never drawn with a label — see below.

**Provisional card**: one assembled without a kind-0. Short TTL so it
self-corrects rather than pinning a name-less card.

## Decisions

### The card is the house view, and says so only in alt text

`/overview` is called anonymously, so the server resolves the default (house)
observer. The coin is therefore always the *global* point of view and never uses
the personalized ring. The image alt text mirrors the coin's own aria-label
("…verification score 98 out of 100, global view"); the card itself carries no
label, because `VerificationCoin` is deliberately label-less and the words
"Verification Score" live only in the in-app explainer.

### `flagged` is never surfaced

`UserOverviewData` carries `flagged_by_observer`/`flagged_count` and there is a
red flagged band. The card clamps to the unverified floor instead. This card is
auto-generated for any pubkey anyone pastes, on our domain, with no appeal path:
a grey "no signal on this person" is fair to publish that way, a red "trusted
people reported them" is not. Flagging stays in-app, where there is context and
recourse.

### Tier comes from the server; only its presentation is duplicated

The bands live in `app/core/tier_thresholds.py` and are the only thing that
knows the observer's verified line. We consume the `tier` bucket and map it to
fill/text colour locally. The duplication is asymmetric on purpose: a stale
colour is visible and harmless, a stale threshold would be a wrong score. Text
colours come from `DARK_TEXT_TIERS`, which derives them from measured WCAG
contrast — white on Aurora Cyan is 1.85:1, so "just use white" is wrong.

### Metadata comes from the relay, not Vespa

Vespa is downstream of strfry via the redis ingest queue, so the relay's
coverage is a strict superset and cannot lag ingest. One in-cluster relay, from
config — not the original six-relay public fan-out, which is what produced a
~12s worst case, and not `nprofile` hints, which are attacker-supplied
addresses. Hints are parsed (the TLV walk must step over them) and discarded.

### The request `Host` header is never read

Every absolute URL is built from `APP_BASE_URL`. Trusting `Host`/`X-Forwarded-Host`
let a forged header put an attacker's origin into `og:image`. Config beats an
allowlist: there is no header to validate if we never read one. It also means a
crawler arriving on a legacy alias still consolidates onto the canonical domain.
`prod-values.yaml` lists `brainstorm.nosfabrica.com` first for historical
reasons, but `brainstorm.world` is canonical — so this is set explicitly, not
inferred from list order.

### Image URLs are content-addressed

`og:image` carries `?v={hash}` over name, picture, nip05, rank and tier. HTML
gets a short `max-age`, images a long immutable one. A changed avatar or rank
yields a new URL within minutes; already-shared messages keep the card that was
true when shared. Whatever the tagline eventually becomes, it must be **stable
for a given card** or it desyncs from the hash.

Limits: only helps crawlers that re-fetch the page, and `?v=` must become a path
segment if a CDN that strips query strings is ever put in front.

### Fonts are vendored and fatal if missing

`assets/` ships Figtree, Noto Color Emoji (CBDT) and Noto Sans JP; the runtime
image is `scratch` and has no system fonts. Startup fails if the directory
yields no faces or `FONT_FAMILY` is absent. The old `load_system_fonts()`
fallback meant a bad build stayed *alive* while rendering every card with no
text at all. Verified before committing the ~19 MB: usvg emits CBDT glyphs as
image nodes and resvg decodes them via tiny-skia, and fallback across the three
families is automatic. Hangul is not covered; those names hit the
strip-unrenderable backstop and trim cleanly rather than showing boxes.

### `panic = "abort"` is off

This process decodes attacker-supplied images and rasterises
attacker-influenced text. A panic must fail one request via `CatchPanicLayer`,
not restart the pod and drop every in-flight crawler with it.

### The crawler split lives in the UI pod's nginx

Not the shared F5 ingress. Staging and prod are the same cluster
(`deploy_targets.conf`), so a map in the controller config carries prod blast
radius; `setup_ingress.sh` calls itself the single source of truth for that
config, so a map added out-of-band is dropped on its next run; and the
controller validates `Condition.variable` against built-in NGINX variables, so
a custom `$is_og_bot` likely fails the CRD webhook anyway. The UI-nginx failure
mode is reproducible with `docker run` and `curl` before anything ships.

Consequence worth stating: `ogPreview.enabled` and a UI image carrying the
nginx config are independent halves. Either alone is safe — an unrouted service
idles, an unresolvable upstream falls back to the SPA — but both are needed for
unfurls to work and **neither reports the other missing**, because "og is
absent" and "og is misconfigured" are the same observable.

## Accepted risk

Avatar-URL validation is in-process and pre-connect, so DNS rebinding between
check and socket is unmitigated. The cluster runs flannel, which does not
implement NetworkPolicy — an egress policy would apply cleanly and enforce
nothing, which is worse than none. Accepted because the fetched body is decoded
and never echoed: blind SSRF at worst, no exfiltration path.
