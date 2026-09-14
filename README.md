# brainstorm-og

Share-card service for Brainstorm profiles. When a crawler (Slack, X, Discord,
Telegram, Facebook, …) fetches a profile URL, it gets per-profile `<meta>` tags
and a generated card image — instead of the SPA's generic static card. Humans
are never routed to those; they keep getting the React app unchanged.

It also serves **link previews** (`/link-preview`) for the UI: the title,
description and image of a third-party page linked in a note, which the browser
can't read itself. See `CONTEXT.md` for why that lives here.

The app has no hard dependency on it: the UI's nginx falls back to serving the
SPA when this service is absent or down, so removing it degrades unfurls back to
the SPA's generic card rather than breaking anything.

## Routes

- `GET /p/{npub|nprofile|hex}` → HTML with OG/Twitter meta tags. This is the
  canonical share route; it accepts exactly the ids that
  `Brainstorm-UI/client/src/lib/shareId.ts` accepts.
- `GET /profile/{id}` → the same, kept only for links already in the wild.
  `og:url` and `canonical` still point at `/p/`.
- `GET /s/{code}` → the same card for a **short share link**. The code is
  resolved to a pubkey via the API and cached; `og:url` and `canonical` point at
  `/p/`, since the card names the profile rather than the link to it. An
  unresolvable code is a 404, never a card for a profile that doesn't exist.
  Registered under `/S/` too: the QR payload is uppercase, and nginx proxies the
  URI unchanged.
- `GET /og/{id}.png?v={hash}` → 1200×630 PNG card.
- `GET /link-preview?url={absolute http(s) url}` → JSON `{code, message, data}`
  describing **someone else's** page: the inverse direction from the routes
  above, which are about ours. The URL is attacker-controlled, so every hop is
  address-checked, redirects are followed by hand and capped at 3, and the body
  is capped while it streams. Reading stops once `</head>` closes (never before
  8 KB — some sites put their tags just past it), which cuts what we pull from
  a third party by ~59%. A page whose head runs past the cap is truncated, not
  refused. Non-`http(s)` or reserved addresses are 400,
  non-HTML is 415, an upstream that fails or times out is 502 / 504. The `url`
  is never logged. `data` carries `{kind, title, description, image, siteName, url}`,
  all nullable — `og:` first, then `twitter:` (X publishes nothing else), then
  `<title>` / `meta[name=description]`. `image` resolves against the final URL
  and is nulled unless it passes the same address check we dial by, since the
  browser loads it on our say-so. Rate limited per client IP, at two rates: 600
  a minute for our own SPA (`Sec-Fetch-Site: same-origin`, or a `Referer` whose
  origin is `APP_BASE_URL`), 20 for everything else, over which it is a 429. The
  address is read from `X-Forwarded-For` counting `TRUSTED_PROXY_HOPS` back from
  the **right**, so the entry the caller sent is never the one we key on.
  `kind` is `page`, or `image` / `video` when the URL is itself media served
  without a file extension (read from `Content-Type`, body never read; JPEG, PNG,
  GIF, WebP, AVIF, MP4, WebM — other media stays 415). robots.txt is honoured
  per RFC 9309 (token `BrainstormBot`, `Crawl-delay` ignored); a disallowed URL
  is a 200 with nulls, an unreadable robots.txt a 502 that blanks the host for
  60s. A 429 or 503 pauses the whole host for its `Retry-After` (60s–10 min).
  Results are cached for a day (failures for five minutes; a host that is
  backing off or whose robots.txt can't be read is re-checked on the host's
  own clock instead) keyed on a
  normalised URL — lowercased scheme and host, no fragment, and `utm_*`,
  `fbclid`, `gclid`, `msclkid`, `igshid` stripped from both the key and the
  request we send; `ref` is left alone. Loaded through a single flight, so a
  burst of viewers of one note is one outbound fetch. A successful response
  carries the same `max-age`, so the browser and nginx cache it too.
- `GET /healthz` → JSON with the loaded font family and face count, plus
  `entry_count` and `weighted_size` per cache. Cheap enough for a readiness
  probe: moka's own figures, read without forcing its housekeeping, so they lag
  slightly rather than costing anything.

