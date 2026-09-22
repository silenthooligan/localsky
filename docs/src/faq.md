# Questions and terms

## Do I need Home Assistant?

No. LocalSky has its own web app, weather inputs, watering engine, scheduler, and controller adapters. The HA integration is an optional companion.

## Can it run completely locally?

The server, stored data, supported LAN devices, and controller commands can stay on your network. Online forecasts, cloud hardware, radar services, remote AI providers, and most push delivery need their respective external services.

Loss of internet does not turn missing forecast evidence into permission to water. Automatic watering can hold when required data is unavailable. Plan your connections around the behavior you need.

## Which Home Assistant repository do I install?

[localsky-apps](https://github.com/silenthooligan/localsky-apps) installs the server on Home Assistant OS. [localsky-ha](https://github.com/silenthooligan/localsky-ha) adds LocalSky entities and actions to HA. You can use the companion with a server running elsewhere in Docker.

## Can HA keep its WeatherFlow integration?

Yes. Feed its sensors into LocalSky through HA passthrough. Use the preceding-minute rain mapping for HA's local precipitation sensor; LocalSky accumulates that into a daily total. It cannot reconstruct minutes missed while disconnected.

## Why did it skip today?

The **Daily log** records evaluated daily outcomes and their reasons. **Watering decisions** explains the current evidence and projections. If the server was off or no decision was recorded, absence of a run is not proof of a particular skip reason.

## Can I use it without irrigation?

Yes. Connect weather sources and use the dashboard, forecasts, history, and API.

## Where is my data?

The installation stores configuration, its migration ledger, history, and account data in the persistent data directory. Zone photos are stored separately within the data tree by default. Keep a [backup](backup-restore.md) outside the host.

## Does LocalSky send telemetry?

The installed app has no usage or crash-reporting service. Configured providers receive their normal requests: forecasts use your location, cloud hardware uses its credentials, and an enabled remote advisor receives the context needed for its response. Update checks are optional.

The public website and documentation have their own site analytics. They are separate from your installation.

## Can I run a second instance?

Use a separate data directory and port. For testing, isolate it from real controllers. Two independent schedulers pointed at the same valves can conflict; LocalSky is not an active-active controller cluster.

## What does beta mean?

LocalSky is in its 0.x release series. Behavior and API contracts can change between releases. Read release notes, back up before updating, and verify a new controller configuration under supervision.

## Can an AI assistant use the API?

Yes, if your connector can reach the instance. Start with the [AI integration guide](ai-integrations.md), OpenAPI read profile, and example clients. Keep watering and configuration commands outside a read connector. API tokens themselves are not read-scoped.

## Terms used in the app

| Term | Meaning |
|---|---|
| ET0 | Reference evapotranspiration: modeled water loss from a reference surface. |
| ETc | Plant water demand, adjusted from ET0 using a crop coefficient. |
| Kc | The crop coefficient for the plant and season. |
| TAW | Water available to roots between field capacity and wilting point. |
| MAD / RAW | Allowed depletion fraction, and the corresponding readily available water depth. |
| Depletion | Estimated water missing from the root zone. |
| Soil model | Carries the water balance forward and schedules from depletion and its trigger. |
| Weekly model | Allocates a weekly target after accounting for rain and irrigation. |
| Water plan | A projection across coming days, updated as evidence changes. |
| Cycle and soak | Short watering passes separated by time for infiltration. |
| SSE | Server-Sent Events: a persistent connection for snapshot updates. |
