# ar-translate

## Purpose

Wire adapters. Five inbound dialects (OpenAI chat, Anthropic Messages, Responses, Ollama, Gemini) each map to one canonical chat shape. `render_for_wire` renders the canonical form back out for each provider wire.

## Configuration

None at runtime. The dialect is chosen by the route the client calls.

## Extension points

Add a dialect by implementing `ArTranslate<In>` for it and a `to_canonical` function. Add a provider wire by adding a renderer to `outbound.rs` and a case to `render_for_wire`.

## Limitations

Each mapping covers the fields the canonical shape carries. Fields a dialect has and the canonical form lacks are dropped on translation, not passed through.

## Model experience

A client gets back the same dialect it sent: an Anthropic request returns an Anthropic message, even when the upstream speaks OpenAI.
