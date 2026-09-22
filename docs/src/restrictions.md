# Watering restrictions

Enter permitted days, forbidden hours, and duration limits in **Settings > Watering restrictions**. LocalSky enforces the rules you configure; it does not fetch or certify the rules for your address. Verify them with the responsible authority.

## How a restriction interacts with the engine

Restrictions are evaluated before the weather skip rules. When a
restriction blocks watering right now, the engine skips and the verdict
reason names the rule (for example, "Watering restriction (HOA summer):
today is not an allowed watering day"), so you see the restriction rather
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
and the list matching the parity you set here is the one that binds. (It
carries a third list that binds every address regardless; see [allowed
weekdays](#allowed-weekdays).)

Parity matters only for a rule that depends on it. A rule whose
odd and even lists name the same days binds on its own even at **N/A**,
because it never depended on your house number in the first place. Only
two things need a parity to decide: two weekday lists that differ, and a
date rotation keyed on the address. Those stand aside at **N/A** rather
than guessing, so that half of the rule blocks nothing, while the rest of
the same restriction (its every-address days, its forbidden hours, its
cap) still applies. The page warns you loudly when an enabled restriction
is in that state. Pick Odd or Even and save to enforce the schedule.

## The restriction fields

Each restriction has an id (a short snake_case key), a display name (what
shows up in the verdict reason), and an enabled toggle. Disabling keeps
the entry but stops it being evaluated, which is handy for a seasonal rule
you do not want to delete. Beyond those, a restriction is a stack of
gates, any of which is inactive when you leave it blank, plus two
fields that are not gates at all: the head types this rule spares and
the zones it is limited to.

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

### Allowed weekdays

The days you are allowed to water, given as three checkbox rows: one for
odd addresses, one for even addresses, and one for every address. The odd
and even rows are read against your [address parity](#address-parity);
the every-address row is for a rule that does not depend on your house
number and is read whatever your parity is. An empty row is no gate at
all (water any day).

Each row that applies is its own gate and every one of them has to pass,
so a rule that fills the every-address row and a parity row allows only
the days that appear on both. If today fails either, the run skips with
"today is not an allowed watering day".

Use the odd and even rows for a rotation schedule. Use the every-address
row for a flat "everyone waters the same two days" rule; that is the row
the Two days a week starter template writes. Identical days in the odd
and even rows work too, and bind even at N/A parity, but the
every-address row says what you mean and does not trip the parity
warning.

### Date rotation and the 31st

Some districts rotate by the calendar date rather than by the weekday.
**Date rotation** offers four settings: **None** (the default, no gate),
**Odd addresses on odd dates, even on even**, **Everyone on odd dates**,
and **Everyone on even dates**. The by-address setting is the one that
needs your address parity; at N/A it cannot decide and lets the date
through. On a date the rotation forbids, the run skips with "today is not
an allowed date for this address".

**Nobody waters on the 31st** is a separate checkbox, and its whole
reason is arithmetic: 31 is odd and so is the 1st that follows it, so a
31-day month would otherwise hand the odd side two watering days in a
row. Districts that rotate by date usually write the exception in. The
checkbox is honored on its own as well, with no rotation set, and it is
checked before the rotation: on the 31st the skip reason is "the 31st is
never a watering day" whatever the rotation would have said.

### Days per week

An optional cap on how many days in a Sunday-to-Saturday week may have a
run. LocalSky counts the distinct days that already watered this week,
today excluded, and once that count reaches the cap the morning run
skips with "this week's allowance of watering days is used up". Any
watering counts toward the week, a morning run or a zone you started by
hand alike, because the count comes from your run history rather than
from the schedule you planned. Leave the field blank for no cap.

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

### Exempt sprinkler types

The heads this rule spares, picked as chips: rotor, spray, MP rotator,
drip, bubbler, and other. Many districts exempt drip and other low-volume
irrigation from the schedule, so check what your own rules exempt before
you set this.

Putting a zone's **Sprinkler type** on the exempt list removes that rule's
day and time gates for that zone. The engine then checks every other
applicable restriction, safety gate, soil condition, and owner rule before
the zone can run. An exempt drip bed can water while a restricted lawn
waits, but a freeze or operator hold still stops it. The zone card and
dispatch use the same completed verdict. Set each zone's head type in the
[zone editor](zones.md#watering-settings).

A duration cap is not exempted along with the schedule. The tightest cap
across the active restrictions is worked out for the yard as a whole and
applied to every zone's planned minutes, exempt heads included.

### Only these zones

The zones this rule is limited to, picked as chips. Leaving every chip
off is the usual setting and binds the rule to the whole yard; naming
zones is what narrows it. Only those named zones inherit that rule's day
and time gates. Every other zone is still checked against the restrictions
that apply to it and the rest of the decision ladder.

## Starter templates

The page has three one-click starter templates so you do not start from a
blank form. Each adds a generic restriction you then edit for your area:

- **No midday watering**: forbids 10:00 to 16:00, all year, any day.
- **Two days a week**: water Wednesday and Saturday only, plus the same
  no-midday window.
- **Odd/even address days**: odd addresses water Wednesday and Saturday,
  even addresses Thursday and Sunday (a common parity rotation).

After adding a template, open it with **Edit**, adjust the days, hours,
and dates to match your local rules, then save the restriction. Adding
the same template again replaces it rather than
duplicating it.

## Saving

Adding, saving, or deleting a restriction persists that change immediately.
Selecting your address parity also saves immediately. The engine picks up
the saved configuration on its next tick. A failed save shows an error;
check it before assuming a new restriction is in effect.

## Where to read more

- [Skip rules at a glance](skip-breakdown.md): the full veto ladder,
  including where restrictions sit.
- [Skip rules in depth](skip-rules.md): how each input becomes a verdict.
- [Irrigation engine](irrigation-engine.md): the scheduling and duration
  math a cap is applied against.
