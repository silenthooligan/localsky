# Operational error codes

Each operational failure identifies its boundary, failed operation, stable code and
next useful check. Evidence is included only when LocalSky can observe it.
HTTP 500 identifies a server response; HA or proxy logs are still required to
identify the exception that produced it. LocalSky never guesses that exception.

Codes and structured fields are the client contract. Messages can improve
without changing their identity. Credentials, raw response bodies and request
URLs are excluded. Batch failures retain individual causes and mapping indexes;
retry failures retain each attempted result. SQLite extended codes, OS codes,
TLS reasons and parse positions are included when observable.

API 2.4.0 adds diagnostics to privileged source health and supported operation
responses. Partial polling failures remain visible without discarding good
observations or changing their measurement age. A subsequent complete poll
clears the failure. Update checks retain the last successful result separately
from a failed attempt.

In Settings, expand a source's **Technical details** to copy its current failure.
Failed irrigation actions and update checks expose the same details. API errors
include `request.id`, `request.method`, `request.route` and an
`X-LocalSky-Request-Id` response header. Search that ID in LocalSky logs.
Validation responses retain their field/rule details. Browser network failures
cannot reveal DNS or TLS internals hidden by the browser; use its network panel.

Cause trees retain up to 16 causes per group and four nested levels. When a
provider produces more failures, `omitted_causes` states how many were excluded.
Updates checks report feed failures; image download/install failures are owned
by Docker, Home Assistant Supervisor or your deployment manager and appear in
those systems' logs. LocalSky does not install its own upgrades.

