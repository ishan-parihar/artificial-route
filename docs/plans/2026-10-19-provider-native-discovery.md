# Provider-native model discovery (priority over models.dev)

Status: **implemented + deployed 2026-10-19** (commit `cc264ae`). Operator ruling: /v1/models windows must come from the providers themselves first, models.dev second, the 128K floor last.

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

## Acceptance — verified live on the VPS (2026-10-19)

- `beam-free` 128000 → **1048576**, `nemotron-3.5-super-vl` 128000 → **1000000**, `laguna-s-2-1` 128000 → **262144** — all from kilocode's native catalog.
- `glm-5.3-flash` / `step-5-preview` / `deepseek-v4-1-flash` unchanged (1M / 1048576 / 1M via models.dev — nvidia's native `/v1/models` is a bare list, so models.dev remains the correct source there).
- Implementation notes beyond the design: the alias fix became a catalog-scoped model-match fallback in `published_window` (models.dev's `kilo` key is found by asking which view entry names the model, not by alias tables — providerMeta aliases were the wrong source); native fetches ride the provider row's first credential and are skipped for OAuth/custom-wire rows; failures log at debug and never clear published windows.
