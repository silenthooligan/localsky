# Reverse proxy and HTTPS

LocalSky listens on plain HTTP (default `:8090`). On a trusted LAN with
built-in auth enabled that is a reasonable place to stop. To reach it
from the internet, put a TLS reverse proxy in front and let it terminate
HTTPS.

Two things matter for any proxy:

1. Pass `X-Forwarded-Proto: https` so LocalSky marks its session cookie
   `Secure`.
2. Pass `X-Forwarded-For` so `auth.trusted_networks` and login rate
   limiting see the real client address.

Server-Sent Events (`/api/stream`, `/api/v1/irrigation/stream`,
`/api/v1/forecast/stream`) are long-lived responses: disable buffering
and give them a long (or no) read timeout.

## Caddy

```caddy
localsky.example.com {
    reverse_proxy 127.0.0.1:8090 {
        flush_interval -1   # stream SSE unbuffered
    }
}
```

Caddy sets the forwarding headers and provisions certificates
automatically. If you also gate with Caddy-side auth (forward_auth,
OAuth plugins), exempt `/pkg/*` and `/sw.js`: hydration assets are
fetched without credentials and a redirect there breaks the app shell.

## nginx

```nginx
server {
    listen 443 ssl;
    server_name localsky.example.com;
    # ssl_certificate ...; ssl_certificate_key ...;

    location / {
        proxy_pass http://127.0.0.1:8090;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
    }

    # SSE: no buffering, no read timeout.
    location ~ /stream$ {
        proxy_pass http://127.0.0.1:8090;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_buffering off;
        proxy_read_timeout 24h;
    }
}
```

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

Traefik streams responses by default and sets the forwarding headers.

## Home Assistant integration through a proxy

The HACS integration talks to whatever host/port you pair it with. On
the LAN, pair it straight to `:8090` (with an API token when auth is
required) and keep the proxy for browsers; nothing else is needed.
