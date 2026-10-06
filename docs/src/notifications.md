# Notifications

Choose which channels should receive run, decision, and device alerts in **Settings > Notifications**. Events include:

- **Zone started** and **zone stopped**, with the duration.
- **Daily watering outlook**, only if you enable it, once at your chosen local time.
- **A zone that did not start** because the controller refused the command, and **a controller that is not answering** when the morning needed it.
- **A valve that may still be open**: its shutoff was due and the controller has not confirmed closing it. LocalSky keeps retrying; this is the one notification worth walking outside for.
- **Water moving with nothing running**, when a flow meter is connected.
- **A forecast source that went quiet**, and **a soil probe that stopped reporting**.

Three channels deliver them: **Web Push** to a subscribed browser or the installed app, **ntfy** to any topic on any ntfy server, and **Slack** through an incoming webhook. Enable any or all under Settings, then Notifications; the wizard asks for the ntfy and Slack URLs on a new install. The Home Assistant MQTT block on the same page is a different feature, the discovery publisher for entities and sensor states; see the [HACS integration](hacs.md) page. The dashboard-only nudges (a tuning report is ready, a run cap was raised) go to Web Push alone.

## Device choices and quiet hours

Open **Settings > Notifications** in each phone or browser and select **Connect this device**. Choices are saved on your LocalSky server for that subscription, so they apply while the PWA is closed and survive server restarts. Reconnecting an existing subscription keeps its choices. Disconnecting removes that subscription; use **Notifications on this device** to pause delivery and retain your preferences.

Watering starts, finishes, equipment problems, soil sensor problems, an offline forecast source and watering configuration changes are enabled by default. Weather alerts, weekly tuning suggestions and daily outlooks are opt-in. Each switch explains its trigger.

