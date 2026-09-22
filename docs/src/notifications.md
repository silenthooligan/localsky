# Notifications

Choose which channels should receive run, decision, and device alerts in **Settings > Notifications**. Events include:

- **Zone started** and **zone stopped**, with the duration.
- **Daily verdict** once per day, the first time the morning's decision is made (skip, run, run extended, with the reason).
- **A zone that did not start** because the controller refused the command, and **a controller that is not answering** when the morning needed it.
- **A valve that may still be open**: its shutoff was due and the controller has not confirmed closing it. LocalSky keeps retrying; this is the one notification worth walking outside for.
- **Water moving with nothing running**, when a flow meter is connected.
- **A weather source that went quiet**, and **a soil probe that stopped reporting**.

Three channels deliver them: **Web Push** to a subscribed browser or the installed app, **ntfy** to any topic on any ntfy server, and **Slack** through an incoming webhook. Enable any or all under Settings, then Notifications; the wizard asks for the ntfy and Slack URLs on a new install. The Home Assistant MQTT block on the same page is a different feature, the discovery publisher for entities and sensor states; see the [HACS integration](hacs.md) page. The dashboard-only nudges (a tuning report is ready, a run cap was raised) go to Web Push alone.

## ntfy and Slack

ntfy wants a server (the public `https://ntfy.sh` or your own) and a topic; an access token is optional. LocalSky posts one message per event with the headline as the title. Slack wants an incoming webhook URL; LocalSky posts the headline in bold and the detail on the next line. A sink that fails is logged and never blocks the others. Email delivery is not supported.

## Web Push

Subscribe each browser or installed web app that should receive notifications. Delivery depends on browser permission, platform support, and the browser's push service. Use a secure context such as HTTPS.

Web Push needs a VAPID keypair so the push service can verify that notifications are signed by your LocalSky instance. The keypair is generated once and reused for the life of the deployment.

LocalSky loads the keypair from environment variables at startup:

| Variable | What it is |
|---|---|
| `VAPID_PRIVATE_KEY_PATH` | Path (inside the container) to a PEM private key file. Both PKCS#8 (`BEGIN PRIVATE KEY`) and SEC1 (`BEGIN EC PRIVATE KEY`) PEMs are accepted |
| `VAPID_PUBLIC_KEY` | The matching public key as **unpadded base64url** (87 characters): the raw 65-byte uncompressed P-256 point, the same `applicationServerKey` format browsers use. Padded or standard base64 is rejected at startup with a log warning |
| `VAPID_SUBJECT` | Optional contact URI (`mailto:` or `https:`) the push service can use to reach you. Defaults to the LocalSky project URL |

If the variables are missing or the key file is unreadable, the dispatcher logs one warning at startup and silently drops every event; the rest of the app keeps running.

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

Restart the container after setting the variables; the keypair is read once at startup.

The `[notifications.web_push]` block you may see in `localsky.toml` or `GET /api/v1/config` (`vapid_public`, `vapid_private_path`, `vapid_subject`) mirrors these env vars so the settings UI can display them. Setting the TOML block alone does not enable push; the environment variables are the live configuration path.

### 3. Verify the server side

```bash
curl http://localhost:8090/api/v1/push/vapid-key
```

A configured instance returns `{ "public_key": "BNJxRy7..." }`. A `503` with `{ "error": "vapid not configured" }` means the keys did not load; check the container logs for `push:` warnings (unreadable PEM path, malformed public key).

### 4. Subscribe a device

Open the dashboard on each phone / laptop / tablet that should receive notifications. Go to **Settings -> Notifications -> Web Push** and tap **Subscribe this device**. The browser asks for notification permission; allow it. The dashboard registers a push endpoint with the public key, and saves the subscription for delivery.

To stop receiving on a device: tap **Unsubscribe** in the same panel, or clear the site data in the browser. Endpoints that a browser has revoked are pruned automatically the next time a push to them fails.

### Troubleshooting

- **The subscribe control reports push as unavailable**: the server did not load a VAPID keypair, or the history database (where subscriptions are stored) was not openable at startup. `GET /api/v1/push/vapid-key` distinguishes the two: `503` means keys, and `503` from `POST /api/v1/push/subscribe` with `"history db not configured"` means the database.
- **iOS does not show notifications**: iOS 16.4+ supports Web Push but only for PWAs added to the home screen via Share -> Add to Home Screen. A regular Safari tab will not ring.
- **No notifications after subscribing**: confirm the server side with `GET /api/v1/push/vapid-key`, then check delivery during a supervised run you already intend to make. Check the container logs for `push: send ... failed` lines.

## What fires when

| Event | Trigger |
|---|---|
| Zone started | A zone's running state flips from off to on |
| Zone stopped | A zone's running state flips from on to off (carries the run duration in minutes) |
| Daily verdict | The first verdict computation of each day (skip / run / run extended, with the reason text) |

There is no general quiet-hours policy. Repeated state changes can produce repeated notifications; investigate a flapping controller rather than relying on notification grouping to hide it.
