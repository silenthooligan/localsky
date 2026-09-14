# Migrating your watering off Home Assistant

This guide is for people who run irrigation **inside** Home Assistant today,
with integrations like Smart Irrigation, Irrigation Unlimited, the
OpenSprinkler integration, or a vendor cloud (Rachio, Hydrawise, B-hyve),
and want LocalSky to become the watering brain while HA stays the dashboard.

The end state looks like this:

- **LocalSky computes everything**: ET from your weather, the weekly
  per-zone water balance, skip rules, and the morning schedule. Nothing it
  decides is read from Home Assistant. As of 0.7.22 that includes the skip
  thresholds and the four operator controls, which used to live in
  `input_*` helpers; see [Upgrading to 0.7.22: your helpers stop
  deciding](#upgrading-to-0722-your-helpers-stop-deciding) below.
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

## Upgrading to 0.7.22: your helpers stop deciding

Seven Home Assistant helpers were still deciding when and whether your yard
watered. 0.7.22 reads each of them one last time, writes the value into
LocalSky's own storage, and stops reading the entity.

| Helper | What it decided | Where it lives now |
|---|---|---|
| `input_number.irrigation_max_wind_mph` | The wind skip threshold | Settings > Skip rules |
| `input_number.irrigation_min_temp_f` | The cold skip threshold | Settings > Skip rules |
| `input_number.irrigation_rain_skip_in` | The rain skip threshold | Settings > Skip rules |
| `input_datetime.irrigation_pause_until` | The timed pause (Rain delay on the irrigation page) | LocalSky's own storage, set and released under Rain delay |
| `input_select.irrigation_override_tomorrow` | Tomorrow's one-day override | LocalSky's own storage |
| `input_boolean.irrigation_pause` | The pause switch (the Vacation pause toggle on the irrigation page) | LocalSky's own storage, set and released from the Vacation pause toggle |
| `input_boolean.irrigation_dry_run` | Dry-run mode | LocalSky's own storage |

**You do not have to do anything, and the value LocalSky takes is the value
that was already deciding**, so the first morning after the upgrade decides as
the morning before it did. There are two exceptions, both spelled out below
and both named on screen. A threshold helper set outside the range LocalSky
can hold moves to the nearest value LocalSky can hold, and the migration
notice names it and prints both numbers. And a vacation pause, pause switch or
dry run already sitting in LocalSky's own storage from a standalone era starts
deciding again, because a Home Assistant deployment stored those and never
read them; check Rain delay, the Vacation pause toggle and Dry run on the
irrigation page after upgrading.

The three `input_number` helpers are worth understanding, because they
**outranked** the matching thresholds in Settings whenever the helper existed.
If your Settings page showed 10 mph while the helper held 12, the number
deciding was 12 and the Settings page was decorative. After the upgrade
Settings holds 12, LocalSky still uses 12, and editing Settings finally works.
The irrigation page raises a one-time notice naming every entity, what
LocalSky uses now, and both numbers wherever the two disagreed.

**Nothing is deleted from Home Assistant.** All seven helpers stay where they
are, and they stop doing anything. Turning `input_boolean.irrigation_pause`
on will not pause watering. **If an automation writes to any of them, point it
somewhere else or it will stop having an effect with nothing to show for it:**

- The three thresholds are `number` entities the LocalSky integration already
  publishes. Write those instead. LocalSky accepts 0 to 50 mph, 20 to 70 F and
  0 to 10 inches, and refuses anything outside that with a message naming the
  range. The shipping integration builds the sliders from fixed ranges of its
  own (0 to 50 mph, 20 to 60 F, 0 to 1 in), all inside what LocalSky accepts;
  from manifest schema 1.6 an integration can take the bounds from LocalSky
  instead.
- The pause, the one-day override and dry run have no entity of their own yet.
  Use `POST /api/irrigation/action` with an API token (see
  [api.md](api.md#irrigation-control-endpoints)).

Once you have repointed anything that wrote them, deleting the helpers is safe
and changes nothing. As of 0.9.0 LocalSky reads none of the seven on any
install: the one-time migration pass that copied their values across is gone
along with the reads it gated. If your install never ran that pass (it
upgraded straight from a release before 0.7.22, or ran without a persistence
database), the values those helpers held did not carry over. Set the pause,
the one-day override, dry run and the three thresholds in LocalSky; the
migration notice names any control that was never recorded.

Some specifics worth knowing:

- **A threshold helper set outside the range LocalSky can hold moves to the
  nearest value it can hold.** LocalSky holds 0 to 50 mph, 20 to 70 F and 0 to
  10 inches. Setting `input_number.irrigation_max_wind_mph` to 99 is how
  people switch the wind gate off, and a helper's own maximum is whatever you
  gave it, so nothing stopped one going past LocalSky's range. Whatever it
  held was the number deciding, because the helper outranked Settings.
  It becomes 50 mph, which still means effectively never wind-skip; reverting
  it to the Settings value would have started skipping on the first breezy
  morning. The migration notice names any threshold this happened to and
  prints what the helper held beside what LocalSky is using.
- **A helper that is missing, or holding something that is not a number or a
  mode at all, is never adopted.** It is recorded by name, LocalSky keeps what
  it had, and the read is retired anyway. For the three thresholds that moves
  nothing: a missing or unreadable helper already resolved to the Settings
  value, which is the value LocalSky goes on using.
- **The four controls are handled more carefully, because for them an absence
  is not the same as the value they hold.** A control Home Assistant reports
  as `unavailable` or `unknown` is left alone: LocalSky keeps reading it and
  tries again later. That state means the helper exists and is briefly broken,
  which is what a helpers reload or a restore from backup looks like, and it
  reads the same on every poll, so waiting for a steady answer proves nothing
  about it. A control that is simply absent is concluded absent only after
  Home Assistant has answered identically, with an unchanging entity count,
  for five minutes. **A vacation pause set in
  `input_datetime.irrigation_pause_until` is not dropped by an upgrade that
  lands in the middle of a Home Assistant restart.**
- **A control LocalSky already holds its own answer for keeps that answer.**
  An install that ran standalone and later gained `HA_URL` can still have old
  helpers sitting in Home Assistant; what you set in LocalSky is the more
  recent answer, so it wins and the helper is retired. This is the one place a
  control that was not deciding starts deciding: a Home Assistant deployment
  stored that value and never read it. A vacation pause counts as an answer
  only while it is still running, so an expired one is not kept and the
  helper's pause is taken instead. The migration notice names every control
  this happened to, and says plainly when the result is that watering is held
  and where to release it.
- **Home Assistant being down during the upgrade holds nothing and stops
  nothing.** The yard waters on the values it already has, exactly as in any
  other outage. LocalSky simply waits.
- **The one-day override no longer needs your midnight automation.** LocalSky
  expires it at your own local midnight, stamping the day it was set on in
  your configured timezone. You can delete that automation. An override
  already sitting in LocalSky's own storage when you upgrade reads as no
  override: it predates the day stamp, so there is no day it can honestly
  claim. Set it again if you still want it. One taken from
  `input_select.irrigation_override_tomorrow` by the migration is stamped with
  the day it was read, so it applies to that day and expires that midnight.
- **A Skip or Force set from the Override control still does not take effect
  on a Home Assistant deployment, and this release does not change that.**
  The control writes it to LocalSky's own storage and the Home Assistant
  snapshot builder fills the same field with "auto" on every tick, so the
  engine never sees it: the panel shows Skip while the yard waters. The defect
  predates this release, nothing stored in those two controls starts deciding
  here, and it is fixed on its own. Standalone installs are unaffected.
- **With no persistence database mounted**, the three thresholds still migrate
  and the four controls do not, because a control needs somewhere to be kept.
  Their helpers keep working, and the migration notice names them and says not
  to delete them. Mount `/data` and restart to finish.
- **The migration record survives every way a config can be written.**
  Rolling `localsky.toml` back to a snapshot taken before the upgrade restores
  the values in that snapshot and leaves the record in place, and so do saving
  in Settings, the raw config editor, restoring a backup taken before the
  upgrade, and re-running the setup wizard. The helpers do not go back in
  charge on any of them. That is deliberate: this release tells you the
  helpers are inert and invites you to delete them, so pointing a read back at
  one you have since deleted would read a live vacation pause as no pause. A
  backup restore is where that would have been worst, because the restored
  config takes effect immediately while the restored database only loads at
  the next restart.
- The record of what happened is in `localsky.ledger.toml` beside the
  config (0.9.0 moved it there from `[[ha_adoption]]` in `localsky.toml`),
  permanently: entity, the value taken, the value it replaced, what the helper
  held if it had to be moved into range, and when.
- **A power cut in the middle of the migration cannot lose a value.** The
  control values are written to LocalSky's database and flushed to disk before
  the record is written to `localsky.toml`. A machine that dies between the two
  comes back with the values written and nothing recorded, so the next poll
  redoes exactly the same writes from the same reading and records them then.
  The reverse order would retire a read whose value never landed.

### Reads that stay

- **A zone's soil sensor.** A zone you pointed at a Home Assistant entity
  still reads that entity. That is a sensor you named, not a decision being
  outsourced.

That is the whole list. Two reads that older releases documented here are
gone in 0.9.0: the four legacy soil names (`sensor.<zone>_soil_moisture` for
`back_yard`, `front_yard`, `side_yard` and `back_yard_shrubs`) went with the
environment-variable zone list they served, and the nine
`sensor.open_meteo_*` REST sensors are not consulted because every forecast
figure, today's modelled rain included, now comes from LocalSky's own
forecast. You can delete all of them.

## Upgrading to 0.7.22: your zones may start watering

Read this before you upgrade if LocalSky is already talking to Home
Assistant.

Until 0.7.22, run lengths on a Home Assistant deployment were sized by a
Smart Irrigation entity (`sensor.smart_irrigation_<zone>`) and by nothing
else. If you do not have that HACS integration, LocalSky read the absent
entity as a zero deficit, planned zero minutes on every zone, and
dispatched nothing, on every morning since you installed it. Your yard has
been watering on whatever else you had, or not at all.

0.7.22 sizes runs from LocalSky's own weekly water budget on every
deployment, so those zones start watering on the first morning after the
upgrade.

**If you never set a zone's weekly target and sessions per week, LocalSky
infers them from the zone's species**: 1.00 inches a week over two sessions
for warm-season turf, scaled by each species' own peak crop coefficient, so
established shrubs start at 0.55 inches over one session and a vegetable bed
at 1.15 inches over two. Each session is held to the zone's maximum
run time, 60 minutes unless you changed it, and on a default zone the first
eligible morning lands on that ceiling.

If you had tuned `input_number.irrigation_<zone>_weekly_budget_in` or
`input_number.irrigation_<zone>_sessions_per_week` in Home Assistant, those
two values are not carried across: LocalSky stops reading them and uses the
zone's own config, or the inferred default. Enter them as **Weekly target**
and **Sessions per week** on the zone.

Do this before the next morning window:

1. Open **Settings**, then **Zones**, open each zone and set **Weekly
   target** and **Sessions per week**; blank shows the inferred default in
   the box. Check **Max run time** there too.
2. If you want everything held while you review, set **Rain delay** on the
   irrigation page first, long enough to cover the review. On a Home Assistant
   deployment that writes `input_datetime.irrigation_pause_until`, which this
   release adopts with the pause intact, so the hold survives the upgrade;
   release it under Rain delay afterwards rather than in Home Assistant. Do
   not use the Override control for this on a Home Assistant deployment: a
   Skip stored there decides nothing, as the Override control bullet above says.

The Irrigation page and the Zones page raise a one-time notice listing every
zone that is watering on an inferred target and the target it will use, and
the zone list under Settings marks the same zones. Dismiss the notice once
every zone carries a target you set. On a Home Assistant deployment LocalSky
also logs a warning naming those zones the first time after a start that it
plans a run for one, and sends one push notification to subscribed devices.


## Phase 1: Stand LocalSky up next to what you have

Nothing breaks in this phase; you're adding, not replacing.

1. Install LocalSky (Docker or the binary) and run the setup wizard:
   location, weather sources, your controller, zones.
2. **Controller:** add it natively (the wizard can scan it for stations).
   This does not interfere with an existing HA integration reading the same
   hardware; both can watch it at once.
3. **Sensors:** if some sensors only exist in HA (a Zigbee soil probe, a
   Z-Wave rain gauge), add an HA passthrough source (kind =
   `"ha_passthrough"`) and map those entities. Supported hardware can also be
   read directly. If HA already reads your Tempest, choose the
   [HA WeatherFlow path below](#keeping-weatherflow-in-home-assistant), especially
   when both applications share a host. See [sensors.md](sensors.md) for mappings.
4. Install the **LocalSky integration** in HA, following
   [hacs.md](hacs.md): search for LocalSky in HACS and install it. One
   gotcha: if your LocalSky has an owner account, create an API token in
   LocalSky (Settings > Account) *before* adding the integration,
   because the config flow asks for it.
   After that, it discovers the instance on your network; entities
   appear immediately.

### Keeping WeatherFlow in Home Assistant

LocalSky can use HA's WeatherFlow sensor readings while HA keeps receiving
Tempest broadcasts. The WeatherFlow integration reads the station; LocalSky's
**HA passthrough** source reads its sensor entities through HA's REST API.
The optional LocalSky integration in HA exports LocalSky's results in the
opposite direction. Do not map those LocalSky exports back into its inputs.

1. In **Settings > Devices**, disable or remove LocalSky's Tempest UDP source.
   Its socket closes on the next configuration check, normally within 15 seconds.
   This also persists across restart; LocalSky does not require a direct listener.
2. If HA's WeatherFlow integration failed because the port was occupied, reload
   that integration after LocalSky releases it. Confirm HA's station sensors update.
3. Add an **HA passthrough** source in LocalSky. Enter the HA base URL and a
   long-lived token. Use **Field mappings** to select each reading and enter
   the corresponding WeatherFlow sensor entity ID. HA's declared units are
   converted automatically; do not convert them a second time.
4. In **Settings > Devices**, find **Which source provides each reading** and
   choose that HA source for the desired readings.
   Per-reading preferences and ordered fallbacks express your intent directly.
   The configured source priority controls automatic ordering among eligible
   sources. The generic add-source form starts at **50**; legacy environment
   synthesis uses **30** for HA. Existing priorities are preserved. A particular
   number is not required to enable HA, and a forecast fill cannot displace a
   fresh live reading just because its numeric priority is higher.
5. Restart LocalSky after saving the changed sources. A newly added HA connection
   starts at boot; the restart also clears the source-change watering hold.
6. Keep a forecast provider enabled. HA passthrough polls current sensor states
   every **30 seconds** and does not import forecasts or the full native event stream.

HA being reachable does not make an old sensor reading fresh. LocalSky preserves
each entity's `last_reported` timestamp, including unchanged values that HA reports
again. If an older HA version omits that field, it uses `last_updated` conservatively.
Missing, invalid, future-dated or restored states are not current observations.
The configured source age limit determines when another source or a watering hold
takes over. A new temperature report cannot refresh an old rain or soil reading.

**Map WeatherFlow precipitation to Rain last minute.** HA's local WeatherFlow
sensor reports the preceding minute's accumulation. LocalSky converts its units,
adds each observation once and restores today's recorded minutes after restart.
WeatherFlow's `last_reset` identifies the physical report; repeated HA polls or
republishing that state cannot count it again. Totals stay separate for each
source and reset at midnight in LocalSky's configured timezone.
[HA documents the precipitation interval](https://www.home-assistant.io/integrations/weatherflow/).

Use **Rain today** only for an entity that already supplies a daily total; choose
one rain mapping per source. LocalSky cannot reconstruct minute reports missed
while it or HA was offline. Accumulation starts with reports actually received,
so an existing daily-total entity remains useful when upstream outage recovery
is needed. Predicted rain stays separate from measured rain. For sustained wind,
map WeatherFlow's wind average; its wind speed entity also receives rapid samples.

Illuminance, lightning count and lightning distance can also be selected in
Field mappings. These are sampled sensor values, not individual lightning events.
Disabling LocalSky's native listener trades its direct update cadence and event
coverage for the readings HA exposes; the engine uses the resulting evidence
and freshness normally.

## Phase 2: Watch them disagree

Run both brains side by side for a few days. LocalSky's Irrigation tab shows
tonight's plan, every zone's verdict, and the "why" behind each number
(Settings has a Simulator and Rule Lab for what-ifs). Compare against what
your HA setup decides. Tune species, soil texture, and sprinkler rates in
LocalSky's zone settings until you trust its plan.

Expect a few settling days before the numbers converge. LocalSky sizes
runs from a rolling seven-day water balance, and on a fresh install it has
no rain ledger and no recorded runs yet, so the first plans lean on the
forecast provider's past-day archive alone. Don't tune against day one;
give it several days of weather, rain, and recorded runs before comparing
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
| HA passthrough source (kind = `"ha_passthrough"`) | LocalSky polls HA's `/api/states` every 30 seconds. A request failure marks the source unreachable. Sensor report times remain independent of HTTP success, so frozen values expire under the source's age limit. Other configured sources can supply the missing fields. Missing required weather or an unavailable configured soil probe can hold watering; the displayed reason explains which evidence is missing. |
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
6. **Delete when you are ready**: the seven `input_*` helpers LocalSky used
   to read. As of 0.7.22 they decide nothing, LocalSky holds every one of
   their values itself, and deleting them changes nothing. Two exceptions: on
   an install with no persistence database mounted, the four control helpers
   were never taken over and are still deciding, and on an install with no
   `localsky.toml` the migration has not run and all seven are. The
   migration notice says so in either case; do not delete those until
   `/data` is mounted, or the setup wizard has written a config, and LocalSky
   has run the migration. They are not
   orphans and step 4's sweep does not cover them, so they will sit there
   until you remove them by hand. Before you do, check that no automation
   writes to one: it will keep firing and stop having an effect. See
   [Upgrading to 0.7.22: your helpers stop
   deciding](#upgrading-to-0722-your-helpers-stop-deciding).

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
| Smart Irrigation ET calculations | Native ET engine (FAO-56 ET0) feeding the weekly per-zone water balance | [irrigation-engine.md](irrigation-engine.md) |
| Smart Irrigation seasonal adjustment | The seasonal dial under Settings, applied to every zone's planned minutes. Kc curves per species and the heat multiplier are computed and shown, but they size no run | [zone-math.md](zone-math.md) |
| Irrigation Unlimited schedules | Smart-morning scheduler + per-zone budgets | [irrigation-engine.md](irrigation-engine.md) |
| Irrigation Unlimited sequences | The morning run is a sequence: zones dispatch one after another, with cycle-and-soak splitting per zone | [irrigation-engine.md](irrigation-engine.md) |
| Multiple schedules per zone | Manual schedules alongside the smart scheduler, plus per-zone weekly budget and sessions-per-week | [configuration.md](configuration.md) |
| HA automations for rain skip | Skip rules + Rule Lab (Settings > Logic) | [skip-rules.md](skip-rules.md) |
| Vendor app weather skip | Forecast-aware verdicts, visible per zone | [verdict-strip.md](verdict-strip.md) |
| Rain delay button | Pause/resume: the dashboard pause control or `localsky.pause` / `localsky.resume` from HA | [hacs.md](hacs.md#service-reference) |
| Manual-run services / scripts | `localsky.run_zone` and `localsky.stop_zone` services, or open the zone's valve entity | [hacs.md](hacs.md#service-reference) |
| Zone switches in HA | `valve.<zone>` via the integration (a legacy switch shim exists, disabled by default) | [hacs.md](hacs.md#per-zone) |
| "Is it running" sensors | Per-zone running `binary_sensor` via the integration | [hacs.md](hacs.md#per-zone) |
