# ar-registry

## Purpose

The provider catalog. A baseline of JSON tables is embedded at build time with `include_str!` and parsed lazily: `registry.json` (providers), `freeBudgets.json`, `modelLifecycle.json` and `providerMeta.json`. A discovery overlay fetches models.dev for providers outside the baseline.

## Configuration

No runtime settings. `ar_registry::check_embedded()` parses every table and checks base URLs, and `aroute serve` runs it before it binds.

## Extension points

Add a provider by adding its entry to `registry.json` (or the generator that writes it). The `discovery` overlay fills gaps at runtime. `vector` is feature-gated and off by default because it costs memory.

## Limitations

A malformed table is logged and served empty, so the server refuses to start rather than run without its catalog. Base URLs must resolve: an unresolved one is dropped with an error. Discovery never empties a running proxy's model list on a network failure.

## Model experience

`GET /v1/models` lists the catalog with stale-while-revalidate, so a model list is never empty because a fetch failed. A model a vendor has retired is hidden.
