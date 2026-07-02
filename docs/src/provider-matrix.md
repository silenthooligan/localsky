# Weather providers and what they measure

LocalSky pulls weather from two kinds of source: a **local station** sitting in
your yard (or one you own, routed through a vendor cloud) and a **cloud weather
service** that fills the gaps your hardware does not cover. They are not equal,
and LocalSky never pretends they are. A real station that you own outranks every
cloud service for the readings it actually covers; the cloud is there to fill
the rest. This page lays the whole picture out as one wide table so you can read
across a provider and see, field by field, exactly what its number really is.

## Measured vs Nowcast vs Model vs Forecast

Every cell in the table below is one of a small set of honest words. The number
on your dashboard might look the same whether it came from a gauge in your grass
or a model grid 9 km away, so LocalSky labels its **nature**, not just its value:

- **Measured**: a real instrument reading from a physical station. It can lag
  and it may not be your exact yard (an official station can be an airport
  miles away), but it is an actual observation, not a computed estimate.
- **Radar**: a gauge-corrected radar rain estimate (NOAA MRMS). It is
  observation grade, not a model forecast: it measures the rain that actually
  fell on a 1 km cell over your block. The best off-yard rain read short of your
  own gauge.
- **Nowcast**: a very-short-range analysis blending live radar and station
  reports (Pirate Weather in the US and Canada). Only seconds of lag, but it is
  a grid estimate, not a direct measurement.
- **Model**: a model or ML estimate of the **current** conditions. Close to now,
  but computed, never a direct reading.
- **Forecast**: a model or ML **prediction**, never a measurement. This is what
  every model provider's rain really is, including Pirate's: its rain is HRRR
  and GEFS model output, not radar, even when its temp and wind are a live
  nowcast.

The headline rule: **local stations win for what they cover, and cloud services
fill the gaps.** A station measures your yard; a cloud service estimates it. When
both are present, LocalSky takes the station for the fields it has and reaches
for the cloud only where the station is silent.

## Legend

| Word | Meaning |
|---|---|
| **Measured** | Real instrument observation from a physical station |
| **Radar** | Gauge-corrected radar rain estimate (observation grade) |
| **Nowcast** | Live radar plus station analysis (seconds of lag, grid estimate) |
| **Model** | Model or ML estimate of current conditions (computed, not measured) |
| **Forecast** | Model or ML prediction (never a measurement) |
| - | The provider does not supply this reading |

## The full provider capability matrix

Rows are grouped: **local stations** first (a real station you own, the only
sources that are Measured across the board), then the **cloud weather services**
that fill in when you have no hardware for a given reading.

| Provider | Temp | Humidity | Wind | Rain rate | Rain accumulation | Pressure | Solar | UV | Lightning |
|---|---|---|---|---|---|---|---|---|---|
| **Tempest** (local) | Measured | Measured | Measured | Measured | Measured | Measured | Measured | Measured | Measured |
| **Ecowitt** (local) | Measured | Measured | Measured | Measured | Measured | Measured | Measured | Measured | Measured |
| **Ambient Weather** (your station, cloud) | Measured | Measured | Measured | Measured | Measured | Measured | Measured | Measured | - |
| **Netatmo** (your station, cloud) | Measured | Measured | Measured | Measured | Measured | Measured | - | - | - |
| **La Crosse** (your station, cloud) | Measured | Measured | Measured | - | Measured | - | - | - | - |
| **NWS** (official station) | Measured | Measured | Measured | Measured | - | Measured | - | - | - |
| **NOAA MRMS** (radar rain) | - | - | - | Radar | Radar | - | - | - | - |
| **Pirate Weather** | Nowcast | Nowcast | Nowcast | Forecast | - | Nowcast | - | Nowcast | - |
| **OpenWeather** | Model | Model | Model | Forecast | - | Model | - | Model | - |
| **Apple WeatherKit** | Model | Model | Model | Forecast | - | Model | - | Model | - |
| **Open-Meteo** | Model | Model | Model | Forecast | Forecast | Model | Model | Model | - |
| **Met.no** | Model | Model | Model | Forecast | - | Model | - | - | - |

## Provider profiles: liveness, freshness, locality

The matrix above says what each provider *covers*. This table says what each
provider *is*: how live its number is, how often it refreshes and for how long
that number stays good, and how close to your yard it resolves. Every value here
is joined straight from the code LocalSky runs, so the guide cannot drift from
the app: the identity comes from the honest source catalog
(`src/sources/cloud_catalog.rs`), the refresh cadence is each adapter's poll
interval, and the "good up to" window is the freshness ceiling from
`src/config/region.rs`.

Rows follow the same order as the capability matrix (local stations first, then
cloud), but the app presents providers by **honesty rank** (NWS, then your own
cloud station, then radar, then the nowcast and model tiers) while irrigation
decisions follow **rain-trust rank**: a real gauge and radar QPE outrank every
model no matter how honestly it labels itself. Both orderings live in the catalog
on purpose.

