# Changelog

All notable changes to LocalSky are documented here. Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Versions follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.7.15] - 2026-08-23

### Added

- **A tuning report that grades your zone settings against what actually happened.** Each zone's page now carries a Tuning panel: LocalSky watches two weeks of runs, skips, rain, and soil-probe readings and turns them into at most one plain suggestion per zone, each with the evidence behind it and an Apply button that writes the change through the same validated path as the settings editor. It catches sessions quietly shorted by the duration cap, soil settings whose water bucket cannot be right, a probe that dries faster or slower than the configured soil predicts, and the sprinkler rate your heads actually deliver (backed out of the probe's rise across waterings, the catch-cup test without the cups). When there is not enough data to judge, the report says exactly what is missing instead of guessing. See [Tuning report](tuning-report.md).
- **A forecast-skip scorecard.** One line on the irrigation page: of the days LocalSky skipped for expected rain, how often the rain actually came, with each skip judged against the window it claimed (same day, next day, or the following three days). It appears once at least three skip days could be scored. Skips for rain already falling or already on the ground confirm themselves, so they are counted on their own line instead of being graded as forecast calls.
- **A weekly tuning notice.** With notifications enabled, LocalSky sends at most one "tuning report ready" notice per week, and only when a zone has something worth applying. The reminder survives restarts without repeating itself.
- **Zone soil and sprinkler edits now apply live.** Changing a zone's soil texture, sprinkler type, precipitation rate, or slope (from the settings editor or a tuning Apply) reshapes the next computed run and cycle plan immediately; previously those particular fields quietly waited for a restart without saying so.

## [0.7.14] - 2026-08-13

Unknown now means unknown. Values LocalSky never measured stop dressing up as data.

### Fixed

- Placeholder and sentinel values stop publishing as data. Readings LocalSky does not have now show as unknown (a dash in the app, `unavailable` in Home Assistant) instead of a fabricated number: precipitation probability and leaf wetness before any source reports them, water level on controllers that never report one (previously a made-up 100%, or 0% on a Home Assistant setup missing the entity), today's ET0 during a forecast outage (previously a flat 5.0 mm presented as measured), and today's temperature range and humidity on standalone installs (previously 0°/0° and 0%). Today's range now also comes from the live forecast first instead of legacy Home Assistant sensors only.
- A forecast without a probability series no longer zeroes the expected-rain math. Probability-less forecast rain now counts at full value in the weighted 3/7-day outlooks and the tomorrow-rain skip, so the engine holds water ahead of a storm instead of watering into it, and skip reasons only claim a confidence percentage when the provider actually reported one.
- Home Assistant stops growing dead sensors. Precipitation probability, wet bulb, wind lull, rain-last-minute, illuminance, water level, and per-zone soil sensors are only created when the install actually has the source, station, controller capability, or soil probe behind them.
- An install without a location no longer shows another city's weather. The forecast pipeline previously fell back to fixed default coordinates and presented that forecast (and its timezone) as local; it now waits, visibly, until a location is set.
- An install with no location no longer pretends to have one. The radar map used to fall back to a fixed point in the mid-Atlantic at neighborhood zoom, station marker included, so an unlocated install looked confidently located near Philadelphia (one beta tester reasonably read it as their location not applying). With no location set, the map now shows a continent-scale view with no station marker and a note linking to the location settings, and it recenters the moment a location is saved.
- The weather dashboard shows a loading state after a restart instead of rendering zero-degree readings while the first data arrives.
- The NWS and Met.no integrations no longer send a placeholder contact (`you@example.com`) as their User-Agent. Left empty, the field now auto-derives a real, current identity for this install; you can still supply your own contact string. Installs whose configuration still carries the old auto-filled identity from 0.7.10 through 0.7.12 are migrated to the derived one automatically at request time.

### Changed

- **A tomorrow-rain forecast with no reported probability now counts at full value.** If your forecast source supplies a rain amount but no probability, LocalSky may now skip a watering it previously ran, because the expected rain is no longer multiplied down to zero. A zone whose soil probe reads below its dry floor still waters through this skip.

## [0.7.13] - 2026-08-12

The setup wizard works before the setup exists. One fix in two places, no breaking changes.

### Fixed