**Quiet hours** default to **10 PM to 7 AM**, using the timezone displayed on the page (your LocalSky location, not a traveling phone's timezone). Routine alerts during quiet hours are skipped. Enabled **urgent equipment alerts** bypass quiet hours by default: a valve that may still be open, or measured flow with no zone commanded on. You can turn off that exception. Pausing the device or disabling the urgent category silences those alerts too.

Web Push messages are not retained for offline delivery. An offline phone will not receive a backlog when it reconnects. Browser permission, OS settings and connectivity still control final delivery and sound.

## Weather alerts

These optional PWA alerts use **fresh measured readings**, not forecast estimates or storm-potential scores. They are not official weather warnings and require a source that provides the relevant measurement.

| Choice | Trigger | Clears when |
|---|---|---|
| Rain starts | Station rain rate becomes positive | Station rain rate returns to zero |
| High wind | Sustained wind reaches 25 mph / 40 km/h | Wind falls to 20 mph / 32 km/h |
| Freezing temperature | Air temperature reaches 32°F / 0°C or lower | Temperature rises to 34°F / 1°C |
| High temperature | Air temperature reaches 95°F / 35°C | Temperature falls to 90°F / 32°C |
| Nearby lightning | A detector reports a strike within 10 miles / 16 km | No nearby detection for 30 minutes |

Conditions must clear before another alert, with at least one hour between alerts of each kind. The state is saved across restarts. The first rain/wind/temperature reading establishes a baseline; nearby lightning can notify immediately. Stale readings and forecast-filled values do not trigger alerts. Conditions that begin during quiet hours do not produce a catch-up notification afterwards.

## Daily outlook

**Daily watering outlook on this device** is off by default, including on upgrade. Enable it and choose a time outside quiet hours (9 AM by default). Your plan remains available in the app at any time.

Each device gets at most one outlook per local day. This limit survives restarts and forecast changes. LocalSky skips a missed 15-minute delivery window rather than sending an old outlook later. Zone plans may change before watering starts.

**Server and shared channels > Shared channel outlook** separately controls the optional summary for ntfy and Slack. PWA preferences and quiet hours do not change those channels.

## ntfy and Slack

ntfy wants a server (the public `https://ntfy.sh` or your own) and a topic; an access token is optional. LocalSky posts one message per event with the headline as the title. Slack wants an incoming webhook URL; LocalSky posts the headline in bold and the detail on the next line. A sink that fails is logged and never blocks the others. Email delivery is not supported.

## Web Push

Subscribe each browser or installed web app that should receive notifications. Delivery depends on browser permission, platform support, and the browser's push service. Use a secure context such as HTTPS.

Web Push needs a VAPID keypair so the push service can verify that notifications are signed by your LocalSky instance. The keypair is generated once and reused for the life of the deployment.

If the page says setup is needed, enable Web Push in **Setup** to generate the server keypair. Under **Settings > Notifications > Server and shared channels**, **Send push alerts** enables or pauses server delivery without removing keys. Then **Connect this device** on each phone or browser and allow notifications when asked. Runtime readiness includes keys supplied through environment variables.

Watering notifications offer **Stop watering** on supported browsers. This stops the current run and cancels the remaining Quick Run queue. An ordinary tap opens the app. If the alert is old, the connection is lost, or sign-in has expired, LocalSky reports that Stop was not confirmed and opens the watering controls. The phone must be able to reach your instance; a notification is not an offline remote control.

While the app is open, a watering strip stays available across pages with the current zone and **Stop**. Quick Run also shows approximate time remaining and queue progress. The strip stays visible when a stop needs attention.

### Manual key configuration

For deployments managed through environment variables, LocalSky supports the following fallback when no Web Push keypair is saved in configuration:

| Variable | What it is |
|---|---|
| `VAPID_PRIVATE_KEY_PATH` | Path (inside the container) to a PEM private key file. Both PKCS#8 (`BEGIN PRIVATE KEY`) and SEC1 (`BEGIN EC PRIVATE KEY`) PEMs are accepted |
| `VAPID_PUBLIC_KEY` | The matching public key as **unpadded base64url** (87 characters): the raw 65-byte uncompressed P-256 point, the same `applicationServerKey` format browsers use. Padded or standard base64 is rejected at startup with a log warning |
| `VAPID_SUBJECT` | Optional contact URI (`mailto:` or `https:`) the push service can use to reach you. Defaults to the LocalSky project URL |

If neither configuration nor environment provides a readable keypair, Web Push is unavailable; other notification channels continue working.

### 1. Generate the keypair

`openssl` produces exactly what LocalSky loads:

```bash
mkdir -p ./localsky-keys

# Private key: SEC1 PEM ("BEGIN EC PRIVATE KEY"), P-256.
openssl ecparam -genkey -name prime256v1 -noout \
    -out ./localsky-keys/vapid-private.pem

# Public key: the raw 65-byte uncompressed point, base64url, no padding.
openssl ec -in ./localsky-keys/vapid-private.pem -pubout -outform DER \
    | tail -c 65 | base64 -w0 | tr '+/' '-_' | tr -d '='
```

The second command prints an 87-character string starting with `B`; that is your `VAPID_PUBLIC_KEY`. Keep the PEM file safe: the config backup bundle (`GET /api/v1/backup`) deliberately excludes the keys directory, so back it up yourself.

> **Note on the `web-push` Node CLI:** `npx web-push generate-vapid-keys` emits the private key as a raw base64url scalar, not a PEM file. That string cannot be dropped into `vapid-private.pem` as-is (and wrapping it in `BEGIN PRIVATE KEY` markers does not make it PKCS#8). Use the `openssl` flow above instead; it needs no extra tooling.

### 2. Mount the key and set the environment

The private key lives in a host directory mounted read-only into the container. With Docker Compose:

```yaml
environment:
  - VAPID_PUBLIC_KEY=BNJxRy7...87-chars
  - VAPID_PRIVATE_KEY_PATH=/keys/vapid-private.pem
  - VAPID_SUBJECT=mailto:you@example.com
volumes:
  - ./localsky-keys:/keys:ro
```

The app runs as uid 10001. Unlike the writable `/data` volume (whose ownership the container fixes automatically), the keys directory is mounted read-only, so the container cannot adjust it for you. Make sure uid 10001 can read the PEM on the host:

```bash
chown 10001:10001 ./localsky-keys/vapid-private.pem
chmod 440 ./localsky-keys/vapid-private.pem
```

Restart the container after changing its environment variables.

The `[notifications.web_push]` block in `localsky.toml` (`vapid_public`, `vapid_private_path`, `vapid_subject`) takes precedence over these environment variables.

### 3. Verify the server side

```bash
curl http://localhost:8090/api/v1/push/vapid-key
```

A configured instance returns `{ "public_key": "BNJxRy7..." }`. A `503` with `{ "error": "vapid not configured" }` means the keys did not load; check the container logs for `push:` warnings (unreadable PEM path, malformed public key).

### 4. Subscribe a device

Open the dashboard on each phone / laptop / tablet that should receive notifications. Go to **Settings > Notifications** and tap **Connect this device**. The browser asks for notification permission; allow it. The dashboard registers a push endpoint with the public key, and saves the subscription for delivery.

To remove a device: tap **Disconnect** in the same panel, or clear the site data in the browser. To pause alerts and keep your choices, turn off **Notifications on this device** instead. Endpoints that a browser has revoked are pruned automatically the next time a push to them fails.

### Troubleshooting

- **The subscribe control reports push as unavailable**: the server did not load a VAPID keypair, or the history database (where subscriptions are stored) was not openable at startup. `GET /api/v1/push/vapid-key` distinguishes the two: `503` means keys, and `503` from `POST /api/v1/push/subscribe` with `"history db not configured"` means the database.
- **iOS does not show notifications**: iOS 16.4+ supports Web Push but only for PWAs added to the home screen via Share -> Add to Home Screen. A regular Safari tab will not ring.
- **No notifications after subscribing**: confirm the server side with `GET /api/v1/push/vapid-key`, then check delivery during a supervised run you already intend to make. Check the container logs for `push: send ... failed` lines.

## What fires when

| Event | Trigger |
|---|---|
| Zone started | A zone's running state flips from off to on |
| Zone stopped | A zone's running state flips from on to off (carries the run duration in minutes) |
| Daily watering outlook | Opt-in; once per local day at your chosen time, with no restart repeats or late catch-up |

Routine forecast updates stay in the app. Actual watering events and equipment
alerts are independent of the optional daily outlook schedule.
