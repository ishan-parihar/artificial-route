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
| `auto/*` 16-factor + `virtualFactory.ts` | `autoCombo/scoring.ts+engine.ts` | all eight names the reference advertises (`README.md:351-362`): `auto,auto/coding,auto/fast,auto/cheap,auto/offline,auto/smart,auto/lkgp,auto/chaos`; `auto/offline` carries its own `offline-friendly` pack and `auto/lkgp` the explicit spelling of the stickiness every `auto` variant applies; `auto_variant_for_model()` is the pure "is this requested model an alias?" check the server stream calls |
| `simulate_route|explain_route` | `statusDecisionTable|decisionTrace` | both call one `rank()`, so a dry run and its explanation cannot disagree |
| `quota-share-fair` | `quotaShare*` | DRR order then power-of-two over live in-flight; a persisted deficit map is the only gap |
| `fusion` fan-out, `pipeline` chaining | `dispatchPrelude.ts::tryFusionDispatch|tryPipelineDispatch`, `fusion.ts`, `services/pipeline.ts` | `dispatch_fusion()` returns the first 2xx with a full verdict trace; `dispatch_pipeline()` chains stage output into the next request; the judge synthesis the TODO recorded as blocked now ships end to end — `ar-route/src/fusion_judge.rs` + the server's `fusion_response` branch |
| Inbound wire-in: Anthropic Messages, Responses, Ollama | `translator/request/*` | landed: `ar-server/src/translate.rs` dispatches each dialect into `ar-translate` (`anthropic_to_canonical`/`responses_to_canonical`/`ollama_to_canonical`), and `ar-translate/src/outbound.rs` renders canonical back to every provider wire; the OpenAI route alone forwards the client's bytes verbatim |

| Still deferred | Source | Cost |
|---|---|---|
| ~~server-side live discovery~~ **shipped** | `modelDiscovery.ts+reactiveModelSync.ts` | **landed in `6a55836`.** `DiscoveredCatalog` (ar-server/src/models.rs) is a `ModelCatalog` impl — the seam P0's `ModelsCache` doc predicted was the only thing that had to change. The fetch lives in `prefetch` (off the request path) so `load()` is a memory read; `Server::spawn_discovery_refresh` runs a `DISCOVERY_TTL` tick. Discovered cards are **unioned over** the configured ones, so arming it can only add a model. Off unless `AR_MODEL_DISCOVERY` is `1/true/on/yes`. **The drop at `discovery.rs:5-6` is only partially reversed:** the overlay is wired, but the scheduler is a TTL tick, not the reference's recorded 24-hour sweep, so that row stays open on its own terms |
| ~~`quota-share-fair` deficit persistence~~ **shipped** | `quotaShare*` DRR | **landed in `6a55836`.** `DeficitMap` (strategy.rs) carries credit across calls: each round adds `weight/total`, the winner pays 1, so long-run frequency converges on the weight ratio. Process-global and keyed by `execution_key` alone, following `TargetLoads`' own documented choice (`strategy.rs:194-196`): no combo dimension, because `Candidate` has none and a reference deliberately scopes by combo is being traded off against a simpler invariant. Scoped by the route's model as `route()` supplies it. Capped at the reference's own 1000, whole-scope eviction, and `charge` now prunes departed targets so the inner map cannot grow per provider forever |
| the 12-hour soak run itself | `docs/05-roadmap.md` P6 accept line | harness is `scripts/soak.sh` and the 3-minute proof is recorded; a 12h run is **in flight** (launched after `6a55836`, fresh CSV). A prior attempt was killed at 54 min by a `pkill -f` pattern that matched the soak's own server — the partial (54 live samples, RSS 29100→28828KB, −272KB, healthz 200 throughout, 0 failures) is a flat-line datum over a short window, **not** the 12h claim. **Not yet claimed** |
| ~~`ar-obs` has no in-tree consumer~~ **shipped** | this build's own state | the crate ships and is tested, but `ar serve` serves its own four Prometheus counters from `ar-server/src/metrics.rs` and nothing calls `TraceWriter` or `AuditLedger`; the P5 obs half is a library, not a request path |

### Landed since the deferred table was written (2026-10-03)

These rows were deferred when this table was first written and have since landed; they are recorded here so the deferred table above stays true to its own contract.

