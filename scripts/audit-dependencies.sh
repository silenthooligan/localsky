#!/bin/sh
# One policy for private deployment, release preflight and public CI.
# Existing exceptions have no direct upgrade path:
# - rsa: VAPID/web-push dependency, no patched release.
# - webpki 0.102 / pemfile: rumqttc 0.25's pinned TLS dependencies.
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
  --ignore RUSTSEC-2026-0249
