# LocalSky as an HACS integration (roadmap)

This document describes a *future, separate project*: a Python-side HACS integration that exposes LocalSky as a native Home Assistant integration. Distinct from LocalSky itself.

LocalSky's current relationship with HA has two production paths:

- **Mode 2 (outbound)**: LocalSky publishes via MQTT discovery; HA auto-discovers `sensor.localsky_*` entities. Recommended today.
- **Mode 3 (HA-driven)**: LocalSky dispatches controller actions through HA's service-call API. Legacy continuity for users already running HA-driven irrigation.

A third path, **HACS integration**, would let HA users get LocalSky as a first-class integration installable from the HACS marketplace, without MQTT in the middle.

## What HACS is

The [Home Assistant Community Store](https://hacs.xyz/) is HA's marketplace for community-built integrations and custom dashboards. Users install HACS once, then add individual integrations through it. Each integration is a small Python project conforming to HA's `DataUpdateCoordinator` + `Entity` patterns.

## What a LocalSky HACS integration would do

The integration polls LocalSky's REST API and creates HA entities natively. Conceptual flow:

```
[HA running on user's host]
    |
    v (HACS-installed Python custom_component)
[LocalSky HACS integration]
    |
    v (HTTP polls every 30s)
[LocalSky REST API on the same LAN]
    |
    v
[/api/snapshot, /api/irrigation/snapshot, /api/forecast/snapshot]
```

The integration would create HA entities matching the MQTT discovery layout LocalSky publishes today:

- `sensor.localsky_<zone>_bucket_mm`
- `sensor.localsky_<zone>_et_today_mm`
- `sensor.localsky_<zone>_planned_seconds`
- `binary_sensor.localsky_<zone>_running`
- `sensor.localsky_verdict_today`
- `sensor.localsky_<zone>_soil_moisture` (when sensors connected)
- One device per LocalSky deployment

Plus actions: `service.localsky.run_zone(zone, seconds)`, `service.localsky.stop_zone(zone)`, `service.localsky.stop_all()`.

## Why this path makes sense

The MQTT discovery path (Mode 2) requires:

- A working MQTT broker (operator runs Mosquitto or uses HA's built-in)
- The HA MQTT integration configured
- The right discovery prefix set in LocalSky

HACS would skip all of that. Click "Add Integration" → "LocalSky" → enter the LocalSky URL → done. HA polls REST; entities appear. No broker, no discovery prefix, no MQTT debugging.

It's also a place to surface LocalSky-specific affordances that don't translate cleanly to MQTT entities:

- Run-history Gantt as an HA custom card
- The 7-day verdict strip as a Lovelace UI element
- Native HA service calls that map 1:1 to LocalSky's REST control endpoints

## Project shape

The HACS integration is a **separate Python project**, in its own repository:

- Suggested name: `homeassistant-localsky`
- Repository: e.g. `github.com/silenthooligan/homeassistant-localsky`
- Language: Python 3.11+ (matches HA's current version)
- License: Apache-2.0 (same as LocalSky)
- Size: ~300-500 LOC of Python (HA integrations are small)

Key files (matches HA's custom component layout):

```
custom_components/localsky/
├── __init__.py            # entry point, DataUpdateCoordinator setup
├── manifest.json          # HA + HACS metadata
├── config_flow.py         # UI config flow (host/port/optional API key)
├── const.py               # domain name, scan interval, default ports
├── coordinator.py         # polls /api/snapshot + /api/irrigation/snapshot
├── sensor.py              # sensor.localsky_* entity classes
├── binary_sensor.py       # binary_sensor.localsky_* entity classes
├── services.yaml          # service definitions for run_zone / stop_zone
├── services.py            # service handler implementations
└── strings.json           # localized UI strings
hacs.json                  # HACS-side metadata
README.md                  # install instructions, screenshots
```

## Why not build it inside LocalSky's repo?

Three reasons:

1. **Different release cadence**: HA integrations need to track HA's quarterly version. LocalSky shouldn't be coupled to that schedule.
2. **Different runtime**: LocalSky is Rust + WASM. The HACS integration is Python. Mixing the two in one repo complicates CI without payoff.
3. **Different audience**: HACS users want the "click install" experience. LocalSky users want "single Docker container." Splitting repos keeps the two stories crisp.

The HACS integration depends on LocalSky's REST API ([docs/api.md](api.md)) being stable. Once LocalSky tags v1.0 (API stable), the HACS project becomes a viable side project.

## Prerequisites for shipping HACS

Before the HACS project can be useful:

- [ ] LocalSky API stabilized at v1.0 with semver guarantees on the JSON wire format
- [ ] `/api/health` endpoint reliable for the coordinator's "is the host up?" check
- [ ] `/api/irrigation/snapshot` schema documented in OpenAPI / JSON Schema (already published at `/api/config/schema`, planned for the snapshot endpoints too)
- [ ] Stable controller dispatch via `POST /api/irrigation/action` (today; need to verify the wire shape is final)

When those are in place: a Python developer who knows HA's `DataUpdateCoordinator` pattern can ship the HACS integration in ~1-2 weekends.

## Who builds it?

Not LocalSky's maintainers. The Rust + agronomy + meteorology surface is enough work for the upstream team. The HACS integration is a perfect community contribution: low-risk Python in a well-trodden HA pattern, with a clear consumer (HA users who want LocalSky native).

If you'd like to build it, see [CONTRIBUTING.md](../CONTRIBUTING.md) on cross-project work and open a discussion on the main LocalSky repo to coordinate.

## What about a custom dashboard card?

Separate but related: Lovelace custom cards (also distributed via HACS) for LocalSky-specific UI elements:

- `localsky-verdict-strip-card`: renders the 7-day forward verdict strip in Lovelace
- `localsky-zone-card`: a single-zone status card with bucket bar + planned-run countdown
- `localsky-history-gantt-card`: 30-day run history Gantt

These plug into the HACS integration's entities. Build them as a separate project (`hacs-localsky-cards` or similar) using lit-html or vanilla web components.

## Roadmap relationship

| LocalSky version | HACS dependency status |
|---|---|
| 0.1 | not viable yet (API wire format not stable) |
| 0.2 | API stabilizes; HACS project can start |
| 0.5 | HACS integration alpha, community-tested |
| 1.0 | LocalSky tags 1.0; API semver-locked; HACS integration matures |

## See also

- [docs/api.md](api.md): the REST surface the HACS integration would call
- [docs/standalone.md](standalone.md): comparison of all integration modes
- HACS upstream documentation: https://hacs.xyz/docs/publish/start
- Home Assistant custom integration documentation: https://developers.home-assistant.io/docs/creating_component_index
