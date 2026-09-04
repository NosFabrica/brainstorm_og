# brainstorm-og

Renders the share card and crawler-visible meta tags for `/p/{id}` and for the
short share links `/s/{code}` resolve to. Replaces
`Brainstorm-UI`'s `useShareMeta.ts`, a client-side stopgap invisible to crawlers.

## Language

**Share card**: the rendered 1200×630 image. _Avoid_: OG card, link preview,
preview image. "OG" survives only in protocol names (`og:image`, `/og/`).

**Unfurl**: what a third-party crawler does with a share link. _Avoid_: preview,
embed.

**Provisional card**: one assembled without a kind-0. Short TTL so it
self-corrects rather than pinning a name-less card.

**Short link / code**: `/s/{code}`, where the code stands in for a pubkey plus
relay hints. Say "code" for the identifier and "short link" for the URL — not
"short url", which reads as either. Minted by `brainstorm_server`; immutable
once minted, which is why resolution is cached without a TTL.

## Decisions

### The card publishes no score

No coin, no tier, no number — on the card or in the meta description. An unfurl
lands in front of someone with no stated point of view, no explainer and no
recourse, and it is auto-generated for any pubkey anyone chooses to paste. The
score stays in-app where all three of those exist.

This also removed the `flagged` question entirely. Previously the card clamped
to the unverified floor so a reported account never rendered red; now there is
nothing to clamp.

`/overview` is still called, but only for the follower and following counts in
the meta description. The `tier` bucket it returns is ignored.

### Metadata comes from the relay, not Vespa

One in-cluster relay, from config — not the original six-relay public fan-out,
which is what produced a ~12s worst case, and not `nprofile` hints, which are
attacker-supplied addresses. Hints are parsed (the TLV walk must step over them)
and discarded.

That relay is **neofry**, not strfry. neofry is the data ingress — the router
streams kind 0 into it — while strfry is the read-only egress and is synced
separately. Pointed at strfry on staging, every lookup returned "no kind-0
event" and every card rendered a truncated npub instead of a name: the relay
answered, it just had nothing.

### The request `Host` header is never read

Every absolute URL is built from `APP_BASE_URL`. Trusting `Host`/`X-Forwarded-Host`
let a forged header put an attacker's origin into `og:image`. Config beats an
allowlist: there is no header to validate if we never read one. It also means a
crawler arriving on a legacy alias still consolidates onto the canonical domain.
`prod-values.yaml` lists `brainstorm.nosfabrica.com` first for historical
reasons, but `brainstorm.world` is canonical — so this is set explicitly, not
inferred from list order.

### Image URLs are content-addressed, plus a render epoch

`og:image` carries `?v={hash}` over exactly what the card draws — name, picture,
nip05 — and nothing else. Counts and scores are excluded on purpose: they move
constantly and are not drawn, so hashing them would mint a new URL for a
byte-identical image on every GrapeRank run.

The hash also folds in `RENDER_EPOCH`, because a hash over inputs cannot express
"the renderer changed". Without it a visual fix produces identical URLs and
never reaches anything already holding one — and those are served immutable for
a year. This bit us twice in one day: an avatar fix and the coin removal both
left cached cards stale. The chart sets it from the Helm release revision, since
staging rides a mutable image tag and a tag-derived value would be constant
across exactly the deploys that most need to propagate.

Whatever the tagline eventually becomes, it must be **stable for a given card**
or it desyncs from the hash.

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

### The link-preview client filters addresses at DNS resolution

Not only before the connection. A pre-connect check leaves a window in which
the name re-answers with an internal address, and the socket opens on the one
nobody judged. The fetching client resolves through `net::resolver`, which
applies the same address policy to the resolver's answer — and that answer is
what the connector dials, so there is no second, unchecked resolution.

The pre-connect `validate_and_resolve` stays. It is what turns a refused URL
into a 400 the caller can cache, and it judges IP-literal hosts, which never
reach the resolver at all — the connector parses those itself.

A whole answer is rejected when any address in it is reserved, rather than
trimmed to the survivors. A name handing back one internal address is not one
we want to reach at its other one, and it keeps the two checks agreeing.

## Accepted risk

Avatar-URL validation is in-process and **pre-connect only**, so DNS rebinding
between check and socket is unmitigated on that path. The cluster runs flannel,
which does not implement NetworkPolicy — an egress policy would apply cleanly
and enforce nothing, which is worse than none. Accepted because the fetched
body is decoded and never echoed: blind SSRF at worst, no exfiltration path.

That reasoning does **not** extend to `/link-preview`, which echoes parsed
content back to the caller and so would turn the same window into a read
primitive. It is closed there by the resolver above rather than accepted.
Giving `avatar_http` the same resolver is worth doing and deliberately has not
been done yet; until it is, the note above is the whole of the argument.
