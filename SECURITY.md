# Security Policy

## Supported Versions

LocalSky is pre-1.0. The current minor release receives security patches; older minors do not.

| Version | Supported |
|---|---|
| Latest 0.x | Yes |
| Anything older | No |

## Reporting a Vulnerability

Please report security issues privately via GitHub Security Advisories:

1. Open the [Security tab](../../security) on the LocalSky repository.
2. Click **Report a vulnerability**.
3. Provide a short description of the issue and steps to reproduce.

Or email the maintainer (address in the repository profile). Please do not file public issues for security-impacting bugs.

## What counts as a security issue

- Authentication bypass on the local web UI
- Remote code execution from data ingested via any source adapter
- Disclosure of secrets (HA tokens, VAPID private keys, controller passwords) in logs or HTTP responses
- Path traversal or arbitrary file read via API endpoints
- SQL injection in any of the persistence stores

## What does not count

- Crashes from malformed config files when the file is operator-supplied
- Behavior changes when a misconfigured controller is pointed at the wrong hardware
- The default state of LOCALSKY_DEMO=1 simulating data

## Response window

Initial response within 5 business days. Best effort to patch and release within 30 days for HIGH severity, 90 days for MEDIUM. CVE assignment via GitHub Security Advisories where applicable.

## Security hardening checklist

Operators running LocalSky in production:

- Run behind a reverse proxy with TLS termination (Caddy, nginx, Traefik)
- Use a non-root container user
- Mount `/data` and `/keys` read-write but everything else read-only
- Rotate the VAPID keypair on operator turnover
- Use a per-deployment HA long-lived token (revoke promptly when retired)
- Avoid exposing the LocalSky port directly to the public internet; if remote access is needed, use Tailscale or WireGuard
