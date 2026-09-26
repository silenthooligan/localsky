# The LocalSky guide

<div class="ls-doc-hero">
<p class="ls-eyebrow">THE LOCALSKY GUIDE · v{{LOCALSKY_VERSION}}</p>
<p class="ls-doc-tagline">Know your weather.<br><em>Understand your watering.</em></p>
<p>LocalSky brings weather, soil conditions, and irrigation into one app on your own hardware. See what happened today, what is planned next, and why.</p>
<a class="ls-doc-primary" href="getting-started.html">Install LocalSky →</a>
<a class="ls-doc-secondary" href="https://demo.localsky.io">Explore the demo</a>
</div>

## Find your path

<div class="ls-doc-paths">
<a class="ls-doc-path" href="getting-started.html"><span class="ls-path-number">01</span><strong>Set up LocalSky</strong><span>Choose Docker or Home Assistant OS, connect devices, and finish your first setup.</span><span class="ls-path-link">Start here →</span></a>
<a class="ls-doc-path" href="daily-use.html"><span class="ls-path-number">02</span><strong>Use it every day</strong><span>Read today's status, understand the next run, and find the history behind it.</span><span class="ls-path-link">Explore the app →</span></a>
<a class="ls-doc-path" href="developers.html"><span class="ls-path-number">03</span><strong>Build an integration</strong><span>Connect dashboards, automations, and AI tools with the API and working examples.</span><span class="ls-path-link">Open the developer guide →</span></a>
</div>

## How LocalSky fits

**LocalSky is the server and app.** It collects readings, keeps local history, plans watering, and talks to your controller. Run it with Docker or as a Home Assistant OS app. Use it for weather alone if you do not have irrigation.

**The Home Assistant integration is an optional companion.** It brings LocalSky's readings, valves, and actions into HA. Existing HA sensors can also supply LocalSky through a separate passthrough source.

**Your connections determine what needs the internet.** Local devices can communicate over your LAN. Online forecasts, radar, cloud controllers, and remote advisors need their respective services. LocalSky itself requires no vendor account or subscription.

## Common tasks

| I need to… | Read |
|---|---|
| Connect my weather station | [Weather and soil sensors](sensors.md) |
| Keep HA's WeatherFlow integration | [Use HA weather sensors](hacs.md#use-home-assistant-weather-sensors) |
| Set up sprinklers | [Controllers](controllers.md), then [Zones](zones.md) |
| Understand a skipped run | [Watering decisions](irrigation-engine.md) |
| See runs and skipped mornings | [History](history.md) |
| Recover or move an installation | [Backup and restore](backup-restore.md) |
| Diagnose a problem | [Troubleshooting](troubleshooting.md) |
| Query weather or forecast data | [API quick start](api-quickstart.md) |

This guide describes the released version above. Your installation also includes its own version-matched copy at **/docs**, available on your LAN.

[GitHub](https://github.com/silenthooligan/localsky) · [Release notes](https://github.com/silenthooligan/localsky/releases) · [Report a problem](https://github.com/silenthooligan/localsky/issues)
