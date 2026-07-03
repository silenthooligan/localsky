# AI advisor (optional)

A fully optional natural-language layer over the engine's state. Point
LocalSky at any OpenAI-compatible endpoint, a local Ollama or
llama.cpp instance on your network, or nothing at all.

What it does when enabled:

- Writes the **Advisor** note on the irrigation dashboard: a one-or-two
  sentence plain-English read of today's verdict (what will run or
  skip, and the concrete conditions behind it), shown under the hero
  verdict and refreshed as conditions change (the explanation is cached
  for about five minutes). Also available at
  `GET /api/v1/irrigation/explanation`.
- Cross-checks the snapshot for anomalies: inconsistencies between the
  live station, the forecast, and the verdict (for example a rain gauge
  reading zero while the radar says it is pouring). Served as a
  structured list at `GET /api/v1/irrigation/anomalies`, refreshed
  hourly.

The advisor is a read-only narration layer; there is no chat
interface. If the provider is unreachable, the dashboard simply omits
the advisor note and the deterministic explanation stands on its own.

What it never does:

- Make watering decisions. The deterministic engine decides; the
  advisor only narrates and explains it.
- Send your data anywhere you didn't point it. Local endpoints stay
  local; the provider is your choice and "None" is a first-class
  setting.

Configure under Settings > Logic > LLM advisor, or during setup (the
step is skippable and defaults to off).
