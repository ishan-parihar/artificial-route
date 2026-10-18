# ar-exec

## Purpose

Upstream dispatch. `post_chat` and `post` take a canonical chat, send it to a provider, and return a live response stream. `render_request` is the one wire gate both use. Also media and OAuth helpers, the SSE reader, and URL normalization.

## Configuration

Per-provider base URL, auth class and wire come from `ar-registry`. The OAuth session comes from `ar-config`. No separate settings are read here.

## Extension points

A new provider wire is one renderer, selected in `render_request` and implemented in `ar-translate`'s outbound mappers. Auth goes through the header helpers, so a new auth class is one match arm.

## Limitations

Only the default executor and the seven wire renderers are ported. The other OmniRoute executors (Bedrock SigV4, Vertex, and the web-session ones) are dropped as `docs/02` directs. Transport failures surface as `ExecError`, and the router decides whether to fail over.

## Model experience

A transport failure is reported as an `ExecError`; the router decides whether to fail over.
