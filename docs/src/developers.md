# Build with LocalSky

Use LocalSky as a data source for dashboards, automations, reports, and AI tools. The API provides current conditions, forecast windows, irrigation state, and recorded history. SSE streams deliver snapshot updates.

<div class="ls-doc-paths">
<a class="ls-doc-path" href="api-quickstart.html"><strong>Make your first request</strong><span>Connect, authenticate, and query a forecast window.</span><span class="ls-path-link">API quick start →</span></a>
<a class="ls-doc-path" href="api.html"><strong>Find an endpoint</strong><span>Browse the reference by weather, irrigation, devices, or administration.</span><span class="ls-path-link">API reference →</span></a>
<a class="ls-doc-path" href="ai-integrations.html"><strong>Connect an AI tool</strong><span>Import a read profile and preserve source, age, and uncertainty.</span><span class="ls-path-link">AI integration guide →</span></a>
</div>

## Pick an interface

| Need | Interface |
|---|---|
| Current state or a one-time query | REST JSON |
| Live dashboard updates | [SSE](api-streams.md) |
| Native Home Assistant entities and actions | [Companion integration](hacs.md) |
| Existing broker automations | MQTT publishing and supported MQTT inputs |
| Custom weather hardware | [Sensor ingest](api-devices.md#data-ingest) |
| An OpenAPI-compatible client | [Download the read profile](openapi.json) |

The OpenAPI file describes selected read endpoints. It is an importable connector profile, not an exhaustive specification of every LocalSky route. Set its server URL to your own instance.

## Start with working examples

[Download the Python client](examples/localsky_client.py) or [JavaScript client](examples/localsky-client.mjs). Both probe the server, check API compatibility, and query a forecast window without issuing watering commands.

The examples read `LOCALSKY_URL` and an optional `LOCALSKY_TOKEN` from the environment. Keep credentials in your client or connector's secret storage.

## Contract basics

- **Base path:** `/api/v1`.
- **Response contract:** API **{{LOCALSKY_API_VERSION}}**. The version in the URL and response contract are separate.
- **Authentication:** bearer API token when required.
- **Unknown data:** null remains unknown. Some legacy snapshot numbers also require their accompanying validity or timestamp fields.
- **Time:** epoch values are UTC seconds. Daily grouping uses the installation timezone.
- **Units:** follow field suffixes. Display preferences do not change API units.
- **Errors:** preserve the response status, code, and request ID.

For consequential automation, evaluate source age, coverage, and the fields you actually need. A successful HTTP response can contain unavailable data.

## AI-readable documentation

[llms.txt](llms.txt) is a concise navigation index. [llms-full.txt](llms-full.txt) contains the guide in plain text. These files help tools find documentation; they do not grant network access or credentials.

[Connect AI tools](ai-integrations.md) · [Errors](api-errors.md) · [Compatibility](api-versions.md)
