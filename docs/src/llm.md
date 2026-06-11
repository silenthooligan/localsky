# AI advisor (optional)

A fully optional natural-language layer over the engine's state. Point
LocalSky at any OpenAI-compatible endpoint, a local Ollama or
llama.cpp instance on your network, or nothing at all.

What it does when enabled:

- Writes the morning advisory: a two-sentence plain-English summary of
  what will run, what will skip, and why.
- Answers "should I water before the party Saturday?" style questions
  against live state in the chat panel.

What it never does:

- Make watering decisions. The deterministic engine decides; the
  advisor only narrates and explains it.
- Send your data anywhere you didn't point it. Local endpoints stay
  local; the provider is your choice and "None" is a first-class
  setting.

Configure under Settings > Logic > LLM advisor, or during setup (the
step is skippable and defaults to off).
