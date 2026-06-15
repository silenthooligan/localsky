# Migrating your watering off Home Assistant

This guide is for people who run irrigation **inside** Home Assistant today,
with integrations like Smart Irrigation, Irrigation Unlimited, the
OpenSprinkler integration, or a vendor cloud (Rachio, Hydrawise, B-hyve),
and want LocalSky to become the watering brain while HA stays the dashboard.

The end state looks like this:

- **LocalSky computes everything**: ET from your weather, per-zone soil
  buckets, skip rules, and the morning schedule.
- **LocalSky talks to your controller directly** (OpenSprinkler, Rachio,
  Hydrawise, B-hyve, Rain Bird, MQTT), so watering works even when HA is
  down.
- **Home Assistant keeps everything it had, through one integration**: the
  LocalSky integration publishes every sensor, zone valve, forecast, and the
  run/stop/pause services as native HA entities.
- The old HA-side irrigation stack is removed, so your HA instance stops
  carrying duplicate logic and orphaned entities.

Nobody's setup is identical; treat the steps as a checklist and skip what
doesn't apply.

## Phase 1: Stand LocalSky up next to what you have

Nothing breaks in this phase; you're adding, not replacing.

1. Install LocalSky (Docker or the binary) and run the setup wizard:
   location, weather sources, your controller, zones.
2. **Controller:** add it natively (the wizard can scan it for stations).
   This does not interfere with an existing HA integration reading the same
   hardware; both can watch it at once.
3. **Sensors:** if some sensors only exist in HA (a Zigbee soil probe, a
   Z-Wave rain gauge), add an HA passthrough source (kind =
   `"ha_passthrough"`) and map those entities. Everything else (Tempest,
   Ecowitt, forecast models) comes in natively. See
   [sensors.md](sensors.md) for a worked example.
4. Install the **LocalSky integration** in HA, following
   [hacs.md](hacs.md). Two gotchas: it is not in the HACS default
   catalog yet, so add `https://github.com/silenthooligan/localsky-ha`
   as a HACS custom repository first; and if your LocalSky has an owner
   account, create an API token in LocalSky (Settings > Account)
   *before* adding the integration, because the config flow asks for it.
   After that, it discovers the instance on your network; entities
   appear immediately.

## Phase 2: Watch them disagree

Run both brains side by side for a few days. LocalSky's Irrigation tab shows
tonight's plan, every zone's verdict, and the "why" behind each number
(Settings has a Simulator and Rule Lab for what-ifs). Compare against what
your HA setup decides. Tune species, soil texture, and sprinkler rates in
LocalSky's zone settings until you trust its plan.

Expect a few settling days before the numbers converge. LocalSky's
per-zone water buckets start at field capacity on a fresh install (the
model assumes the soil starts full), so the first plans can be smaller
than what your old system would water until daily ET draws the buckets
down to their real level. Don't tune against day one; give the model
several days of weather, rain, and recorded runs before comparing
seriously.

While you're watching, make sure the old system is the **only** one with a
live schedule. LocalSky doesn't actuate anything until its controller is
enabled with zones assigned, but it's worth confirming you don't have two
schedulers armed.

## Phase 3: Flip the brain

1. **Disarm the HA-side scheduler first** so nothing double-waters:
   - *Irrigation Unlimited:* turn off the controller master switch
     (`switch.irrigation_unlimited_c1_m`) or set `enabled: false` on its
     schedules.
   - *Smart Irrigation:* disable the automation that applies its duration
     to your valves.
   - *Vendor apps (Rachio/Hydrawise/B-hyve):* disable the schedule in the
     vendor app; leave weather skip features off so they don't fight
     LocalSky.
2. In LocalSky, confirm the controller is enabled and every zone is mapped
   to a station.
3. LocalSky schedules the next morning run automatically; the Irrigation
   tab shows when and why.
4. Watch one full watering cycle. The History tab records every run and
   skip with the reason.

Rollback is symmetric: re-enable the old schedule and disable LocalSky's
controller. Nothing in this guide deletes data until Phase 4.

### When Home Assistant is unavailable

The point of the flip is that HA stops being a single point of failure
for watering. What actually happens during an HA outage depends on which
LocalSky pieces still touch HA:

