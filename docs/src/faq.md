# FAQ

### Does my data leave my network?

Only when you ask it to. By default LocalSky makes no calls home, and the app itself runs no analytics. The outbound traffic that can exist:

- Forecast sources you configure (Open-Meteo, NWS, OpenWeather, Pirate Weather, MET Norway): polled requests carrying your coordinates and any API key you supplied. NWS and MET Norway also require an identifying User-Agent by their terms of service; if you leave that field empty, LocalSky sends `localsky/<version> (instance-<first 8 characters of the install id>; +https://localsky.io)`, and you can set your own contact string instead.
- Cloud-bridged hardware you add (Tempest WebSocket, Netatmo, Ambient Weather, Tuya, YoLink sources; Rachio, Hydrawise, B-hyve controllers): those vendors' clouds, with the credentials you entered.
- The optional update check: a plain daily GET to the project's version manifest at `localsky.io/latest.json`, off by default, opt-in via `[updates].check_enabled`. The request carries the running version in its User-Agent (so the maintainer can see which versions are in use); no per-install identifier or config data rides along.
- Web Push notifications, if you enable them: encrypted payloads to your browser's push service.

Pure-LAN setups (local station, OpenSprinkler, no forecast sources) generate zero outbound traffic.

### Do I need Home Assistant?

No. LocalSky is a complete standalone product: its own engine, scheduler, controller drivers, dashboard, and notifications. HA is one optional integration path among several. See [Standalone mode](standalone.md).

### What hardware works with it?

Weather: Tempest, Ecowitt gateways and soil probes, Davis WeatherLink Live, Synoptic Data (pulls your nearest real station over a free token), NOAA MRMS radar rain (US radar-derived rainfall), plus cloud and generic MQTT/webhook sources; see [Weather + soil sensors](sensors.md). When you configure more than one source, per-field priority chains let each reading (rain, wind, temperature, and so on) fall through an ordered primary-then-backup list, so the merged picture keeps updating even if one source goes quiet. Irrigation: OpenSprinkler is the canonical direct-LAN controller, with HA service-call, MQTT, cloud (Rachio, Hydrawise, B-hyve, Rain Bird), and others; see [Controllers](controllers.md).

### What does "beta" mean here?

LocalSky is in its 0.x release line (check Settings > About, or `GET /api/v1/info`, for the exact version you are running). The engine math (FAO-56) is stable, but the API wire format is not semver-locked until 1.0, and features and config fields can still change between releases. Config files carry a `schema_version` and migrate forward automatically at boot, so upgrades are safe; still, keep backups, and rehearse new controller setups with the `dry_run` controller before letting the engine drive real valves.

### Where is my data?

Everything lives in the `/data` volume you mounted: `localsky.toml` (configuration), `irrigation.db` (SQLite: run history, sensor samples, accounts, tokens), and a small instance-identity file. Nothing is stored in any cloud.

### Can I move LocalSky to a different host?

Yes. Either copy the `/data` directory to the new host, or use the built-in bundle: `GET /api/v1/backup` downloads a tar.gz of config plus a consistent database copy, and `POST /api/v1/backup/restore` loads it on the new instance. See [Backup and restore](backup-restore.md).

### Can I run two instances?

You can (separate data volumes, different ports), and a second instance in demo mode is a handy sandbox. What you should not do is point two live engines at the same controller: each one runs its own scheduler, so the same zones would be dispatched twice.

### Why did it skip watering today?

There is always a recorded reason per zone: rain already received, rain expected, wind, temperature, soil moisture from a probe, restriction calendars, and so on. The UI shows the exact threshold that tripped. A zone that is not skipping but still plans zero minutes reads ON HOLD on its card, with the weekly water balance's own reason beside it. See [Skip thresholds explained](skip-breakdown.md) and [History and reporting](history.md).

### Can I enter thresholds in metric?

Display units are configurable in Settings > Units. You choose the units for temperature, rainfall, wind, pressure, distance, and zone area independently, and the choice sets a household default that any individual device can override with its own preference. Every reading and every plain-language reason renders in the units you picked. The one exception is input: the skip-threshold input fields (already-wet, max wind, min temperature, rain skip, and friends) currently accept imperial values only; metric input is on the roadmap. The docs list metric equivalents next to every default so you can translate while you tune.

### Does it need internet access?

Not for the core loop. A LAN weather station plus a LAN controller (Tempest or Ecowitt plus OpenSprinkler, say) keeps measuring, deciding, and watering with the WAN unplugged. Forecast-driven features (forecast merge, rain-hold lookahead, the 7-day verdict strip) need egress to whichever forecast providers you configured.

### Is there telemetry?

No tracking lives in the app: no usage reporting, no crash reporting, no analytics SDK, nothing sent to the maintainer beyond the optional update check above, off by default. When you enable it, the daily request to `localsky.io` carries the running version in its User-Agent (no per-install identifier), and (as with any web request) the server can see your IP; the maintainer reads those access logs only as aggregate version counts. One separate case: NWS and MET Norway require an identifying User-Agent by their terms, so requests to those two agencies carry a short per-install tag (the first 8 characters of the install id) unless you set your own contact string in the source's settings. That identity goes to the weather agency you chose, never to the maintainer. Nothing else is collected, and nothing is stored in the app.

## Glossary

- **ET0**: reference evapotranspiration; how much water (mm/day) a standardized grass surface would lose to evaporation plus transpiration under today's weather.
- **ETc**: crop evapotranspiration; ET0 adjusted to your actual lawn (ETc = ET0 x Kc), how much water the lawn loses each day.
- **Kc**: crop coefficient; a per-species, season-aware multiplier that converts ET0 into ETc.
- **MAD**: management allowed depletion; the fraction of TAW the engine lets the soil dry out before watering is triggered.
- **TAW**: total available water; how much water (mm) the root zone can hold between field capacity (full) and wilting point (empty).
- **Soil bucket**: a per-zone depletion model where rain and irrigation fill and ETc drains. The soil scheduling model waters a zone when the bucket's deficit crosses its trigger and refills it; the deficit shows on every zone's tiles whichever model governs.
- **Weekly water balance**: the default scheduling model. A gross weekly target per zone, settled against observed rain (credited per day, each day capped at what the root zone can hold), water already applied, and a probability-weighted forecast credit; the remainder is split across the sessions still expected this week. A zone can run on the soil bucket instead (`scheduling_model`).
- **Verdict**: the engine's daily decision for the yard: run or skip, with the reason attached.
- **HAL**: hardware abstraction layer; the Rust trait every controller adapter implements, so the engine speaks one language to OpenSprinkler, Rachio, HA service calls, and the rest.
- **FDR**: frequency domain reflectometry; the measuring principle behind common soil-moisture probes, whose raw readings LocalSky calibrates into a percentage.
- **zeroconf**: zero-configuration networking (mDNS); LocalSky announces itself as `_localsky._tcp` on the LAN so clients like the Home Assistant integration can find it without you typing an IP.
