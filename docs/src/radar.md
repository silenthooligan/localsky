# Live radar

The Live Radar panel is a real weather map: animated precipitation,
optional storm and lightning overlays, and a short-range precipitation
forecast that extends the loop past "now". It centers on your station
location and works the same everywhere on Earth, because the imagery
sources are chosen by region rather than hardcoded to one country.

This page covers what the radar shows, the providers behind it, how the
region-aware default picks them, and how to take manual control.

## What the radar shows

The map opens centered on your configured latitude and longitude. The
base layer is animated precipitation: a loop of recent radar frames
running through the present moment. A time label on the map names the
frame you are looking at; the loop plays on its own and you can let it
run.

On top of the precipitation loop, optional overlays add context:

- **Precipitation forecast**: extends the animation into the future
  (see below).
- **Severe weather alerts (US)**: NWS active-alert polygons, colored by
  severity (red extreme, orange severe). Tap one for the headline.
- **Tropical cyclones**: active storms worldwide (position, track, and
  forecast cone where the agency provides them). The label localizes to
  your region (hurricanes, typhoons, or cyclones).
- **Lightning strikes**: recent strikes from your local station and,
  when enabled, the Blitzortung community network.
- **Wind flow**: an animated particle field of current winds.

Every overlay degrades quietly: if a source is unreachable the rest of
the map keeps working, and an overlay with nothing to show (a quiet
storm basin, no active alerts) simply renders empty.

## The Layers drawer

A **Layers** chip sits over the map (top right). Open it to see every
available layer in two groups: imagery providers first, then feature
overlays. Each row has an On/Off pill and an info expander with a short
legend (color scale, refresh cadence, source). Toggle a layer on or off
and the change is immediate.

Your toggles are remembered per browser. The first time you open the map
it starts from the deployment's default layers (set under Settings,
Radar); after that, this device keeps whatever you last turned on. A
toggle you made survives a layer temporarily leaving the menu (for
example after a location change), so you do not lose your preferences.

## The providers, and what each is good for

LocalSky draws imagery from public, key-free weather services. There are
two kinds:

- **Animated radar + nowcast** sources serve a rolling loop of frames
  and drive the time animation.
- **Reflectivity mosaics (WMS)** are high-detail regional radar
  composites served as map tiles.

| Provider | Kind | Coverage | Good for |
|---|---|---|---|
| **LibreWXR** | Radar + nowcast | US, Canada, Europe, Japan, Taiwan, SE Asia | The regional default where covered: real radar plus a 60-minute nowcast |
| **RainViewer** | Radar | Global | The worldwide fallback: animated precipitation anywhere on Earth |
| **IEM NEXRAD** | Reflectivity (WMS) | US (CONUS) | Sharp, street-scale US base reflectivity |
| **NOAA nowCOAST** | Reflectivity (WMS) | US incl. Alaska, Hawaii, Caribbean, Guam | US detail beyond the contiguous 48 |
| **Environment Canada GeoMet** | Reflectivity (WMS) | Canada | National 1 km precip-rate composite |
| **DWD** | Reflectivity (WMS) | Germany / Central Europe | RADOLAN precipitation composite |
| **FMI** | Reflectivity (WMS) | Finland | National dBZ composite |

The two US reflectivity mosaics (IEM NEXRAD and nowCOAST) crossfade with
the animated layer: when you zoom in, the high-resolution mosaic takes
over for street-scale detail; when you zoom out, the animated loop
dominates. You get the smooth animation at a glance and the sharp detail
up close, with no manual switching.

## Auto: the region-aware default

By default the provider menu is **Auto**. LocalSky reads your station
location and offers global composites always, plus any regional source
whose coverage includes you. Catalog order is preserved so the menu
reads global first, then regional.

In practice:

- **Inside a LibreWXR region** (US, Canada, Europe, Japan, Taiwan, SE
  Asia): LibreWXR leads as the default radar, with RainViewer kept as
  the global fallback, and your country's reflectivity mosaic added when
  one exists.
- **Outside the LibreWXR regions** (for example Australia): RainViewer
  is the default radar, since it covers the whole planet.
- **Border areas** get both neighboring national composites on purpose
  (a Toronto user sees both the Canadian GeoMet layer and nearby US
  NEXRAD), because radars near the line still paint useful returns
  across it.

You do not have to configure anything for this to work. Auto follows
wherever your station is.

## Custom: choosing your own providers

To override the regional default, go to **Settings, Radar** and switch
the provider menu from **Auto** to **Custom**. The list seeds from
whatever Auto currently resolves to, so you start by editing the
recommendation rather than a blank slate.

In Custom mode every catalog provider has an On/Off pill, and a
**Recommended** badge marks the ones Auto would have picked for your
region. Any provider is allowed anywhere: this is deliberate, so you can
keep, say, a US reflectivity layer enabled in Europe to compare how two
sources render the same system. The coverage label tells you where a
source actually paints tiles; nothing stops you from enabling one out of
its region.

Two notes:

- A Custom menu must have at least one provider enabled. An empty Custom
  list would round-trip as Auto, so Save is blocked until you enable one.
- The stored list always keeps catalog order regardless of the order you
  clicked, so your saved configuration stays stable across edits.

### Default layers

The same Settings, Radar page has a **Default layers** section: the
layers (providers and feature overlays) that start visible for a browser
with no saved preference. This sets the first-load experience; once a
device has toggled layers on the map, those per-browser choices win. A
default for a provider you removed from the menu is simply ignored, so
leaving extras lit is harmless.

## The precipitation forecast layer

The **Precipitation forecast** overlay extends the radar loop into the
future. When you scrub or let the animation play past "now", it keeps
going into forecast frames, each clearly tagged "+Nm forecast" so a
prediction is never mistaken for an observation.

Where the radar source supplies a real nowcast (LibreWXR), those native
radar frames carry the forecast out to about an hour. Everywhere else
the forecast is an Open-Meteo model precipitation grid sampled over the
visible map and drawn as an animated heatmap for the next couple of
hours. It is lazy: nothing is fetched until you turn the layer on, and
it refetches as you pan.

## Attribution

Every provider and overlay carries its source attribution in the map
controls and in the Layers drawer expander. Some sources require it: the
Blitzortung lightning credit (CC BY-SA 4.0) is shown whenever community
strikes appear, and the WMS composites name their issuing agency. The
attribution line on the map adapts to whichever sources are actually
contributing at the moment.

## Where to read more

- [Forecast sources and merge](forecast.md): how the forecast numbers
  the precip layer draws are produced.
- [Weather and soil sensors](sensors.md): the local station behind the
  lightning layer.
- [Configuration reference](configuration.md): the `ui.radar` config
  block field by field.
