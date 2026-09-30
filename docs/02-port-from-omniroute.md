# 02 — Port as reference from OmniRoute

Base: `../OmniRoute/`. Do NOT copy TS. Re-implement logic in Rust, keep constants.

## registry/ <- `open-sse/config/`

| Source | Rust target | Notes |
|---|---|---|
| `providers/registry/<256 dirs>/` + `providerRegistry.ts` (`getRegistryEntry/getRegisteredProviders/getProviderCategory/getUnsupportedParams`) | `ar-registry: HashMap<Strng, ProviderDef>` via `include_str!(registry.json)` | Port data, drop accessors you don't call. Build-time fetch, not runtime. |
| `providers/shared.ts`, `alternateFormats.ts` | `ProviderDef{base_url, auth: OAuth\|ApiKey\|NoAuth, wire: OpenAI\|Anthropic\|Gemini, unsupported_params}` | — |
| `src/shared/constants/providers.ts` + `providers/{oauth,noauth,local,search,web-cookie,audio,cloud-agent}.ts` | `auth_class` enum | P0: `ApiKey` only. Drop web-cookie/OAuth. |
| `providerModels.ts`, `connectionBillingCatalog.ts`, `ollamaModels.ts`, `freeModelCatalog*.ts`, `vertexModels.ts` | `registry.json` models section | — |
| `credentialLoader.ts::loadProviderCredentials` | `ar-config` env inject `$OPENAI_API_KEY` via `shellexpand` | — |

## translate/ <- `open-sse/translator/`

`registry.ts/bootstrap.ts(initTranslators)/formats.ts/paramSupport.ts`,
`request/` inbound wire -> canonical, `response/` canonical -> outbound,
`src/lib/translator/streamTransform.ts` SSE.
Highest leverage. Support P0: OpenAI chat inbound single adapter. P1: Anthropic Messages, Ollama, Responses.
Reuse `ar-llm/conversion/*` where overlap — don't write two converters.

## exec/ <- `open-sse/executors/`

`default.ts+default/{urlNormalizers,poolConfig}+base/{headers,mergeAbortSignals}+defaultResolver.ts::getDefaultExecutor+credential.ts`.
Port `default` only. Skip 158 special executors (bedrock SigV4, vertex, claude-web browser, codex OAuth) until P5.

## Deferred routing parity (P2, not dropped)

P0 `priority|round-robin|cost-optimized|lkgp` = lean-routing (~80% traffic). What landed, and what is still open:

| Landed in `ar-route` | Source | Notes |
|---|---|---|
| `weighted|fill-first|p2c|least-used|random|strict-random` | `combo/targetSorters.ts` | seeded, so a decision is reproducible from its inputs |
| `headroom|reset-window|reset-aware|quota-weighted` | `quotaStrategies|headroomRanking` | no clock; the rollover is the quota store's job, upstream of the sorter |
| `context-relay|context-optimized|cache-optimized` | `promptCacheAffinity|comboContextCache` | stateless HRW prefix pin, scoped by the requested model |
| `auto/*` 16-factor + `virtualFactory.ts` | `autoCombo/scoring.ts+engine.ts` | `auto,auto/coding,auto/fast,auto/cheap,auto/smart,auto/chaos`; `auto_variant_for_model()` is the pure "is this requested model an alias?" check the server stream calls |
| `simulate_route|explain_route` | `statusDecisionTable|decisionTrace` | both call one `rank()`, so a dry run and its explanation cannot disagree |
| `quota-share-fair` | `quotaShare*` | DRR order then power-of-two over live in-flight; a persisted deficit map is the only gap |
| `fusion` fan-out, `pipeline` chaining | `dispatchPrelude.ts::tryFusionDispatch|tryPipelineDispatch`, `fusion.ts`, `services/pipeline.ts` | `dispatch_fusion()` returns the first 2xx with a full verdict trace; `dispatch_pipeline()` chains stage output into the next request. The fusion *judge* is still open — see below. |

| Still deferred | Source | Cost |
|---|---|---|
| `expiry-first` | `ACCOUNT_FALLBACK_STRATEGY_VALUES` | account-scoped ordering over key expiry, not quota windows; needs a key-expiry signal `Candidate` does not carry |
| fusion judge synthesis | `fusion.ts::handleFusionChat` + `judgeModel` | a second dispatch over a composed prompt, needing a body composer (panel answers are a `ChunkStream`) and a place to name the judge model — `TODO(#P2-fusion-judge)`. Returning one panel answer is honest; faking a synthesis is not. |
| Wire in: Anthropic Messages, Responses, Ollama | `translator/request/*` | one adapter each, P2 |
| Modality: vision/audio/video, `/v1/ocr`, `/v1/audio/translations`, image-gen | executors media family | P6, separate adapters |

### Strategy-name notes

* **Renamed, with the reference spelling kept as an alias.** The variant is
  `Strategy::QuotaShareFair`, spelled `quota-share-fair`; `parse` also accepts
  `"quota-share"`, the reference's *internal* spelling
  (`INTERNAL_ROUTING_STRATEGY_VALUES`, used by the auto-minted `qtSd/` combos
  and deliberately kept out of the user-facing list). Both spellings dispatch —
  a config carrying the internal one used to fall through to
  `Deferred("unknown")` and a 501 that said "unknown".
