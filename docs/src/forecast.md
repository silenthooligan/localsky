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

## Choosing your forecast source

An install with no hardware uses Open-Meteo automatically (free, no API
key), so you see a forecast immediately; it is the recommended
zero-config pick. To drive the forecast pipeline with a different
provider, open Settings > Devices and use the forecast source picker in
the section titled **Which source provides each reading**. "Automatic"
keeps Open-Meteo as the low-priority failover; selecting a provider
(NWS, Pirate Weather, MET Norway, OpenWeather, or any enabled
forecast-capable source) pins it to win regardless of the per-source
priority ranking. If the pinned source goes offline the forecast still
works by falling back to the next source, so a pin never blanks the
forecast.

## Forecast tracks

Under **Settings > Devices > Cloud weather**, add an **Extra forecast model**
with a short name, such as `nbm`. Choose a model that covers your location;
NOAA NBM covers the continental United States. Up to four models can be kept.

Each track refreshes every 30 minutes and keeps its last successful forecast
across restarts. A failed refresh leaves those values available with their
original age. Settings shows when a track needs attention. Removing a track
stops its refreshes; its historical archive remains until normal retention
expires. Track changes take effect within 15 seconds.

Tracks provide a separate forecast for automations and comparisons. They do
not change the dashboard forecast, current station readings or watering
decisions. `merged` always means the forecast selected for the main app.

### Ask for a time window

The [Home Assistant integration](hacs.md#forecast-window-action) provides
`localsky.get_forecast_window`. For another client, use
`GET /api/v1/forecast/window?track=nbm&from=<epoch>&to=<epoch>`.
Both endpoints select hourly timestamps inclusively, up to 48 hours apart.
Each timestamp is the **start** of its hour. For the two hours starting at
13:00 and 14:00, pass those two timestamps.

Responses include rain probability, maximum hourly rain, total rain and
temperature extremes, plus the underlying rows, forecast age and coverage.
Values remain in inches and Fahrenheit regardless of display settings.
A summary is `null` if its requested hours or measurements are missing;
reported zero still means zero. Check the age and the fields you need before
using a window. A declared model that has not fetched yet returns an empty
window; an unknown track returns 404.

### Review earlier forecasts

LocalSky stores hourly rain amounts and probabilities from each received
forecast for the next 48 hours, including the provider, model and fetch time.
Earlier issuances are kept so a later forecast cannot rewrite what was known
before a watering run. The archive starts when this version is installed.

`GET /api/v1/forecast/archive` serves paged JSON or CSV for `merged` or a named
track, with an optional lead-hour filter. Retention is 400 days; responses
are capped at 5,000 rows per page. These are forecasts, not measured rainfall.
See the [API reference](api.md) for paging and completeness details.
