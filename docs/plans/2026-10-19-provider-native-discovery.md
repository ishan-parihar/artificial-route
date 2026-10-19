# Provider-native model discovery (priority over models.dev)

Operator ruling (2026-10-19): /v1/models context windows must come from the
providers themselves first, models.dev second, the 128K floor last. Today the
discovery overlay fetches models.dev only, so kilocode-routed models (laguna,
beam-free, nemotron-3.5-super-vl) sit at the floor although kilocode's own
catalog publishes real windows.

## Evidence (probed live 2026-10-19)

- `https://api.kilo.ai/api/gateway/models` (kilocode, 402 rows): OpenRouter
  shape — `context_length` + `top_provider.{context_length,
  max_completion_tokens}` on every row. laguna/beam/nemotron all carry real
  figures there.
- models.dev **does** publish kilocode under `kilo` (the registry's id is
  `kilocode`; the overlay's provider key lookup missed the alias — secondary
  bug found: `published_window` matches by provider id only).
- nvidia + cline native `/v1/models`: bare id lists, no window fields — no
  native source exists for those; models.dev carries their windows already.

## Design

One extra fetch in the discovery loop, merged by specificity (native wins,
models.dev fills gaps, config/floor unchanged):

1. `models.rs::fetch_once` (models.dev doc GET) stays as today.
2. New: `fetch_provider_native` — for each configured provider with an
   `api_base_url`, GET `{base}/models` (bearer key from the provider row),
   parse OpenRouter-shaped rows (`context_length` /
   `top_provider.context_length`). Providers that answer a bare list (nvidia,
   cline) parse to zero windows and are skipped — one fetch, no per-provider
   special cases, no harm.
3. Lookup priority in `published_window`: native catalog first, then
   models.dev view, then `providerMeta`, then the 128K floor (already the
   card-builder order — only the source feeding `published_window` grows).
4. Alias fix in `published_window`: also try the provider's registry alias
   (`kilocode` → `kilo`) so the models.dev leg stops missing on alias-only
   entries.
5. Auth: reuse the provider row's key (same secret the executor already
   holds; no new credential surface). Anonymous providers send no header.
6. Cadence: same 60s SWR tick as models.dev; native failures are logged and
   never empty the merged view (offline-first contract unchanged).

## Out of scope

- Anthropic/Gemini-shaped native catalogs (no routed provider uses them).
- Caching native responses to disk (in-memory SWR only, same as models.dev).

## Acceptance

- `beam-free`, `nemotron-3.5-super-vl`, `laguna-s-2-1` cards report their
  kilocode-published windows instead of 128000.
- `glm-5.3-flash` / `step-5-preview` / `deepseek-v4-1-flash` unchanged
  (1M / 1048576 / 1M) — models.dev leg still works.
- models.dev outage does not clear windows previously published.