| Provider | Key | Liveness | Refreshes | Good up to | Locality |
|---|---|---|---|---|---|
| **Tempest** (local) | none | Live LAN station | 60s | 600s (10min) | Your exact yard |
| **Ecowitt** (local) | none | Live LAN station | 30s (gw samples ~16s) | 600s (10min) | Your exact yard |
| **Davis** (local) | none | Live LAN station | 10s | 600s (10min) | Your exact yard |
| **Ambient Weather** (your station, cloud) | free key | Real station via cloud | 60s | 3900s (65min) | Your exact yard |
| **Netatmo** (your station, cloud) | free key | Real station via cloud | 10min | 3900s (65min) | Your exact yard |
| **La Crosse** (your station, cloud) | free key | Real station via cloud | 5min | 3900s (65min) | Your exact yard |
| **NWS** (official station) | none | Official observation, lags 30-90min | 30min | 2100s (35min) | Nearest station, often an airport 5-30 miles away |
| **NOAA MRMS** (radar rain) | none | Radar QPE (observation grade) | 3min | Rate 900s (15min), accum 7200s (2hr) | 1 km radar grid over your block |
| **Pirate Weather** | free key | Split: live nowcast + model rain forecast | 10min | 3900s (65min) | ~3 km grid in the US |
| **OpenWeather** | paid | Model forecast | 10min | 3900s (65min) | ~500 m to 2 km cell |
| **Apple WeatherKit** | paid | Model forecast | 10min | 3900s (65min) | Tuned to your coordinates (most precise cloud) |
| **Open-Meteo** | none | Model forecast | 1-6 hr upstream | 2100s (35min) | ~2 to 13 km model grid |
| **Met.no** | none | Model forecast, no live rain reading | 30min | 2100s (35min) | ~2.5 km Nordics, 9 km or more for a US yard |

Two rows need a word beyond the cell:

- **Pirate Weather splits down the middle.** Its temp, humidity, wind, pressure,
  and UV are a live **Nowcast** (live radar plus station reports, seconds of lag
  in the US), but its **rain is a Forecast**, HRRR and GEFS model output, **not**
  radar. Read its wind as live and its rain as a prediction, never the reverse. A
  free key sharpens the live temp and wind reads, and a real gauge still settles
  whether rain hit your yard.
- **Met.no ranks last for irrigation.** It is the only provider that emits **no
  live rain reading at all** (`emits_current_rain = false`) and its
  probability-of-precipitation is **synthesized**
  (`pop_is_synthetic = true`), the only provider true for either. Its rain is not
  just a forecast, it is a fabricated probability with no live reading behind it.

## How to read it

A few rows reward a second look:

- **The local stations (Tempest, Ecowitt) are Measured everywhere.** Every cell
  is your own instrument. This is why a station you own outranks every cloud
  service for the fields it covers: no cloud cell on this table beats a Measured
  one. Ecowitt's coverage is modular (the readings light up as you add the
  matching sensor), but the gateway is capable of every column.
- **The PWS rows (Ambient, Netatmo, La Crosse) are Measured too, just cloud
  routed.** They are your own consumer station reached through the vendor cloud,
  so every field they report is a real on-site measurement, the same gauge a
  direct LAN hookup would read. They cover fewer columns than a Tempest because
  the hardware varies (Netatmo needs the add-on anemometer for wind and has no
  solar or UV; La Crosse is temp, humidity, wind, and a daily rain total).
- **NWS is a real Measured observation,** but from the nearest official station,
  often an airport 5 to 30 miles away. It can simply miss the rain that fell on
  your yard. It reports a current rain rate but not a running daily total.
- **NOAA MRMS is rain only, and it is Radar, not a forecast.** It measures the
  rain that actually fell on a 1 km cell over your block: the best off-yard rain
  read short of your own gauge. It supplies nothing else.
- **Pirate Weather splits.** Its temp, humidity, wind, pressure, and UV are a
  live **Nowcast** (live radar plus station reports, seconds of lag in the US),
  but its **rain is a Forecast**, HRRR and GEFS model output, not radar. So the
  same Pirate row is honest-blue for wind and honest-amber for rain. A free key
  sharpens the live temp and wind reads in the US even though its rain is a
  forecast, and a real gauge still settles whether rain hit your yard.
- **OpenWeather, WeatherKit, Open-Meteo, and Met.no are model providers.** Their
  current readings are **Model** (a computed estimate of now) and their rain is
  a **Forecast** (a prediction). Open-Meteo is the keyless backstop and is the
  only one of the four that also models solar and a daily rain total; Met.no is
  the coarsest for a US yard (a roughly 9 km grid) and synthesizes its rain
  probability rather than modeling it.

## Lightning is local only

No cloud service on this table reports lightning. It comes only from a station
with a strike sensor (Tempest's hub or an Ecowitt WS6006). If lightning matters
to you, that is a hardware reading, not something a cloud key can buy.

## What this means for watering

LocalSky decides whether to skip a run on the most trustworthy **rain** signal
it can find, in this order: a gauge on your own yard, then NOAA MRMS radar QPE,
then an NWS station observation, then the nowcast and model providers. The table
is why: a Measured or Radar rain cell is a fact about water that fell; a Forecast
rain cell is a prediction that can report rain that did not fall or miss a small
cell. LocalSky will use a forecast when it is all that is available, but it never
labels one "live," and a real gauge always settles the question.

For the merge mechanics behind this (priority, per-field fallback, and
bias-correction against your own station), see
[Forecast sources and merge](forecast.md). For wiring the hardware that earns the
Measured rows, see [Weather and soil sensors](sensors.md).
