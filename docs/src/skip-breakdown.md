# Skip rules and thresholds

The engine checks every planned run against a short list of vetoes,
in order. First trip wins; the reason is recorded and shown.

| Rule | Default | What it protects |
|---|---|---|
| Rain in the recent window | 0.20 in (5 mm) | Don't water what the sky watered. |
| Observed rain recently | ~0.25 in over today plus the past day | Measured rain skips on its own, independent of any soil reading. |
| Rain expected in the next hours | forecast x probability | Don't water ahead of a storm. |
| Wind | 10 mph (16 km/h) | Spray pattern integrity (drift loss). |
| Freeze / low temperature | 38 F (3.3 C) | Ice on hardscape, plant shock. |
| Soil moisture (per zone, with a probe) | zone target band | The probe outranks the model. |
| Allowed days / restrictions | local rules | Water-authority schedules, municipal restrictions, HOA rules. |
| Vacation pause / dry-run | manual | You said so. |

Thresholds are tunable in Settings under Logic (and live-tunable from
the Irrigation tab's threshold sliders). The History tab's "Why it
skipped" panel aggregates which rules actually fired over the window,
so you can see whether a threshold is doing real work or just noise.

Not every skip is final. A soft forecast-rain veto can be demoted back
to a run when the zone is measured dry: the soil floor moat lets a
trustworthy dry reading override rain the sky only promised but has not
delivered. Measured rain (the observed-rain backstop above) is not soft
and is never demoted this way.

A bad or offline soil probe cannot block or force a run on its own.
When a probe looks untrustworthy, its value is inferred from its
trustworthy neighbours (quarantine), so one flaky sensor never vetoes a
zone or falsely triggers one.

Heat advisory is the one rule that extends instead of vetoes: when the
forecast high crosses its threshold, planned runs stretch by the
configured multiplier.
