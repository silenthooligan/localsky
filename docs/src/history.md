# History

The **Run log** leads with recorded activity. Choose a range or month and
search by zone or reason. Sessions are grouped by their start date in your
installation's timezone; expand a session to inspect its cycles and original
records. Manual watering and the normal automatic schedule retain their sources.

**Watering insights** has a separate, clearly labeled 30-day, 90-day, or
one-year window:

- **Watering time** counts valve-open minutes, excluding soak waits and
  duplicate controller observations. It is duration, not measured volume.
- **Watering sessions** counts recorded watering events.
- **Skipped zone mornings** and **Why scheduled zones held** use recorded
  automatic outcomes, once per zone and local day. A later watering outcome
  replaces an earlier hold. A changing live forecast is not a completed skip.
- Daily trends, the calendar, and the per-zone split show where watering occurred.
  An empty day means no watering was recorded; it does not prove a skip.

**Rain forecast review** is collapsed below the watering insights. Expand it
to compare completed-day forecasts with gauge evidence. This feedback does not
measure water saved. Incomplete days and unavailable observations are not scored.

Print creates a report; Download CSV exports the stored records. History lives
in LocalSky's own SQLite database. A failed history request is reported as
unavailable, never as zero watering.

## Today's run and tomorrow's projection

The Irrigation page separates **Today · normal irrigation run** from
**Tomorrow · projected**. Today's result comes from stored automatic-run rows,
including the reasons recorded at dispatch. When those rows are absent, LocalSky
says there is no automatic-run record rather than guessing from today's weather.
Manual runs remain visible in the run log.

Tomorrow uses tomorrow's forecast verdict. The morning check time includes its
day and timezone. Normally LocalSky works backward from sunrise minus 15 minutes
by the sequence's watering and soak duration. A zero-minute plan checks 15 minutes
before sunrise; freeze forecasts can move watering to a safe post-sunrise window.
The time can move as the plan changes. Fresh evidence and the applicable rules
determine the final decision at the check.
