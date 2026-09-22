# Why watering is held

Open **Watering decisions** and choose the zone. Start with its short reason, then expand the evidence if you need the threshold or source behind it.

## Common reasons

| Reason | What to inspect |
|---|---|
| Enough water available | Recent observed rain, applied irrigation, and the zone's soil balance |
| Rain expected | Forecast amount, probability, coverage, and whether the zone can wait |
| Wind or cold | Selected current measurement and the relevant forecast window |
| Soil wet | Bound probe, calibration, timestamp, and saturation threshold |
| Required data unavailable | Source health and the affected zone's missing evidence |
| Paused or restricted | Owner controls, allowed days, time windows, and duration caps |
| Restart required | Saved startup-dependent changes and the restart notice |
| Manual schedule applies | The zone's enabled Override schedule |

A reason shown now describes the current evaluation. Use the Daily log to find why a past automatic morning was held.

## Overrides have limits

Force bypasses specified recommendations, not every protection. It cannot make missing required evidence valid or clear a restart hold.

A reliable dry probe can affect a soft forecast-rain recommendation. An absent or untrusted configured probe does not borrow a neighbor's reading as permission to water.

## If the result seems wrong

Check the selected source and original observation time first. Then check zone bindings, soil, application rate, and scheduling model. Change a threshold only when its meaning and the evidence justify it.

[Rules and thresholds](skip-rules.md) · [History](history.md) · [Troubleshooting](troubleshooting.md)
