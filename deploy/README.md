# Deploying brainstorm-og

The service is a passive backend: it renders cards for whoever asks. Nothing
reaches it until something routes share pages its way, and that routing lives
in **the UI container's nginx**, not here and not in the ingress.

## Where the routing lives, and why

`Brainstorm-UI/nginx.conf` carries the `/p/`, `/e/`, `/s/` locations (the SPA
with an SSI include of `/__og/meta/…`), the internal `/__og/` location that
proxies here, and `/og/`; `Brainstorm-UI/client/index.html` carries the
fallback block and the include. That file is the single source of truth for the split — this repo
deliberately keeps no copy, because two copies drift.

It is not at the F5 ingress because **staging and prod share a cluster**
(`brainstorm-k8s/deploy_targets.conf`). Putting a `map` in the controller's
`http-snippets` would make a Brainstorm change able to break prod ingress for
everything on that cluster, and `setup_ingress.sh` declares itself the single
source of truth for controller config — so a map added out-of-band gets dropped
the next time anyone runs it. The UI-nginx failure mode, by contrast, is a local
`curl` away.

Full reasoning: [`../CONTEXT.md`](../CONTEXT.md).

## Kubernetes

Enabled by default in `brainstorm-k8s/charts/brainstorm`. It renders
`templates/og.yaml` (Deployment + ClusterIP on 8080) and sets `OG_UPSTREAM` on
the UI Deployment.

```yaml
ogPreview:
  image:
    tag: sha-xxxxxxx        # prod pins immutable tags
  appBaseUrl: ""            # blank derives https://<first domains.ui host>
```

Two things to get right:

**`appBaseUrl` in prod.** Every absolute URL the service emits comes from it,
and prod's `domains.ui` leads with the legacy `brainstorm.nosfabrica.com` for
DNS-transition reasons. Left blank there, every unfurl would canonicalise onto
the wrong domain. `prod-values.yaml` sets it explicitly to
`https://brainstorm.world`.

**The two halves.** `ogPreview.enabled` and a UI image containing the nginx
config are independent. Either alone is safe — an unrouted service idles, and an
unresolvable upstream falls back to the SPA — but you need both for unfurls to
actually work, and neither reports the other missing.

Deploy with `./deploy_staging.sh --ui --og`.

## Docker Compose

Add to `docker-compose.yml`:

```yaml
brainstorm-og:
  image: ghcr.io/nosfabrica/brainstorm_og:latest
  restart: unless-stopped
  depends_on: [brainstorm-server]
  environment:
    API_BASE_URL: http://brainstorm-server:8000
    LOCAL_RELAY_URL: wss://search.brainstorm.world
    APP_BASE_URL: http://localhost:3000
```

No published ports — the UI proxies to it internally. The UI needs no
configuration: `OG_UPSTREAM` defaults to `brainstorm-og:8080`, which is the
compose service name.

## Verify

```bash
NPUB=npub1...
NEVENT=nevent1...
H=<ui-host>

# Every visitor gets the page's own tags, whatever its User-Agent...
for UA in "Twitterbot/1.0" "Signal-Android/7" "Mozilla/5.0 Safari/605.1.15"; do
  curl -s -A "$UA" https://$H/p/$NPUB | grep -E 'og:title|og:image"'
done

# ...in the SPA page itself...
curl -s https://$H/p/$NPUB | grep -c '<div id="root">'

# ...and sub-paths are NOT included. These keep the generic tags:
curl -s https://$H/p/$NPUB/hops      | grep 'og:title'
curl -s https://$H/p/$NPUB/followers | grep 'og:title'

# A note: its own first image, or its card.
curl -s https://$H/e/$NEVENT | grep -E 'og:title|og:description|og:image"'

# The card itself, and its content-addressed URL:
curl -s https://$H/p/$NPUB | grep -o 'og/[^"]*'   # .../og/<id>.png?v=<hash>

# A forged Host must NOT change og:url:
curl -s -A "Twitterbot/1.0" -H 'Host: evil.com' https://$H/p/$NPUB | grep og:url

# Fonts loaded (a fontless image renders every card blank):
kubectl -n <ns> logs deploy/<release>-brainstorm-og | grep 'fonts loaded'
```

Then paste the profile URL into the Facebook Sharing Debugger (the only
validator with a real cache-buster), LinkedIn Post Inspector, opengraph.xyz,
Telegram's `@WebpageBot`, or a private Slack channel. Slack caches ~30 min, so
use a fresh npub per attempt.

## Disable

Set `ogPreview.enabled: false`. `/p/*` serves the SPA with its generic tags —
the include's `error_page … = @share_meta_none` turns the service being gone
into an empty include, so no UI change is needed to turn it off.
