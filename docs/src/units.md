# Units

Units are display only. The engine does all of its math in metric
internally and converts at the boundary, so switching units changes what
you read, not how LocalSky waters. (Zone area is the one value you enter
yourself, so its unit does feed the water math; see [Zones](zones.md).)

Find the control under Settings, then Units. It has two independent
layers, picked by the "Applies to" switch at the top.

## Household default (whole deployment)

Imperial or Metric for the whole install. It is stored in
`/data/localsky.toml` as `deployment.units` and travels on the irrigation
snapshot, so every device that follows the household updates on the next
tick. This layer has an explicit **Save** button, because it changes
shared server config.

- **Imperial**: F, inches, mph, inHg, miles, square feet.
- **Metric**: C, mm, km/h, hPa, km, square meters.

The setup wizard pre-selects this from your location.

## This device only (per browser)

A single device can opt out of the household default and keep its own
units, saved in that browser's `localStorage`. There is no Save button
here: each pick persists the moment you make it, and a short "Saved on
this device" line confirms it. Your other devices and the household
default are untouched.

Pick a whole system (Imperial or Metric), or choose **Custom** to set
each measurement on its own: temperature, rainfall, wind speed,
pressure, distance, and zone area. Switch back to "Household default" and
the per-device keys are cleared, so the device follows the deployment
again.