## The card

Identity only: the dark surface from `YourNetworkCard`'s dark variant, the
brand wordmark, the avatar, the display name and nip05.

**No score.** The card shows no verification score, tier or coin, and neither
does the meta description — an unfurl reaches people with no context and no
recourse, so we don't publish a number about someone there. The score stays
in-app, where it has a stated point of view and an explainer next to it.

This is not a copy of the app's in-app share panel, which is a separate surface
with a different audience and is free to look different.

## Data sources

- **Metadata** (name/avatar/nip05): kind-0 from the in-cluster relay, and only
  that one. Not a public fan-out — that is what made the original design slow —
  and not Vespa, which is downstream of the relay via the redis ingest queue.
  `nprofile` relay hints are parsed but never dialled.
- **Relationship counts**: one cached call to
  `GET {API_BASE_URL}/user/{hex}/overview`, used solely for the follower and
  following numbers in the meta description. The score it also returns is
  ignored.

Everything is cached in a bounded in-memory LRU (no disk).

## Cache invalidation

`og:image` carries a content hash: `/og/{id}.png?v={hash}` over exactly what the
card draws — name, picture, nip05 — plus `RENDER_EPOCH`. The HTML is served with
a short `max-age` and the image with a long immutable one, so a changed avatar
produces a new image URL within minutes, while already-shared messages keep the
card that was true when they were shared. Old `?v=` URLs stay valid forever.

Counts and scores are deliberately **not** hashed: they move constantly and the
card doesn't draw them, so including them would mint a new URL for a
byte-identical image on every GrapeRank run.

`RENDER_EPOCH` is the escape hatch for the opposite problem. A hash over inputs
alone cannot express "the renderer changed", so a visual fix would produce
identical URLs and never reach anything already holding one. The chart sets it
from the Helm release revision, so each upgrade issues fresh URLs.

This only helps crawlers that re-fetch the page — nothing forces Facebook to
re-scrape. If a CDN is ever put in front that strips query strings from cache
keys, `?v=` must become a path segment.

## Timing

Measured on the release image (Apple silicon, OrbStack), **upstreams over the
public internet** — `wss://purplepag.es` and the staging API. In production both
are in-cluster, so the cold numbers below are a pessimistic bound: they are
dominated by network round-trips, not by anything this service computes.

| Path | Cold (cache miss) | Warm (cache hit) |
|---|---|---|
| `GET /p/{id}` — meta HTML | 1.2 – 2.2 s | ~0.8 ms |
| `GET /og/{id}.png` — card | 0.33 – 0.57 s | ~1.0 ms |
| Rasterisation alone (no avatar to fetch) | ~99 ms | ~1 ms |

Reading that: of a ~2 s cold meta request, roughly 100 ms is ours. The rest is
the kind-0 lookup and the overview call, which run concurrently, so the cost is
the slower of the two. The card is cheaper than the meta HTML only because the
meta request has already populated the card cache; a card fetched first pays the
same upstream cost. Avatar fetching adds ~0.4 s against an arbitrary remote host
and is the single largest component of a cold card once metadata is cached.

`REQUEST_DEADLINE_SECS` (4 s) bounds the whole thing, because crawlers give up
somewhere between 5 and 10 s. Anything slower renders with partial data rather
than timing out.

**Stampede protection**: 20 concurrent requests for a pubkey nobody has asked
for yet complete in 0.66 s wall clock and produce exactly **one** upstream
lookup — `get_with` coalesces the rest onto the same in-flight load. Without it
that is 20 relay connections and 20 rasterisations for one card.

Reproduce with the commands in the "Run locally" section against a fresh
container; the caches are in-memory, so restarting is what resets them.

## Configuration (env)