| Piece | Behavior while HA is down |
|---|---|
| Direct controllers (OpenSprinkler, Rachio, Hydrawise, B-hyve, Rain Bird, MQTT) | Unaffected. LocalSky talks to the hardware itself; schedules run normally. |
| HA passthrough source (kind = `"ha_passthrough"`) | LocalSky polls HA's `/api/states` every 30 seconds. When HA stops answering, the source is flagged unreachable and stops producing readings: the mapped fields simply stop updating, and the engine keeps computing from its remaining sources (your station and forecast models). A zone whose soil probe is an HA entity reads as probe offline and falls back to the modeled water bucket until HA returns. |
| `ha_service_call` controller | Every valve command is an HTTP call into HA. With HA down the dispatch fails: LocalSky logs the failure, abandons that zone's remaining cycle segments, moves on to the next zone, and does not retry until the next scheduled window. Nothing waters through this controller during the outage, which is exactly why this guide moves you onto a direct controller. |

## Phase 4: Clean up Home Assistant

Once you trust LocalSky, remove the old stack so HA stops carrying noise.
Order matters: dashboards first, then integrations, then leftovers.

1. **Repoint dashboards and automations.** Anything referencing the old
   integration's entities (zone switches, "running" sensors, duration
   numbers) has a LocalSky equivalent entity now. Swap references before
   removing integrations so tiles don't break.
2. **Remove the integrations.** Settings > Devices & services: remove the
   Smart Irrigation / OpenSprinkler / vendor config entries. For
   YAML-configured Irrigation Unlimited, delete its YAML block and restart.
3. **Remove the HACS components.** HACS > installed: remove Smart
   Irrigation, Irrigation Unlimited, and their dashboard cards (e.g.
   irrigation-unlimited-card) if nothing else uses them.
4. **Sweep for orphans.** Settings > Entities, filter by the old
   integration names; HA marks removed integrations' leftovers as
   unavailable. Remove them. Developer tools > Statistics also lists
   orphaned long-term statistics you can purge.

   > **Purging statistics is irreversible.** Once you delete an
   > entity's long-term statistics, years of recorded history for that
   > entity are gone with no undo. If any of it matters (seasonal water
   > usage comparisons, ET history), export it first, or just leave the
   > orphans; they cost almost nothing.

5. **Keep**: the LocalSky integration, and the HA passthrough source
   only if it still feeds sensors that exist nowhere else.

## What about the controller's own HA integration?

After the flip, an OpenSprinkler/Rachio/Hydrawise HA integration is
redundant: LocalSky publishes the same zones and state, and having two
write paths to the hardware invites conflicting commands from old
dashboard buttons. Recommended: repoint dashboards to the LocalSky
entities and remove the controller's HA integration. Keep it only if you
have automations that talk to controller features LocalSky doesn't expose.

## Quick mapping reference

| You had | LocalSky equivalent | Where it's documented |
|---|---|---|
| Smart Irrigation ET calculations | Native ET engine (FAO-56, per-zone buckets) | [irrigation-engine.md](irrigation-engine.md) |
| Smart Irrigation seasonal adjustment | Kc curves per species + the engine's heat multiplier | [zone-math.md](zone-math.md) |
| Irrigation Unlimited schedules | Smart-morning scheduler + per-zone budgets | [irrigation-engine.md](irrigation-engine.md) |
| Irrigation Unlimited sequences | The morning run is a sequence: zones dispatch one after another, with cycle-and-soak splitting per zone | [irrigation-engine.md](irrigation-engine.md) |
| Multiple schedules per zone | Manual schedules alongside the smart scheduler, plus per-zone weekly budget and sessions-per-week | [configuration.md](configuration.md) |
| HA automations for rain skip | Skip rules + Rule Lab (Settings > Logic) | [skip-rules.md](skip-rules.md) |
| Vendor app weather skip | Forecast-aware verdicts, visible per zone | [verdict-strip.md](verdict-strip.md) |
| Rain delay button | Pause/resume: the dashboard pause control or `localsky.pause` / `localsky.resume` from HA | [hacs.md](hacs.md#service-reference) |
| Manual-run services / scripts | `localsky.run_zone` and `localsky.stop_zone` services, or open the zone's valve entity | [hacs.md](hacs.md#service-reference) |
| Zone switches in HA | `valve.<zone>` via the integration (a legacy switch shim exists, disabled by default) | [hacs.md](hacs.md#per-zone) |
| "Is it running" sensors | Per-zone running `binary_sensor` via the integration | [hacs.md](hacs.md#per-zone) |
