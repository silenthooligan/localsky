# Forecast merge

LocalSky never trusts a single forecast. Configured forecast sources
(Open-Meteo by default; NWS, MET Norway, OpenWeather, Pirate Weather
optional) are merged by priority with per-field fallback, then
**bias-corrected** against what your own station actually measured:
if the model consistently runs 2 degrees hot over your yard in July,
the merge learns that and compensates, per field, per calendar month.

The hourly canvas shows 48 hours of temperature, precipitation
probability and amount, wind, and cloud cover. The 7-day row feeds the
verdict strip. Forecast-aware skip rules read the same merged data, so
the number you see is the number the engine acted on.

Sources are health-tracked: a polled model is "fresh" within its
poll cadence (about 30 minutes for Open-Meteo) and the merge fails
over to the next source when one goes quiet.
