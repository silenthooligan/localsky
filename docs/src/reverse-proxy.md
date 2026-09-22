# HTTPS and reverse proxies

Use HTTPS for access beyond a trusted LAN. LocalSky listens on HTTP; a reverse proxy supplies TLS and forwards requests to it.

## Configure both sides

1. Complete LocalSky setup and enable [authentication](authentication.md).
2. Restrict direct access to LocalSky's port so external clients go through the proxy.
3. Set `auth.trusted_proxies` to the proxy's actual address.
4. Forward the client address and scheme. Disable response buffering for event streams.

For a proxy connecting from the same host through loopback:

```toml
[auth]
mode = "required"
trusted_proxies = ["127.0.0.1/32", "::1/128"]
trusted_networks = []
```

For a separate container or host, use its real source address instead. LocalSky accepts forwarded addresses only from a declared proxy and reads the chain from the right, stopping at the first untrusted hop.

## Caddy

This example assumes the proxy reaches LocalSky at `127.0.0.1:8090`:

```caddy
localsky.example.com {
    @private_paths path /ingest/* /api/ingest/* /api/v1/ingest/* /metrics
    respond @private_paths 403

    reverse_proxy 127.0.0.1:8090 {
        flush_interval -1
    }
}
```

Point your domain to the proxy and allow it to obtain a certificate. Keep hardware ingest reachable over the LAN where needed.

## nginx

Supply your certificate paths in this server block:

```nginx
server {
    listen 443 ssl;
    server_name localsky.example.com;
    ssl_certificate /etc/ssl/localsky/fullchain.pem;
    ssl_certificate_key /etc/ssl/localsky/privkey.pem;

    location ~ ^/(ingest|api/ingest|api/v1/ingest)/ {
        return 403;
    }
    location = /metrics {
        return 403;
    }

    location / {
        proxy_pass http://127.0.0.1:8090;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_buffering off;
        proxy_read_timeout 24h;
    }
}
```

Appending the observed peer with `$proxy_add_x_forwarded_for` is compatible with LocalSky's trusted-proxy chain. If there are multiple proxy hops, configure each trusted hop deliberately.

## What to expose

Hardware ingest accepts observations from devices that cannot send LocalSky credentials. Keep it private: fabricated observations can affect watering decisions. Restrict metrics if you do not want operational counters public.

Static assets, login, and docs must load for a new browser. If you add proxy-side authentication, ensure it handles those requests and SSE without redirect loops. Verify the full app in a signed-out browser.

## Check the result

- Sign in through HTTPS and reload the app.
- Open Weather or Irrigation and confirm updates continue without refreshing.
- Confirm a remote client cannot bypass authentication through port 8090.
- Check that hardware ingest is blocked externally but still works for the local device.

A 401 on a privileged route often means a missing credential or an incorrectly declared proxy. See [authentication](authentication.md#trusted-networks-and-proxies).