* **`quota-weighted` is its own strategy, not a synonym.** It is the only member
  of `ROUTING_STRATEGY_VALUES` with no counterpart here until now, and its
  reference implementation (`orderTargetsByQuotaWeighted`) filters to
  `remainingPercent > 0` — which drops every unmonitored pool and returns an
  *empty list* for a fleet that reports no quota at all. The port keeps an
  unknown quota as a last-place tier: ordered, never removed.
* **A name this build does not carry is reported by its own name.**
  `expiry-first` parses to `Deferred("expiry-first")` rather than falling through
  to `Deferred("unknown")`, so the 501 quotes the config line the operator wrote.
  Same for `auto/*`. A genuine typo is still `Deferred("unknown")`.
\n## route/ P0 scope <- `open-sse/services/combo/`\n\n* `comboAttemptLoop.ts+executeTargetAttempt/Classify/Gates` -> `attempt_loop()` with `Result<AttemptOutcome, Retry|Failover|Abort>`.\n* P0 `strategyDispatch.ts+targetSorters.ts` subset (4 strategies). Rest per the tables above.\n* `dispatchPrelude.ts::tryFusionDispatch|tryPipelineDispatch` -> `dispatch_fusion()` (concurrent fan-out over the panel, first 2xx, full verdict trace) and `dispatch_pipeline()` (sequential stages, stage N's answer is stage N+1's only turn, final response only, no fallback). `pick` still answers both with the panel leader / first stage: a comparator cannot express a fan-out or a chain.\n* `sessionStickiness/recordLkgpPin/pinRecovery/staleLkgpClear` -> `lkgp_pin: HashMap<Strng, ProviderId>` + TTL.\n* `fallbackPolicy.ts+comboResolver.ts` -> `fallback_chain: Vec<ProviderId>`.\n* Resilience: ONE layer — `chatCore/connectionCooldown.ts` per-key exp backoff + 429 Retry-After. Skip breaker, quota-share, shadow. The cooling state reaches selection through `pick_filtered(..., skip)`, so a cooling key cannot win a `priority` pick and then cost the attempt loop a round trip.\n* Quota: `QuotaStore,accountBuckets,fairShare,burnRate,enforce` -> `governor` buckets + work-conserving lend.\n\n## tokens/ <- counting + accounting\n\n* `shared/utils/tiktokenCounter.ts::countTextTokens` (`MAX_EXACT_TOKEN_COUNT_CHARS=50000`) -> `tiktoken-rs`.\n* `lib/quota/tokenEstimator.ts` -> pre-flight cheap path.\n* `lib/usage/tokenAccounting.ts` -> `NormalizedUsage` 1:1 port.\n* `usageLedger/costCalculator/modelPricingRegistry/budgetGuard` -> ledger + per-key USD caps. Flat-rate `$0` cost, full quota.\n\n## sync/\n\n* `modelsDevSync.ts` (861) + `pricingSync.ts` (LiteLLM) -> build-time fetch -> `registry.json`.\n* `modelDiscovery.ts+reactiveModelSync.ts` -> runtime live discovery P1.\n* `sync/bundle.ts+cloudSync.ts` -> DROP. `tokenRefresh/*+oauth/*` -> DROP P0.\n\n## compress/ <- `open-sse/services/compression/`\n\nPort `caveman+RTK+lite+planResolution(hdr>combo>profile>adaptive>default)+hardBudget+stats`.\nDrop ONNX/omniglyph/quantumLock/worker-pool/14 migrations. Validate with `eval:compression` corpus.\n\n## embeddings/\n\n`lib/embeddings/service.ts::createEmbeddingResponse` (511) + `familyGuard` + `embeddingRegistry` -> `POST /v1/embeddings` via same `exec`. Vector side P1 (see 04).\n\n## cli/ <- `bin/`\n\n`omniroute.mjs` (377) + `program.mjs` (40) + `commands/registry.mjs` pattern -> `clap` derive.\nP0 `serve|models|providers|combo|doctor|run|configure` (AXI gates in `06-axi-mcp.md`). Rest P2.\n\n## cache/\n\n`cacheLayer.ts` LRU -> `ar-cache` `quick_cache` 32MB (see 04). `semanticCache.ts` two-tier -> P1.\n`promptCache/` affinity -> prefix-pin. `idempotencyLayer.ts` -> `Idempotency-Key` 24h.\n\n## mcp/ <- `open-sse/mcp-server/` (control-plane, not data-plane)\n\n110 tools: essential-8 + advanced-8 + cache/compression 13 + CCR 6 + discovery/web + skills/catalog + proxy/pricing + combo/routing.\nP1 `ar-mcp --features mcp` essential-8 only, wrapping same `ar-route|ar-tokens|ar-obs` fns. Full catalog in `06-axi-mcp.md`.\n