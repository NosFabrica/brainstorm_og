# Enabling OG link previews

`brainstorm-og` is **optional and off by default**. Nothing routes to it until you
add the rule below. Both paths use the same idea: send only crawler User-Agents on
`/profile/*` (and all `/og/*`) to the service; humans keep getting the SPA.

## Kubernetes (Helm chart `brainstorm-k8s`)

Set in values (or `--set`):

```yaml
ogPreview:
  enabled: true
  image:
    repository: ghcr.io/nosfabrica/brainstorm_og
    tag: sha-xxxxxxx          # pin a build
```

This renders:
- `templates/og.yaml` — `brainstorm-og` Deployment + ClusterIP Service (port 8080),
  wired to the in-cluster API (`/user/{hex}/overview`) and strfry relay.
- a `server-snippets` block on the UI VirtualServer
  (`templates/virtualserver.yaml`) that does the User-Agent routing in raw nginx
  (the controller already runs with `--enable-snippets=true`).

Disable by removing `ogPreview.enabled` (or set `false`) — the snippet and the
Deployment both vanish; `/profile/*` serves the SPA exactly as before.

## One-click / docker-compose

1. Build/pull the image as `brainstorm-og-service` (compose references that name) or
   point the `brainstorm-og` service at `ghcr.io/nosfabrica/brainstorm_og`.
2. The `brainstorm-og` service is already in `docker-compose.yml` (no published
   ports — internal only).
3. Use the nginx-based UI (`improve-dockerfile-image` branch) and replace its
   `nginx.conf` with [`nginx/ui-nginx-with-og.conf`](nginx/ui-nginx-with-og.conf),
   which adds the `map` + `/profile/` + `/og/` routing. The OG upstream is resolved
   lazily, so the UI still boots if `brainstorm-og` is absent.

To disable: drop the `brainstorm-og` service and use the stock UI `nginx.conf`.

## Verify

```bash
# bot sees per-profile meta:
curl -A "Twitterbot/1.0" https://<ui-host>/profile/<npub> | grep og:

# the card image:
curl https://<ui-host>/og/<npub>.png -o card.png

# a human still gets the SPA (no bot UA -> index.html, not the OG service):
curl -s https://<ui-host>/profile/<npub> | grep -c '<div id="root">'
```

Then paste the profile URL into Slack / the Twitter Card Validator / opengraph.xyz.
