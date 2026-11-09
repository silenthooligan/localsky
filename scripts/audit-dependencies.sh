#!/bin/sh
# One policy for private deployment, release preflight and public CI.
# Existing exceptions have no direct upgrade path:
# - rsa: VAPID/web-push dependency, no patched release.
# - webpki 0.102 / pemfile: rumqttc 0.25's pinned TLS dependencies
#   (RUSTSEC-2026-0382 added 2026-10-23: rumqttc 0.25.1 is the newest
#   release and requires rustls-webpki ^0.102.8 itself; the fix is in
#   0.103.16. Affects MQTT TLS only. Drop when rumqttc moves to 0.103).
# - crypto-bigint 0.5 (RUSTSEC-2026-0387 added 2026-11-08): web-push 0.11.0
#   is the newest release and requires jwt-simple ^0.12, which pins
#   elliptic-curve 0.13 and crypto-bigint 0.5; the fix is in the 0.7 line
#   (0.7.6+), which the direct p256 0.14 path already uses. Affects VAPID
#   signing for web push only. Drop when web-push moves to jwt-simple 0.13+.
# - paste / proc-macro-error2: unmaintained Leptos internals.
# - smartstring: unmaintained dependency of rhai 1.25.
# Revisit these when their parent dependency changes. New findings fail.
# Yanked versions are reported separately; they are not a vulnerability waiver.
set -eu
exec cargo audit \
  --deny unmaintained --deny unsound \
  --ignore RUSTSEC-2023-0071 \
  --ignore RUSTSEC-2026-0104 \
  --ignore RUSTSEC-2026-0098 \
  --ignore RUSTSEC-2026-0049 \
  --ignore RUSTSEC-2026-0099 \
  --ignore RUSTSEC-2025-0134 \
  --ignore RUSTSEC-2024-0436 \
  --ignore RUSTSEC-2026-0173 \
  --ignore RUSTSEC-2026-0249 \
  --ignore RUSTSEC-2026-0382 \
  --ignore RUSTSEC-2026-0387
