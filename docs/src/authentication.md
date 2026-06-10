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

- `disabled`: the pre-auth behavior. Right when a reverse proxy already
  guards access, or on an isolated trusted network.
- `required`: the UI redirects to `/login`; API calls need a session
  cookie or an API token. New wizard installs that create an owner
  account get this automatically.
- `trusted_networks`: lets the home LAN stay frictionless while VPN/WAN
  clients must sign in. Each entry is a CIDR matched against the client
  address (first `X-Forwarded-For` hop when present, so set your proxy
  up to send it).

## What stays public

These paths never require credentials, by design:

| Path | Why |
|---|---|
| `/pkg/*`, `/sw.js`, root static assets | Compiled assets; browsers fetch them without credentials |
| `/api/v1/info` | Pairing probe; carries `auth_required` so clients know to ask for a token |
| `/login`, `/api/v1/auth/{status,login,setup}` | The way in |
| `/ingest/*` | Weather hardware (Ecowitt consoles, webhooks) cannot authenticate |
| `/api/v1/health` | Liveness for Docker healthchecks; anonymous callers get a trimmed body (status/version/uptime only) |
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
clients that cannot set headers (the browser EventSource sends the
session cookie automatically, so this is only for external consumers).

## Lockout recovery

If you lose the owner password, stop the container and delete the
`users` rows from the database, then restart and re-run account creation:

```bash
sqlite3 /path/to/data/irrigation.db "DELETE FROM auth_sessions; DELETE FROM api_tokens; DELETE FROM users;"
```

Physical access to the data volume is the trust anchor, the same as
Home Assistant's.
