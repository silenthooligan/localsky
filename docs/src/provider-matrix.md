# Provider capabilities

Use a local instrument for the readings it actually measures. Use other stations and weather services to fill gaps, with their age and location in view.

A provider's capability does not guarantee that a reading is present now. Hardware options, coverage, station reporting, credentials, and failures all affect availability.

## Understand the source label

| Type | What the value represents |
|---|---|
| Measured | An instrument observation at a station. A nearby station may differ from your yard. |
| Radar | An estimate derived from radar observations, sometimes adjusted with gauges. |
| Nowcast | A short-range analysis or estimate of current conditions. |
| Model | Computed current conditions rather than a direct instrument reading. |
| Forecast | Predicted future conditions. |

Radar rainfall is an estimate over an area. A forecast value is not evidence that rain fell. Polling a provider successfully does not refresh the observation timestamp inside its response.

## Choose by purpose

| Source | Useful for | Check before relying on it |
|---|---|---|
| Tempest, Ecowitt, Davis | Readings from your own weather hardware | Installed sensors, local reception, observation age |
| Ambient Weather, Netatmo, La Crosse | Your station's observations through its cloud | Hardware modules, credentials, cloud availability |
| Home Assistant passthrough | Existing HA weather and soil sensors | Entity mapping, units, source timestamp, precipitation interval |
| NWS observations | Measured conditions from an official station | Distance and freshness; a fresh temperature does not guarantee fresh wind |
| Synoptic Data | Observations from another physical station | Selected station, coverage, token, available fields |
| NOAA MRMS | Radar rainfall estimates in supported US coverage | Product age and accumulation period |
| Open-Meteo, NWS forecasts, MET Norway, Pirate Weather, OpenWeather, WeatherKit | Forecasts and modeled fields exposed by the adapter | Model coverage, forecast age, missing fields, provider requirements |

## Near-real-time wind

Prefer a fresh on-site anemometer where available. NWS wind is measured, but its station may be distant or its wind observation stale. Open-Meteo wind is modeled.

Inspect the field's source and observation time in LocalSky. A selected primary does not win when it has no eligible reading; the configured chain can fall through to another source. Decide whether a modeled fallback is suitable for your use case.

## Rain needs its interval

Current rain intensity, rain in the preceding minute, today's total, and forecast rain are different inputs. Do not interchange them based on a similar entity name.

For HA's local WeatherFlow precipitation sensor, choose **Rain last minute (accumulate today)**. LocalSky accumulates received intervals into daily totals; it cannot recover intervals missed while disconnected.

Measured rain history and forecast rain stay distinct in watering explanations and the API.

## Forecast tracks

The main forecast serves the app and watering decisions. Extra models provide separate comparison and integration tracks. An NBM track, for example, remains a forecast, including when accessed through Open-Meteo.

[Reading selection](sources.md) · [Forecast selection](forecast.md) · [Connect sensors](sensors.md)
