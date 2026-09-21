# Forecast fixtures

Captured from Open-Meteo on September 20, 2026 using LocalSky's full forecast
query and `models=ncep_nbm_conus`. `nbm-conus.json` uses the public reference
point 39 N, 104 W; `nbm-outside.txt` uses 48 N, 11 E. Neither is an installation
location. The latter is intentionally not valid JSON: the upstream response
contains lowercase `nan` coordinates and no forecast arrays.

NBM coverage and variables: <https://open-meteo.com/en/docs/gfs-api>.
Rain in the raw response ends at the hour stamp. LocalSky normalizes it to
hour start, leaving the final hour unknown when the next sample is absent.
