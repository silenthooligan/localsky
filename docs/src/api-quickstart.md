# Your first API request

You need a reachable LocalSky instance and, if authentication is enabled, an API token from **Settings → Account**.

## 1. Identify the server

```sh
curl --fail-with-body http://YOUR_SERVER:8090/api/v1/info
```

Confirm `service: "localsky"`, inspect `api_version`, and note `auth_required`. The `/info` endpoint is public. LocalSky 0.9.2 uses API 2.4.0 at the existing `/api/v1` paths.

## 2. Authenticate

Store the token in your shell or client secret store, then send it as a bearer credential:

```sh
export LOCALSKY_URL="http://YOUR_SERVER:8090"
# Set LOCALSKY_TOKEN through your shell or secret manager.
curl --fail-with-body \
  -H "Authorization: Bearer $LOCALSKY_TOKEN" \
  "$LOCALSKY_URL/api/v1/forecast/snapshot"
```

Use HTTPS when requests leave a trusted network. LocalSky does not expose a token scope that limits credentials to reads; enforce allowed operations in the client or an intermediary.

Browser clients must use the same origin or an appropriately configured server-side proxy. LocalSky does not provide permissive cross-origin API access.

## 3. Query a forecast window

Use the [Python example](examples/localsky_client.py):

```sh
python localsky_client.py --hours 3
```

Or the [JavaScript example](examples/localsky-client.mjs):

```sh
node localsky-client.mjs --hours 3
```

The clients select hourly forecast timestamps and call:

```text
GET /api/v1/forecast/window?track=merged&from=<first-hour-epoch>&to=<last-hour-epoch>
```

The bounds are **inclusive**. Three hours starting at 13:00 use rows stamped 13:00, 14:00, and 15:00. Each row describes rain during the following hour.

## 4. Check the evidence

A window includes `complete`, `age_s`, coverage counts, and nullable summaries.

`complete: true` means the expected hourly timestamps are present. It does not guarantee that all rain or temperature values are present. For a rain-dependent decision, also require non-null `precip_sum_in` and sufficient freshness.

A true zero is data. `null` means unavailable. Do not use expressions such as `value || 0` to fill missing readings.

## 5. Expand the integration

[OpenAPI read profile](openapi.json) · [Live SSE updates](api-streams.md) · [Forecast reference](api-weather.md) · [AI tools](ai-integrations.md)

For control operations, use the separate [irrigation reference](api-irrigation.md). Do not retry a watering command blindly after a timeout; first check whether the controller accepted it.