| Code | Meaning | Next check |
|---|---|---|
| `LS_HTTP_401` | upstream rejected authentication | Check the source credential and proxy Authorization forwarding. |
| `LS_HTTP_403` | upstream denied access | Check the source account permissions and proxy access rules. |
| `LS_HTTP_404` | upstream API route was not found | Check the configured API base address and supported API version. |
| `LS_HTTP_429` | upstream rate limited the request | Check the provider quota and polling interval. |
| `LS_HTTP_REDIRECT` | upstream redirected the API request | Configure the API address directly, without an interactive login redirect. |
| `LS_HTTP_SERVER` | upstream returned a server error | Check source and proxy logs at this timestamp; compare direct and proxied API responses. HTTP status alone does not identify the server exception. |
| `LS_HTTP_REJECTED` | upstream rejected the request | Check the reported status against the provider API contract. |
| `LS_NET_TIMEOUT` | request deadline expired | Check source availability and latency from the LocalSky host. |
| `LS_NET_CONNECT` | connection to the source failed | Check DNS, routing, port and TLS trust from the LocalSky host; use the OS error kind/code when present. |
| `LS_TLS_CERTIFICATE` | source TLS certificate validation failed | Use the certificate reason to fix the certificate chain, validity or hostname. Keep certificate verification enabled. |
| `LS_TLS_PROTOCOL` | TLS negotiation with the source failed | Check the source HTTPS port and TLS configuration; use the reported TLS reason. |
| `LS_NET_REQUEST` | HTTP request could not be sent | Check the configured API address and credential header format. |
| `LS_NET_BODY` | response body transfer failed | Check for interrupted or malformed HTTP responses in source/proxy logs. |
| `LS_DATA_DECODE` | response could not be decoded | Check that the endpoint returns the documented data format, not a login page. |
| `LS_JSON_SYNTAX` | response contains invalid JSON syntax | Check the API/proxy response format at the reported line and column. |
| `LS_JSON_SHAPE` | JSON does not match the expected response schema | Check the source API version and response schema at the reported line and column. |
| `LS_JSON_TRUNCATED` | JSON response ended before the value was complete | Check source/proxy logs for a truncated response. |
| `LS_CONFIG_URL` | configured API address is invalid | Set a complete HTTP or HTTPS API base address with a host. |
| `LS_CONFIG_SCHEME` | API address must use HTTP or HTTPS | Correct the source URL scheme. |
| `LS_NET_DNS` | source hostname did not resolve | Check name resolution inside the LocalSky container; inspect the reported resolver error kind/code. |
| `LS_NET_TARGET_BLOCKED` | source address is not a permitted device endpoint | Use a normal LAN or public address; loopback, link-local, metadata and multicast targets are rejected. |
| `LS_NET_CLIENT_INIT` | HTTP client initialization failed | Check the LocalSky runtime TLS trust configuration. |
| `LS_DATA_SIZE_LIMIT` | response exceeded the permitted byte limit | Check the API response size and the reported limit; the response was not ingested. |
| `LS_HA_ENTITY_STATE` | HA response was not the requested state | Check the mapped entity endpoint and HA/proxy routing; no partial readings were published. |
| `LS_HA_NO_MAPPINGS` | no mapped entities are available for HA recovery | Configure weather or soil entity mappings and inspect the bulk API failure in HA logs. |
| `LS_HA_FALLBACK_FAILED` | HA bulk read and mapped-entity fallback both failed | Inspect both recorded causes; check HA/proxy logs at this timestamp. No partial readings were published. |
| `LS_STREAM_CLOSED` | upstream stream closed | Check source availability and reconnect; retained observations keep their original age. |
| `LS_STREAM_PROTOCOL` | stream protocol failed | Use the protocol category to check endpoint compatibility and proxy WebSocket support. |
| `LS_MQTT_PROTOCOL` | MQTT connection or protocol failed | Use the broker return code or protocol category to check broker credentials, permissions and connectivity. |
| `LS_UDP_IO` | UDP listener operation failed | Use the OS error kind/code to check the bind address, port ownership and host network configuration. |
| `LS_UPDATE_MANIFEST` | release manifest contains an invalid or missing value | Check the identified manifest field and release feed; the previous successful version check has been retained. |
| `LS_PROVIDER_OFFLINE` | provider is unavailable | Check the provider connection and its last recorded transport failure. |
| `LS_LLM_MODEL_UNAVAILABLE` | requested language model is unavailable | Check the configured model name and the provider's installed model list. |
| `LS_CONFIG_MISSING` | configuration file was not found | Check the data mount and complete setup if this is a new installation. |
| `LS_CONFIG_SCHEMA` | configuration schema is not supported by this binary | Check the reported schema versions and use a compatible LocalSky release before restoring or loading this configuration. |
| `LS_CONFIG_SNAPSHOT_MISSING` | requested configuration snapshot was not found | Refresh the available snapshots and select an existing version. |
| `LS_TOML_PARSE` | configuration contains invalid TOML or an incompatible value | Check the reported byte offset and field against the configuration schema; do not share secrets from the file. |
| `LS_DATA_SERIALIZE` | data could not be serialized | Check the reported operation and data schema; this is a LocalSky error requiring a reproducible report. |
| `LS_DATA_DATE` | stored date could not be parsed | Check the reported field and parser category against the expected ISO date format. |
| `LS_STORAGE_CHECKPOINT_BUSY` | database checkpoint could not finish | Check long-lived database readers and retry the operation; durability has not been confirmed. |
| `LS_STORAGE_IO` | filesystem operation failed | Use the operation, OS error kind and code to check storage permissions, available space and mount availability. |
| `LS_SQLITE` | database operation failed | Use the SQLite extended code and operation to check schema, constraints, locks, disk space or database integrity. |
| `LS_SQLITE_SHAPE` | database result does not match the expected schema | Inspect the identified database operation and column index; verify migrations completed on this database. |
| `LS_TASK_CANCELLED` | background operation was cancelled | Check shutdown or worker cancellation at this timestamp; completion was not confirmed. |
| `LS_TASK_PANIC` | background operation panicked | Check the panic trace at this timestamp and include the operation and LocalSky revision in a bug report. |
| `LS_WATERING_HELD` | watering is held before dispatch | Read the accompanying hold reason and system readiness; no controller command was sent. |
| `LS_CONTROLLER_OFFLINE` | no current controller status is available | Check the controller connection and the preceding status failure at this timestamp. |
| `LS_CONTROLLER_ZONE_MAPPING` | zone has no station mapping on this controller | Edit the zone's Controller station and select the matching physical valve. |
| `LS_CONTROLLER_UNSUPPORTED` | controller does not support this operation | Check the controller's reported capabilities and choose a supported operation. |
| `LS_CONFIG_FIELD` | configuration field is invalid | Correct the identified field and its validation message before saving. |
| `LS_MQTT_DISCONNECTED` | MQTT broker is disconnected; command was not delivered | Check the broker connection and credentials; the shutoff deadline remains armed. |
| `LS_MQTT_DEVICE_OFFLINE` | MQTT device reports offline; command was not delivered | Check device power and its availability topic; the shutoff deadline remains armed. |
| `LS_MQTT_QUEUE` | MQTT request could not be queued | Check the broker worker lifecycle and request channel; no delivery was confirmed. |
| `LS_RETRY_EXHAUSTED` | all permitted attempts failed | Inspect each recorded attempt and its operation; correct the underlying failure before retrying. |
| `LS_DATA_COMPRESSION` | compressed payload could not be decoded | Check the source content encoding and payload integrity; inspect the decoder OS error kind when present. |
| `LS_BATCH_FAILED` | every requested item failed | Inspect the per-item causes and mapping indexes; no successful result was available. |
| `LS_BATCH_PARTIAL` | some requested items failed | Inspect the per-item causes and mapping indexes; successful readings retain their own age. |
| `LS_QUERY_NO_SAMPLE` | query returned no numeric sample | Run the identified query against the source and check the selected series, time range and value type. |
| `LS_QUERY_REJECTED` | source rejected the query | Check the identified query and source query logs at this timestamp. |
| `LS_DATA_FIELD_MISSING` | response is missing a required field | Check the identified response field against the provider API version and mapped device. |
| `LS_SOURCE_DEVICE_MISSING` | no matching configured device was found | Check the account, device identifier and source mappings. |
| `LS_PROVIDER_REJECTED` | provider reported an application-level failure | Look up the reported provider code and operation; inspect provider logs when no code was supplied. |
| `LS_CONFIG_SIGNING_KEY` | signing key is not a valid supported PKCS#8 private key | Check the configured key format and algorithm; do not paste the key into diagnostic reports. |
| `LS_GRIB_MESSAGE` | payload contains no readable GRIB message | Check the selected radar product and provider response format. |
| `LS_GRIB_DECODE` | GRIB data decoding failed | Check the reported decoder stage and product; retain the product timestamp for a reproducible report. |
| `LS_GRIB_OUTSIDE_GRID` | deployment location lies outside the product grid | Check deployment coordinates and select a product covering the location. |
| `LS_GRIB_GRID` | GRIB grid geometry or cell index is invalid | Check the selected product grid type and the identified geometry field. |
| `LS_SOURCE_DIAGNOSTIC_MISSING` | source adapter returned an untyped failure | Include the source ID, operation and LocalSky revision in a bug report: this adapter discarded the failure type and needs instrumentation. |

- `LS_RESTORE_SCHEMA`: incompatible database schema or recovery journal; preserve the recovery files and inspect the named step.
- `LS_API_REJECTED`: request rejected; response validation details and request ID identify the route and rule.
- `LS_API_SERVER`: server operation failed; correlate request ID, timestamp, route and response detail with server logs.
- `LS_BROWSER_NETWORK`: the browser could not reach LocalSky. Browser fetch does not expose reliable DNS/TLS causes; inspect its network panel.
