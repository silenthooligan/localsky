# Accounts and API tokens

Create an owner account during setup or in **Settings > Account**. LocalSky stores the account, sessions, and API-token hashes in its database. The configuration file controls access policy.

## Choose an access policy

| Mode | Behavior |
|---|---|
| `required` | App access requires a session, an API token, or a configured trusted-network exception. Creating an owner in the setup wizard enables this mode. |
| `disabled` | Ordinary app access is open. Sensitive configuration, backup, and restart routes still apply a credential or network trust check. |

Use required authentication when other people can reach the instance. An API token is an operator credential; LocalSky does not currently provide a read-only token scope.

```toml
[auth]
mode = "required"
session_ttl_days = 30
trusted_networks = []
trusted_proxies = []
```

## Create a token for an integration

1. Sign in as the owner at **/login**.
2. Open **Settings > Account** and create a named token.
3. Copy the token into the integration's secret storage. The plaintext is shown once.
4. Revoke it from the same page when the integration no longer needs access.

Creating and revoking tokens requires a real authenticated owner identity, even with authentication disabled. Access from a trusted LAN alone is insufficient.

Send the token in a header:

```http
Authorization: Bearer lsk_your_token
```

SSE endpoints also accept `access_token` in the query string when a client cannot set headers. This works only on paths ending in `/stream`; URLs can appear in logs, so prefer headers or the browser session. See [live updates](api-streams.md).

## Trusted networks and proxies

`trusted_networks` grants a login exception to matching client addresses. `trusted_proxies` identifies machines allowed to supply the client's address. They serve different purposes.

LocalSky uses the TCP peer by default. Only a peer in `trusted_proxies` can supply `X-Forwarded-For`. LocalSky reads that chain from the right, skips trusted proxy hops, and uses the first untrusted address.

Use the narrow address range of your actual proxy. Do not trust an entire LAN or Docker network if untrusted clients can connect from it. Without correct proxy configuration, clients can share the proxy's identity and rate-limit bucket. Disabled mode also refuses its normal private-peer privilege shortcut when forwarding headers appear without a declared proxy.

[Reverse proxy setup](reverse-proxy.md) includes matching examples.

## An authenticating proxy

An existing identity gateway can authorize privileged operations through a header:

```toml
[auth]
trusted_proxies = ["127.0.0.1/32"]
proxy_auth_header = "X-Auth-Request-Email"
proxy_auth_allow = ["you@example.com"]
```

The direct peer must be a trusted proxy, and the proxy must overwrite incoming copies of that header. The allow-list is case-insensitive; an empty list accepts any non-empty identity supplied by the trusted proxy.

This identity supports privileged configuration, backup, and restart routes. It does not replace the normal session/token requirement throughout required mode or grant API-token administration.

## Public endpoints

The pairing probe `/api/v1/info`, login/setup entry points, static assets, and bundled docs are available before login. Anonymous health requests receive reduced detail. Hardware ingest and `/metrics` also have public routes; limit their reach at the network or proxy. Complete first-time setup before exposing an installation.

## Recover a lost owner account

If you still have a valid session, manage the account there. Otherwise, stop LocalSky and preserve a complete [backup](backup-restore.md) before changing its database. Owner recovery removes existing sessions and tokens, so integrations will need new credentials.

For an installation you physically administer, with LocalSky stopped:

```bash
sqlite3 /opt/localsky/data/irrigation.db \
  "BEGIN; DELETE FROM auth_sessions; DELETE FROM api_tokens; DELETE FROM users; COMMIT;"
```

Restart on a trusted network, recreate the owner, and verify required authentication before restoring external access. Keep the original database copy until access and history are confirmed.
