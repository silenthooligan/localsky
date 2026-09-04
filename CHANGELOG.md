# Changelog

All notable changes to LocalSky are documented here. Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Versions follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.8.0] - 2026-09-04

### Added

- **Every zone now shows its real soil deficit.** LocalSky replays the last two weeks of measured ET, rain, and completed runs through a per-zone soil bucket and shows the result on the zone card, the zone detail, and the dashboard's Soil deficit tile, negative when the zone needs water. The dash from 0.7.22 remains only where no bucket can be derived: a zone list from `LOCALSKY_ZONES` with no species or soil texture configured. The Home Assistant soil-deficit sensor and the MQTT bucket sensor publish again on zones that carry a value.
- **A new soil scheduling model waters each zone by its own soil instead of a weekly quota.** Pick it under Settings, then Engine, or per zone in the zone editor. A zone it governs waters when its deficit crosses the zone's own trigger, and each run refills the deficit, so sandy zones water small and often while clay zones water big and seldom, with no session math to set. Forecast rain holds a run only when it would refill that zone's deficit; when more zones trigger than the pre-sunrise window fits, the driest water first and the rest say they wait for tomorrow. A weekly target you set by hand stays honored as a ceiling on what the model may apply in a week. On a brand-new install the model earns its way in: a zone shows no deficit and keeps the weekly behavior until three days of the two-week window carry real evidence, a measured ET day, a rain day, or a completed run. From there the mornings assume a dry yard: days the window does not cover are charged at the model's daily mean, so each zone waters toward filling its bucket at its run cap for the first few mornings, every safety gate still applying, until real coverage accumulates over the full window. Off by default for existing yards, on for new installs; the switch of the default for existing installs comes in a later release.
- **The soil model shows its work on every zone, whichever model governs.** The zone detail's Soil model block says what it waters, or would water, today and when it waters next, and the tuning report adds a comparison line on every zone the weekly plan governs: what the soil model would have watered this morning against what the weekly plan did, on your own yard's numbers.
- **The tuning report stops telling sandy yards to pick a wetter soil.** A zone on sand computes about a day of demand per filling, which is how fast-draining soil really behaves, and the report read that as a misconfiguration and suggested loamy sand. Owners followed it and the misstated texture now costs real behavior, since texture sets how much storm rain counts. A small bucket draws a suggestion only when a root-depth override explains it; oversized buckets keep their one-step correction.
- **Rain the soil can bank per day is in the zone editor.** Settings, then Zones, then the zone, after Sessions per week. Blank shows the value derived from that zone's soil texture and root depth in the box and follows the pickers live; set a number (0.05 to 5 inches) to override it. Sandy soils want it low. The tuning report's balance lines print both figures for a clipped week, so the arithmetic behind a session is auditable in place.
- **Existing yards get a one-time offer to switch to the soil model.** Open the Irrigation or Zones page and a popup says what the soil model sees on your own yard: how many zones carry a live soil deficit, and how many it would have watered differently than the weekly schedule today. Open Engine settings from it to switch, snooze it for 30 days, or dismiss it; a dismissal is kept on the server, so it holds across restarts, browsers, and devices, and closing the popup puts it away until next time. Yards already on the soil model, or with every zone pinned to it, never see the offer. The two notices from 0.7.22, the Home Assistant migration and the inferred watering targets, now pop the same way instead of sitting on the page, and dismissals already made still hold. One notice appears per page load. Whatever else is queued waits for the next one, so deciding hands the page straight back to you.

### Changed

- **The 0.8.0 surfaces got an alignment pass before release.** The Deficit tiles show the magnitude (the label carries the direction), so a thirsty zone reads 0.20 instead of -0.20 beside prose calling the same number a 0.20 inch deficit. A held soil zone's ON HOLD line is now a plain sentence in your display units instead of an echoed machine string with stacked lead-ins and raw millimeters. The zone detail's Soil model panel carries a GOVERNS or PREVIEW pill, states the minutes that actually dispatch (naming the seasonal adjustment when it scaled them), says when it is still gathering evidence and the weekly plan is sizing the runs, and marks the early assumed-dry mornings as an estimate that firms up as evidence lands. The 7-day strip shows a SOIL tag on days the soil model waters through a forecast-rain hold and a RAIN* tag when a hold covers only weekly-model zones, instead of hiding both in hover text; the wording behind them dropped its internal vocabulary everywhere it appeared. Every zone count (the dashboard tiles, the Zones page numbers) now counts only zones that will actually water, so the page totals can no longer disagree with the cards under them, and the labels say this morning, which is when the runs happen. The zone editor puts the Scheduling model above the fields it changes and its Engine default option says which model that is right now; the zone list shows each zone's model and rain cap without opening the editor, and a small model tag appears on zones pinned away from the engine default. Switching the model now confirms with a plain statement of what changes; the setup wizard's final page states the model a new install starts on. The soil-vs-weekly comparison line surfaces on the zone's Tuning panel whenever the two models disagree, with where to switch, instead of sitting in a collapsed note; one name is used everywhere for the weekly target and the rain cap. The Watering targets notice lists only zones that actually water on an inferred weekly target, so it no longer tells a soil-governed yard to set weekly session targets.

### Fixed