- Cloud weather toggles in the setup wizard now save to your setup draft ([#7](https://github.com/silenthooligan/localsky/issues/7)). The sources step embeds the same cloud weather panel the settings pages use, and its toggles wrote the live configuration directly. On a fresh install the live configuration has no location yet, and a configuration without a location cannot be saved, so every toggle failed with a validation error and the step could not be completed. The toggles now write the wizard draft instead: the on/off state and the "set your location first" callout read the draft you are building (a location entered earlier in the wizard counts), adding a key to a keyed provider works the same way, and everything you turn on goes live when you finish setup.
- Failed settings loads now show the server's reason ([#7](https://github.com/silenthooligan/localsky/issues/7)). When a settings or setup page could not load its data, the banner showed only the bare status code ("HTTP 422") and dropped the response body, so the explanation the server sent never reached you. Load and save failures now share the same reading of the server's error: the failing validation rules, the hint, or the detail line, with the raw status kept only as a last resort.

## [0.7.12] - 2026-08-12

A refused settings save now explains itself, and precipitation probability becomes a sensor. No breaking changes.

### Fixed

- A refused settings save now says why ([#6](https://github.com/silenthooligan/localsky/issues/6)). Every settings page saves the whole configuration, so a validation problem anywhere (most commonly an unset location on an install that skipped the wizard) blocked every save with a bare "config_invalid" and no mention of the actual rule. The message now lists the failing rules ("location is 0,0 (null island); set your real coordinates"), the API's 422 body carries a flat one-line summary for scripts, and the cloud weather page points at the location settings up front when the location is unset, since every cloud provider fetches weather for your coordinates.

### Added

- **Precipitation probability is now a sensor.** The merged probability of precipitation was already computed and served on the conditions API, but it was not among the entities the Home Assistant integration creates. It now appears on the Forecast device as `Precipitation probability`, giving a cheap "is rain likely right now" reading without pulling the whole hourly forecast. Distinct from `Rain tomorrow probability`, which is tomorrow's daily figure.

## [0.7.11] - 2026-08-11

Hourly forecasts reach Home Assistant. Nothing changed in the app itself; this release keeps the app, the integration and the add-on on one version.

### Added

- **The Home Assistant weather entity now publishes an hourly forecast**, not only a daily one. Calling `weather.get_forecasts` with `type: hourly` returns up to 48 hours carrying temperature, apparent temperature, precipitation, probability of precipitation, wind, humidity and cloud cover. LocalSky already fetched every one of those; only the daily summary was reaching Home Assistant, so an automation could ask what the weather is doing now but not what it will be doing at a given hour.
- The gain is largest with a forecast source that models convection. NWS marks thunderstorm hours explicitly and carries a per-hour chance of precipitation, so an automation can act on an afternoon before it arrives instead of reacting once the storm is overhead. Clear hours now read as clear-night after dark rather than sunny, which the daily summary never had to distinguish.

Needs the matching 0.7.11 Home Assistant integration. The app and add-on are unchanged.

## [0.7.10] - 2026-08-08

Lightning data correctness. If you built an automation on the lightning sensors, read the first two entries: one of them was silently breaking alerts.

### Fixed

- **The one-hour lightning strike counter never came back down.** Every observation trimmed strikes older than an hour out of the rolling buffer but then republished the previous count, so once a storm ended the number froze at its last total until the next strike arrived, sometimes days later. It now counts the buffer it just trimmed and decays to 0 on its own. This matters beyond the display: a Home Assistant trigger of the form "strikes above 0" only fires when the value crosses the threshold, so a counter stuck above 0 never re-armed and the alert stayed silent through every storm after the first. Such an automation starts working again on its own once the counter reaches 0.
- **Lightning average distance published 0 for "no strikes this minute."** Stations report a bare 0 in a quiet reporting interval, and LocalSky passed it through on a channel marked as a distance measurement, where 0 reads as a strike directly overhead rather than as no reading. A consumer checking `distance < 10` saw a phantom storm between strikes, and the natural guard `distance > 0` threw away real readings. The value is now unknown (`null` on the API) whenever the interval detected no strikes.

### Added

- New **Lightning last strike distance** sensor: the distance to the most recent strike still inside the one-hour window. Unlike the interval average, it persists between strikes and clears when the last strike ages out, so "how far away is the storm" is a single sensor read. Available automatically in Home Assistant after the update.

### Changed

- The in-app "Documentation" and "All documentation" links now open the manual bundled with your install instead of the public site, so they work offline and always match the running version. Every other help link already did.

## [0.7.9] - 2026-08-08

Cycle interleaving turns on by default, cycle-and-soak settings apply without a restart, sign-in via an authenticating reverse proxy, and a new Engine settings page. One behavior change to review if your water comes from a well; no breaking API changes.

### Changed

- **Cycle interleaving is now on by default.** With `engine.interleave_cycles` unset, zones now water during each other's soak pauses (one valve at a time, soaks never shortened), so cycle-and-soak mornings finish sooner. **If your irrigation is fed by a well or another low-recovery supply, turn this off**: the serial plan's idle soak gaps double as recovery time for your supply, and interleaving fills them with more pumping. Flip the toggle on the new Engine settings page, set `interleave_cycles = false` in `localsky.toml`, or answer "Well or low-recovery supply" to the setup wizard's new water-supply question. Existing installs that never set the key pick up the new default on upgrade; installs that set it explicitly are unchanged.
- Cycle-and-soak settings (`interleave_cycles` and `soak_minutes`) now apply on the next scheduler tick instead of requiring a restart. Saving them reshapes the next morning's dispatch plan and the "next run" estimate immediately, and the restart-required banner no longer appears for these two settings.
- New **Engine** settings page (Settings, then Engine): the cycle-and-soak controls (soak time, cycle interleaving) and the seasonal water-budget dial now live there. The Skip rules page keeps only the skip-ladder thresholds. Saved values you do not edit are preserved either way.

### Added

- Sign-in via an authenticating reverse proxy. If oauth2-proxy, Authelia, or a similar gateway fronts LocalSky, set `auth.proxy_auth_header` (for example `X-Auth-Request-Email`) and LocalSky treats requests that arrive from a proxy in `auth.trusted_proxies` carrying that header as an authenticated operator on the privileged routes (config writes, backups, restart), in both auth modes. An optional `auth.proxy_auth_allow` list restricts which identities qualify. The header is only honored from the declared proxy's own address, so clients cannot spoof it, and your proxy must strip or overwrite it on client traffic. See [Authentication](authentication.md).
- The setup wizard's zones step now asks what feeds your sprinklers (municipal or pressurized, well or low-recovery, or not sure) and sets the cycle-interleaving default to match, so a well-fed install never has to discover the toggle after the fact.

### Fixed

- Saving an unrelated settings change no longer reports "restart required" claiming a weather source changed. A config save re-compared the stored sources against the submitted ones through a map whose key order was not stable, so installs with an Ecowitt gateway poller carrying several calibrated soil channels saw a phantom source change on every save.
- A configuration save rejected because the request came through a reverse proxy LocalSky is not set up to trust now explains itself: the error names `auth.trusted_proxies` (and `auth.proxy_auth_header`) instead of a bare "unauthorized", and the settings pages show that explanation on a failed save instead of a generic failure.

## [0.7.8] - 2026-08-08

Cycle interleaving, an ET0 unit fix that was quietly shrinking displayed and projected ET 25x, and timezone-correct day boundaries for containerized deployments. No breaking changes.

### Added

- Cycle interleaving (opt-in). With `engine.interleave_cycles` on, other zones water during a zone's soak pauses instead of the sequence idling through them, so the total morning sequence takes roughly the longest zone's cycle chain instead of the sum of every zone's. One valve still runs at a time, soak times are treated as minimums (a soak can stretch, never shrink), and zone order is preserved. Off by default: on installs fed by a well or other low-recovery supply, the idle soak gaps double as source recovery time. Toggle it under Settings alongside the other engine tuning, or set it in `localsky.toml`; applies after a restart. See [Irrigation engine](irrigation-engine.md). ([#5](https://github.com/silenthooligan/localsky/issues/5))

### Fixed

- "ET today" no longer collapses at midnight ([#4](https://github.com/silenthooligan/localsky/issues/4)). The default forecast source reports ET0 in inches and the merge layer read it as millimeters, so the moment a day rolled from "tomorrow" to "today" its ET dropped about 25x (a -0.18 in day showed as -0.01 in). The wrong value also fed the 7-day soil projection (curves barely declined, hiding drying) and the exported "ET0 today" sensor (under-driving any external bucket math built on it). Today, tomorrow, and the 3-day average now resolve through the same full-day ladder in the same units: a directly mapped ET0 source first, then the forecast provider's own daily ET0, then the computed estimate. The mapped-source field is a full-day figure by contract; map a full-day sensor to it, not a since-midnight accumulator.
- Day boundaries and local-time rules now follow the configured timezone, not the container's. In a UTC container (the common Docker setup): rain-today and ET0-today accumulators reset at the deployment's local midnight instead of mid-evening, forecast-accuracy observations file (and join) under the right date, manual-schedule weekdays flip at local midnight, and watering restrictions (allowed hours, odd/even day parity) evaluate against the deployment's wall clock instead of UTC.
- A station or mapping that reports ET0 with a declared unit of inches is now converted to millimeters on the way onto the merge bus, closing the same unit-mismatch class for non-default sources.
- Saving the skip-rules settings page no longer resets engine fields the page does not edit (disabled rules, soil-probe quarantine tuning, the observed-rain window).
- The pre-sunrise start time now accounts for cycle-and-soak pauses. Sequences with soak splits previously started as if soaks took no time and overshot the finish target (sunrise minus 15 minutes) by the total soak time; the scheduler now works true wall time backwards from the target, under both serial and interleaved dispatch.
- The "ET0 spent so far today" figure no longer charges the whole day at once right after midnight when the forecast snapshot has not refreshed since the previous evening.

## [0.7.7] - 2026-07-19

Security hardening, reliability fixes, and automatic backups. No breaking changes.

### Added

- Scheduled local backups. Set `LOCALSKY_AUTO_BACKUP_HOURS` and LocalSky writes a full backup bundle on that interval to `LOCALSKY_BACKUP_DIR` (default `/data/backups`, inside your mounted volume), keeping the newest `LOCALSKY_BACKUP_KEEP` (default 7). Same format as the download-backup bundle, so it restores through the same flow. Off by default. See [Backup, restore, and recovery](backup-restore.md).

### Security

- Retiring a zone's soil probe now requires the same authorization as other configuration changes, so it can no longer be triggered anonymously on a LAN-open instance.
- Behind a reverse proxy with no `trusted_proxies` set, LocalSky no longer treats forwarded requests as trusted local callers on privileged routes (backup download, raw config, restart). It fails closed and requires a login instead. If you run behind a proxy, set `auth.trusted_proxies` (see [Authentication](authentication.md)) so your own access keeps working.
- API token listing and revocation are now scoped to the signed-in owner, preventing cross-owner token access ahead of any multi-user support.

### Fixed

- A misbehaving or hostile device on a configured source or controller can no longer exhaust memory by returning an oversized response. Every outbound device and API read is now size-capped.
- Irrigation shutoff: if a controller's settings change its identity while a zone is running, the shutoff backstop now closes the valve through the active controller instead of dropping it, closing a rare stuck-valve window.
- A weather source whose task crashes now restarts automatically with backoff, instead of going silent until the next container restart.
- Manual schedules are no longer skipped for the day if a prior dispatch ran long and pushed past the scheduled minute. A short catch-up window still fires them exactly once.
- A zone with a misconfigured (effectively zero) precipitation rate now waters for its full planned time instead of being silently skipped, and logs the misconfiguration.

## [0.7.6] - 2026-07-05

### Fixed

- Ecowitt gateway poller: a single missed poll no longer logs "unreachable" and flips source health. The poller retries once in-cycle (2s) and only flags after two consecutive failed cycles; the gateway's embedded server has brief busy windows that caused constant flapping pairs in the log.

## [0.7.5] - 2026-07-05

In-app restart, device-management fixes, and source ranking coherence. No breaking changes.

### Added

- In-app restart: the "Restart required" banner now carries a **Restart now** button, so a config change that needs a restart never sends you off to manage the container. `POST /api/v1/system/restart` (privileged, same bar as config writes): on HAOS it uses the Supervisor's add-on self-restart; on Docker/service deployments it exits cleanly and the restart policy relaunches it. Refuses with 409 while a zone is actively watering (`force: true` overrides; shut-off backstops + boot valve reconciliation cover the interruption). The UI shows a wait overlay and reloads itself when the app is back.

### Fixed

- Removed probes no longer show as "probe offline" on the Sensors page: a zone with no bound soil sensor still emitted a per-zone soil entry (phantom offline rail item + detail card). Unbound zones now produce no soil forecast at all.
- Sources predating the region ranking that still sat at the flat default priority (50) are lifted once at boot to their researched region rank (e.g. NOAA MRMS to 75 in the US), so the global fallback ranking agrees with the per-field chains without hand-editing a number. Any later manual value, including setting it back to 50, sticks.
- Ecowitt source editor: gateway login/password fields were missing from the form, so gateway-side probe removal could not be enabled from the UI (the config has supported it since 0.7.3). Both optional; password masked and redacted like every other source secret.

## [0.7.4] - 2026-07-04

In-place device management, HA ingress write fix, and a major query perf win. No breaking changes.

### Added

- Devices: soil probes on a device card are managed rows (bind-to-zone select + Remove), not read-only chips.
- Sensors: probe detail view carries the same Remove action + a link to the soil-probe manager.

### Fixed

- HA ingress: writes now work from the HA sidebar. Two independent blockers: the ingress URL shim rebuilt bodied requests (PUT/POST) as streamed uploads, which browsers refuse over HTTP/1.1; and the CSRF origin check rejected HA's UI origin. Bodies now pass as bytes, and Supervisor-ingress requests (X-Ingress-Path, session-validated upstream) are exempt from the origin check. Fixes wizard draft saves, license accept, and all settings saves.
- Fresh install: `GET /api/config` 404'd with no config file and locked every settings pane. Now returns the default config; saving creates the file.
- Perf: latest-reading-per-channel queries ran correlated MAX(epoch) subqueries over full history (~15s at 2M rows, serializing the shared SQLite connection). Covering index + group-max rewrite: ~0.1s.
- Probe removal deletes the channel's recorded readings (incl. battery/temp/EC siblings), so removed probes no longer resurrect from history.
- Soil-probe manager was unreachable from navigation; now linked from soil-carrying Devices cards and the Sensors page header.

## [0.7.3] - 2026-07-04

Single-pane device management and a Home Assistant ingress fix: retire a soil probe (and clear its Ecowitt gateway registration) in one click, and in-app navigation now works when LocalSky is opened from the Home Assistant sidebar. No breaking changes; upgrade in place.

### Added

- Device removal from LocalSky (single-pane management): every soil probe on the Sensors page has a "Remove probe" action that clears its binding and stops the offline warning for that zone. For probes on an Ecowitt gateway, if you set the gateway login on the source, LocalSky also unregisters the sensor on the gateway in the same click (using the gateway's disable state, so it stays removed instead of being auto-re-added). It is honest per device about what it can and cannot remove upstream. See the new "Removing and disabling devices" documentation.

### Fixed

- Home Assistant ingress: opening a page from the sidebar or the mobile nav no longer lands on "404, no such page". When LocalSky is embedded in the Home Assistant sidebar, in-app navigation was applying Home Assistant's ingress path prefix twice, sending the browser to a doubled `/api/hassio_ingress/<token>/api/hassio_ingress/<token>/...` address that cannot resolve. The first page loaded, so the dashboard appeared, but nothing you clicked worked (and the setup wizard dropped you onto that broken navigation at the end). Navigation now resolves correctly whether LocalSky is opened through the Home Assistant sidebar or on its own port.

## [0.7.2] - 2026-07-03

Forecast resilience and a weather product that adapts to your climate: a default multi-provider failover chain, restart-proof forecasts, the full Open-Meteo variable set, and condition-aware dashboard cards. No breaking changes; upgrade in place.

### Added

- Forecast failover, on by default: every located install now carries its region's free forecast authority alongside Open-Meteo (NWS and NOAA MRMS radar in the US, MET Norway in Europe and the Nordics), so one provider going down no longer takes the forecast, the 7-day verdicts, or rain-skip inputs with it. Existing installs get the sources automatically on upgrade; delete one and it stays deleted. When a backup is serving, the forecast header says so ("via NWS · backup") and links to source status, and data served by an Open-Meteo mirror is labeled "(mirror)".
- Open-Meteo endpoint resilience: if the primary Open-Meteo host is unreachable, LocalSky transparently retries against two verified Open-Meteo mirror hosts before failing over to another provider.
- The forecast now survives restarts: the last good forecast is persisted and rehydrated at boot (with its original fetch time, so staleness rules still apply), instead of starting empty until a provider answers. If no forecast has ever arrived, the weather panels and verdict strip now say so plainly after a few seconds instead of shimmering forever.
- Richer forecast data on the 7-day cards: "burst" and "soaker" chips describe each rain day's character (short and heavy vs long and steady), from the model's precipitation-hours data.
- Transpiration stress tile (VPD): flags days when plants lose water faster than usual (sustained vapour pressure deficit above 1.6 kPa). Advisory only; it never changes a skip decision.
- Model soil, advisory (nerd mode): the weather model's own root-zone moisture estimate and its 48-hour drying trend, plus 6 cm soil temperature. Zone probes remain authoritative everywhere decisions are made.
- Today's water balance now notes how much of the day's evapotranspiration the model says has already been spent, so a morning glance isn't charged the whole day's loss up front.
- Condition-aware "Heads up" cards on the Weather home: Winter (snowfall, snow on the ground, freezing level, coldest night), Low visibility (fog windows), Storm potential (instability, peak gusts, pressure trend), and Heat (feels-like and wet-bulb peaks). Each card appears only while its condition actually holds at your location, so a mountain install, a coastal install, and a plains install each see what matters locally and nothing else.
- Self-hosted Open-Meteo support: point the open_meteo source's new `endpoint` option at your own open-meteo instance and it leads the endpoint ladder, with the hosted service and its mirrors as automatic fallback. Served data is labeled "(self-hosted)".

### Changed

- The rain outlook's "Tomorrow" bar now shows the decision's real input: rain measured today that still counts toward tomorrow's skip appears as a hatched "carried" segment with the combined total, instead of a bare forecast zero that appeared to contradict a "skipping on recent rain" verdict.
- The in-app documentation version banner now always matches the running release.

### Fixed

- Offline soil-probe warnings no longer name a specific sensor model. LocalSky reads only the soil channel's reading, never the hardware model, so it now says "soil probe" instead of guessing a model that could be wrong for your hardware.

### Security

- The self-hosted Open-Meteo endpoint is fetched through the same SSRF-hardened, IP-pinned client (redirects disabled, loopback/link-local/cloud-metadata targets refused, response size capped) that every other operator-configurable URL in LocalSky already uses.

## [0.7.1] - 2026-07-02

A hardening and polish release on top of 0.7.0: more robust watering safety backstops, honest source health, a broad accessibility pass, and Home Assistant fixes for weather-only installs. No breaking changes; upgrade in place.

### Fixed

- Home Assistant, weather-only installs: an install with no controller or zone now keeps its forecast sensors (evapotranspiration, days since rain, rain-tomorrow probability, wind-gust forecast, forecast source) and its Home Assistant connectivity sensor. Previously these were dropped along with the irrigation entities. (Update the LocalSky integration to 0.7.1.)
- Home Assistant: forecast sensors now group under a "Forecast" device instead of "Irrigation", and flow-rate and leaf-wetness sensors are published only when a source actually provides them, so installs without that hardware no longer get always-zero phantom sensors. A new "Force override guard" sensor names the safety rule a forced run overrode, so an automation can alert on it.
- Watering safety backstops made more robust: a scheduled cycle interrupted by an unreachable controller mid-sequence keeps its automatic shut-off timer armed (so a valve that failed to close is still closed by the backstop), overlapping runs of the same zone can no longer shorten that timer, and a valve left open by a crash is reconciled closed at boot even when the history database is unavailable. The background schedulers and the shut-off enforcer now survive an internal error and keep running rather than stopping silently.
- The freeze and heat-advisory chips on the irrigation view no longer show a doubled degree symbol.
- Faster loads: content-hashed app assets are cached long-term by the browser (they already change name every release) instead of being re-checked on every visit.

### Changed

- Honest source status: a healthy weather source that is simply outranked by a higher-priority one now reads "standby" or "watching" instead of "falling through", and the soil gateway that provides your zones' moisture reads "active". The health summary and the source list now agree.
- The daily "decided on backup data" notice and the degraded-data health metric now fire only when the decision truly ran on missing live data or a stale forecast, not when a reading is served by a configured backup source in the normal way.
- Restoring a configuration from a backup now applies to the running engine immediately (thresholds, restrictions, schedules) and tells you when a restart is still needed. A configuration file that exists but fails to load (a typo, an unset variable, an older build) now logs a clear error instead of quietly starting as if unconfigured.
- Data retention is honored by every writer: "keep forever" and custom retention are no longer overridden by a 90-day default on one path.
- Broad accessibility pass: chart data carries a text summary for screen readers, navigation announces the current page, status is never conveyed by color alone, more text meets AA contrast, focus is managed on page changes and trapped correctly in dialogs, and the installed app is no longer locked to portrait.

### Security

- The configuration read (`GET /api/config`) now redacts secrets that ride inside a source's URL, request headers, or request body (generic HTTP and REST-poll sources), plus the username-half of cloud credential pairs.
- Cloud-controller API keys can no longer appear in an error message or log line when the controller is unreachable.
- Outbound requests to user-configured hosts are size-capped, and backup restore is hardened against a decompression bomb and a wrong-database upload.
- The public read-only demo no longer accepts anonymous sensor-data posts, and the address-search relay is no longer reachable unauthenticated.

## [0.7.0] - 2026-07-01

LocalSky's version jumps from 0.5 to 0.7 to bring the whole portfolio onto one shared version: the app, the Home Assistant integration (localsky-ha), and the Home Assistant add-on now all ship as 0.7.0 and move in lockstep from here. (0.6 was an internal-only iteration; the public app moves straight to 0.7.0.)

A large release centered on full, understandable control over where every reading comes from: a drag-to-order backup chain per reading, one unified source list, display units everywhere, a cloud-first experience for users without hardware, smarter irrigation safety, and a broad visual and accessibility pass.

### Added

- Per-reading priority and backup chain (Settings > Devices): every headline reading (temperature, humidity, wind, rain, pressure, solar/UV) shows an ordered chain of every source that can provide it. The top source that is reporting wins, and if it goes quiet the next takes over, so a reading is never lost. "Automatic" shows the smart default order for your region; drag a row (or use the up/down arrows) to make it "Custom". The order you set is the priority the engine uses, per reading.
- One unified weather-source list: local stations, cloud services, and every other source kind live in a single list where each source appears once, with its live status, an on/off toggle, its sensors, and edit/remove. Cloud services you have not turned on yet show separately as coverage you can add.
- Display units: choose how temperature, rainfall, wind, pressure, distance, and zone area are shown (Settings > Units). A household default applies to the whole install, and any device can override it. Every reading and every plain-language decision reason renders in your chosen units.
- Cloud weather is first-class: an install with no hardware uses Open-Meteo (free, no API key) automatically and sees weather right away, and a forecast-source selector chooses which provider drives your forecast.
- Honest cloud rain: NOAA MRMS radar (a live radar rain rate plus a gauge-corrected hourly total) joins the catalog, and each source is labeled by how honest each reading is, per reading: measured, radar, real-time nowcast, or model forecast.
- New source: Synoptic Data (the nearest real weather station's measured wind, pressure, temperature, and humidity; free token).
- Elevation auto-fills from your location in setup (still editable).
- Soil-probe anomaly surface: a probe that goes offline, or reads as a wild outlier versus its neighbours, is flagged on the irrigation and zones views.

### Changed

- Editable, migrated source and controller IDs: rename a source or controller and every reference to it moves too (its per-reading picks, forecast pin, zone soil binding, and zone-to-controller links), so a rename never leaves anything dangling.
- Source settings, made approachable: a single device hub, plain-language descriptions with a "where to get this" link for keyed services, and an at-a-glance view of what each provider covers. Open-Meteo is the recommended zero-config pick.
- The irrigation hero now explains the upcoming run, which zones will water and why, before it runs.
- A unified colour language across the weather and irrigation views: blue for water and wet, amber for drying out, teal for satisfied, red for freeze, wind, and restrictions. The rain gauge fills blue up to the skip threshold, then teal once it is met.
- A broad visual and accessibility pass: consistent depth and motion, clearer entity identity, improved keyboard and screen-reader focus, larger touch targets, and settings panels that widen to use a large screen.

### Fixed

- Irrigation safety: rain that actually fell today now skips the NEXT scheduled watering on its own, independent of any soil sensor. Previously the next day's decision looked only at the forecast, so a real afternoon rain could still let the next morning's run proceed.
- A single bad soil probe (reading far drier than its neighbours) is distrusted and inferred from the rest of the yard, so it can no longer water a saturated zone.
- Watering duration was inflated by a heat index that paired the forecast high with the current (often night-time) humidity; it now pairs each day's high with that day's own humidity.
- Zones: clicking a zone card now reliably opens that zone on the first click.
- Display consistency: every reading respects your unit preference (no more mixed millimetres and inches).

## [0.5.0-beta.1] - 2026-06-22

### Added

- First-class DIY / ESP32 irrigation controller support, no boxed controller required:
  - New "DIY (HTTP)" controller: drive any board over a small documented HTTP/REST contract (`GET /status`, `GET /zones`, `POST /zone/{id}/run|stop`, `POST /stop_all`, optional bearer token). Fully pollable, so status readback, zone discovery, and the setup wizard's "Test connection" / "Scan zones" all work. Selectable in the setup wizard and Settings > Controllers.
  - The MQTT controller now supports optional state readback: per-zone `state_topic` (with `state_on_payload`), plus a controller-level `availability_topic` and `flow_topic`. With these set, the board's real running state, online/offline status, and live flow feed the dashboard. Command-only (fire-and-forget) behavior is unchanged when they're omitted.
  - Reference firmware in `examples/`: an ESPHome config for the MQTT path and an ESP32 Arduino sketch for the HTTP path, plus a "DIY & ESP32 controllers" documentation page spanning beginner (copy-and-flash) to advanced (raw contract).
- Sticky irrigation overrides: set a global, or per-zone, Auto / Skip / Force decision that persists until you clear it (instead of a one-shot skip). Surfaced on the zones cards and the controls panel.
- New `sensor.localsky_wind_gust_forecast` exposing the day's forecast peak wind gust (Open-Meteo). A wind-shadowed station under-reports gusts, so the forecast feeds the high-wind irrigation skip and is available for Home Assistant automations.

### Fixed

- PWA reliability: `/pkg` WASM and JS assets are now content-hashed and the service worker is push-only, self-cleaning stale caches. This fixes the class of bug where a phone could load a stale asset pair and render the desktop layout or fail to hydrate after an upgrade.

### Changed

- DIY ESP32 boards via ESPHome / Tasmota now go through the MQTT or DIY (HTTP) controller. The non-functional `esphome_native` option (its backend is not yet built) is no longer offered in the controller picker, so a saved controller can no longer silently fail to water.

## [0.4.0-beta.3] - 2026-06-14

A security fixes and hardening release. Upgrading is recommended, especially for instances reachable beyond a trusted LAN.

### Changed

- Home Assistant integration links updated for the renamed `localsky-ha` repository.

### Upgrade notes

- Behind a reverse proxy, set `trusted_proxies` so LocalSky sees the real client IP.

## [0.4.0-beta.2] - 2026-06-14

This release builds out the irrigation and sensor side and makes the whole product easy to set up and learn: flow metering, a first-class sensors experience, point-and-click setup for every data source, documentation built into the app, and contextual help on every screen.

### Features

- Sensors view: a first-class Sensors page showing every gateway and probe with live readings, battery, and signal, with one place to bind a probe to a zone.
- Guided sensor setup: the first-run wizard discovers a gateway's probes and binds them to your zones in a single step.
- Flow metering: LocalSky reads a controller's flow meter and shows live GPM during a run. A clear capable / connected / live distinction means it only reports a meter you actually have.
- Soil sensors wired end to end: labeled forms for Ecowitt and MQTT soil, and an MQTT probe bound to a zone now feeds the engine's skip decisions directly.
- Point-and-click setup for every data source: adding or editing any weather or sensor source (host, port, URL, tokens, API keys, model, poll cadence) is now a labeled form, with the raw JSON kept as an advanced escape hatch.
- LibreWXR radar and smarter forecast sourcing: LibreWXR joins the catalog as a region-aware default radar provider and a forecast source, alongside an Open-Meteo precipitation-forecast layer.
- Documentation built in: the full handbook ships inside the app and opens same-origin at /docs, so it matches your exact build and works offline or on a LAN with no public domain.
- Help on every screen: a question-mark popover with a short explainer and a "Read full doc" link now sits on every complex screen, with new pages for radar, restrictions, advanced settings, the devices hub, and manual schedules. The controller picker links straight to the controller docs.
- Show and hide on secret fields: API keys, tokens, and passwords have a reveal toggle so you can confirm what you pasted.

### Bug fixes

- Setup wizard now completes on a fresh install: license acceptance is saved with the draft, so the toggle keeps its state across steps and the final step no longer rejects an accepted license.
- Setup wizard notification choices (Web Push, MQTT, ntfy, Slack) are carried into the saved configuration instead of being dropped before the final step.
- Flow is no longer reported as present when no meter is connected; the reading now reflects the real device signal.
- OpenWeather sources save correctly (they previously failed to persist).
- The radar Layers panel sizes to its content so settings stay visible, and opens and closes more smoothly.

### Security

- Source credentials (app_key, client_secret, refresh_token) are redacted from the config API instead of returned in cleartext. OAuth client IDs are shown as the public identifiers they are.

### API

- Contract 1.11.0: additive `GET /api/v1/sensors/inventory` (gateways, soil probes, flow).

## [0.4.0-beta.1] - 2026-06-13

Live Radar grows from a single precipitation layer into a full weather map: choose your imagery providers, overlay national alerts and worldwide tropical systems, add community lightning and wind flow, and manage all of it from one Layers panel.

### Features

- Radar provider catalog with a Settings > Radar control: pick which imagery providers the map offers (Auto chooses the best regional source, or define a custom menu) and which layers start on. Sources are region-aware, and your layer choices persist per browser
- National Weather Service alert overlay: severity-colored warning polygons (red extreme, orange severe), tap any polygon for the headline, refreshed every couple of minutes
- Worldwide tropical cyclone tracking: hurricanes, typhoons, and cyclones normalized from the responsible agencies (NOAA NHC/CPHC, JMA, JTWC) into position markers, track lines, and forecast cones; empty when the basins are quiet
- Choose your national forecast model: the weather model behind your forecast is now configurable, from a built-in catalog of national and global models
- Wind flow layer: animated particle flow of current 10 m winds over the visible map, warmer colors for stronger wind, refetched as you pan
- Opt-in Blitzortung community lightning strikes, off by default
- Layers panel: one Layers chip opens a drawer of Imagery and Overlays, each with a toggle, an expandable legend, and source attribution; it overlays the map without resizing it and replaces the old legend rail and layer control. A stacked-layers icon, accent outline, and active-count badge make the picker unmistakable, and a footer link jumps straight to Settings > Radar
- API contract 1.10.0: additive radar endpoints for the tropical-cyclone feed, the wind grid, and the forecast-model catalog

### Bug fixes

- Outbound National Weather Service requests now identify LocalSky by its project URL instead of a personal contact, per the NWS API policy

## [0.3.0-beta.2] - 2026-06-11

### Features

- Serve under a URL prefix: LocalSky honors the `X-Ingress-Path` header from prefix-stripping reverse proxies, so it runs correctly behind a subpath, while direct port access keeps working unchanged

### Bug fixes

- Fresh installs no longer show four phantom irrigation zones; zones come from your configuration (the wizard), and a pristine instance starts empty
- Web Push subscriptions work on fresh installs (the subscription table was only created on databases carried over from v1)

## [0.3.0-beta.1] - 2026-06-11

### Features

- History run log: per-day rows for every start, duration, and skip, with day watered totals
- Run log search (zone, reason, watered/skipped), 7/30/90/All range chips, and a month jump; the log fetches its own window so All is genuinely all
- Dated x-axes on History and zone charts (oldest to newest), zero-floored y-axis
- Rule manager: enable/disable, reorder, delete; template farm with six curated starting points
- Built-in skip gates are operator-controllable per gate behind a warning acknowledgement; control and legal gates stay locked; the trace marks gates disabled by operator
- Segmented On|Off toggle pills on gates and rules
- History retention setting (`persistence.runs_retention_days`, default keep forever) with daily prune
- Soil probe fault detection: 24h without a valid reading surfaces in `/api/health` (degraded), the health banner, and a one-time push naming the zone
- Per-zone verdict enforcement at dispatch: zones whose own verdict says skip are logged with the reason and not watered
- Operator-opt-in analytics tag (`LOCALSKY_ANALYTICS_*` env, off by default)
- Demo mode seeds 30 days of synthetic run history
- API contract 1.8.0: additive `soil_probe_faults` on snapshot and health

### Bug fixes

- Rain today reads the native Tempest daily accumulator (local-midnight rollover, restart-safe reseed, persisted to history); the HA WeatherFlow per-minute precipitation entity is no longer misread as a daily total
- Days-since-rain takes the min of the regional model and the station's own observed history
- The yard-wide saturation gate names zones missing soil readings instead of going silently inapplicable
- Scheduler no longer double-records completed runs
- Title/subtitle spacing normalized across page headers, panels, and gate rows

## [0.2.0-beta.1] - 2026-06-10

The v2 burndown. Lays a ports-and-adapters foundation underneath the existing v0.1 deployment without changing observable behavior, plus the standalone, UI, and ops work to make LocalSky a viable open-source product.

### Added

#### Launch hardening (auth, identity, discovery, ops)

- Built-in authentication: owner account (argon2id), browser sessions, show-once API tokens for integrations; `[auth]` policy block with `mode` (default `disabled` on upgrades, `required` for new wizard installs that create an account), rolling `session_ttl_days`, and `trusted_networks` CIDRs. Login page, wizard Account step, Settings Account section with token management. Static assets, `/api/v1/info`, ingest receivers, and liveness stay public; anonymous health is trimmed to liveness-only
- Stable instance identity (`/data/instance-id`) surfaced in `/api/v1/info` (`uuid`) and announced over mDNS as `_localsky._tcp.local.` with version + auth TXT records (config-gated via `[network] mdns_enabled`), enabling Home Assistant zeroconf discovery
- Timezone inference: offline lat/lon to IANA lookup autofills the wizard, persists on apply, and backs `GET /api/v1/location/timezone`; the wizard Location step now persists to the draft and gained address search via the Nominatim proxy
- Config validation (`GET /api/v1/config/validate`): structured errors/warnings with stable codes; errors block wizard apply and `PUT /api/config`
- One-sweep network discovery for the wizard (`GET /api/wizard/discover`): passive Tempest detection, Ecowitt UDP broadcast probe, OpenSprinkler LAN sweep; Scan-my-network panels with one-click Add on the Sources + Controllers steps
- Backup + restore: `GET /api/v1/backup` (tar.gz of config + consistent database copy + manifest), `POST /api/v1/backup/restore` (validated config applies live; database stages and swaps at next boot), snapshot listing, and Download/Restore controls under Settings Advanced
- Opt-in update check (`[updates] check_enabled`): daily GitHub releases poll behind `GET /api/v1/updates`; off by default, no telemetry
- Wizard honors its skip promises: skipped Sources synthesize Tempest UDP + Open-Meteo defaults, controllers are optional (weather-only installs are first-class), and DryRun controllers are fully testable with sample zone discovery
- API contract 1.6.0: additive `auth_required` + `uuid` on `/api/v1/info`; new `/api/v1/auth/*` endpoint family

#### Devices + Home Assistant parity

- Device model: a unified registry of gateways, controllers, cloud services, and the HA bridge, each grouping the sensors or zones it provides. `GET /api/v1/devices` and a Devices settings panel
- Native Ecowitt gateway support without Home Assistant: a local-API poller (`ecowitt_gw_poll`, reads `/get_livedata_info` and records soil/weather to history; handles both `ch_soil` and `ch_ec` probes) alongside the existing push receiver
- Native LAN gateway discovery: Ecowitt UDP broadcast (`GET /api/v1/devices/discover`) with a "Discover gateways" button; multi-homed hosts are probed per interface with computed subnet broadcasts
- Home Assistant device import over the WebSocket API (device + entity registries), scoped to weather/soil/irrigation-relevant devices

#### Standalone runtime (Home Assistant optional)

- Native pause / one-day override persisted locally so a no-HA deploy can be paused (HA helpers no longer required)
- Config-fed per-zone weekly water budgets so any configured zone gets a run-time, not just a fixed set
- Configurable Home Assistant controller entity prefix (`deployment.ha_sprinkler_prefix`) so the HA path works for any controller naming, not one hardcoded deployment

#### Engine (was SI + IU, now native)

- FAO-56 Penman-Monteith reference ET0 with ASCE-EWRI 2005 simplified variant and Hargreaves-Samani 1985 fallback when only temp range + lat + DOY are available
- Single-bucket soil water balance with TAW/RAW/MAD per zone; depletion-driven scheduling
- Infiltration-aware cycle-and-soak splitter that respects per-soil + per-slope infiltration rates
- 12-species grass catalog with monthly Kc piecewise curves and UF/IFAS citations (St. Augustine, Bermuda, Zoysia, Bahia, Centipede, KBG, TTTF, PRG, plus ornamental shrubs, vegetable garden, drip / xeriscape)
- 7-class USDA soil texture catalog (FAO-56 Table 19 + USDA NRCS Part 652) with FC, WP, AW, and slope-graded infiltration
- 17-rule skip ladder extracted to `engine::skip_rules`; v0.1 hardcoded constants exposed as `SkipRuleParams` config fields whose defaults preserve previous verdicts exactly
- 7-day verdict strip and water-budget projections derived from native engine

#### Configuration

- Full TOML schema (deployment, features, sources, controllers, zones, llm, notifications, engine params, mqtt, webpush)
- `${VAR}` env interpolation; `env_compat` layer synthesizes a v2 Config from legacy env vars so existing deployments boot unchanged
- Versioned migrations between schema revisions
- Atomic writes (tmp + fsync + rename) with snapshot-before-write into `config_snapshots` (20-version retention) and always-reachable `/api/config/rollback`
- Recursive secret redaction with sentinel + unredact roundtrip on PUT so `/api/config` never leaks tokens
- Validation: target_min < saturation, area > 0, precip in (0, 200], no whitespace in ids, lat/lon ranges; structured error responses

#### First-run wizard + settings UI

- 8-step first-run wizard: Welcome -> Location -> Sources -> Controllers -> Zones -> LLM -> Notifications -> Review + Apply
- Wizard REST endpoints (`/api/wizard/draft`, `/apply`, `/test_source`, `/test_controller`, `/scan_zones`, `/geocode`)
- Settings UI under `/settings/*` with editors for location, sources (list + add/remove/test), controllers (list + add/remove/test), zones (list + per-zone form), LLM, notifications, advanced/engine
- `<SetupGate/>` redirects to `/setup/welcome` until `/data/localsky.toml` exists

#### Persistence

- Hand-rolled migration runner with versioned SQL files and monotonic-version enforcement
- `runs` evolved to v2 (status, controller_id, source, et0_mm, etc) via table-rebuild migration; DB-backed in-flight run state (no in-memory loss across restarts)
- `sensor_history` time-series store with `(epoch, source_id, key)` PK and per-source freshness query
- `verdict_history` with `inputs_json` for engine replay against historical conditions
- `config_snapshots` retention trigger; `push_subscriptions` moved out of `push/store.rs` into persistence layer

#### Controllers HAL

- `IrrigationController` port with three adapters at launch: DryRun (demo + tests), OpenSprinklerDirect (HTTP API, MD5 auth), HaServiceCall (legacy continuity)
- Arc-swap controller registry; hot-reload swaps the default mid-session without interrupting in-flight runs
- `ControllerCaps` declared per adapter (flow_meter, rain_sensor, master_valve, multi_zone_parallel, history_query, remote_program_upload)
- LAN discovery in the wizard: an HTTP /24 sweep finds OpenSprinkler controllers. (LocalSky advertises itself over mDNS for clients to find; it does not browse mDNS for controllers.)

#### Sources + standalone sensors

- `WeatherSource` port + `MergedSnapshot` with per-field provenance `{value, source_id, observed_at}`
- Per-field merge policies (max for rain/wind, min for low temp, configurable priority for ET0)
- DemoReplay synthetic source; TempestUdp (LAN); OpenMeteo (with `et0_fao_evapotranspiration` query)
- **Standalone sensor paths** (no HA required):
  - **MQTT subscribe source**: connects to any broker, wildcards + JSON path + scale/offset; pairs cleanly with the outbound publisher
  - **Ecowitt local source**: POST receiver at `/ingest/ecowitt` for GW1100/GW2000 gateways
  - **HTTP webhook source**: generic JSON POST at `/ingest/webhook/<id>` for ESPHome, custom integrations, scripts

#### LLM provider abstraction

- `LlmProvider` port with two adapters: OllamaProvider (native `/api/chat`) and OpenaiCompatProvider (`/v1/chat/completions`, covers OpenAI, Anthropic-compat shims, vLLM, LM Studio, llama.cpp `/v1`, and any private gateway)
- Boot-time `auto_detect` probes `localhost:11434`, `:8080`, `:1234`; first success wins
- `Advisor` accepts `Arc<dyn LlmProvider>`; TTL cache and prompts unchanged; cache key includes model so swap invalidates cleanly

#### HA bridge (optional, not required)

- MQTT discovery publisher: HA users get auto-created `sensor.localsky_*`, `binary_sensor.localsky_zone_*_running`, `switch.localsky_zone_*_run_now` without LocalSky reading HA
- Outbound publish skips entities tagged `attribution = "LocalSky"` on inbound MQTT to prevent feedback cycles
- Legacy `HaServiceCall` controller for v0.1 continuity

#### Health, observability, demo

- `/api/health` reports per-source freshness (fresh/stale/offline) + per-controller summary + DB + LLM
- `LOCALSKY_DEMO=1`: synthetic data feeder populates TempestStore, IrrigationStore, ForecastStore so the dashboard renders fully without any real hardware; cycling verdicts; 7-day forecast variety; 4 demo zones

#### UI

- Mobile parity polish: zone math reveal + 14-day daily-totals sparkline on `/irrigation/zone/:slug`
- Design system primitives (`<Panel/>`, `<Card/>`, `<Sheet/>`, `<Toggle/>`, `<Slider/>`, `<SegmentedControl/>`, `<FormField/>`, `<EmptyState/>`)
- `<Sheet/>` is viewport-aware (bottom-sheet mobile, centered modal desktop)

#### Open-source readiness

- Apache-2.0 license, NOTICE with citations, `.env.example`, expanded `.gitignore`
- Public README, CONTRIBUTING, SECURITY, CODE_OF_CONDUCT, this CHANGELOG
- Docs site under `docs/` covering getting-started, standalone, controllers, sensors, irrigation-engine, grass-species, soil-textures, skip-rules, configuration, api, hacs

## [0.1.0]

The first LocalSky build: Tempest UDP weather, Open-Meteo forecast, an OpenAI-compatible LLM advisor, and a glass-morphism PWA UI.