| Var | Default | Notes |
|---|---|---|
| `BIND_ADDR` | `0.0.0.0:8080` | |
| `API_BASE_URL` | `http://brainstorm-server:8000` | overview stats |
| `APP_BASE_URL` | `https://brainstorm.world` | **every** absolute URL we emit |
| `LOCAL_RELAY_URL` | `ws://strfry:7777` | the only relay queried |
| `CACHE_MAX_ENTRIES` | `500` | card LRU size |
| `CACHE_MAX_BYTES` | `67108864` (64 MB) | rendered-PNG LRU cap |
| `CACHE_TTL_SECS` | `600` | origin re-render interval |
| `PROVISIONAL_TTL_SECS` | `60` | TTL when no kind-0 was found |
| `HTML_CACHE_MAX_AGE` | `300` | meta HTML `Cache-Control` |
| `IMAGE_CACHE_MAX_AGE` | `31536000` | card `Cache-Control`, immutable |
| `FETCH_TIMEOUT_SECS` | `3` | per upstream call |
| `AVATAR_TIMEOUT_SECS` | `5` | avatar image fetch |
| `AVATAR_MAX_BYTES` | `5242880` (5 MB) | body cap before decode |
| `REQUEST_DEADLINE_SECS` | `4` | whole-request budget |
| `LINK_PREVIEW_TIMEOUT_SECS` | `3` | per redirect hop |
| `LINK_PREVIEW_DEADLINE_SECS` | `5` | whole link-preview budget; must fit the router's |
| `LINK_PREVIEW_MAX_BYTES` | `512000` | body cap; over this the page is truncated |
| `LINK_PREVIEW_CACHE_TTL_SECS` | `86400` | preview TTL, and the response `max-age` |
| `LINK_PREVIEW_CACHE_MAX_BYTES` | `16777216` (16 MB) | preview cache cap, weighed on key + value |
| `LINK_PREVIEW_RATE_TRUSTED` | `600` | previews per window from our own SPA |
| `LINK_PREVIEW_RATE_UNTRUSTED` | `20` | previews per window from everything else |
| `LINK_PREVIEW_RATE_WINDOW_SECS` | `60` | the window both rates are counted over |
| `TRUSTED_PROXY_HOPS` | `2` | entries back from the right of `X-Forwarded-For` |
| `MAX_CONCURRENT_PREVIEWS` | `16` | in-flight link-preview fetches; queues past this |
| `ROBOTS_CACHE_TTL_SECS` | `86400` | how long a read robots.txt is trusted |
| `ROBOTS_CACHE_CAPACITY` | `4096` | origins whose robots.txt is held |
| `ROBOTS_TIMEOUT_SECS` | `2` | robots.txt fetch; stays under the page timeout |
| `RENDER_EPOCH` | crate version | salt for `?v=`; bump to force re-render |
| `MAX_CONCURRENT_RENDERS` | `32` | in-flight card renders; queues past this |
| `ASSETS_DIR` | `assets` | bundled fonts |
| `FONT_FAMILY` | `Figtree` | must match a family in `ASSETS_DIR` |

`APP_BASE_URL` is the whole story for URL generation: the request's `Host`
header is never read. A forged `Host: evil.com` therefore cannot put an attacker
origin into `og:image`, and a crawler arriving on a legacy alias still
consolidates onto the canonical domain.

The service refuses to start if `ASSETS_DIR` yields no fonts, or if
`FONT_FAMILY` is not among them. That is deliberate: the previous
`load_system_fonts()` fallback meant a bad build stayed *alive* and rendered
every card with no text at all.

## Run locally

```bash
APP_BASE_URL=http://localhost:8080 \
API_BASE_URL=https://brainstormserver-staging.nosfabrica.com \
LOCAL_RELAY_URL=wss://purplepag.es \
cargo run

NPUB=npub1...
curl -s -A Twitterbot/1.0 localhost:8080/p/$NPUB | grep og:
curl -s localhost:8080/og/$NPUB.png -o card.png
```

## Routing (who reaches this service)

The UI pod's own nginx sends crawler User-Agents on `/p/*` and `/s/*` here (the
latter matched case-insensitively), and all of `/og/*` for everyone. Deliberately not the ingress: staging and prod share a
cluster, so a map in the shared controller config would carry prod blast radius.
See [`deploy/README.md`](deploy/README.md); the reasoning is in [`CONTEXT.md`](CONTEXT.md).