- **Metric yards read metric, and the engine stopped asking the machine what time it is.** Two of the most common skip reasons there are, a windy forecast and a wet week, still spoke miles per hour and inches to metric households, because the numbers behind those sentences were not being sent to the browser. They are now, and both read in your units. Everything that still falls back to the server's wording is a sentence with no measurement in it, and a test keeps it that way. The tuning report's balance lines, the panel that explains the watering arithmetic, follow the household's unit setting instead of printing inches for everyone. Separately, the engine no longer reads the timezone from the machine it runs on. That mattered most for watering restrictions, which are a legal question about your wall clock: a server left on the wrong timezone could water on a banned day or block every legal morning. The deployment's calendar is now handed to the engine, and the whole test suite passes under any timezone, which it did not before.
- **A sweep for anywhere else the app answered its own questions.** An audit across the codebase turned up forty places where something outside the engine computed, guessed, or read back a fact the engine already had, and these are the ones you would have noticed. The dashboard's two rain bars drew their threshold marks at fixed numbers, so a yard that tuned either threshold saw a mark that no longer matched the line the engine skips on. A zone skipping because its soil is already wet showed as a plain grey "Skipped" on the Week page with nothing saying why, and a skip caused by having no weather data at all showed the same way on both the Week page and the 7-day strip; both now say what happened. A zone with no configuration was watering two and a half times as long as a configured one asking for the same depth, because its fallback sprinkler rate disagreed with the catalog's own answer for an unknown head. The Cloud and Weather chain drew a source with no set priority in the wrong place. The hero's sentence could promise water for a zone the tile beside it said was holding. The push notification that tells you to set a weekly target no longer fires for zones the soil model governs, which do not use one. And the moisture projection, the pause banners, the soil hold lines and the 7-day tags all now read what the engine decided rather than re-deriving it from the wording of a sentence.
- **The screens stopped keeping their own copy of what the engine knows.** The engine was built to be the one place irrigation is decided, but it compiled server-side only, so nothing on screen could call it. Anything a page had to show before asking the server was retyped by hand, and those copies drifted. The engine is pure arithmetic, so it now compiles for the browser too and the screens call it. What that fixes, on your yard: the dashboard's heat, freeze and wind tiles asked the engine which of its gates tripped instead of re-testing the weather themselves, so the wind tile no longer goes loud on nights the engine was never going to skip, and the heat tile now honors the thresholds you set and the switch that turns the gate off. Reset on the Skip rules page writes the values the engine actually uses; it had been writing three the engine never chooses, so clicking it moved a yard onto settings no default install runs. The setup wizard's species cards state the numbers the catalog really holds, having drifted from them on most species. The zone page's water-use estimate includes the heat multiplier the engine applies, so the days-until-watering estimate stops running long in a heat wave. The soil moisture projection uses your configured capture efficiency rather than a fixed one. The soil model charges the same starting demand the weekly plan does when a zone has no measured days yet. The tuning report prices a proposed longer run through the seasonal dial, the way the morning is actually dispatched, so a yard dialled below 100% stops being told a raise will not fit when it will. And six places that decided the yard was paused by reading the wording of a sentence now read the pause itself, including the timed vacation pause one of them missed.
- **What a zone is planted with decides its water, not what it is called.** Two starting values were guessed from words in the zone's name: the weekly target a zone waters toward before you set one, and the crop coefficient behind the 7-day soil moisture projection. A zone whose name contained shrub, garden or bed started on half an inch a week, everything else on a full inch, and every projection ran at a fixed coefficient belonging to no species and no season. So a vegetable bed, which transpires harder than turf, started on less than half the water it wants; a lawn a previous owner named for a flower bed was watered like a shrub; and a dormant winter lawn was projected drying about twice as fast as the engine expected. Both now come from the species you pick in the zone editor: the weekly target scales by that species' peak crop coefficient against reference turf, and the projection reads its actual crop-coefficient curve for today, shifted six months below the equator so a southern-hemisphere yard reads January as summer. Turf keeps the inch a week it always started on. Zones with a target already set are untouched, and a zone list from LOCALSKY_ZONES with no species keeps the old name-based default, since the name is the only signal such an install has.
- **The soil table now matches the source it cites, on every texture.** Three entries sat outside FAO-56 Table 19, the published table the catalog names: loam claimed more available water than any soil in that class holds, and it claimed more than silt loam, which inverts the ordering every soil table carries. Loam, clay loam and clay are now inside the published range, so a loam zone banks 0.89 inches of a day's rain instead of 1.30 and its bucket triggers a watering sooner. Sand, loamy sand, sandy loam and silt loam were already inside the range and did not move, so yards on those textures see no change. A test holds every texture inside its published band from here on. The zone editor also stopped keeping its own copy of these numbers: the cap the editor shows and the cap the engine clips at now read the same table.
- **A sandy yard no longer skips a whole week of watering after one storm.** The weekly balance credited rain as a raw 7-day total, so 1.2 inches in one afternoon read as more than a 1.0 inch weekly target and every zone planned zero for seven days. On sand that water is gone within hours: soil holds only so much per day, and the rest drains below the roots. Rain now credits per day, and each day counts only up to what the zone's root zone can hold; the rest does not count against the week, so watering resumes mid-week once the soil's share is spent. What one day can count is derived from the zone's soil texture and root depth: at default turf roots, sand banks 0.35 inches, loamy sand 0.47, sandy loam 0.77, loam 0.89, clay 0.83, clay loam 0.94, and silt loam 1.00, every figure taken from the published FAO-56 soil table. A loam yard therefore only notices this in storms above 0.89 inches in a single day, and any week whose rain never beat the cap on any day settles exactly as before, wording and all. When a day is clipped, the zone's ON HOLD line says so plainly: what fell, what counted, and that the rest drained past the roots. The forecast credit takes the same per-day cap, so a forecast 2 inch day cannot bank 2 inches either. Water you applied by irrigation still counts in full: runs are sized to what the soil takes, a storm is not ([#9](https://github.com/silenthooligan/localsky/issues/9)).

## [0.7.22] - 2026-09-01

LocalSky stops reading Home Assistant to decide anything and carries the seven helper values across first, Home Assistant deployments that were planning nothing start watering, the app tells you why a zone is not watering, and it stops showing a soil deficit it never measured.

### Fixed

- **Every zone's "Deficit" read exactly 0.00 and never changed.** That number came from a Home Assistant integration called Smart Irrigation and from nothing else, so on an install without it the reading missed and printed a zero, on every zone, on every refresh, since the day you installed LocalSky. The zone card, the zone detail, the dashboard's Soil deficit tile and the zone math panel now show a dash, because nothing measured it ([#9](https://github.com/silenthooligan/localsky/issues/9)).
- **Every zone's "Today" tile read 0 min whatever the zone had done.** Nothing on either deployment path adds up a zone's valve-open minutes since local midnight, and the field was a plain zero rather than an absent value, so the tile printed "0 min" next to a hold line saying an inch of water had already been applied this week. It now reads a dash on the zone card and on zone detail, and Home Assistant no longer gets a `<slug>_run_today` sensor recording that zero into long-term statistics. Same treatment as the soil deficit above: no producer, no number ([#9](https://github.com/silenthooligan/localsky/issues/9)).
- **Rain Delay and the vacation pause never worked on a Home Assistant deployment.** The pause expiry was read from `input_datetime.irrigation_pause_until` as a whole number, and Home Assistant reports that field as a decimal, so it never parsed and the pause silently resolved to "not paused" on every install, always. Tapping Rain Delay wrote the entity, the next refresh read nothing back, and the yard watered on schedule with nothing on screen saying otherwise. It parses both shapes now. If you have tapped Rain Delay in the past and watched it water anyway, that was this.
- **An install zoned by `LOCALSKY_ZONES` with no `localsky.toml` got per-zone verdicts for four zones it does not have, and none for its own.** The per-zone decision is produced once per entry in the soil list. That list is built from the zones in `localsky.toml`, so on an install with none there it fell back to four fixed zone names carried over from the deployment this app was first built for: back yard, front yard, side yard, back yard shrubs. A yard of seven zones on that shape received four verdicts belonging to zones it does not have, and none for the seven it does. The fallback is now built from your own zones; each has no soil reading unless it is one of those four names, and the soil rules skip a zone with no reading instead of treating it as a probe that went quiet. An install with zones in `localsky.toml` already had a verdict per zone, probe or no probe, and nothing changes for it.
- **A zone that is not watering now says why, in words.** LocalSky has always worked out the reason per zone and per day, and never showed it anywhere. A zone the weekly water budget zeroed now reads **ON HOLD** rather than IDLE, with the reason under it: the week is already covered by rain and prior watering, rain is forecast within 24 hours, or the session spacing has not elapsed since the last run. It is on the zone card and the zone detail ([#9](https://github.com/silenthooligan/localsky/issues/9)).
- **A manual schedule now says which days it turns smart watering off.** A schedule's mode defaults to Override, which stops smart watering for that zone on every day the schedule covers, and nothing on any screen said so. Someone who added a schedule because smart watering was not running had switched it off on those days without being told. The zone card and the schedule's own card now name the days, and on a day an Override schedule covers, the zone reads **ON HOLD** naming the schedule instead of the same IDLE a zone with nothing planned shows. The list of days stays on screen on the day the schedule is firing, so a zone blocked all seven days reads differently from one paused for a day. If the weekly budget was holding the zone as well, both reasons are shown, so turning the schedule off does not leave you with a zone that still will not water and nothing saying why. Delete the schedule to hand those days back to the engine, or switch it to Floor if you want the scheduled run to stay as a minimum. Note that a Floor run resets the session spacing too: the engine only adds on top once the zone's spacing has elapsed, so on a zone whose schedule fires as often as its own weekly session cadence there is still no day for it to add on.
- **The rain-defer gate treated an unlikely drizzle as certain rain.** It added up the next 24 hours of forecast rain without regard to how likely any of it was, and cancelled the day's watering at a tenth of an inch. A model showing scattered afternoon storms at 20% could cancel watering nearly every day, invisibly. The forecast is now weighted by its own probability, the same way the rest of the water balance already treats it.
- **The rain-defer threshold in your config is now the one the engine uses.** `engine.session_rain_defer_in` is documented and editable, and the engine read a fixed value instead, so changing it did nothing at all.
- **The "capped" warning on a zone's math panel never fired without Home Assistant.** It read a flag computed from the Smart Irrigation soil deficit, which a standalone install never had, so on those installs the panel told every zone it was under its cap even when the weekly budget wanted a longer session than `max_run_minutes` allows. With the deficit gone on every install, the warning is computed from the weekly budget instead. The Scheduled row now says "capped at N min" in the warning color when tonight's run is sitting on the ceiling because something wanted more than the ceiling: the weekly budget's own session, the seasonal adjustment, or a condition rule. The seasonal adjustment used to shorten a run at the ceiling and say nothing, while a condition rule doing the identical thing said so. A zone with no run planned says nothing about a cap at all, because there is no run for a ceiling to have shortened.
- **Upgrading over MQTT discovery left the deleted soil-deficit sensor holding 0.00.** The sensor stopped being published, but its discovery and state topics were retained on the broker, so Home Assistant kept the entity and kept showing the last fabricated value. LocalSky now clears both retained topics, which removes the entity. If you have already turned MQTT publishing off, clear them on the broker by hand: publish an empty retained message to each of the two topics, per zone, with `mosquitto_pub -h <broker> -r -n -t '<discovery_prefix>/sensor/<node_id>/zone_<slug>_bucket_mm/config'` and again with the same topic ending `/state`. `<discovery_prefix>` is `homeassistant` unless you changed it, and `<node_id>` is your deployment name, slugified.
- **A morning where the controller refused to start a zone now appears in History.** A failed dispatch wrote no record of any kind, so a morning that failed looked exactly like a morning with nothing planned. The failure is now recorded per zone with the controller's own explanation. On a morning where every zone's record is a dispatch failure, it does not count as the morning being handled: if LocalSky restarts while the watering window is still open and the controller is reachable again, the catch-up waters. A morning that mixes a failure with a zone the engine itself decided to skip does count as handled, because the skip is a decision about the yard; the refused zones wait for tomorrow. If the controller failed partway through and two or more zones had already watered, the morning still counts as handled and the zones that never ran wait for tomorrow.
- **A zone your controller reports as already running is never started a second time.** Dispatch now checks the live state before it commands a zone on, so a catch-up run after a restart cannot re-open a valve that is already open. Controllers that cannot report zone state (MQTT) are unaffected: an unknown state is not treated as running, so those installs water exactly as before.
- **`sessions_per_week` is now held to 1 through 7.** Above 7 the spacing between sessions worked out to less than a day, which stopped the gate that keeps a zone from watering twice in one day. Values outside the range are refused when you save, with the range in the message. A value already in your config file is clamped into range as it loads, so the file still loads AND your other settings still save: the refusal gates whole-config writes, so without the clamp one stale value would have refused every unrelated save until you found it by hand.
- **An install with both Home Assistant and a controller of its own never recorded that it had watered.** With `HA_URL` set and a Rachio, an OpenSprinkler, or any other controller configured, LocalSky sent Run and Stop to the controller but read each zone's running state from a `binary_sensor.opensprinkler_*` entity that does not exist on that install. Running was permanently false, so no run was ever written to History and the weekly water balance credited none of the water applied. A controller that reports its own state is now what the running state comes from, per zone. Until this release the missing rows changed no run length on that install: run lengths on a Home Assistant deployment were sized by the Smart Irrigation entity and by nothing else, and the weekly balance was computed and shown there but sized nothing. From this release the balance sizes every run on every deployment, and it needs those rows: it credits the water already applied against the week's target and paces sessions off the last recorded run, so without them every session would have been sized as if the week were untouched and a zone could have planned a session on every morning it was otherwise clear to water. The fix lands with the change that needs it. Check each zone's Weekly target and Sessions per week under Settings, then Zones, if the cadence is not what you want.

  One further change comes with it. History rows are how LocalSky knows a morning was already handled, so on an install where the Smart Irrigation entity had it dispatching at all, a restart inside the watering window re-ran the morning because no row said it had happened. Now that runs are recorded, a restart after two or more zones have watered leaves the rest of that morning alone.

  Master enable and water level on that install now come from the controller too. Neither reaches the watering decision; the indicator simply changes where it is read from.
- **Rain delay on a Home Assistant deployment with no pause helper silently did nothing.** The 24h, 48h and 72h buttons wrote to `input_datetime.irrigation_pause_until`, and on an install that never created that helper the write went nowhere, no error was shown, and the next tick read no pause. Tapping Rain delay and then watching the yard water is now not possible: the pause is kept by LocalSky.
- **The vacation pause and dry-run toggles answered 500 on every standalone install.** Both wrote Home Assistant `input_boolean` helpers on every deployment, so with no Home Assistant there was nothing to write to, and the engine read a value nothing could set. Both are LocalSky's own now and work with no Home Assistant at all.
- **The one-day override for tomorrow could not clear itself.** Resetting it each midnight was a Home Assistant automation you had to write yourself, so a standalone install that set "skip tomorrow" kept saying skip tomorrow, every day, forever. LocalSky now expires it at your own local midnight, stamping the day it was set on in your configured timezone. An override already sitting in LocalSky's own storage when you upgrade reads as no override, because it predates the day stamp and there is no day it can honestly claim; set it again if you still want it. One taken from `input_select.irrigation_override_tomorrow` by the migration is stamped with the day it was read, so it applies to that day and expires that midnight.

### Known limitations

- **A Skip or a Force you set from the Override control still does nothing on a Home Assistant deployment.** The control writes it to LocalSky's own storage, which is where it belongs, and the Home Assistant snapshot builder fills the same field with "auto" on every tick, so the engine never sees it. The panel shows Skip and says "Skipping every zone until you switch back to Auto" while the yard waters, and the same holds for a per-zone Skip or Force on a zone card. This predates this release and this release does not change it: those two controls behave exactly as they did before the upgrade, and nothing stored in them starts deciding. It is being fixed on its own, separately from the Home Assistant migration below, because switching a stored Force on for the first time is a watering change that needs a release of its own to explain it. Standalone installs are unaffected: the Override control has always worked there.
- **If LocalSky restarts while a smart-morning run is in progress, that run is missing from History.** A smart-morning run is recorded when the zone stops, so a restart between start and stop loses the record. The water still went on the ground; nothing on screen will say it did, and the weekly water balance will not count it, so the zone may be watered again sooner than it needed. It predates this release. Runs you start by hand and runs from a manual schedule are not affected the same way: those are written to History the moment they are dispatched, at their full planned length. Because LocalSky closes every valve at boot, a restart part-way through one of those leaves History and the balance crediting more water than actually fell. Restarting outside your watering window avoids both.

### Added

- **Weekly target and Sessions per week are in the zone editor.** Settings, then Zones, then the zone: the two numbers that size every run. Blank shows the default LocalSky infers from the zone name in the box (0.50 inches over 1 session for a name containing shrub, garden or bed, 1.00 inches over 2 sessions otherwise), and the zone list marks every zone still watering on that inferred default. Until now the only ways to set them were the raw config editor, the tuning report's Apply, and `PUT /api/config`.

### Changed

- **Home Assistant deployments: run lengths now come from the weekly water budget, and zones that were planning nothing will start watering.** Run lengths there used to be sized by a Smart Irrigation entity and by nothing else. An install without that HACS integration read the absent entity as a zero deficit, so it planned zero minutes on every zone and the smart morning dispatched nothing, on every morning since it was installed. Those installs begin dispatching real valve runs on the first morning after this upgrade. Run lengths now come from the same weekly water budget standalone installs have always used. **If you never set a zone's weekly target and sessions per week, LocalSky uses a default inferred from the zone name**: 0.50 inches a week over one session for a zone whose name contains shrub, garden or bed, and 1.00 inches over two sessions for every other zone, each held to that zone's maximum run time (60 minutes unless you changed it). On a default zone that works out to the full 60 minute ceiling on the first eligible morning. **Before the next morning window, open Settings, then Zones, open each zone and set Weekly target and Sessions per week, and check Max run time.** Both fields are new in the zone editor in this release; blank shows the inferred default in the box, and the zone list marks every zone still on an inferred target. The Irrigation page and the Zones page raise a one-time notice naming every zone that is watering on an inferred target and the target it will use; it is dismissible and goes quiet once every zone carries a target you set. On a Home Assistant deployment LocalSky also logs a warning naming those zones, with the target and the seconds planned, the first time after a start that it plans a run for one, and sends one push notification to subscribed devices. To hold everything while you review, set Rain delay on the irrigation page before you upgrade, long enough to cover the review. Do not use the Override control for that on a Home Assistant deployment: a Skip stored there decides nothing, as the known limitation above says. Do not use `input_boolean.irrigation_pause` either: the migration below adopts that helper's value in this same release and stops reading it, so turning it back off in Home Assistant will not release the hold. Release the Rain delay hold under Rain delay afterwards, and a pause-switch hold from LocalSky's Vacation pause toggle. The migration notice says so on screen if you upgraded with either on.
- **An install whose zones come from `LOCALSKY_ZONES` with no `localsky.toml` keeps its planned minutes.** The allocator plans for the zones it holds budget rows for, and those rows are built from the config file, so an install zoned by the environment variable had zones and no rows. With the allocator now sizing dispatch on both paths, every one of those zones would have planned zero seconds, which is the number the `<slug>_planned_run` sensor and the `zone_<slug>_planned_seconds` MQTT sensor publish and that Irrigation Unlimited automations drive valves from. Those installs would have stopped watering with nothing on screen. The allocator now gets a row for every active zone, so they plan from the same name-inferred defaults described above and appear in the same notice.
- **LocalSky no longer reads Home Assistant to decide how much to water.** Three reads are gone: the Smart Irrigation entity that supplied the soil deficit and the crop coefficient, and the `input_number` helpers that could override a zone's weekly target and its sessions per week. The crop coefficient now comes from LocalSky's own species catalog, by species and day of year, and it flips seasons south of the equator. Nothing is deleted from your Home Assistant: the Smart Irrigation entity and those `input_number` helpers stay exactly where they are, LocalSky just stops reading them. **Those two helpers, `input_number.irrigation_<zone>_weekly_budget_in` and `input_number.irrigation_<zone>_sessions_per_week`, are not carried across.** If you had set either, enter the value on the zone under Settings, then Zones, as Weekly target and Sessions per week; otherwise, from the first morning after the upgrade, the zone waters on the value in your config or, with none, on the default inferred from its name.
- **LocalSky no longer reads Home Assistant to decide anything, and it carried all seven helper values across before it stopped.** Seven helper entities were still deciding: `input_number.irrigation_max_wind_mph`, `input_number.irrigation_min_temp_f` and `input_number.irrigation_rain_skip_in`, which **outranked** the matching thresholds in Settings whenever the helper existed; `input_datetime.irrigation_pause_until` and `input_select.irrigation_override_tomorrow`, the vacation pause and the one-day override; and `input_boolean.irrigation_pause` and `input_boolean.irrigation_dry_run`, the pause and dry-run switches. Once Home Assistant has answered the same way for long enough to be believed, LocalSky reads all seven once, writes each value into its own storage, and never reads those entities again. How long depends on what it found, and the rules are spelled out below.

  **The value LocalSky adopts is the value that was already deciding**, so the first morning after the upgrade decides as the morning before it did. What changes is that the number is now somewhere you can see and edit. If your Settings page said 10 mph while the helper said 12, LocalSky is using 12, the same as always, and Settings now says 12 too. There are two exceptions, both spelled out below and both named on screen: a threshold set outside the range LocalSky can hold moves to the nearest value it can hold, and a vacation pause, pause switch or dry run already sitting in LocalSky's own storage from a standalone era starts deciding again, because a Home Assistant deployment stored those and never read them. If a pause is what LocalSky kept, or what came across, the migration notice says the yard is held and where to release it: a timed pause under Rain delay, the pause switch from the Vacation pause toggle. Check Rain delay, the Vacation pause toggle and Dry run on the irrigation page after upgrading.

  The irrigation page raises a one-time, dismissible notice naming every entity, what LocalSky uses now, and, for any value where the two disagreed, both numbers and which one was in effect. It speaks on every Home Assistant install the migration ran on, including one that never created a single helper: nothing was adopted there, but the four controls became LocalSky's own, which is what makes Rain delay work.

  **Nothing is deleted from your Home Assistant.** All seven helpers stay exactly where they are. They no longer do anything: turning `input_boolean.irrigation_pause` on will not pause watering, and setting one of the `input_number` helpers will not change a threshold. **If a Home Assistant automation writes to any of them, point it somewhere else or it will stop having an effect with nothing to show for it.** The three thresholds are `number` entities the LocalSky integration already publishes. For the pause, the one-day override and dry run, use `POST /api/irrigation/action` with an API token. After upgrading, deleting the helpers is safe and changes nothing.

  Two exceptions, and the notice names them on screen if either applies to you. On a deployment with no persistence database mounted, the four control helpers were never taken over and are still deciding; do not delete those until `/data` is mounted and LocalSky has restarted. And on a deployment with no `localsky.toml`, one zoned by `LOCALSKY_ZONES` alone, the migration has nowhere to record itself and does not run, so all seven helpers are still deciding exactly as before; LocalSky logs that once at start, and the notice says so. Finishing the setup wizard writes the file, and the migration runs on its own after that.

  **A helper holding a number outside what LocalSky can represent is adopted at the nearest value it can hold, and the notice prints both.** That is the one place a threshold moves. Setting `input_number.irrigation_max_wind_mph` to 99 is how people switch the wind gate off, and a helper's own maximum is whatever you gave it, so nothing stopped one going past what LocalSky can hold. Whatever it held was the number deciding, because the helper outranked Settings. LocalSky holds 0 through 50 mph, 20 through 70 F and 0 through 10 inches, so 99 mph becomes 50 mph, which still means effectively never wind-skip. Reverting to the Settings value instead would have started skipping on the first breezy morning without saying so. The migration notice names any threshold this happened to and prints what the helper held next to what LocalSky is using.

  **A helper that is missing, or holding something that is not a number or a mode at all, is never adopted as a value.** It is recorded by name, LocalSky keeps the value it already had, and the read is retired anyway. For the three thresholds that changes nothing whatsoever: a missing or unreadable helper already resolved to the Settings value, which is the value LocalSky goes on using.

  **The four controls are treated more carefully than that, because for them an absence is not the same as the value they hold.** A control that Home Assistant reports as `unavailable` or `unknown` is left alone entirely: LocalSky keeps reading it and tries again later. An entity in that state exists and is briefly broken, which is what a helpers reload or a restore from backup looks like, and it says so in exactly the same words on every poll, so waiting for a steady answer proves nothing about it. A control that is simply absent is only concluded to be absent once Home Assistant has answered identically, with an unchanging entity count, for five minutes. Home Assistant answers `/api/states` long before its `input_*` helpers exist, and on the Home Assistant OS add-on LocalSky starts before Home Assistant has finished coming up, so a first answer with no helpers in it is ordinary rather than rare. **A vacation pause set in `input_datetime.irrigation_pause_until` is never dropped by an upgrade landing in the middle of a Home Assistant restart.**

  **A control LocalSky's own storage already holds an answer for keeps that answer.** An install that ran standalone and later gained `HA_URL` can still have the old helpers sitting in Home Assistant; the value you set in LocalSky is the more recent one, so it wins and the helper is simply retired. This is the one place a control that was not deciding starts deciding: a Home Assistant deployment stored that value and never read it. A vacation pause counts as an answer only while it is still running, so an expired one is not kept and the helper's pause is taken instead. The migration notice names every control this happened to, and says so plainly when the result is that watering is held.

  **Home Assistant being down during or after the upgrade holds nothing and stops nothing**: the yard waters on the values it already has, exactly as it did during any other outage, and the migration waits.

  **Two reads stay, and a third on one install shape, and this says so rather than implying otherwise.** A zone with a Home Assistant entity assigned as its soil sensor still reads that entity: that is a sensor you pointed LocalSky at by name, not a decision LocalSky is outsourcing. An install whose zones come from `LOCALSKY_ZONES` with no `localsky.toml` still reads `sensor.<zone>_soil_moisture` and `input_number.irrigation_<zone>_saturation_pct` for the four zone names the previous release read them for, back_yard, front_yard, side_yard and back_yard_shrubs, and for no other zone; an install with zones in `localsky.toml` never made those reads and still does not. And nine legacy `sensor.open_meteo_*` REST sensors still sit at the bottom of the forecast ladder as a fallback for a field no configured source owns: `rain_today`, `rain_tomorrow`, `rain_3day`, `eto_today`, `eto_tomorrow`, `eto_3day_avg`, `temp_max_today`, `temp_min_today` and `humidity_mean_today`. Deleting `sensor.open_meteo_rain_today` on an install with no rain gauge is the one with teeth: nothing in LocalSky reads today's modelled rain from its own forecast yet, so today's rain would drop to 0.00 and stop firing the rain skip. Those rungs need a new native reading rather than a migration, and they are the next release's work.

  On a Home Assistant deployment with no persistence database mounted, the three thresholds still migrate and the four controls do not, because a control needs somewhere to be kept; their helpers keep working, and the migration notice names them and says not to delete them. Every install keeps a record of what happened in `localsky.toml`, so the answer to "why is my max wind 12" is in the config file six months from now.

  That record is a migration ledger rather than a setting, so it survives what a setting would not. Saving anything in Settings cannot drop it, the raw config editor cannot drop it, restoring a backup taken before the upgrade cannot drop it, re-running the setup wizard cannot drop it, and rolling `localsky.toml` back to a snapshot from before the upgrade restores the values it was asked for while leaving the record in place. None of those put the helpers back in charge, which matters because this release tells you the helpers are inert and invites you to delete them: putting a read back on a helper you have since deleted would read a vacation pause as no pause. A backup restore is where that would have been worst, because the restored config takes effect immediately while the restored database only loads at the next restart.

  The code that reads those entities is still present in this release, unreachable on any install that has migrated, so that an install that has not run the pass yet keeps working. It is deleted in the next release.
- The "Why this duration?" panel no longer prints a formula. The one it showed belonged to that Smart Irrigation integration and matched nothing LocalSky computes. The panel is now in two parts: throughput and the scheduled minutes, which are what set the run length, and below them the crop coefficient, heat multiplier and capture efficiency, which are computed and shown but size nothing, alongside the soil deficit, which reads a dash because nothing computes one. They had been sitting in one list with multiply and divide signs on them under a heading asking why the duration is what it is.
- A second, unreachable copy of the "Why this duration?" panel is deleted. No page rendered it, and it was where that Smart Irrigation formula and a hard-coded limit of four zones had survived. The panel you actually see is a different one, and it has always shown every zone.
- The demo no longer shows a soil deficit either. A demo number nobody's install can produce is how the screenshots came to promise one.
- Documentation: the engine guide described a soil depletion bucket as the model that decides watering. It is written and tested but nothing calls it, so the guide now describes the weekly water balance, which is what actually decides, and says the bucket is coming. The whole book is swept the same way: water budget, troubleshooting, manual schedules, zone math, configuration, Home Assistant migration, soil textures, soil sensors, the first-soil-sensor walkthrough, sensors, zones and the standalone comparison. The setup wizard's own description of the engine and the species line in the zone editor said it too, and no longer do; nor do the schedules page, the zone editor's soil-sensor field, the help page and the Home Assistant page, which now name the weekly water balance, and the nerd mode description now names what nerd mode actually shows. The water budget page had said the weekly target is computed from ET and moves with the season; it is a flat per-zone setting and always was. The configuration reference now lists `weekly_budget_in` and `sessions_per_week`, which decide how long a zone runs and were missing from it. It also listed `capture_efficiency` and `et0_method` as if editing them did something. Neither reaches the watering decision: capture is fixed at 0.70 and the ET0 method is always chosen automatically, and there is no setting that makes a dashboard read "ASCE". Both still parse, and the reference now says they are inert. `capture_efficiency` keeps one live reader, the tuning report's measured-sprinkler-rate check on a zone with a soil probe bound.
- API contract 1.25.0, manifest schema 1.6. `zones[].bucket_mm` and `zones[].math.bucket_mm` are nullable, and the per-zone soil bucket sensor is no longer published to Home Assistant, the same treatment water level and the per-zone soil sensors already had. `zones[].today_run_minutes` is nullable for the same reason and is now null on every install, so the `<slug>_run_today` descriptor is gated out too; a Home Assistant install loses that per-zone sensor, which was reporting a fabricated zero. MQTT never published it, so there is nothing retained to clear. New `zones[].smart_suppressed` names the days an Override schedule is stopping smart watering. New `water_budgets[].target_inferred` is true when that zone's weekly target and sessions per week came from the name-inferred default rather than your config; the Zones page reads it for the one-time notice. New validation error `zone_sessions_per_week_range`, and `PATCH` of a zone's `sessions_per_week` outside 1..=7 is refused. Additive `ha_adoption[]` on the irrigation snapshot and in `localsky.toml`, one entry per retired Home Assistant helper with the value taken, the value it replaced, and, where a threshold sat outside the range LocalSky can hold, what the helper actually held. `POST /action` with `set_threshold` writes `engine.skip_rules` rather than an `input_number` helper once that helper is retired, and answers `400` outside 0 to 50 mph, 20 to 70 F and 0 to 10 in, where the old path passed any number straight to the helper; the sliders the shipping integration builds (0 to 50 mph, 20 to 60 F, 0 to 1 in) all sit inside those ranges, so no slider value is refused; `toggle` writes LocalSky's own control store rather than an `input_boolean`. Both keep their request and response shapes.

  Manifest schema 1.6 adds `min`, `max` and `step` to `number` descriptors, so the integration builds the three threshold entities on the range the server enforces. **An existing Home Assistant integration keeps working and needs no change**: built against 1.5 it builds those three unbounded, and a write outside the range reaches the server and is refused there with a message naming the range. Existing "soil bucket" entities become unavailable and can be deleted, and on the MQTT path they are removed for you.

## [0.7.21] - 2026-08-29

Pick your controller's zone from a list instead of matching names by hand.

### Changed

- **The zone editor's "Controller station" field now lists your controller's own zones and you pick one.** Where LocalSky can ask the controller (OpenSprinkler, Rachio, a DIY HTTP board, the simulated controller) the field shows its zones by the controller's name for each and stores its id, so a Rachio zone called "Front Lawn" can fire a LocalSky zone called "Front Yard" with nothing typed and nothing renamed. Where the controller cannot be asked (Hydrawise, B-hyve, Rain Bird, Home Assistant) the field is the text box it has always been, and it stays a text box when a controller is offline, out of daily requests, or simply reports nothing, with a line saying which. There is always somewhere to type an id by hand ([#8](https://github.com/silenthooligan/localsky/issues/8)).
- **A zone's own binding is now the thing that fires it, and your controller's zone map keeps working as before.** On startup, a zone with no station of its own picks up the id the controller's map already held for it, so an existing install waters exactly as it did and its bindings become visible on the zones themselves. Nothing is overwritten, nothing is removed, and a binding you set by hand is never touched.
- Renaming a zone is no longer offered as a way to make a controller find it. A zone's internal id is permanent, because its run history, its soil sensor, its Home Assistant entities and its links are all stored under it, and the guide, the editor, and the "zone will not start" message now all say so. Editing a zone's Name is free and always was; only the internal id is fixed.
- API contract 1.24.0: `ZoneConfig` gains `controller_zone_name`, the controller's own name for the bound zone, as a display label that nothing dispatches on. `controller_station` keeps its shape and is now optional in a hand-written config. New validation warnings `zone_unbound`, `zone_station_unparseable` and `zone_controller_not_built`. Both `PUT /api/config` and `PUT /api/config/raw` answer `422 zone_key_renamed` for a save that renames a zone key, with `?allow_zone_key_change=1` to override. The unmapped-zone `400` hint stops suggesting a rename. No Home Assistant integration change is required.

### Added

- **Bind every zone a controller scan found, in one step.** Scanning a controller under Settings, then Devices now ends in a table: each zone the controller reported, its id, and a dropdown of your zones. Choosing there binds them all at once. It binds zones you already have and never creates one, and it refuses to point two of the controller's zones at the same one of yours ([#8](https://github.com/silenthooligan/localsky/issues/8)).
- The zone card and the zone editor now show which of the controller's zones a zone fires, by the controller's own name for it, so you can check the wiring without opening anything.

### Fixed

- **A zone bound to nothing now says so, instead of quietly never watering.** A zone with no station and no entry in its controller's zone map looked completely healthy: it just never ran. It now carries an "Unbound" mark on its card, a line in its editor, and a warning in the config check, before the first night it fails to water.
- **A Home Assistant zone bound by the "Controller station" field now actually runs.** The editor has asked HA users for an entity id in that field for some time and nothing read it, so a zone bound that way never watered. It is now used, alongside the controller's own entity map, and a value that is not an entity id is ignored with a log line rather than sent to Home Assistant. The MQTT controller says plainly that its zones bind by command topic instead, and the ESPHome text no longer implies the field does anything there. If a Home Assistant zone carries both a station value and an entry in the controller's entity map, the station value now wins: the log names every zone whose target moved, so check it once after upgrading and clear the station field on any zone that was already watering correctly.
- A zone on the simulated controller no longer reads as unbound. It accepts any zone whether bound or not, so marking it needed attention was wrong and taught people to ignore the mark.
- An MQTT zone with something typed in its station field no longer reads as bound. MQTT zones bind by command topic and payloads, which that field cannot carry, so anything in it was never going to water the zone.
- A zone on an ESPHome native controller now says the adapter is not built yet, which is the real reason it never waters, instead of reporting on bindings that could not fire either way.
- **A zone holding an id its controller cannot use now says so, instead of looking bound and never watering.** Move a zone from one brand of controller to another and its old id stays in the station field: a Rachio zone UUID means nothing to a Hydrawise, which addresses zones by relay number. The field was not empty, so nothing flagged it, and the only symptom was a zone that quietly never ran. The zone now carries the same Unbound mark as any other, and the config check names the value, the controller, and what that controller expects instead. A zone whose controller still has it in its own zone map keeps watering and is not flagged.
- Editing an unrelated setting on a zone can no longer clear its binding. The zone form writes the station field on every save, so a save made while the controller could not be reached would have blanked it. A blank now only clears a binding when the controller's zones were actually listed and you chose "(not bound)".
- Renaming a zone key is no longer possible by accident. Doing so silently orphaned that zone's run history, its overrides, its in-flight run record, its dismissed suggestions, its soil sensor channel, its Home Assistant entities and its retained MQTT topics. The save is now refused with an explanation on both config save paths, and the raw editor offers a confirm button for the case where one zone was genuinely deleted and another added.
- Adding a zone whose name matches one you already have no longer replaces it. The add form inserts by internal id, so a second "Back Yard" quietly overwrote the first, taking its binding and its settings with it. It now says which zone already uses that id and asks you to edit that one.
- Binding zones after a controller scan now shows which zones are already bound, pre-selected, instead of showing every row as unbound. Checking a working install could otherwise read as "nothing here is bound yet" and lead to rebuilding bindings that were already right; a bind that moves an existing one now says how many moved.
- A controller zone that reports no id of its own can no longer be bound, which would have blanked a working zone while reporting success.
- Switching a zone's controller now clears the station id, which only ever meant something to the controller it came from. Carrying it over left the zone pointed at an id the new controller has never heard of, and because the field was not empty nothing reported it.
- A controller that cannot be reached is no longer asked again every time you click into the station field. The result is remembered until you press Rescan, so a rate-limited account is not spent by a few clicks.

## [0.7.20] - 2026-08-29

### Fixed

- **Zones bound to a Rachio controller could send the wrong zone id and fail to start.** If a zone's "Controller station" field held a station number, that number replaced the zone UUID the controller's own scan had found, and Rachio rejected every attempt to run the zone. Rachio addresses zones by UUID, so a station number there is now ignored, with a log line naming the zone, and the scanned UUID is what gets used. Hydrawise, B-hyve, and Rain Bird do address zones by number, so on those three the field still binds the zone and still overrides the scanned map ([#8](https://github.com/silenthooligan/localsky/issues/8)).
- **A zone whose name differs from the controller's name for it could not start, and nothing said why.** Scan zones keys the controller's map by the controller's own zone names, while a run looks it up by your zone's slug, so a zone you called "Front Yard" never matched a Rachio zone called "Front Lawn". The failure now lists the keys the controller actually has next to the slug that missed, and names both fixes: rename so the two match, or put the vendor's zone id in the zone's "Controller station" field. Clearing that field does not help, and the message no longer suggests it ([#8](https://github.com/silenthooligan/localsky/issues/8)).
- **Controller failures now say what actually went wrong instead of a bare status code.** A rejected credential, a spent daily API budget, a vendor rejecting the zone id, and a controller that could not be reached all arrived as the same "HTTP 502", and a zone that is not mapped to a controller zone arrived as a bare "HTTP 400". In every case the server's explanation was discarded before it reached the screen. Each now reports its own reason, carrying the vendor's own message when there is one, and the same reason is written to the log at the moment of the attempt ([#8](https://github.com/silenthooligan/localsky/issues/8)).
- A zone command that failed could show no error at all: the result arrived after a live update had already replaced the part of the page waiting for it, so on the Zones page the only message left was the timeout warning 25 seconds later. Fixed on the zone detail pane, the zone cards, and the "now running" banner; the result now reaches the page whenever it arrives.
- Error messages stay on screen until dismissed. They carry the controller's own explanation, which can run several sentences, and the five-second timer they shared with routine notices was not long enough to read one. Routine notices still clear themselves.
- After a run is accepted, a cloud controller can take up to a full poll interval to report it. The Zones page used to call that a controller that did not confirm, and sent you to the Sensors page, where a Rachio never appears. It now says the controller accepted the change and how often it reports state; when the controller really has not reported, it points at Settings, then Devices, and the controller's Scan zones button. On a Home Assistant deployment with no controller configured, it now points at the sprinkler entities instead.
- A rate-limited controller no longer reports a stale allowance. If the refusal itself carried no remaining-requests count, LocalSky answered with the number from an earlier successful call, so a spent daily budget could read as hundreds of requests left on the very screen explaining the failure.
- The zone editor, the setup wizard's zone step, and the manual all described "Controller station" as a valve number. They now say what each controller kind actually addresses zones by, including that Hydrawise, B-hyve, and Rain Bird cannot scan and are bound by that field.

### Changed

- API contract 1.23.0: `POST /api/v1/irrigation/action` no longer answers `502` for every controller failure. A rejected controller credential answers `424`, a rate-limited controller `429` (body carries `rate_limit_remaining`), an unmapped zone `400` (body carries `mapped_zones`), an unsupported operation `501`; `502` is now only a controller error, an incomplete request, an offline controller, or an adapter that failed to start. Every error body carries a stable `code` to branch on instead of the status. A client that pinned `502` as the controller-failure status must widen. Successful `run`, `stop`, and `stop_all` responses gain `confirm_within_s`. No Home Assistant integration change is required.

## [0.7.19] - 2026-08-29

Results, pending work, and waiting suggestions stay where you can see them. No breaking changes.

### Fixed

- The "Restart required" banner now stays on screen while you work anywhere in a long settings form. Saving a controller from the bottom of the form left the notice scrolled out of view at the top of the page, so an applied change looked like it silently did not take ([#8](https://github.com/silenthooligan/localsky/issues/8)).
- Scan results now show up where you are looking: after a successful zone scan, the controller editor's advanced section opens on its own and the filled-in config JSON scrolls into view, instead of leaving the result behind a closed fold ([#8](https://github.com/silenthooligan/localsky/issues/8)).
- Unsaved changes on the controllers settings page are flagged on the Save button itself: an attention ring plus an "Unsaved changes" marker that clear once the save lands, so a committed edit waiting on the final Save is no longer easy to miss.

### Added

- The Zones entry in the navigation shows how many zones have an active tuning suggestion, on desktop and on the phone tab bar. The count updates as suggestions are applied, snoozed, or dismissed, and disappears at zero.

### Changed

- The sensors page now reads like the rest of the app: sensor cards carry the teal sensor identity, health shows as a dot plus a word everywhere, and only the rows that actually respond to a click respond to hover.

## [0.7.18] - 2026-08-28

### Fixed

- **Scan zones now fills the controller's zone map, and zones imported in the setup wizard now work for Rachio, Hydrawise, B-hyve, and Rain Bird.** The controller editor's scan used to report the zones it found while leaving the config JSON untouched, so saving persisted an empty zone map; and zones imported from a wizard scan were saved but never reached dispatch on the four cloud controllers. Both paths now produce a working binding: the editor merges scan results into the zone map (existing hand-edits survive a rescan), the wizard's imported zones are honored at dispatch, and when both bind the same zone the zone entry wins.
- Scanning or testing an existing cloud controller no longer fails on the redacted secret: the stored token is used automatically, and a secret that cannot be resolved is reported plainly instead of being sent to the vendor cloud as the literal placeholder text.
- Rachio watering runs now show up: running state and remaining minutes are read live from the Rachio cloud, so the dashboard reflects a running zone and completed runs are recorded in History, the water balance, and session spacing like any other controller.

### Added

- **Rachio device auto-discovery.** The controller Test button resolves your account's device from just the API token and offers to fill the device id in, so nothing needs hand-copying from a browser developer console. The test result also shows Rachio's remaining daily API request budget when the cloud reports it.
- A `poll_interval_s` setting for Rachio (default 120 seconds, minimum 60) controls how often live status is read from the cloud.

### Changed

- **Rachio polling now respects Rachio's daily request budget.** Status reads are throttled to the poll interval (the cached snapshot serves in between), so the fast dashboard refresh no longer translates into a cloud call per tick; the previous cadence could exhaust Rachio's daily allowance before morning watering.
- **Stopping one zone on Rachio, B-hyve, or Rain Bird now says what it does: it stops the whole device.** These vendor clouds offer no per-zone stop. The Stop confirmation and the logs state the real scope, and History records the real (shortened) length for every run the stop ended, not just the zone that was tapped. The automatic shutoff backstop also checks with the controller before enforcing on these devices: a zone that already finished on its own timer is released quietly, and a stop is issued only for a zone still reported running (or unknowable), so a normal multi-zone morning can no longer have each zone cut short by the previous zone's deadline. Manual runs get the same enforcement slack as scheduled ones.
- Rachio now advertises only the capabilities the polled cloud API can deliver: no flow meter, no rain sensor, and no history query. (Push webhooks, which could provide exact run events and rain-sensor state, are a documented future path.)

## [0.7.17] - 2026-08-24

The weekly budget becomes a true water balance, and tuning suggestions learn to stay quiet when told. No breaking changes.

### Fixed

- **Sessions no longer overshoot the weekly target.** The old sizing multiplied delivery by a heat factor and divided by a capture factor, inflating run length by up to about 1.9x against a target that already means "inches per week including rain"; it also only ever looked at forecast rain, so rain that had already fallen and watering already done never counted, and a soaked week could still schedule full sessions. Sessions now size against the true balance: the weekly target minus observed rain, minus irrigation already applied, minus a forecast credit covering only the days until the zone's next session. A week the sky already covered reads "covered by rain and prior watering" and waters nothing.
- **The forecast credit is corrected against your own yard's history.** The per-month forecast-bias model (predicted vs observed rain, recorded daily) now feeds the balance: a forecast that chronically under-calls your microclimate credits less future rain, and one that over-calls credits less of its promises. Months without enough rain days apply no correction; the tuning report shows the sample count behind the current multiplier.
- **Session spacing works.** The minimum interval between sessions (7 days divided by sessions per week) was never armed on live installs because the last-run time was not populated; zones could be sized as eligible every morning. Spacing now reads the run history and actually gates.
- **A day's recorded rain can no longer be erased by a gauge dropout.** The daily observed-rain ledger keeps the day's maximum: a gauge going quiet mid-storm no longer resets the day's total to zero, and each day is tagged with the kind of source that measured it. Days with no rain-capable source at all record an explicit placeholder that never trains the bias model and never counts as a measured-dry day.
- **History charts agree with the balance.** Watered-minutes charts, totals, and day headers now count each watering once (a manually started run used to count twice: once as the request, once as observed hardware activity), using the same evidence rule the balance credits. Dry-run controller activity is recorded as such and never counts as water, and a manual run stopped early is recorded at its real length instead of its planned one.
- Automated security scanning of releases and dependencies now gates the build pipeline (dependency audit and container scan).

### Added

- **Snooze and dismiss for tuning suggestions.** Every suggestion now offers Snooze 30 days and a quieter Do-not-suggest-this-again beside Apply. Snoozing silences that exact suggestion for 30 days; dismissing silences that setting on that zone permanently, even as its numbers drift. Silencing one suggestion never hides the rest: the next check's suggestion takes its place. A silenced suggestion drops out of the zone cards, the counts, the irrigation-page strip, and the weekly notification (a week whose every suggestion is silenced sends nothing). One muted line remains on the zone's panel with an Undo.
- **An honest observed-rain source for gauge-less installs.** The balance's observed-rain term resolves through a ladder and names its source: your gauge or radar day totals first, then the forecast provider's past-day model archive; installs with neither run on the corrected forecast alone, and the report line names which one applied. A week your own gauge measured is never overridden by a wetter regional model. The Open-Meteo `past_days` setting is now honored (1 to 7 days, default 3). On US installs without gauge or radar day totals, the tuning report notes that a rain source reporting day totals (NOAA MRMS day-total products qualify) unlocks the observed-rain credit.
- The tuning report states each zone's balance plainly: observed rain, irrigation applied, and forecast credit, each with its source and window, plus the full numeric breakdown behind any suggestion.

### Changed

- **Run times get shorter.** With the inflation gone and real rain and watering credited, most zones will see noticeably shorter sessions than 0.7.16 sized, and wet weeks will drop to zero. What you configured as the weekly target is now delivered as configured, not multiplied.

## [0.7.16] - 2026-08-24

### Added

- **A per-zone run limit you control.** Each zone's editor gains a Max run time field (whole minutes, 5 to 360; blank keeps the familiar 60). The limit applies live on save, no restart. Raising it past 60 minutes asks for a one-tap confirmation naming the zone and the new limit, and every device with notifications enabled gets a notice once the save lands; the save is never blocked. Long runs are still split by cycle-and-soak, and a watering restriction's own per-zone cap always wins.
- Deleting a zone now asks for confirmation first; it previously deleted on a single click.
- The Zones settings page now shows the restart banner when a change needs one (a zone added or removed, a station remapped); it previously saved with no indication a restart was needed.

### Changed

- **The tuning report now suggests raising the run limit first.** When sessions are chronically trimmed, the first suggestion is to raise the zone's run limit so each session delivers its full water; splitting the week across more sessions is now the fallback, used when the raise would pass 360 minutes or the longer morning would no longer finish before sunrise. A soil deficit too large to refill in one run gets the same real suggestion instead of an informational note.
- The tuning surfaces read at a glance: the recommendation card carries an attention stripe, a suggestion tag, a confidence chip, and a current-to-suggested line; zone cards and the zones page show which zones have suggestions and how many; the irrigation page's strip moves above the data when a suggestion is waiting and now also appears on phones.

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
