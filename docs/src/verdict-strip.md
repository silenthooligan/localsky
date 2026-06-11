# 7-day verdict strip

The row of day cards at the top of the Irrigation tab. Each card is the
engine's answer to one question: **"if this day were tonight, would we
water?"** computed against the merged forecast for that day.

Each card shows the day's weather glyph, the high/low, expected rain,
and a verdict pill:

| Verdict | Meaning |
|---|---|
| **Run** | Conditions clear every skip rule; zones water their planned minutes. |
| **Skip** | A rule trips (rain, wind, cold, soil already wet); the reason is on the card. |
| **Extend** | A heat trigger lengthens runs beyond the baseline plan. |
| **Off** | Watering is paused (vacation mode) or outside allowed days. |

Only **tonight's** card is a commitment; later days re-evaluate every
forecast refresh, so a Tuesday "skip" can become "run" as the rain
chance fades. The strip exists to answer "do I need to think about
watering this week?" at a glance.

The same verdict logic powers per-zone pills on the Zones page; a zone
can disagree with the day (its own soil probe says wet) and skip alone.
