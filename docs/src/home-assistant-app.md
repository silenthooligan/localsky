# Install on Home Assistant OS

The LocalSky app runs the server on your Home Assistant OS machine. The optional HACS integration adds that server's entities to Home Assistant.

## Install the server

[![Add the LocalSky app repository](https://my.home-assistant.io/badges/supervisor_add_addon_repository.svg)](https://my.home-assistant.io/redirect/supervisor_add_addon_repository/?repository_url=https%3A%2F%2Fgithub.com%2Fsilenthooligan%2Flocalsky-apps)

1. Add `https://github.com/silenthooligan/localsky-apps` under **Settings → Apps → App store → Repositories**.
2. Install **LocalSky**, start it, and choose **Open web UI**.
3. Complete LocalSky's setup wizard.
4. If you want HA entities, [install the companion integration](hacs.md).

The package supports **amd64** and **aarch64**. Existing Supervised installations with an app store can also use it. For HA Container, install LocalSky [with Docker](getting-started.md).

## Configure the app

The app option `home_assistant: true` enables access through the Supervisor API. LocalSky can use that connection for HA device import and passthrough without a separate HA token.

`log_level` controls LocalSky log verbosity. Keep `info` for normal use and use `debug` while collecting evidence for a problem.

Weather sources, zones, irrigation rules, and accounts are configured inside LocalSky.

## Connect local devices

The app uses host networking. Port **8090** must be available; the direct address is **http://YOUR_HA_HOST:8090**. The sidebar also provides access through HA ingress.

Tempest broadcasts and mDNS discovery still depend on the network. If HA's WeatherFlow integration already handles Tempest, use [HA passthrough](hacs.md#use-home-assistant-weather-sensors) and release LocalSky's UDP listener.

## Back up and update

Home Assistant backups include the app's persistent data. The app stops briefly during a backup for a consistent database copy. LocalSky also offers its own backup download.

Update the app and companion integration to matching versions. Review the release notes and verify device health and zone status afterward.

[App repository](https://github.com/silenthooligan/localsky-apps) · [Backup guide](backup-restore.md) · [Troubleshooting](troubleshooting.md)
