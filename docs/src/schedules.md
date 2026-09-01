# Manual schedules

Most of the time you want LocalSky's smart engine to decide when and how
long to water: it reads the weather, settles each zone's weekly water
balance, and fires the zone on the mornings its session spacing allows. Manual schedules are the
escape hatch for the cases where you want a zone on a clock instead, a
fixed weekday and time you set yourself. You might use one for a drip line
on a flower bed the engine does not model well, for a city that mandates a
fixed watering window, or just because you prefer a predictable morning
run.

Manual schedules live under **Settings, Manual schedules**. Each schedule
fires one zone, on the weekdays you pick, at the start time you set, for a
duration you set. Smart irrigation keeps running for every zone that does
not have a schedule; manual and smart coexist zone by zone.

## How a manual run interacts with the engine

This is the part worth getting right, because it is the whole point of the
feature. Every schedule has a **mode**, and the mode decides what the
smart engine does for that zone on the days the schedule fires.

### Override (the default)

In **Override** mode the manual schedule replaces the smart engine for
that zone, for that day. When an enabled Override schedule applies to a
zone today, the engine zeroes its own planned run for that zone so it does
not water on top of your manual run. The engine's own figures for the
zone (throughput, crop coefficient, heat multiplier, capture efficiency)
still compute and still show on the zone's detail panel, but its planned
run for that day is zero and the panel reads "Scheduled 0 min". LocalSky
does not show what the smart run would have been. On those days
the zone card and the zone detail read ON HOLD and name the schedule. The manual
schedule is the only thing that fires.

Use Override when you want full manual control of a zone on the days the
schedule covers: on those days the clock you set is exactly what runs, no
more, no less (restrictions aside, see below). On the days it does not
cover, the zone is a normal smart zone again, and the engine sizes it from
the weekly water balance with the schedule's own water already counted
against that week's target. Cover every weekday, or delete the schedule,
if you want the zone to water only on your clock.

### Floor

In **Floor** mode the manual schedule is a minimum, not a replacement.
The manual run fires on schedule, and the smart engine may add more runs
for that zone if the weekly water balance says the lawn needs more water
than the scheduled run delivered. Think of it as "at least this much,
plus whatever the engine adds on top."

Floor is for minimum-coverage patterns: a guaranteed baseline run with
the engine topping up during a heat wave.

Two things decide whether the engine ever gets to add on top, and both
can close the door completely:

- A Floor run is watering like any other, so its water counts against the
  zone's weekly target. A schedule that already delivers the week's
  target leaves the engine nothing to add.
- A Floor run also **resets the session-spacing clock**. The engine paces
  a zone at `floor(7 / sessions_per_week)` days between sessions, and it
  measures that from the last run of any kind, including yours. So a
  schedule that fires as often as the zone's own session cadence leaves
  the engine no eligible day at all: a weekly Floor schedule on a
  1-session-a-week bed (the default for any zone whose name contains
  shrub, garden or bed) never opens the gate, and neither does a
  Monday/Wednesday/Friday schedule on a 2-session-a-week zone. The zone
  reads ON HOLD, naming the spacing, every day.

If you want a zone fully engine-driven, delete the schedule rather than
switching it to Floor. Switch to Floor when you want the scheduled run to
stay as a guaranteed minimum and you have raised `sessions_per_week`
enough to leave the engine a day to work with.

The two modes differ only in what they do to smart dispatch. The manual
run itself fires identically either way.

## Per-zone behavior

A schedule targets exactly one zone (its **Zone** field), and the mode
applies to that zone alone. Override on the back yard does not suppress
smart on the front yard. You can mix freely: an Override schedule on one
zone, a Floor schedule on another, and pure smart on the rest. You can
also have more than one schedule on the same zone (for example a morning
and an evening run); each fires on its own clock, and if any of them is an
enabled Override for today, smart dispatch for that zone is suppressed for
the day.

## Days, times, and duration

- **Weekdays.** Pick the days the schedule runs. An empty list means it
  never fires (effectively disabled). Days are independent: a schedule set
  to Wednesday and Saturday fires on both, with the same time and
  duration.
- **Start time.** A start hour (0 to 23, 24-hour local time) and a start
  minute (0 to 59). 5 and 0 means 05:00. The dispatcher ticks once a
  minute, so resolution is one minute and the run fires when the clock
  reaches the exact hour and minute you set.
- **Duration.** How many whole minutes the zone runs per fire, at least 1.
  This is the planned length; a watering restriction can shorten it (see
  below), but nothing lengthens it.
- **Enabled.** Disable a schedule to keep the entry but stop it being
  evaluated, the same pattern as restrictions and zones. Handy for a
  seasonal schedule you do not want to delete.

A schedule fires at most once per day per schedule. If two ticks land on
the same minute (clock skew, a leap second), the dispatcher remembers it
already fired today and does not double-run.

## Restrictions still apply

Manual schedules are not a way around your [watering
restrictions](restrictions.md). Before a manual run dispatches, the engine
evaluates the same restriction policy it uses for smart runs. If a
restriction blocks watering right now (wrong weekday for your address
parity, inside a forbidden-hours window, out of season), the manual
dispatch is skipped and a skip row is logged to the runs table with the
rule's reason, exactly like a smart skip. A duration cap from a
restriction also applies: if a rule caps zones at 60 minutes and your
schedule asks for 90, the run is shortened to 60. The tightest cap across
all active restrictions wins.

So a manual schedule sets *your* intent; restrictions still set the legal
floor and ceiling on top of it.

## Saving and when it takes effect

The page edits a working copy. Add or edit a schedule with the form, then
click **Save all changes** at the bottom to persist. Saving round-trips
through the config API. The dispatcher reads the schedule list at startup,
so a newly added or edited schedule takes effect on the next container
restart rather than mid-run. Restrictions and the smart engine pick up
changes on their own next tick, but the manual-schedule clock is read once
at boot.

## Where to read more

- [Watering restrictions](restrictions.md): the rules that gate a manual
  run before it dispatches, and the caps that shorten it.
- [Irrigation engine](irrigation-engine.md): the smart pipeline an
  Override schedule suppresses and a Floor schedule sits on top of.
- [History and reporting](history.md): where a manual run (or its skip
  row) shows up after it fires, attributed to the schedule.
