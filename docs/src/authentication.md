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
trusted_proxies = []     # CIDRs of YOUR reverse proxies, e.g. ["172.18.0.0/16"]
```

- `disabled`: the pre-auth behavior. The right choice when a reverse
  proxy already guards access, or on an isolated trusted network.
- `required`: the UI redirects to `/login`; API calls need a session
  cookie or an API token. New wizard installs that create an owner
  account get this automatically.
- `trusted_networks`: lets the home LAN stay frictionless while VPN/WAN
  clients must sign in. Each entry is a CIDR matched against the client
  address. Read [the section below](#x-forwarded-for-trusted_proxies-and-trusted_networks)
  before setting this on anything reachable from outside your LAN.
- `trusted_proxies`: the CIDRs of your own reverse proxy hops. Set this
  if (and only if) LocalSky sits behind a proxy; it is what makes
  LocalSky believe `X-Forwarded-For`. See below.

## X-Forwarded-For, trusted_proxies, and trusted_networks

How LocalSky determines the client address, exactly:

1. The **TCP peer address of the connection is authoritative.** That is
   the address LocalSky uses by default.
2. `X-Forwarded-For` is **only believed when the peer itself is one of
   your `trusted_proxies`.** When it is, LocalSky walks the header from
   the **right**, skips any hops that are also in `trusted_proxies`, and
   takes the **first hop that is not a trusted proxy** as the client.
   (The rightmost entries were appended by your own proxy chain; anything
   to the left of the first untrusted hop is client-supplied and
   trivially forgeable, so it is ignored.)
3. If the peer is **not** in `trusted_proxies`, `X-Forwarded-For` is
   ignored entirely and the peer address wins. A client that reaches the
   LocalSky port directly therefore cannot spoof its address by sending
   its own `X-Forwarded-For`: the header is only honored from a proxy you
   declared.

That derived client address drives two things: the `trusted_networks`
login bypass and the login/setup rate limiter.

### If LocalSky is behind a reverse proxy, set `trusted_proxies`

Because the peer is authoritative and XFF is ignored unless the peer is a
declared proxy, a proxied deployment that does **not** set
`trusted_proxies` will see **every request as coming from the proxy's own
address**. The consequences:

- **`trusted_networks` matches the proxy, not the real client.** If the
  proxy's address falls inside a `trusted_networks` CIDR, *everyone*
  coming through it skips login; if it does not, *nobody* gets the
  bypass. Either way the bypass no longer keys on the real client.
- **The login/setup rate limiter keys on the proxy.** All clients share
  one bucket, so one noisy client (or a distributed brute-force funneled
  through the proxy) can trip the limit for everyone, and per-client
  throttling is lost.

So: **if you run LocalSky behind a proxy, set `trusted_proxies` to that
proxy's address/CIDR** (for the bundled Docker Compose the proxy is on the
Docker bridge, e.g. `172.18.0.0/16`; for a host-network proxy use its LAN
address). Then XFF is believed from it, and `trusted_networks` + the rate
limiter see the real client again.

### Proxy header hygiene

When you set `trusted_proxies`, your proxy must append (or set) a correct
`X-Forwarded-For`. LocalSky reads the rightmost untrusted hop, so the
common nginx idiom is safe here:

```nginx
proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
```

This appends the real peer your proxy observed to the right of whatever
the client sent; LocalSky skips your trusted-proxy hops and lands on that
appended value, and any client-forged entries sit to its left where they
are ignored. (Caddy and Traefik produce a correct chain by default.)

### Deployment rules

- **Never expose the LocalSky port directly to the internet with
  `trusted_networks` set and `trusted_proxies` empty/wrong.** With
  `trusted_proxies` empty the peer is authoritative, so a direct
  internet client is judged on its real source address (good); but make
  sure the only route to LocalSky is through your proxy (bind LocalSky to
  localhost or an internal Docker network, or firewall the port) so a
  WAN client cannot bypass the proxy and hit a `trusted_networks` range
  directly.
- **Do not list a CIDR in `trusted_proxies` that untrusted clients can
  originate from.** `trusted_proxies` is "believe XFF from here"; if an
  attacker can connect from inside that range, they can forge the client
  address. List only the narrow CIDR(s) your actual proxy uses.
- On a **flat LAN with no proxy**, leave `trusted_proxies` empty. The TCP
  peer address is used and there is nothing to forge below L3;
  `trusted_networks` is fine there as long as the network itself is
  trusted.

### Disabled mode behind a proxy: set `trusted_proxies` or enable auth

In the default `disabled` posture LocalSky still guards the privileged
surfaces (config read/write, the wizard's config-write routes, and the
backup download/restore) by **network position**: a request from loopback or
a private/RFC1918/ULA address is trusted to reach them without a login, while
an internet-public source address is refused. This is the "isolated trusted
LAN / behind a guarding proxy" model.

That private-IP trust keys on the **same derived client address** as
everything else, so the proxy caveat above applies here too, and the failure
is worse: if a reverse proxy fronts LocalSky and `trusted_proxies` is **not**
set, every request's client address is the proxy's own (RFC1918) address.
Because that proxy address is private, **every caller now looks
LAN-trusted** and sails through the privileged gate, including a WAN client
the proxy forwarded. The private-IP bypass is effectively defeated.

So if you put any reverse proxy in front of LocalSky, do one of:

- **Set `trusted_proxies`** to your proxy's address/CIDR (then the gate sees
  the real client again, and only genuinely-private clients are trusted), or
- **Enable `auth.mode = "required"`** so the privileged surfaces demand a
  real session or API token regardless of source address.

Either is sufficient; do not rely on the Disabled-mode private-IP trust alone
once a proxy is in the path. (A new wizard install that creates an owner
account turns on `required` automatically, which closes this for you.)

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

> **Minting a token requires an authenticated owner session, even in
> Disabled mode.** Unlike the config/backup surfaces, the token-admin
> endpoints are gated on a *real owner identity*, not on network position: a
> trusted-LAN or loopback caller is not enough. So on a Disabled-mode install
> the Account page's **Create token** needs you to **sign in at `/login`
> first** (with the owner account you created in the wizard, or under
> Settings, then Account). If you have never created an owner account, create
> one before minting tokens; with zero accounts a token cannot be attributed
> and the request is refused.

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
