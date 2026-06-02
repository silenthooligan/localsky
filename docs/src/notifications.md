# Notifications

LocalSky can push three classes of events to your subscribed devices:

- **Zone started** when an irrigation zone transitions from idle to running.
- **Zone stopped** when a zone finishes, with the duration in minutes.
- **Daily verdict** the first time a non-empty skip-check verdict is computed each local day.

Four delivery channels can be enabled in parallel: Web Push (browser / PWA), MQTT (Home Assistant or any broker), ntfy.sh, and Slack. None are on by default. The configuration block lives under `[notifications]` in `localsky.toml`; see the [configuration reference](configuration.md#notifications) for the field list.

## Web Push (recommended)

Web Push is the closest thing to a "real app notification" without putting LocalSky in any app store. Once a phone or laptop opens the dashboard and subscribes, the OS-native notification surface fires even when the browser is closed.

Web Push needs a VAPID keypair so the push service can verify that the notification is signed by your LocalSky instance. The keypair is generated once and reused for the life of the deployment.

### 1. Generate the keypair

The `web-push` Node CLI is the easiest path:

```bash
npx -y web-push generate-vapid-keys --json
```

You'll get something like:

```json
{
  "publicKey": "BNJxRy7...about-87-chars",
  "privateKey": "vIVJk0...about-43-chars"
}
```

The public key is base64url; the private key is the EC private scalar in base64url. Save both somewhere safe before continuing. If you prefer `openssl`:

```bash
openssl ecparam -name prime256v1 -genkey -noout -out vapid-private.pem
openssl ec -in vapid-private.pem -pubout -out vapid-public.pem
# Then base64-url-encode the raw bytes for the env var; web-push CLI is easier.
```

### 2. Write the private key as a PEM file

LocalSky reads the private key as a PEM file mounted into the container. With the JSON output from `web-push`:

```bash
mkdir -p ./localsky-keys
cat > ./localsky-keys/vapid-private.pem <<EOF
-----BEGIN PRIVATE KEY-----
$(echo -n "<the privateKey value from the JSON>" | base64 -d | base64)
-----END PRIVATE KEY-----
EOF
chmod 600 ./localsky-keys/vapid-private.pem
```

(The `web-push` CLI also has a `--pem` mode that emits a ready-to-use PEM directly; check `npx web-push --help` on your installed version.)

### 3. Configure LocalSky

Either via `localsky.toml`:

```toml
[notifications.web_push]
vapid_public       = "BNJxRy7..."
vapid_private_path = "/keys/vapid-private.pem"
vapid_subject      = "mailto:you@example.com"
```

…or via env vars on the container:

```yaml
environment:
  - VAPID_PUBLIC_KEY=BNJxRy7...
  - VAPID_PRIVATE_KEY_PATH=/keys/vapid-private.pem
  - VAPID_SUBJECT=mailto:you@example.com
volumes:
  - ./localsky-keys:/keys:ro
```

`vapid_subject` is a contact URI the push service uses if your instance starts misbehaving. Use a real `mailto:` you actually read.

### 4. Subscribe a device

Open the dashboard on each phone / laptop / tablet that should receive notifications. Go to **Settings -> Notifications -> Web Push** and tap **Subscribe on this device**. The browser asks for notification permission; allow it. The dashboard registers a push endpoint with the public key, and from that moment LocalSky can wake the device.

To stop receiving on a device: tap **Unsubscribe** in the same panel, or clear the site data in the browser.

### Troubleshooting

- **"Subscribe" button says "Subscriptions are disabled"** the server didn't load a VAPID keypair. Check the container logs for `vapid` warnings; the dispatcher logs a single warning at boot and silently drops every event afterwards if it can't find keys.
- **iOS doesn't show notifications** iOS 16.4+ supports Web Push but only for PWAs added to the home screen via Share -> Add to Home Screen. A regular Safari tab won't ring.
- **No notifications after subscribing** check `GET /api/v1/push/subscriptions` to confirm the device is registered. If it is, the next zone-start event from the engine will fire one.

## MQTT

LocalSky can publish the same three events as MQTT messages to a configured broker. The default discovery prefix is `homeassistant`, so a Home Assistant install on the same broker auto-discovers `sensor.localsky_*` and `binary_sensor.localsky_zone_*_running` entities.

Configure under `[notifications.mqtt]`. See the [HACS integration](hacs.md) page for the entity set Home Assistant gets in return.

## ntfy.sh

Free, no-account push to phones via the [ntfy.sh](https://ntfy.sh) service or a self-hosted ntfy server. Configure a private topic under `[notifications.ntfy]` and add the topic to the ntfy app on your phone.

## Slack

`[notifications.slack].webhook_url` accepts an incoming webhook URL. Events post as plain text to the channel the webhook is bound to. Useful for a household ops channel.

## What fires when

| Event | Channels | Trigger |
|---|---|---|
| Zone started | Web Push, MQTT, ntfy, Slack | A zone's `running` flag flips from off to on |
| Zone stopped | Web Push, MQTT, ntfy, Slack | A zone's `running` flag flips from on to off (carries duration in minutes) |
| Daily verdict | Web Push, MQTT, ntfy, Slack | First non-empty skip-check verdict after local midnight |

There is no rate-limit or quiet-hours logic in v0.1. If a misbehaving controller flaps a zone, every device on every channel hears every flap. Track [the roadmap](https://github.com/silenthooligan/localsky/issues) for a quiet-hours policy.
