# Optional AI advisor

The advisor turns LocalSky's current decision into a short explanation and can report possible inconsistencies for review. Irrigation scheduling and valve commands remain controlled by the deterministic engine.

Configure it under **Settings → Logic → LLM advisor**, or skip it during setup.

## Connect a provider

Choose a supported local Ollama or llama.cpp endpoint, or an OpenAI-compatible endpoint. Enter the reachable address, model, and any required credential, then test the connection.

A local endpoint keeps those requests on your network. A remote endpoint receives the context sent for the explanation. Choose the provider accordingly.

## What you can read

- **Explanation:** a short account of the current verdict, available at `GET /api/v1/irrigation/explanation`.
- **Anomalies:** advisory observations available at `GET /api/v1/irrigation/anomalies`.

Explanations are cached for about five minutes; anomaly checks refresh less often. They are not a record of the exact evidence at a past dispatch.

If the provider is unavailable, use the engine's decision reasons and technical details. The advisor is optional, and there is no built-in conversational control interface.

## Connect your own AI tool

For an external assistant or agent, use the documented API, OpenAPI read profile, and client examples. Treat current state, future projections, and historical records as separate evidence.

[Connect AI tools](ai-integrations.md) · [Developer guide](developers.md)
