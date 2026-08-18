# brainstorm-og

Share-card service for Brainstorm profiles. When a crawler (Slack, X, Discord,
Telegram, Facebook, …) fetches a profile URL, it gets per-profile `<meta>` tags
and a generated card image — instead of the SPA's generic static card. Humans
are never routed here; they keep getting the React app unchanged.

The app has no hard dependency on it: the UI's nginx falls back to serving the
SPA when this service is absent or down, so removing it degrades unfurls back to
the SPA's generic card rather than breaking anything.

## Routes

- `GET /p/{npub|nprofile|hex}` → HTML with OG/Twitter meta tags. This is the
  canonical share route; it accepts exactly the ids that
  `Brainstorm-UI/client/src/lib/shareId.ts` accepts.
- `GET /profile/{id}` → the same, kept only for links already in the wild.
  `og:url` and `canonical` still point at `/p/`.
- `GET /og/{id}.png?v={hash}` → 1200×630 PNG card.
- `GET /healthz` → JSON with the loaded font family and face count.

## The card

Built from the UI's design tokens: the dark surface from `YourNetworkCard`'s
dark variant, the brand wordmark, the avatar, and the **Verification Coin**
(`components/score/VerificationCoin.tsx`) pinned to the avatar's corner —
deliberately label-less, fill carrying tier and the ring carrying point of view.

This is not a copy of the app's in-app share panel, which is a separate surface
with a different audience and is free to look different.

Always the **global (house)** perspective. There is no viewer to personalise
for, so the coin never uses the personalized purple ring.

`flagged` is deliberately never surfaced. The card is auto-generated for any
pubkey anyone chooses to paste, on our domain, with no appeal path — a grey
"no signal on this person" is fair to publish that way, a red "trusted people
reported them" is not. It clamps to the unverified floor; flagging stays in-app
where there is context and recourse.

## Data sources

- **Rank + tier**: one cached call to `GET {API_BASE_URL}/user/{hex}/overview`.
  The tier bucket is taken from the response rather than re-derived — the bands
  live in `brainstorm_server/app/core/tier_thresholds.py` and must not be
  duplicated into a second language. Because the call is anonymous, the server
  resolves the default (house) observer for us.
- **Metadata** (name/avatar/nip05): kind-0 from the in-cluster relay, and only
  that one. Not a public fan-out — that is what made the original design slow —
  and not Vespa, which is downstream of the relay via the redis ingest queue.
  `nprofile` relay hints are parsed but never dialled.

Everything is cached in a bounded in-memory LRU (no disk).

## Cache invalidation

`og:image` carries a content hash: `/og/{id}.png?v={hash}` over the name,
picture, nip05, rank and tier. The HTML is served with a short `max-age` and the
image with a long immutable one, so a changed avatar or rank produces a new
image URL within minutes, while already-shared messages keep the card that was
true when they were shared. Old `?v=` URLs stay valid forever.

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
| `AVATAR_TIMEOUT_SECS` | `2` | avatar image fetch |
| `AVATAR_MAX_BYTES` | `5242880` (5 MB) | body cap before decode |
| `REQUEST_DEADLINE_SECS` | `4` | whole-request budget |
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

The UI pod's own nginx sends crawler User-Agents on `/p/*` here, and all of
`/og/*` for everyone. Deliberately not the ingress: staging and prod share a
cluster, so a map in the shared controller config would carry prod blast radius.
See [`deploy/README.md`](deploy/README.md); the reasoning is in [`CONTEXT.md`](CONTEXT.md).
