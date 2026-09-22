# Live updates with SSE

LocalSky publishes complete JSON snapshots as Server-Sent Events. Use them for dashboards and integrations that need updates without repeated polling.

| Stream | Payload |
|---|---|
| `/api/v1/stream` | Current weather snapshot |
| `/api/v1/irrigation/stream` | Irrigation snapshot |
| `/api/v1/forecast/stream` | Selected forecast snapshot |

The event name is **snapshot**. These are state feeds, not a durable event log.

## Connect from a terminal

```sh
curl --no-buffer --fail-with-body \
  -H "Authorization: Bearer $LOCALSKY_TOKEN" \
  "$LOCALSKY_URL/api/v1/irrigation/stream"
```

Parse SSE event boundaries before decoding the `data:` payload as JSON. A network read can contain part of an event or several events.

Weather and irrigation streams send keep-alives every 15 seconds; forecast uses 30 seconds. A keep-alive is not a new measurement.

## Connect from the app's origin

A browser signed in to LocalSky can use its session cookie:

```javascript
const stream = new EventSource("/api/v1/irrigation/stream");
stream.addEventListener("snapshot", (event) => {
  const state = JSON.parse(event.data);
  // Replace the displayed state; preserve nulls and observation times.
});
stream.addEventListener("error", () => {
  // Show a disconnected state while EventSource reconnects.
});
// When the view is disposed:
// stream.close();
```

Browser EventSource cannot set an Authorization header. LocalSky also accepts `access_token` in the query string on stream paths, but URLs can enter logs and browser history. Prefer a same-origin session or a server-side client that sends the bearer header.

## Reconnect and recover

Reconnect with backoff after transport failures. Treat the next snapshot as current state. There is no documented replay cursor for recovering every missed transition.

Use the history API for recorded runs and decisions. A closed connection must not leave a dashboard showing an old valve state as confirmed current state.

## Reverse proxies

Disable response buffering for event streams and allow long-lived connections. Authentication redirects and short proxy timeouts can interrupt a stream even when ordinary JSON requests work.

[REST reference](api.md) · [Reverse proxy setup](reverse-proxy.md) · [History](api-irrigation.md)
