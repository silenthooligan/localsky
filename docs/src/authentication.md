# Authentication

LocalSky ships with built-in authentication. New installs create an owner
account during the setup wizard; existing installs stay open until you opt
in. Identity (accounts, sessions, API tokens) lives in the SQLite database,
never in `localsky.toml`; the TOML carries only policy.

## Modes

```toml
[auth]
mode = "required"        # "disabled" (default for upgrades) | "required"
session_ttl_days = 30    # rolling browser-session lifetime
trusted_networks = []    # CIDRs that skip login, e.g. ["10.0.0.0/24"]
```

- `disabled`: the pre-auth behavior. The right choice when a reverse
  proxy already guards access, or on an isolated trusted network.
- `required`: the UI redirects to `/login`; API calls need a session
  cookie or an API token. New wizard installs that create an owner
  account get this automatically.
- `trusted_networks`: lets the home LAN stay frictionless while VPN/WAN
  clients must sign in. Each entry is a CIDR matched against the client
  address. Read [the section below](#x-forwarded-for-and-trusted-networks)
  before setting this on anything reachable from outside your LAN.

## X-Forwarded-For and trusted networks

How LocalSky determines the client address, exactly:

1. If the request carries an `X-Forwarded-For` header, LocalSky uses the
   **first hop** (the left-most entry) of that header.
2. Otherwise it uses the TCP peer address of the connection.

That address drives two things: the `trusted_networks` login bypass and
the login/setup rate limiter.

LocalSky has no trusted-proxy list, so it cannot tell a proxy-set
`X-Forwarded-For` from a client-forged one. Any client that can reach
the LocalSky port directly can send
`X-Forwarded-For: 192.168.1.50` and, if `192.168.1.0/24` is in
`trusted_networks`, walk straight past login. Deploy accordingly:

- **Never expose the LocalSky port directly to the internet with
  `trusted_networks` set.** Either leave `trusted_networks` empty on an
  internet-reachable instance, or make sure the only route to LocalSky
  is through your reverse proxy (bind LocalSky to localhost or an
  internal Docker network, or firewall the port).
- **Your proxy must overwrite the header, not append to it.** With
  nginx, use the client address itself:

  ```nginx
  proxy_set_header X-Forwarded-For $remote_addr;
  ```

  Do **not** use `$proxy_add_x_forwarded_for` in front of LocalSky: it
  appends the proxy-observed address to whatever the client sent, which
  leaves a forged address in the first-hop position LocalSky reads.
  Caddy (2.5+) and Traefik overwrite forwarded headers from untrusted
  clients by default, so their stock configs are safe.
- On a flat LAN with no proxy, the TCP peer address is used and there is
  nothing to forge below L3; `trusted_networks` is fine there as long as
  the network itself is trusted.

## What stays public

These paths never require credentials, by design:

| Path | Why |
|---|---|
| `/pkg/*`, `/sw.js`, root static assets | Compiled assets; browsers fetch them without credentials |
| `/api/v1/info` | Pairing probe; carries `auth_required` so clients know to ask for a token |
| `/login`, `/api/v1/auth/{status,login,setup}` | The way in |
| `/ingest/*`, `/api/v1/ingest/*` | Weather hardware (Ecowitt consoles, webhooks) cannot authenticate; block at the proxy for internet-facing deployments ([details](reverse-proxy.md#what-to-expose)) |
| `/api/v1/health` | Liveness for Docker healthchecks; anonymous callers get a trimmed body (no source, controller, or HA detail) |
| `/setup` + wizard APIs | Only until the first account exists |

## Accounts

One owner account for now. Create it in the wizard's Account step, or
later under Settings, then Account. Passwords are stored as argon2id
hashes. Sign-in attempts are rate limited per client address.

## API tokens (integrations)

Integrations authenticate with long-lived API tokens sent as
`Authorization: Bearer lsk_...`:

1. In LocalSky: Settings, then Account, then Create token (name it, e.g.
   `home-assistant`).
2. The plaintext is shown exactly once; store it where the integration
   asks for it. Only a hash is kept server-side.
3. Revoke any token from the same screen; the Home Assistant integration
   starts its reauthentication flow automatically on the next 401.

SSE streams accept `?access_token=lsk_...` as a query parameter for
clients that cannot set headers. It is honored only on paths ending in
`/stream` and ignored everywhere else (the browser EventSource sends
the session cookie automatically, so this is only for external
consumers).

## Lockout recovery

If you lose the owner password, stop the container and delete the
`users` rows from the database, then restart and re-run account creation:

```bash
sqlite3 /path/to/data/irrigation.db "DELETE FROM auth_sessions; DELETE FROM api_tokens; DELETE FROM users;"
```

Physical access to the data volume is the trust anchor, the same as
Home Assistant's.
