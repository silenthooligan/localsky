# API reference

LocalSky exposes REST JSON and SSE at **`/api/v1`**. LocalSky **0.9.2** uses response contract **2.4.0**. The path prefix and contract version are independent.

Start with the [API quick start](api-quickstart.md), or download the [OpenAPI read profile](openapi.json). The profile covers selected read operations; the reference below also documents control and administration.

## Find an endpoint

| Area | Reference |
|---|---|
| Current weather, forecast windows, extra models, archive | [Weather and forecasts](api-weather.md) |
| Plans, zone state, runs, skips, and commands | [Irrigation and history](api-irrigation.md) |
| Devices, entity inventory, sensor history, ingest | [Devices and data ingest](api-devices.md) |
| Configuration, setup, accounts, backups, system operations | [Configuration and administration](api-admin.md) |
| Live snapshot subscriptions | [SSE streams](api-streams.md) |
| HTTP failures, request IDs, and source diagnostics | [Errors and diagnostics](api-errors.md) |
| Compatibility and migration history | [Versions and migrations](api-versions.md) |

## Authentication

Create a token under **Settings → Account** and send:

```http
Authorization: Bearer lsk_YOUR_TOKEN
```

The token is shown once. Store it as a secret. It is not restricted to read operations.

`GET /api/v1/info` is public and reports whether authentication is required. Anonymous health requests receive reduced liveness information. Full source details and normal application data require the configured access policy.

[Authentication details](authentication.md) · [Browser and SSE authentication](api-streams.md)

## Read the values correctly

| Value | Contract |
|---|---|
| Epoch timestamp | UTC seconds |
| Daily date | Installation's configured local calendar |
| `*_f`, `*_in`, `*_mph`, `*_mm` | Units named by the field, independent of display preferences |
| `null` | Unknown or unavailable |
| Numeric zero | May be valid; legacy snapshot fields can require accompanying validity flags |
| Forecast `complete` | Hourly timestamps are covered; also check the required measurements |
| Zone `running_known` | Whether reported running state is known |
| Future water plan | Projection, not a command or a historical outcome |

Ignore additive fields your client does not understand. Check the API major before relying on a response shape.

## Versioning

The legacy `/api` alias remains for older clients on supported route families. New integrations should use `/api/v1`. Some newer routes only have the canonical prefix.

API major versions signal breaking response changes; minor versions add compatible fields or routes. Historical details and deprecated fields are in [Versions and migrations](api-versions.md).

## Client tooling

[Python client](examples/localsky_client.py) · [JavaScript client](examples/localsky-client.mjs) · [OpenAPI](openapi.json) · [AI integration guide](ai-integrations.md) · [llms.txt](llms.txt)
