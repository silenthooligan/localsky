# Radar and maps

The map shows precipitation imagery and optional weather overlays around your configured location. Read the frame time and source before interpreting what is on screen.

## Use the layers drawer

Open **Layers** to enable imagery and overlays. Each layer provides its source and legend. Choices are saved in this browser.

Depending on coverage and available data, overlays include precipitation forecasts, US alerts, tropical cyclones, lightning, and wind flow. A blank or failed layer is not proof that no weather hazard exists.

## Automatic and custom providers

**Settings > Radar > Auto** selects providers based on location. **Custom** lets you choose providers explicitly and requires at least one selection.

| Provider family | Use |
|---|---|
| LibreWXR | Animated radar and available nowcast frames in supported regions. |
| RainViewer | Radar imagery where its network provides coverage. |
| IEM NEXRAD and NOAA nowCOAST | US radar reflectivity layers. |
| NOAA MRMS | US radar rainfall estimates. |
| Environment Canada GeoMet | Canadian radar products. |
| DWD and FMI | Regional German and Finnish radar products. |

A provider listed in the menu may have no data outside its coverage. Selecting it does not extend that coverage.

## Past rain and future rain

Recent radar frames represent observations or estimates of precipitation. Frames beyond the present are labeled as forecasts. Where the imagery source supplies a nowcast, LocalSky can use it; other forecast frames can come from a modeled precipitation grid.

Map imagery is not a yard rain gauge. Use the source-aware readings and recorded history when investigating an irrigation decision.

## Household defaults

Set **Default layers** to choose the first view for a new browser. Once someone changes layers on that device, its saved choices take precedence.

[Forecasts](forecast.md) · [Provider capabilities](provider-matrix.md)
