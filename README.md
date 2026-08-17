# brainstorm-og

Optional, standalone OpenGraph link-preview service for Brainstorm profiles. When a
crawler (Slack, Twitter/X, Discord, Telegram, Facebook, …) fetches a profile URL, it
gets per-profile `<meta>` tags and a generated card image — instead of the SPA's generic
static card. Humans are never routed here; they keep getting the React app unchanged.

It is **off by default**: if you don't deploy it and don't add the ingress rule, nothing
changes. The app has no hard dependency on it.

## What it does

- `GET /profile/{npub|nprofile|hex}` → HTML with OG/Twitter meta tags (title, description,
  `og:image`). Body is a courtesy link; humans don't reach this route.
- `GET /og/{npub|nprofile|hex}.png` → 1200×630 PNG card: avatar, name, nip05, about,
  influence + follower/following counts. Generic fallback card when metadata is missing.
- `GET /healthz` → `ok`.

## Data sources

- **Stats** (influence + follower/following counts): one cached call to
  `GET {API_BASE_URL}/user/{hex}/overview` — the only DB-backed dependency.
- **Metadata** (name/avatar/about): kind-0 from relays only — local relay first, then
  `nprofile` relay hints, then public profile relays. Never the DB/Vespa.

Everything is cached in a bounded in-memory LRU (no disk) and served with a long
`Cache-Control` so a CDN/edge absorbs repeat crawler hits.

## Configuration (env)

| Var | Default | Notes |
|---|---|---|
| `BIND_ADDR` | `0.0.0.0:8080` | |
| `API_BASE_URL` | `http://brainstorm-server:8000` | overview stats source |
| `APP_BASE_URL` | `https://brainstorm.nosfabrica.com` | absolute `og:url` / `og:image` origin |
| `LOCAL_RELAY_URL` | `ws://strfry:7777` | tried first for kind-0 |
| `FALLBACK_RELAYS` | purplepag.es, damus, nos.lol, primal, nostr.wine | comma-separated |
| `CACHE_MAX_ENTRIES` | `500` | card LRU size |
| `CACHE_MAX_BYTES` | `134217728` (128 MB) | rendered-PNG LRU cap |
| `CACHE_TTL_SECS` | `3600` | origin re-render interval |
| `HTTP_CACHE_MAX_AGE` | `86400` | `Cache-Control: max-age` (CDN window) |
| `FETCH_TIMEOUT_SECS` | `6` | per relay / overview call |
| `AVATAR_TIMEOUT_SECS` | `4` | avatar image fetch |
| `ASSETS_DIR` | `assets` | bundled fonts (image ships `/assets`) |

## Run locally

```bash
cargo run
# then:
curl -A Twitterbot localhost:8080/profile/<npub>
curl localhost:8080/og/<npub>.png -o card.png
```

(Locally, system fonts are used when `ASSETS_DIR` has none.)

## Routing (who reaches this service)

A reverse proxy / ingress sends only crawler User-Agents on `/profile/*` here, and `/og/*`
for everyone. See the deployment wiring:

- one-click compose: UI nginx `location /profile/` + `/og/` (see Brainstorm-UI nginx.conf,
  `improve-dockerfile-image` branch).
- k8s: F5 NGINX VirtualServer `matches`/`conditions` on a `$is_og_bot` map.
