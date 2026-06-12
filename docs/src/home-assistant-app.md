# Install as a Home Assistant App

> **This page is for Home Assistant OS (and Supervised) only.** Apps
> (formerly add-ons) are a Supervisor feature, and the Supervisor exists
> only on those two installation types. Check yours in Home Assistant under
> **Settings > About**, the **Installation method** line:
>
> - **Home Assistant OS** or **Supervised**: you are in the right place.
> - **Container** or **Core**: there is no app store on your install. Run
>   the LocalSky server with the [Docker quick start](getting-started.md)
>   instead; it is the exact same software, and everything else in these
>   docs applies unchanged.

If you run Home Assistant OS, you can skip Docker entirely: LocalSky ships
as a Home Assistant app. One click adds the repository, one click installs,
and the Supervisor manages the container, updates, and backups from then
on. It is the same released LocalSky image documented everywhere else in
these docs, packaged for the app store.

## Which piece is which

LocalSky on Home Assistant is always two pieces, and it is worth being
precise about them:

| Piece | What it is | Works on |
|---|---|---|
| **This app** | The LocalSky *server*: data collection, irrigation engine, web UI | Home Assistant OS / Supervised only |
| [HACS integration](hacs.md) | The *bridge* that turns a running server into HA entities | Every HA installation type |
| [Docker install](getting-started.md) | The same server, run anywhere Docker runs | Any machine, HA optional |

You always run exactly one server (this app **or** Docker, never both),
plus the integration if you want entities in Home Assistant. Install the
server first; the integration discovers it automatically over mDNS.

## Requirements

- Home Assistant OS or a Supervised installation (see the callout above)
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
