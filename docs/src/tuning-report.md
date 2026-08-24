# Tuning report

Zone settings are guesses on day one: soil texture picked from a chart,
a sprinkler rate from a catalog, a watering budget from a rule of thumb.
The tuning report watches what actually happened over the last two weeks
(runs, skips, rain, and your soil probes when you have them) and turns
it into at most one plain suggestion per zone, each with an Apply button
and the evidence behind it.

Everything in the report is deterministic arithmetic over your own
recorded outcomes. No AI is involved, and nothing changes until you
apply a suggestion yourself.

## Where it shows up

- Each zone's detail page has a **Tuning** panel below the
  watered-minutes chart.
- The irrigation page shows a one-line strip when any zone has a
  suggestion, plus the forecast-skip scorecard described below.
- If notifications are enabled, LocalSky sends at most one "tuning
  report ready" notice per week, and only when there is something to
  apply.

## What it checks

**Sessions shorted by the run limit.** Every zone has a limit on a
single run (60 minutes unless you set one). When the engine chronically
wants more minutes than the limit allows, each session quietly delivers
less water than the model asked for. The first suggestion is to raise
this zone's run limit so each session delivers in full; the report only
offers it up to 360 minutes and, when it can compute the morning
schedule, only when the longer sequence still finishes before sunrise.
When the raise cannot work, the report falls back to splitting the
week's water across more sessions, or, when even daily capped sessions
cannot deliver the configured weekly target, aligning the target with
what the system can actually deliver. A soil deficit too large to
refill in one run gets the same run-limit suggestion. While an active
watering restriction caps runs below the zone's own limit, run-length
suggestions pause and the report names the restriction's cap instead.

**A water bucket that cannot be right.** From your soil texture, root
depth, and the week's forecast demand, the engine computes how many days
one filling of the root zone should last. A bucket that empties in under
a day and a half, or lasts more than three weeks, points at a gross
misconfiguration: usually a root-depth override or a texture one step
off. The report only flags implausible setups; a plausible bucket is
left alone.

**Drying drift** (needs a soil probe). During stretches of two days or
more with no watering and no rain, your probe's drying rate is compared
against the rate the configured bucket predicts. A probe that dries much
faster than the model says your soil holds less water than configured;
much slower says it holds more. The suggestion is always one step: the
adjacent soil texture, or restoring the species-default root depth when
an override explains the mismatch. Only drying rates are compared, never
absolute probe percentages, because probe scales vary by calibration.

**The sprinkler's real rate** (needs a soil probe). Each watering makes
the probe rise. From the rise across your recent waterings and the
valve-open time, the report backs out the precipitation rate your
sprinklers actually deliver. When that measured rate disagrees with the
configured one by more than 30 percent, the suggestion is to set the
measured rate, the same correction a catch-cup test would give you
without the cups.

## The forecast-skip scorecard

One line for the whole installation: of the days LocalSky skipped
watering because rain was expected, how often did the rain actually
come? Each skip is judged against the window it claimed: a
"rain expected within 4 hours" skip against that day's total, a
"tomorrow rain" skip against the next day's, a three-day forecast
against the following three days. The line appears once at least three
skip days could be scored; until then it says so.

Skips for rain that was already falling, or already on the ground, are
not forecast calls: they confirm themselves, so grading them would only
flatter (or unfairly ding) the forecast. They get their own plain count
instead, with no scoring.

## When there is not enough data

Every check states exactly what it is missing rather than guessing: not
enough completed runs, not enough qualifying dry stretches, too few
probe readings, no clean watering events with a probe response. A zone
without a soil probe shows which checks a probe would unlock. Zones
bound to a live Home Assistant soil entity get the honest version too:
LocalSky keeps no local history for those bindings, so the probe checks
are unavailable.

## What Apply writes

Apply writes exactly the configuration field the suggestion names (and,
for a measured sprinkler rate, marks the rate as measured), through the
same validated path as the settings editor: the change is checked,
saved, and picked up by the engine on its next evaluation. Every apply
snapshots the previous configuration first, so
Settings and the [backup page](backup-restore.md) can roll it back like
any other edit. If the report's data has moved since the page loaded,
Apply refuses with a plain message instead of writing a stale value;
refresh the report and look again.

Applying a run-limit suggestion above 60 minutes asks for the same
confirmation the zone editor uses, and once the save lands a notice
goes to every device with notifications enabled. The save is never
blocked, and an active watering restriction's own per-zone cap still
wins over any raised limit.

## Reading further

- [Skip rules at a glance](skip-breakdown.md) for the rules the
  scorecard judges
- [Soil sensors](soil-sensors.md) for wiring the probes that unlock the
  drying and sprinkler-rate checks
- [Zone math](zone-math.md) for the duration math the cap check reads
