# Why this duration?

Every zone's detail view shows the inputs behind tonight's planned
minutes, because "trust me" is not a number.

**Where the depth comes from.** Tonight's run length is decided by the
[weekly water balance](water-budget.md): a gross weekly target per zone,
settled against observed rain, water already applied, and a
probability-weighted forecast credit, with the remainder split across the
sessions still expected this week. That page is where the arithmetic that
produced the minutes lives.

The panel is in two parts, because only two of its numbers reach the
dispatch.

## What sizes tonight's run

1. **Throughput (mm/hr)**: how fast your sprinklers actually apply
   water, either measured (catch cups) or the catalog default for the
   head type. This week's session depth divided by this rate is where the
   run length starts. It then takes the seasonal adjustment and any
   condition rule's multiplier, and is held to the zone's cap, so the
   minutes on the panel do not have to equal depth divided by throughput.
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

## Not part of tonight's minutes

None of these four scales the run. Three are real outputs of the engine
and feed the ETc figure and the soil projection, not the weekly balance.
The fourth, the soil deficit, has no producer today and reads a dash.

3. **Soil deficit (mm)**: how far the zone's soil sits below full. This
   reads a dash on every current install, because no model computes it
   yet. Its only producer was a Home Assistant integration LocalSky no
   longer reads, and printing a 0.00 there told people their soil was
   full on the strength of a number nothing measured. The per-zone
   depletion model that would fill it is written but not yet wired to any
   decision.
4. **Crop coefficient (Kc)**: the species' seasonal multiplier on
   reference ET (see the grass species catalog). Hemisphere-aware:
   south of the equator the curve shifts six months.
5. **Heat multiplier**: optional extension when the peak heat index
   crosses the heat-advisory threshold. Each day's heat index pairs
   that day's high temperature with that same day's humidity (not the
   current, often night-time, humidity), so a cool morning's humidity
   is never combined with a hot afternoon's peak to inflate the run.
6. **Capture efficiency**: how much of the applied water lands in the
   root zone (wind drift, overspray, runoff losses).

The panel prints no formula. It used to print one belonging to that Home
Assistant integration, which matched nothing LocalSky computes, and which
multiplied the four numbers above as though they set the run length.
