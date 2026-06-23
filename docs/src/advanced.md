# Advanced settings

The Advanced page (**Settings, Advanced**) is for debug visibility,
rollback, and backup. Nothing here changes how the engine decides to
water; these controls only expose what is already happening, or let you
recover a previous state. Most of the toggles are per-device (stored in
this browser's local storage), so turning one on here does not affect
anyone else's view.

## Nerd mode

Nerd mode surfaces the raw engine math everywhere. With it on, every
irrigation panel shows the numbers behind the verdict instead of just the
conclusion: reference evapotranspiration (ET0), crop evapotranspiration
(ETc), soil bucket depth, the species crop coefficient (Kc), the
management allowed depletion (MAD), available water, and root depth.

It is the right setting when you want to understand or audit a decision,
or when you are tuning species and soil settings and want to watch the
math respond. It is per-device and persisted, so you can leave it on for
your own browser without cluttering a shared dashboard.

## Kiosk mode

Kiosk mode hides destructive controls on this device. With it on, the
device cannot trigger any irrigation action: no running a zone, no
stop-all, no threshold edits, no pause toggles. Status, history, and all
the read-only views stay fully visible.

This is for shared and public-facing screens: a wall tablet, a family
device, a kiosk in a lobby. It is per-device, so the screen on the wall
can be locked down while your own browser keeps full control.

## Source freshness

A live status list for the data sources the engine depends on: your
configured weather station (labeled by kind, for example "Tempest weather
station"), the irrigation refresher, and the forecast source. Each
row shows whether the source is reachable and how long ago it last
reported, with a colored pill (fresh, stale, waiting, or offline).
Staleness is judged against each source's expected cadence, so a forecast
that polls every 30 minutes and a station that reports every few seconds
are each graded on their own clock. Use this to confirm a source is alive
before chasing a verdict you do not understand.

## Update check

An opt-in check for new LocalSky releases. Off by default. When you turn
it on, this device asks the project's version manifest at
`localsky.io/latest.json` for the newest release at most once per day and
shows it below the toggle, flagging when a newer version is available with
a link to the release notes. The page is explicit about the trade: that
request reveals this device's IP address to the `localsky.io` server, and
the running version travels in the request's User-Agent so the maintainer
can see aggregate version adoption. No per-install identifier or config
data is sent. That outbound contact is why it is opt-in. Per-device and
persisted.

## Demo mode

A read-only status line showing whether the deployment is running in demo
mode. When active, all controller actions are recorded but never fired
and the weather data is simulated. This is not a toggle on this page:
demo mode is enabled with the `LOCALSKY_DEMO=1` container environment
variable or `features.demo_mode = true` in `/data/localsky.toml`. The
line just tells you which mode you are in.

## Configuration history and rollback

Every time the configuration is saved, LocalSky snapshots the previous
version before writing. The Configuration history panel lists the most
recent versions (up to 20), each with its version number, when it was
applied, and an optional note.

If a change goes wrong, you can roll back to any listed version. The
rollback is performed through the API
(`POST /api/config/rollback?to=<version>`); the panel shows you which
versions are available to target. The first save records version 1, so a
brand-new install starts with an empty list.

## Backup and restore

A full backup in one bundle. **Download backup** produces a single
archive holding your configuration and the entire history database (runs,
sensor readings, and decisions). The VAPID push key and the instance
identity are deliberately left out, so a backup is safe to copy between
installs without cloning a deployment's identity.

**Restore from bundle** uploads a backup to apply. Because a restore
replaces both the current configuration and the history database, it asks
you to confirm before doing anything, and the picked file alone never
triggers it. A configuration restore applies on the next engine tick; a
database restore takes effect at the next container restart.

## Raw TOML editor

A direct editor for `/data/localsky.toml`. It loads the live config as
text, lets you edit it, and validates on save (TOML parse plus the schema
invariants) before writing. This is the escape hatch for adding sources,
controllers, or zones from a template you already have, bypassing the
wizard entirely. Unlike the JSON config API, the raw file shows secrets
in place, so treat the editor accordingly. The container loads the new
config on its next restart.

## Where to read more

- [Backup, restore, and recovery](backup-restore.md): the full backup
  workflow and what each bundle contains.
- [Configuration reference](configuration.md): every field the raw editor
  exposes.
- [Upgrading LocalSky](upgrading.md): version upgrades and the update
  check.
