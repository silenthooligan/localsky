# Why this duration?

Every zone's detail view shows the inputs behind tonight's planned
minutes, because "trust me" is not a number.

**Where the depth comes from.** Tonight's run length is decided by the
zone's governing model. Under the default
[weekly water balance](water-budget.md): a gross weekly target per zone,
settled against observed rain, water already applied, and a
probability-weighted forecast credit, with the remainder split across the
sessions still expected this week. Under the
[soil model](irrigation-engine.md#the-soil-model): the zone's replayed
soil deficit divided by the capture efficiency and the throughput, capped
at the run limit. Those pages hold the arithmetic that produced the
minutes.

The panel is in two parts, because only two of its numbers reach the
dispatch.

## What sizes tonight's run

1. **Throughput (mm/hr)**: how fast your sprinklers actually apply
   water, either measured (catch cups) or the catalog default for the
   head type. This week's session depth divided by this rate is where the
   run length starts. It then takes the seasonal adjustment and any
   condition rule's multiplier, and is held to the zone's cap, so the
   minutes on the panel do not have to equal depth divided by throughput.

   That seasonal adjustment is one dial for the whole yard, on the Engine
   settings page: 100% is the depth as computed, and you turn it down for
   a wet, cool stretch or up for a heat wave. It scales the depth before
   the zone's cap clamps the result, so the minutes here already include
   it; the [irrigation engine page](irrigation-engine.md) carries the
   dial's range and the rest of its behavior.
2. **Scheduled**: the minutes that will actually dispatch, and what the
   zone's safety ceiling did to them. The ceiling is `max_run_minutes`,
   tightened further by any active watering restriction. The row reads
   "capped at N min" when tonight's run is sitting on the ceiling because
   something asked for more than it allows: the weekly balance's own
   session, the seasonal adjustment, or a condition rule's multiplier. The
   zone is being shorted, so raise the ceiling, or raise
   `sessions_per_week` so each session is shorter.

   A zone with no run planned shows its minutes and nothing about a cap.
   There is no run for a ceiling to have shortened, and the reason the zone
   is not watering is on the zone card. A zone's weekly session can outgrow
   the ceiling while today's plan is zero for a separate reason (spacing,
   a rain defer, an Override schedule); the [tuning
   report](tuning-report.md) is where that shows up, because it is a
   statement about the week rather than about tonight.

   A Force override is the one case where minutes appear with no target
   behind them. Set Force on the zone, or globally with the zone left on
   Auto, and a run whose computed length came out zero waters a bounded
   default of five minutes instead, held down to the zone's ceiling when
   that ceiling is shorter. Without the floor, a Force on an already
   satisfied yard flips the verdict to run and then dispatches nothing,
   because a zone planned for zero seconds is skipped. A zone sitting at
   zero for any other reason stays at zero, and so does a day an Override
   manual schedule already covers, so a forced run never stacks on top of
   the run you scheduled yourself.

## Not part of tonight's minutes

On a weekly-governed zone none of these four scales the run; they feed
the ETc figure and the soil projection. On a soil-governed zone the
panel moves the soil deficit and the capture efficiency above the line,
because there the run length IS the deficit divided by the capture
efficiency and the throughput.

3. **Soil deficit (mm)**: how far the zone's soil sits below full,
   negative when the zone needs water. The soil model's replay of
   measured ET, rain and completed runs computes it for every zone with
   a species and a soil texture, whichever model governs; a dash appears
   only where no bucket can be derived (env-var zone lists). Printing a
   0.00 there used to tell people their soil was full on the strength of
   a number nothing measured.
4. **Crop coefficient (Kc)**: the species' seasonal multiplier on
   reference ET (see the grass species catalog). Hemisphere-aware:
   south of the equator the curve shifts six months.
5. **Heat multiplier**: optional extension when the peak heat index
   crosses the heat-advisory threshold. Each day's heat index pairs
   that day's high temperature with that same day's humidity (not the
   current, often night-time, humidity), so a cool morning's humidity
   is never combined with a hot afternoon's peak to inflate the run.
6. **Capture efficiency**: how much of the applied water lands in the
   root zone (wind drift, overspray, runoff losses). Weekly-governed
   zones show the fixed 0.70 the soil projection uses; soil-governed
   zones show the configured `engine.capture_efficiency`, the number
   each refill divides by.

The panel prints no formula. It used to print one belonging to that Home
Assistant integration, which matched nothing LocalSky computes, and which
multiplied the four numbers above as though they set the run length.
