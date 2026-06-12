# Install as a Home Assistant App

If you run Home Assistant OS (or a Supervised install), you can skip Docker
entirely: LocalSky ships as a Home Assistant app. One click adds the
repository, one click installs, and the Supervisor manages the container,
updates, and backups from then on. It is the same released LocalSky image
documented everywhere else in these docs, packaged for the app store.

> **Two pieces, same as always.** The app runs the LocalSky *server* on your
> Home Assistant machine. The [HACS integration](hacs.md) is still the piece
> that turns it into HA entities. Install the app first, then the
> integration; the integration discovers the running app automatically over
> mDNS.

## Requirements

- Home Assistant OS or a Supervised installation (the app store is a
  Supervisor feature; Container and Core installs should use the
  [Docker quick start](getting-started.md) instead)
- An `amd64` or `aarch64` machine (Raspberry Pi 4/5, ODROID, generic x86)
- A free TCP port **8090** on the host

## Install

[![Add repository to my Home Assistant](https://my.home-assistant.io/badges/supervisor_add_addon_repository.svg)](https://my.home-assistant.io/redirect/supervisor_add_addon_repository/?repository_url=https%3A%2F%2Fgithub.com%2Fsilenthooligan%2Flocalsky-apps)

Or manually: **Settings > Apps > App store**, open the overflow menu, choose
**Repositories**, and paste:

```
https://github.com/silenthooligan/localsky-apps
```

Then install **LocalSky** from the store. Installs pull a prebuilt
multi-arch image, so there is no local build step.

## First run

1. Start the app, then click **OPEN WEB UI** (the UI listens on port 8090).
2. The first-run wizard walks you through station setup (Tempest or
   Ecowitt), location, zones, and your irrigation controller, exactly as in
   the [Quick start](getting-started.md).
3. Optional but recommended: install the [LocalSky integration](hacs.md)
   for weather, soil, and irrigation entities in Home Assistant. One click
   adds it to HACS, and it discovers the running app on its own:

   [![Open your Home Assistant instance and add this repository to HACS](https://my.home-assistant.io/badges/hacs_repository.svg)](https://my.home-assistant.io/redirect/hacs_repository/?owner=silenthooligan&repository=localsky-hacs&category=integration)

## How the Home Assistant connection works

The app talks to Home Assistant through the Supervisor proxy. There is no
URL to enter and no long-lived access token to create; device import and
entity blending work out of the box. If you want a fully standalone server
that happens to live on your HA box, turn the `home_assistant` option off.

## Options

| Option | Default | What it does |
|---|---|---|
| `home_assistant` | on | Connect to HA through the Supervisor (device import, entity blending) |
| `log_level` | `info` | Server log verbosity; `debug`/`trace` raise only LocalSky's own namespaces |

Everything else is configured in LocalSky itself, through the wizard and
Settings. The app intentionally does not duplicate that configuration.

## Networking

The app runs on the host network. That is required so it can hear the
Tempest station's LAN broadcast (UDP 50222), reach your Ecowitt gateway and
OpenSprinkler controller, and announce itself over mDNS for integration
discovery. The web UI binds host port 8090; if something else on the
machine already uses it, the app log shows a bind failure at startup.

## Data, backups, and updates

Everything LocalSky stores lives in the app's `/data` volume:
`localsky.toml` and the `irrigation.db` history database. That volume is
included in Home Assistant backups, and the app stops briefly during a
backup so the database is captured consistently. App updates appear in the
store like any other app; the app version tracks LocalSky releases.

## Troubleshooting

- The app's **Log** tab shows the server log at the configured level.
- The watchdog probes `/api/v1/info` and restarts the app if the server
  stops responding.
- Port 8090 already taken: free it or move the other service; the app
  currently uses a fixed port.

The app packaging itself lives at
[github.com/silenthooligan/localsky-apps](https://github.com/silenthooligan/localsky-apps);
issues with LocalSky itself belong on the
[main tracker](https://github.com/silenthooligan/localsky/issues).
