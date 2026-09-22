# Install LocalSky

Choose where the server will run, then use the setup wizard to connect your devices.

| Your setup | Installation |
|---|---|
| A Linux server, NAS, or 64-bit Raspberry Pi | Docker, below |
| Home Assistant OS | [LocalSky app](home-assistant-app.md) |
| Windows or macOS | Docker with Linux containers; see networking below |
| Just exploring | [Open the live demo](https://demo.localsky.io) |

Published images support **amd64** and **arm64**. For scheduled irrigation, use a host that stays on through the watering window. Keep persistent storage for configuration, history, and recovery.

## Install with Docker

```sh
docker run -d \
  --name localsky \
  --restart unless-stopped \
  -p 8090:8090 \
  -v localsky-data:/data \
  ghcr.io/silenthooligan/localsky:latest
```

Open **http://localhost:8090** on the host, or **http://YOUR_SERVER:8090** from another device. The setup wizard opens on a fresh installation.

The named volume `localsky-data` survives container replacement. A writable bind mount also works; the image initializes ownership for its application user. Keep this volume when upgrading.

### Networking for local devices

For **Tempest UDP and LAN discovery on Linux**, use host networking:

```sh
docker run -d \
  --name localsky \
  --restart unless-stopped \
  --network host \
  -v localsky-data:/data \
  ghcr.io/silenthooligan/localsky:latest
```

Choose one of these commands for your installation. Host networking uses the host's port 8090 directly, so there are no `-p` options.

Publishing `50222:50222/udp` on a bridged container does not guarantee that LAN broadcasts reach it. Docker Desktop and virtual networks may also block discovery or station broadcasts. Enter reachable device addresses manually where supported, use HA passthrough, or place LocalSky on a Linux host on the station's network.

[Weather station connections](sensors.md) · [Using HA weather sensors](hacs.md#use-home-assistant-weather-sensors)

## Complete setup

1. **Location:** set the address or coordinates, timezone, and elevation. The local calendar affects schedules and history.
2. **Weather:** add your station or other sources. A new installation can use Open-Meteo without station hardware.
3. **Controller:** add and test a supported controller if you want irrigation. Import its zones where scanning is supported.
4. **Zones:** verify each controller binding and set plants, soil, application rate, and run limits. Finish zone editing in Settings after setup.
5. **Optional services:** choose notifications and an AI advisor if wanted.
6. **Account:** create an owner account to require sign-in. Skipping this leaves authentication disabled.
7. **Review:** save the configuration and follow any restart prompt.

For weather alone, leave controllers and zones empty. Irrigation navigation appears when irrigation is configured.

## Before the first automatic run

Open **Zones** and confirm that each LocalSky zone maps to the intended controller station. Use realistic application rates and duration limits.

Open **Irrigation → Watering decisions**. Check the selected weather sources, required data, zone needs, and planned timing. Disable competing schedules on the controller or in other software when LocalSky will own the schedule.

A controller connection test is not proof that every zone binding is correct. When testing a valve, supervise the specific zone and confirm that Stop closes it.

[Set up zones](zones.md) · [How decisions work](irrigation-engine.md)

## Explore a separate demo

The [public demo](https://demo.localsky.io) needs no installation. For a disposable local demo:

```sh
docker run -d \
  --name localsky-demo \
  -p 8091:8090 \
  -e LOCALSKY_DEMO=1 \
  ghcr.io/silenthooligan/localsky:latest
```

Open **http://localhost:8091**. Demo readings are simulated. Keep the demo separate from your real data volume.

## Next steps

[Your daily view](daily-use.md) · [Connect Home Assistant](hacs.md) · [Accounts and tokens](authentication.md) · [Remote access](reverse-proxy.md)
