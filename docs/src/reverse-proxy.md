# Reverse proxy and HTTPS

LocalSky listens on plain HTTP (default `:8090`). On a trusted LAN with
built-in auth enabled that is a reasonable place to stop. To reach it
from the internet, put a TLS reverse proxy in front and let it terminate
HTTPS.

Three things matter for any proxy:

1. Pass `X-Forwarded-Proto: https` so LocalSky marks its session cookie
   `Secure`.
2. **Overwrite** (never append to) `X-Forwarded-For` with the real
   client address. LocalSky reads the first hop of that header for
   `auth.trusted_networks` and login rate limiting, and it has no
   trusted-proxy list, so an appended header leaves a client-forged
   address in the position LocalSky trusts. See
   [X-Forwarded-For and trusted networks](authentication.md#x-forwarded-for-and-trusted-networks).
3. Server-Sent Events (`/api/v1/stream`, `/api/v1/irrigation/stream`,
   `/api/v1/forecast/stream`, plus their legacy `/api/*` aliases) are
   long-lived responses: disable buffering and give them a long (or no)
   read timeout.

## What to expose

Built-in auth gates most of the app, but a few paths are public by
design. For an internet-facing deployment, narrow them at the proxy:

- **Block `/ingest/*` and `/api/v1/ingest/*` from the internet.** These
  receive sensor data from hardware that cannot authenticate (Ecowitt
  consoles, webhook devices), so they are exempt from auth. Anyone who
  can POST to them can feed LocalSky fabricated weather, and fabricated
  weather steers irrigation decisions. Your weather hardware is on your
  LAN; the internet has no business reaching these paths.
- **Consider blocking `/setup` and `/api/v1/wizard/*` until setup is
  done.** The setup wizard (pages and APIs) is public until the first
  account exists, so a brand-new instance exposed before you finish the
  wizard can be configured by whoever finds it first. Either complete
  the wizard before exposing the instance, or block these paths at the
  proxy until you have created the owner account (after that, LocalSky
  locks them itself).
- **Keep `/pkg/*` and `/sw.js` reachable without credentials.** These
  hydration assets are fetched by the browser without cookies; if a
  proxy-side auth layer intercepts them, the app shell breaks (see the
  warnings in each proxy section below).

Everything else (dashboard pages, the API, uploaded photos) is covered
by LocalSky's own auth when `[auth] mode = "required"`. If you run with
auth disabled, the proxy is your only gate; in that case put proxy-side
auth in front of everything except `/pkg/*`, `/sw.js`, and (if hardware
posts from outside) the ingest paths.

## Caddy

```caddy
localsky.example.com {
    reverse_proxy 127.0.0.1:8090 {
        flush_interval -1   # stream SSE unbuffered
    }
}
```

Caddy sets the forwarding headers and provisions certificates
automatically, and (since 2.5) ignores forwarded headers from untrusted
clients, so the `X-Forwarded-For` LocalSky sees is the real client
address. If you also gate with Caddy-side auth (forward_auth, OAuth
plugins), exempt `/pkg/*` and `/sw.js`: hydration assets are fetched
without credentials and a redirect there breaks the app shell.

To block the ingest receivers from the internet with Caddy:

```caddy
localsky.example.com {
    @ingest path /ingest/* /api/v1/ingest/*
    respond @ingest 403

    reverse_proxy 127.0.0.1:8090 {
        flush_interval -1
    }
}
```

## nginx

```nginx
server {
    listen 443 ssl;
    server_name localsky.example.com;
    # ssl_certificate ...; ssl_certificate_key ...;

    # Block unauthenticated receivers from the internet.
    location ~ ^/(ingest|api/v1/ingest)/ {
        return 403;
    }

    location / {
        proxy_pass http://127.0.0.1:8090;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-For $remote_addr;
        proxy_set_header X-Forwarded-Proto $scheme;
    }

    # SSE: no buffering, no read timeout.
    location ~ /stream$ {
        proxy_pass http://127.0.0.1:8090;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-For $remote_addr;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_buffering off;
        proxy_read_timeout 24h;
    }
}
```

Note `X-Forwarded-For $remote_addr`, **not**
`$proxy_add_x_forwarded_for`. The latter appends to whatever
`X-Forwarded-For` the client sent, and LocalSky reads the first
(client-controlled) hop, which would let an internet client spoof a
`trusted_networks` address and bypass login. `$remote_addr` replaces
the header with the address nginx actually saw.

If nginx itself sits behind another proxy you control (e.g. a
Cloudflare tunnel), use the `real_ip` module to recover the true client
address first, and still send LocalSky a single-value header.

## Traefik (Docker labels)

```yaml
services:
  localsky:
    # ... your localsky service ...
    labels:
      - traefik.enable=true
      - traefik.http.routers.localsky.rule=Host(`localsky.example.com`)
      - traefik.http.routers.localsky.entrypoints=websecure
      - traefik.http.routers.localsky.tls.certresolver=letsencrypt
      - traefik.http.services.localsky.loadbalancer.server.port=8090
```

Traefik streams responses by default and, unless you opt in to
`forwardedHeaders.insecure` or `trustedIPs`, discards forwarded headers
from untrusted clients and sets its own, which is what LocalSky needs.

If you add a Traefik auth middleware (`forwardAuth`, `basicAuth`,
OAuth) in front of LocalSky, exempt `/pkg/*` and `/sw.js` from it (a
higher-priority router for those path prefixes without the middleware).
Hydration assets are fetched without credentials; gating them breaks
the app shell exactly as it does with Caddy or nginx.

## Home Assistant integration through a proxy

The HACS integration talks to whatever host/port you pair it with. On
the LAN, pair it straight to `:8090` (with an API token when auth is
required) and keep the proxy for browsers; nothing else is needed.
