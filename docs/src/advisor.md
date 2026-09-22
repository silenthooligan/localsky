# Today's status and tomorrow's plan

The Irrigation page separates a completed or pending morning from the next day's projection.

## Today

Today's status comes from recorded automatic-run outcomes. It can show watering, a hold with its reason, or no automatic-run record. Later weather changes do not supply a missing historical reason.

A manual run belongs in History even when the normal morning was skipped. Open **Daily log** to see the recorded morning and **Run log** for watering activity.

## Tomorrow

Tomorrow is a forecast-based plan. LocalSky carries soil demand, recent rainfall, and applied watering forward and evaluates the available forecast. The plan can change as conditions and evidence change.

Open **Watering decisions** for the zone list, expected durations, and the reason a zone is held. A projected run is not a dispatched command.

## Why the check time moves

The normal morning window is planned backward from sunrise minus 15 minutes, allowing for the planned watering and soak time. When no watering is planned, the check can fall at that finish boundary. It is still a decision check.

A freeze forecast can move an eligible run into a later safe window. Location, date, timezone, and the current sequence determine the time; it is not a fixed daily alarm.

Use the displayed date and timezone to distinguish today's result from the next check.

[The week ahead](verdict-strip.md) · [Decision rules](irrigation-engine.md) · [History](history.md)
