# Forecast sources and models

LocalSky combines enabled forecast providers into the forecast used by the app and watering engine. Source preference, field availability, observation age, and learned bias affect the result.

## Choose the main forecast

Open **Settings > Devices** and the forecast selection under **Which source provides each reading**. Automatic selection keeps available fallback providers in play. Choosing a preferred provider makes it the first choice; another eligible source can supply data when it is unavailable.

Fallback requires usable data. If every source is stale or missing, LocalSky cannot manufacture a current forecast. Automatic watering holds when required forecast evidence is incomplete.

Bias correction learns from available local observations over time. It can adjust recurring forecast errors; it does not make future rain certain.

## Extra forecast models

Under **Settings > Devices > Cloud weather**, add an **Extra forecast model**. Use a short ID and choose a model covering your location. Up to four tracks can be configured.

Tracks refresh about every 30 minutes and retain their last successful data across restarts. A failed refresh leaves the original age visible. Changes take effect within about 15 seconds.

Extra tracks support comparisons and integrations. They do not change the main forecast or watering decisions. The API track `merged` always refers to the app's selected forecast.

## Query a time window

Use HA's [forecast-window action](hacs.md#forecast-window-action) or `GET /api/v1/forecast/window`.

Both bounds select hourly timestamps inclusively. To request the two hours beginning at 13:00 and 14:00, supply those two start times. Bounds can be at most 48 hours apart.

The result includes temperature, precipitation amount and probability, summaries, age, and coverage. Check the fields you need: `complete` establishes timestamp coverage, while a particular summary can still be null because its measurements are missing. Zero remains a reported zero.

[API quick start](api-quickstart.md) · [Window reference](api-weather.md)

## Compare forecasts with outcomes

LocalSky archives received hourly rain forecasts with their provider and fetch time. Earlier issuances remain available, so a later forecast does not replace what was known before a run.

The archive covers each issuance's next 48 hours, retains up to 400 days, and supports paged JSON or CSV. Collection begins when a supporting version is installed; it does not reconstruct earlier forecasts.

Use the archive for forecast evidence and measured-rain history for actual rainfall. Keep those two sources distinct.
