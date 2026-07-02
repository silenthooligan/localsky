# LocalSky Threat Model & Security Model

This document describes LocalSky's security posture so an operator can deploy it
safely and a security researcher can orient quickly. For vulnerability reporting
and the hardening checklist, see [SECURITY.md](SECURITY.md).

LocalSky is a **self-hosted, LAN-first** irrigation controller and weather app.
It is designed to run on a trusted home network (often behind a reverse proxy or
inside Home Assistant OS ingress), not to be exposed directly to the public
internet. The design choices below follow from that intended deployment.

## 1. Deployment postures (`auth.mode`)

There are two postures. The shipped default is **Disabled**.

| Posture | `auth.mode` | Who can use the read/write API | Intended deployment |
|---|---|---|---|
| **Disabled** (default) | `disabled` | Anyone who can reach the port on the LAN; the *internet-anonymous* caller is still refused on privileged paths (see §3) | Trusted LAN, or behind a reverse proxy / HA-OS ingress that owns authn |
| **Required** | `required` | Only an authenticated owner session or API token | Direct LAN exposure where per-user login is wanted |

Disabled mode is **not** "no security": it is "the LAN is the trust boundary." The
privileged paths in §3 are gated in *both* modes; what Disabled relaxes is the
ordinary read/write API for callers whose source IP is on the local network.

## 2. Network-position admission (`privileged_caller_vouched`)

For the privileged gate, a credential-less caller is vouched for by network
position alone as follows (`src/auth/middleware.rs`):

- **Loopback** or a configured `auth.trusted_networks` CIDR: always vouched, in
  both postures (the same-box owner / explicit-trust case).
- In **Disabled** mode only: any private/LAN address (RFC1918 / ULA / loopback)
  is vouched. This is the "isolated trusted LAN / behind a reverse proxy" model
  and is exactly what a fresh-install LAN owner sends while driving the wizard.
  Only the **internet-anonymous** caller is refused.
- In **Required** mode the bar rises: only an explicit `trusted_networks` /
  loopback match passes without a credential; a bare private IP no longer
  suffices.

## 3. Privileged paths (gated in every posture)

These routes refuse the anonymous-internet caller regardless of `auth.mode`,
because they actuate hardware or read/write configuration and secrets
(`is_privileged_path`, `is_wizard_config_write`):

- `POST/PUT/DELETE /api*/config*` (config writes) and `GET /api*/config/raw`
  (full-surface TOML read).
- `POST /api*/irrigation/action` (run / stop / pause / skip a **physical
  valve**). `POST /api*/irrigation/simulate` is a read-only dry-run preview and
  is intentionally open.
- The wizard's alternate config-write surface: `POST /api*/wizard/apply`,
  `PUT/DELETE /api*/wizard/draft`, `POST /api*/wizard/seed_current`. The wizard
  probe/test/scan/discover endpoints keep a separate LAN onboarding guard and are
  *not* in this set.
- All `/api*/backup*` routes in every method (the download alone exfiltrates
  config + the SQLite DB).

A stricter gate applies to **token administration** (`is_token_admin_path`):
`POST /api*/auth/tokens` (mint) and `DELETE /api*/auth/tokens/{id}` (revoke)
require a *real owner credential* even in Disabled mode. (`GET .../tokens` returns
metadata only, no secrets, and stays on the ordinary path.)

## 4. CSRF / cross-origin (`origin_allowed`)

State-changing requests carrying a browser `Origin` are accepted only when the
Origin is **same-origin** (host equals the request Host) or matches an entry in
`auth.trusted_origins` (full origin or bare host). `"null"` and malformed Origins
are rejected. This blocks a malicious web page on the user's LAN from driving the
API cross-origin even in Disabled mode.

## 5. Trusted proxy / `X-Forwarded-For`

Client IP is resolved with `auth.trusted_proxies` (`client_ip_parts`): the peer
must itself be a trusted proxy before any `X-Forwarded-For` is honored, and then
the **rightmost** hop not in `trusted_proxies` is taken as the client. An
untrusted peer's spoofed XFF header is ignored. Misconfiguring `trusted_proxies`
to include untrusted networks would let a client spoof its source IP, so keep it
to the actual reverse-proxy addresses.

## 6. Outbound requests: the deliberate LAN-SSRF stance (`net::safe_fetch`)

Every operator-overridable outbound client is built through
`safe_fetch::build_safe_client`, which parses the target, resolves the host, and
**pins the connection to the resolved IP**, refusing loopback and private/ULA
ranges (`is_forbidden_target`). So an operator-supplied base URL (Tuya/YoLink
region, InfluxDB/Prometheus/REST poller, HA passthrough, Davis WLL, WeatherKit,
Ecowitt gateway) that resolves to a private/loopback address is refused before
any bytes are sent, preventing a config field from becoming an SSRF primitive.

This is a **deliberate, asymmetric** stance: outbound clients pointed by config
at the LAN are *forbidden*, while inbound LAN ingest is *open* (§7). LAN sensor
gateways (Ecowitt, Tempest, MQTT) reach LocalSky inbound; LocalSky does not need
to reach arbitrary private hosts outbound. The one intended exception is the
local irrigation controller, addressed through the controller adapter layer, not
the generic source-fetch path.

## 7. Open-by-default ingest (`/ingest/*`)

The `/ingest/*` POST receivers (push-style sensor sources) are **open by
default**: a LAN sensor gateway can post readings without authenticating, because
many such gateways cannot present a credential. An optional per-source secret
(`secret_matches`) gates a source when configured. Ingest bodies are size-capped
(LS-API-09). Treat the LAN as the trust boundary for ingest; if that is not
acceptable, set per-source secrets or front the port with an authenticating
proxy.

## 8. Static assets / `/pkg/*`

The hydration WASM/JS/CSS under `/pkg/*` is served anonymously (and with the
crossorigin attributes browsers require) in both postures, so the SPA can boot
before any login. These are public build artifacts containing no secrets; gating
them would break hydration. Responses are compressed (brotli/gzip) transparently;
the live SSE stream (`text/event-stream`) is excluded from compression.

## 9. Secrets

Controller passwords, HA long-lived tokens, and the VAPID private key live in the
operator's config / mounted `/keys` (read-only). They are redacted from the
`/api/config/raw` response body and never logged. The public source mirror is
secret-scanned and sanitized before publication (see the release process); the
internal canonical repo intentionally tracks deployment secrets in git and is not
public.

## 10. Out of scope / known limitations

- **Operator-supplied config crashes** (malformed TOML) are not security issues.
- **Multi-tenant isolation**: LocalSky is single-deployment; there is one owner
  trust domain. Run a second instance for a second site.
- **HTTP controller adapters cannot be conformance-tested offline** because
  `safe_fetch` forbids loopback; they rely on per-adapter response-parsing tests
  (see `controllers/conformance.rs`).
- **Direct public-internet exposure is unsupported**: use Tailscale/WireGuard or
  an authenticating reverse proxy for remote access. A first-request-from-public
  posture warning is planned.

## Reporting

See [SECURITY.md](SECURITY.md) for private disclosure via GitHub Security
Advisories.