| Landed | Commit | Evidence |
|---|---|---|
| `expiry-first` | `481bbbb` | the reference's scoring verbatim at combo level; the reference ranks OAuth *connections* inside credential selection, which this build's one-key-per-provider model has no home for — audit-notes row (g) |
| per-panel-member byte cap on `read_body` | pending (this commit) | `PANEL_BODY_BYTES` = 256KB, over-cap member dropped rather than truncated mid-answer, so a panel is bounded at 10MB; pinned three ways, including a product assertion so neither bound can drift alone — audit-notes row (h) |
| fusion judge synthesis | `f9ce2da` | `judge_model:` in config validated at load, the judge directive ported verbatim, the panel's texts composed from the fan-out's own buffer, the judge's body relayed verbatim — audit-notes row (h) |
| `/v1/audio/translations` | `3b3252b` | same handler body as transcriptions (multipart verbatim, `?model=` routing); the modality row of the deferred table is now empty |
| `import --from omniroute\|litellm` | `5cccd13` | both readers exist and both write `config.yaml` + `registry.json`: `from_omniroute` (import/mod.rs:143) parses a models.dev-shaped map, `from_litellm` (:167) a LiteLLM `model_list`; `assemble` (:210) emits one combo per alias group, and `commands.rs:1322` dispatches the two modes |

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
\n## route/ P0 scope <- `open-sse/services/combo/`\n\n* `comboAttemptLoop.ts+executeTargetAttempt/Classify/Gates` -> `attempt_loop()` with `Result<AttemptOutcome, Retry|Failover|Abort>`.\n* P0 `strategyDispatch.ts+targetSorters.ts` subset (4 strategies). Rest per the tables above.\n* `dispatchPrelude.ts::tryFusionDispatch|tryPipelineDispatch` -> `dispatch_fusion()` (concurrent fan-out over the panel, first 2xx, full verdict trace) and `dispatch_pipeline()` (sequential stages, stage N's answer is stage N+1's only turn, final response only, no fallback). `pick` still answers both with the panel leader / first stage: a comparator cannot express a fan-out or a chain.\n* `sessionStickiness/recordLkgpPin/pinRecovery/staleLkgpClear` -> `lkgp_pin: HashMap<Strng, ProviderId>` + TTL.\n* `fallbackPolicy.ts+comboResolver.ts` -> `fallback_chain: Vec<ProviderId>`.\n* Resilience: ONE layer — `chatCore/connectionCooldown.ts` per-key exp backoff + 429 Retry-After. Skip breaker, quota-share, shadow. The cooling state reaches selection through `pick_filtered(..., skip)`, so a cooling key cannot win a `priority` pick and then cost the attempt loop a round trip.\n* Quota: `QuotaStore,accountBuckets,fairShare,burnRate,enforce` -> `governor` buckets + work-conserving lend.\n\n## tokens/ <- counting + accounting\n\n* `shared/utils/tiktokenCounter.ts::countTextTokens` (`MAX_EXACT_TOKEN_COUNT_CHARS=50000`) -> `tiktoken-rs`.\n* `lib/quota/tokenEstimator.ts` -> pre-flight cheap path.\n* `lib/usage/tokenAccounting.ts` -> `NormalizedUsage` 1:1 port.\n* `usageLedger/costCalculator/modelPricingRegistry/budgetGuard` -> ledger + per-key USD caps. Flat-rate `$0` cost, full quota.\n\n## sync/\n\n* `modelsDevSync.ts` (861) + `pricingSync.ts` (LiteLLM) -> build-time fetch -> `registry.json`.\n* `modelDiscovery.ts+reactiveModelSync.ts` -> runtime live discovery P1.\n* `sync/bundle.ts+cloudSync.ts` -> DROP. `tokenRefresh/*+oauth/*` -> DROP P0.\n\n## compress/ <- `open-sse/services/compression/`\n\nPort `caveman+RTK+lite+planResolution(hdr>combo>profile>adaptive>default)+hardBudget+stats`.\nDrop ONNX/omniglyph/quantumLock/worker-pool/14 migrations. Validate with `eval:compression` corpus.\nIntensity is a dial on the 3 engines, never a 4th: `rtk` takes `minimal|standard|aggressive` (\n`effectiveMaxLines` 1.5x/1.0x/0.5x -> run length 4/3/2) and `caveman` takes `lite|full|ultra`\n(`cavemanRules.ts` `INTENSITY_RANK` gates the rule table; `ultra` = + the abbreviation table).\n`lite` has no ladder. Config names engine+level as `Combo.compression` and the level is REJECTED\non a no-ladder engine rather than accepted-and-ignored, which is the reference's defect.\n\n## embeddings/\n\n`lib/embeddings/service.ts::createEmbeddingResponse` (511) + `familyGuard` + `embeddingRegistry` -> `POST /v1/embeddings` via same `exec`. Vector side P1 (see 04).\n\n## cli/ <- `bin/`\n\n`omniroute.mjs` (377) + `program.mjs` (40) + `commands/registry.mjs` pattern -> `clap` derive.\nP0 `serve|models|providers|combo|doctor|run|configure` (AXI gates in `06-axi-mcp.md`). Rest P2.\n\n## cache/\n\n`cacheLayer.ts` LRU -> `ar-cache` `quick_cache` 32MB (see 04). `semanticCache.ts` two-tier -> P1.\n`promptCache/` affinity -> prefix-pin. `idempotencyLayer.ts` -> `Idempotency-Key` 24h.\n\n## mcp/ <- `open-sse/mcp-server/` (control-plane, not data-plane)\n\n110 tools: essential-8 + advanced-8 + cache/compression 13 + CCR 6 + discovery/web + skills/catalog + proxy/pricing + combo/routing.\nP1 `ar-mcp --features mcp` essential-8 only, wrapping same `ar-route|ar-tokens|ar-obs` fns. Full catalog in `06-axi-mcp.md`.\n