# Devices and data ingest API

Use these endpoints to inspect configured devices, discover channels, and receive supported sensor uploads. Paths use `/api/v1` unless stated otherwise.

## Devices

### `GET /api/v1/devices`

Every gateway, hub, controller, and cloud account LocalSky knows about, each with the sensors or zones it provides (the MA-style device view). Sorted by id.

### `GET /api/v1/devices/discover`

Broadcast LAN discovery (Ecowitt gateways today). Listens for about 3 seconds and returns the gateways found, each with a suggested host the UI pre-fills into an `ecowitt_gw_poll` source.

## Sensors and weather history

These endpoints are mounted only when the history database is available (it is, in any normal Docker deployment with `/data` mounted).

| Endpoint | Method | Purpose |
|---|---|---|
| `/api/v1/sensors/soil` | GET | Soil-moisture channels for the zone picker |
| `/api/v1/sensors/discovered` | GET | Every relevant entity LocalSky can see, grouped by role (HA entities as `ha:<entity_id>`, local POST channels as `source:<src>:<key>`) |
| `/api/v1/sensors/manifest` | GET | Declarative entity inventory for the HACS integration |
| `/api/v1/weather/history?hours=24` | GET | Recent observed-weather series (oldest to newest) for the headline fields; powers the dashboard sparklines |
| `/api/v1/weather/readings` | GET | Recent raw readings from the sensor-history table |

## Radar map data

Server-side data services for the radar map's overlay layers. Canonical prefix only (`/api/v1/radar/*`, no legacy `/api` alias). All three are built from upstream feeds with server-side caching, so map panning does not hammer the upstreams; on an upstream failure they return `502` and the frontend degrades the layer silently.

| Endpoint | Method | Purpose |
|---|---|---|
| `/api/v1/radar/windgrid?bbox=minLon,minLat,maxLon,maxLat` | GET | Wind field for the leaflet-velocity layer: a grib2json-style two-record array (U then V components in m/s) over an 8x8 grid clamped to the bbox. Cached about 30 minutes |
| `/api/v1/radar/precip?bbox=minLon,minLat,maxLon,maxLat` | GET | Short-range precipitation nowcast grid: 8 future 15-minute frames (the next 2 hours) of mm-per-15-min values over the same 8x8 grid, plus a `max_mm` scale hint. Cached about 15 minutes |
| `/api/v1/radar/tropical` | GET | Basin-aware tropical cyclone GeoJSON, normalized from the NHC/CPHC, JMA, and JTWC feeds into one FeatureCollection (positions, tracks, forecast tracks, cones) plus a per-agency `sources` health array. Cached 10 minutes |

## Data ingest

Push-style sensor receivers. Mounted at `/ingest/*` and `/api/v1/ingest/*`, and **unauthenticated by design** because the posting hardware cannot hold credentials; restrict receiver access to the network where the hardware posts. A source ID in a path is not authentication. Do not expose these to the internet: see [what to expose](reverse-proxy.md#what-to-expose).

| Endpoint | Method | Purpose |
|---|---|---|
| `/ingest/ecowitt` | POST | Ecowitt console "custom upload" receiver (form-encoded) |
| `/ingest/webhook/{id}` | POST | Generic HTTP webhook receiver for the configured webhook source `{id}` |

Both return `200` on successful parse so misconfigured downstreams do not trigger retry storms on the device.

[Device setup](devices.md) · [Sensor connections](sensors.md) · [API reference](api.md)
