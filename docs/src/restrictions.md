# Watering restrictions

Many places limit when you may water: a water authority, a council, a
water management district, or an HOA may restrict watering to certain
days, forbid it during the hottest hours, or cap how long each zone runs.
LocalSky's restriction system encodes those rules and feeds them straight
into the skip engine, so the dashboard's verdict already reflects what
you are legally allowed to do.

Restrictions live under **Settings, Watering restrictions**. Check your
local water utility or municipality for the exact rules where you live;
LocalSky's job is to honor them, not to know them.

## How a restriction interacts with the engine

Restrictions are evaluated before the weather skip rules. When a
restriction blocks watering right now, the engine skips and the verdict
reason names the rule (for example, "Watering restriction (HOA summer):
today is not an allowed watering day"), so you see the legal block rather
than a weather explanation.

Multiple restrictions stack. The engine evaluates every enabled,
in-window restriction and the tightest rule wins: if any one of them
forbids watering, the run skips. Duration caps accumulate as the
smallest cap across all active restrictions. Restrictions also stack with
your ordinary skip-rule thresholds (rain, wind, freeze, soil moisture);
the overall verdict is the most restrictive of everything that applies.

## Address parity

Many jurisdictions split the watering schedule by house number: odd
addresses on some days, even addresses on others. Set your parity once,
at the top of the page: **N/A**, **Odd**, or **Even**. Each restriction
carries a separate allowed-weekday list for odd and for even addresses,
and the engine matches the list against the parity you set here.

This setting matters even if you only ever use one restriction. When
parity is **N/A**, the engine treats odd/even weekday rules as "no
weekday gate" and silently ignores them, so a day-of-week restriction
will not block anything. The page warns you loudly if you have an enabled
restriction with weekday rules but parity is still N/A. Pick Odd or Even
and save to enforce the schedule.

## The restriction fields

Each restriction has an id (a short snake_case key), a display name (what
shows up in the verdict reason), and an enabled toggle. Disabling keeps
the entry but stops it being evaluated, which is handy for a seasonal rule
you do not want to delete. Beyond those, a restriction is built from four
gates. Any gate you leave blank is simply inactive.

### Effective window

When the restriction is active across the calendar. Options:

- **All year**: always in effect. Most restrictions use this.
- **Summer (US DST)**: active from the second Sunday of March to the
  first Sunday of November (the US daylight-saving window). Some US water
  districts switch rules with daylight saving.
- **Winter (US standard)**: the complement of the above.
- **Custom range**: an arbitrary start and end (month and day), including
  wrap-around across the new year (for example November 15 to February
  28). A day that overruns its month is clamped to the month end, so
  "February 30" means "end of February" rather than failing silently.

Outside the US, use **Custom range** for any seasonal rule; the DST and
standard windows follow the US daylight-saving calendar specifically.

### Allowed weekdays (odd and even addresses)

The days you are allowed to water, given as two lists: one for odd
addresses, one for even. The engine reads the list that matches your
address parity. An empty list means no weekday restriction (water any
day). If today is not on your list, the run skips with "today is not an
allowed watering day".

Use this for "two days a week" rules and for odd/even rotation schedules.
For a flat "everyone waters the same two days" rule, set the same days in
both the odd and the even list.

### Forbidden hours

A no-watering window, given as a start hour and an end hour (0 to 23 /
24). The window is inclusive of the start hour and exclusive of the end:
a 10 to 16 window forbids watering from 10:00 up to 16:00, and watering
is allowed again at 16:00. The window may wrap past midnight (for
example 22 to 6 forbids the overnight hours). Leave both blank for no
time gate.

This is the right gate for "no watering during the heat of the day"
rules. Inside the window the run skips with "currently inside the
forbidden window".

### Max minutes per zone

An optional hard cap on how long any single zone may run per dispatch.
The tightest cap across all active restrictions wins, and that cap is
then combined with the zone's own duration ceiling, so the shortest
limit always applies. Unlike the other gates, a cap never causes a skip
on its own; it only shortens runs that do go ahead.

## Starter templates

The page has three one-click starter templates so you do not start from a
blank form. Each adds a generic restriction you then edit for your area:

- **No midday watering**: forbids 10:00 to 16:00, all year, any day.
- **Two days a week**: water Wednesday and Saturday only, plus the same
  no-midday window.
- **Odd/even address days**: odd addresses water Wednesday and Saturday,
  even addresses Thursday and Sunday (a common parity rotation).

After adding a template, open it with **Edit**, adjust the days, hours,
and dates to match your local rules, then click **Save all changes** to
persist. Adding the same template again replaces it rather than
duplicating it.

## Saving

The page edits a working copy; nothing is enforced until you click
**Save all changes** at the bottom. That persists the restrictions and
your address parity together, and the engine picks them up on its next
tick. To remove a restriction, use **Delete** on its card, then save.

## Where to read more

- [Skip rules at a glance](skip-breakdown.md): the full veto ladder,
  including where restrictions sit.
- [Skip rules in depth](skip-rules.md): how each input becomes a verdict.
- [Irrigation engine](irrigation-engine.md): the scheduling and duration
  math a cap is applied against.
