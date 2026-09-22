# Advanced settings

Use **Settings > Advanced** for extra diagnostic detail, raw configuration, backups, and display controls.

## Nerd mode

Shows more of the inputs and calculations behind a decision. Use it when comparing source values, zone demand, and rule results. This is a browser preference; it does not change the engine.

## Kiosk mode

Hides irrigation controls on this browser for a shared display. It is a presentation preference, not server authorization. Someone with an operator credential can still call the API.

Use [authentication](authentication.md) and network access controls for an installation that other people can reach.

## Raw configuration

The TOML editor exposes settings beyond the standard forms. Saves validate the configuration and retain snapshots. A change can affect watering or require a restart; read the resulting message before leaving the page.

Use the [configuration reference](configuration.md) for fields and [backup guide](backup-restore.md) before recovery work.

## Source and API diagnostics

Source status is in **Settings > Devices**. Check the field's observation time as well as the connection status.

Use `/api/v1/info` to identify the server, health for component status, and diagnostics for a report. [Error codes](source-errors.md) and [API errors](api-errors.md) explain the detail to preserve.

## Release checks

The browser's release-check preference applies to this device. Server-side checks are separate and optional. Neither setting installs an update.

[Update LocalSky](upgrading.md)
