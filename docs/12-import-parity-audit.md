# 12 — Import parity audit: OmniRoute "visible and enabled" vs `aroute import`

Read-only investigation. Every claim below carries a `file:line` into
`../OmniRoute` (TypeScript) or this repo (Rust). Nothing here is implemented.

The question this doc answers: **OmniRoute shows a curated subset of its static
registry on `/v1/models`. What does `aroute import` reproduce, and what does it drop?**

Headline: OmniRoute has **no per-provider or per-model `enabled` field anywhere in
its provider tree**. Visibility is entirely a *runtime* concept layered on top of a
static registry that is uniformly "enabled". `aroute import` reads only the static
registry — so it reproduces the catalog faithfully and reproduces **none** of the
six runtime filters that decide what a user actually sees.

---

## 1. Enabled / disabled: where is the flag?

### 1.1 The provider tree has no visibility field

`RegistryEntry` (`open-sse/config/providers/shared.ts:133-170`) declares `id`,
`alias`, `format`, `executor`, `baseUrl`/`baseUrls`, `testKeyBaseUrl`,
`testKeyModelsUrl`, `responsesBaseUrl`, `reasoningTransport`,
`requiresReasoningContentEcho`, `messagesUrl`, `urlSuffix`, `urlBuilder`,
`authType`, `authHeader`, `authPrefix`, `headers`, `extraHeaders`,
`requestDefaults`, `oauth`, `models`. **No `enabled`, no `disabled`, no
`visible`, no `hidden`, no `free`.**

`RegistryModel` (`shared.ts:47-105`) declares `id`, `name`, `aliases`,
`liveCatalogIds`, `toolCalling`, `supportsReasoning`, `alwaysReasons`,
`supportedThinkingEfforts`, `supportsVision`, `supportsAudio`, `supportsVideo`,
`supportsXHighEffort`, `maxOutputTokens`, `targetFormat`, `strip`,
`unsupportedParams`, `contextLength`, `maxInputTokens`, `interleavedField`,
`timeoutMs`, `scoresAs`. **No visibility flag either.**

A literal search for `\benabled\b` across the whole provider config tree
(`open-sse/config/providers/**`) returns **exactly one hit**, and it is a prose
comment, not a field — `registry/moonshot/index.ts:4`:
`// currently accepts only reasoning_effort="max" while reasoning is enabled.`

So: **every provider and model in `REGISTRY` is enabled by construction.**

### 1.2 What replaces the flag: five runtime switches

| Switch | Type | Default | Declared at |
|---|---|---|---|
| `settings.blockedProviders` | `string[]` | absent ⇒ empty set | `src/lib/db/settings.ts` — not present in defaults; read at `src/app/api/v1/models/catalog.ts:344` |
| `settings.hidePaidModels` | `boolean` | `false` | `src/lib/db/settings.ts:253` |
| `settings.hideAutoCombos` | `boolean` | `false` | `src/lib/db/settings.ts:261` |
| `settings.hideNoThinkVariants` | `boolean` | `false` | `src/lib/db/settings.ts:264` |
| `settings.modelVisibilityAllowlist` / `modelVisibilityDenylist` | `string[]` | `[]` / `[]` | `src/lib/db/settings.ts:271-272` |

Plus one derived-from-row filter: `provider_connections.is_active`
(`src/lib/db/core.ts:227`, `INTEGER DEFAULT 1`), consumed at
`catalog.ts:382` (`connections.filter((c) => c.is_active !== false)`).

The blocked-provider set is normalised by
`normalizeBlockedProviderSet` (`src/shared/utils/noAuthProviders.ts:8-17`) and
matched by id, by `-search` suffix-stripped id, and by alias
(`isProviderBlockedByIdOrAlias`, `noAuthProviders.ts:19-31`).

### 1.3 Counts today

Static registry — `open-sse/config/providers/index.ts:278` declares
`export const REGISTRY: Record<string, RegistryEntry>`, with **276 entries**
(counted over the file's top-level `key: value` rows). 274 of them have their own
`registry/*/index.ts` file; the rest are composed. Counting `id:`/`"…"` rows
inside `models:` arrays across `registry/**/index.ts` gives **≈1569 static model
rows** (the exact figure is approximate — the arrays are built by spreads,
`buildModels([...])`, and id arrays, which a static scan over-counts slightly).

Runtime state — the live database on this machine
(`~/.omniroute/storage.sqlite`, read-only) has:

```
provider_connections total: 0
combos:                     0
key_value rows for blockedProviders / hidePaidModels / hideAutoCombos /
  hideNoThinkVariants / modelVisibilityAllowlist /
  modelVisibilityDenylist / autoRoutingEnabled:  all absent
```

So **today: 276 providers and ~1569 models enabled, 0 disabled, 0 connections,
0 combos** on this install. The point stands regardless of the current numbers —
*disabled* is an operator-time action recorded in SQLite, not a property of the
provider tree.

---

## 2. The model-list endpoint

### 2.1 Route

`src/app/api/v1/models/route.ts:35-43` — `GET /v1/models` calls
`getUnifiedModelsResponse(request, {}, { scheduleBackgroundRefresh: (task) => after(task) })`
(`catalog.ts:196`). `HEAD` is registered explicitly (`route.ts:25-30`) purely as a
health probe so it does not stream the full catalog.

### 2.2 What it returns

Not "all registry models". The build is an ordered pipeline:

**(a) Auth gate first, uncached.** `catalog.ts:213-217` calls
`getModelCatalogAuthRejection` (`src/app/api/v1/models/catalogRequest.ts:46`)
before any cache lookup, because auth state is per-request and must not be
memoized.

**(b) Active connections.** `catalog.ts:379-382`:
```ts
connections = (await getCachedRawProviderConnections()).map(createLazyConnectionView);
connections = connections.filter((c) => c.is_active !== false)  // i.e. is_active !== false
```
(the source field is `isActive` on the mapped view; the column is `is_active`).

**(c) `activeAliases` — the hard provider gate.** `catalog.ts:486-499`: every
active connection contributes both its provider id and its alias; then every
`NOAUTH_PROVIDERS` entry is added **unless** blocked:
```ts
for (const p of Object.values(NOAUTH_PROVIDERS)) {
  if (isNoAuthProviderBlocked(blockedProviders, p.id, "alias" in p ? p.alias : null)) continue;
  activeAliases.add(p.id);
  if ("alias" in p && typeof p.alias === "string") activeAliases.add(p.alias);
}
```
The static-registry loop then drops any provider with no active alias —
`catalog.ts:1057-1059`:
```ts
if (!activeAliases.has(alias) && !activeAliases.has(canonicalProviderId)) {
  continue;
}
```
**This is the single most important line for parity.** A provider with zero
connections contributes zero models. OmniRoute with an empty database (exactly
this install) would serve an essentially empty catalog for keyed providers.

**(d) Per-model filters**, applied in the static loop at `catalog.ts:1095-1100`:
```ts
if (isModelHiddenBulk(alias, model.id, canonicalProviderId)) continue;
if (isExcludedByProviderConnections(canonicalProviderId, model.id)) continue;
if (shouldHidePaid(canonicalProviderId, model.id, (model as { pricing?: unknown }).pricing)) continue;
if (shouldHideByExposure(canonicalProviderId, model.id)) continue;
```
- `isModelHiddenBulk` (`catalog.ts:444-463`) reads the operator's hidden set from
  `key_value` namespaces `modelCompatOverrides` and `customModels`
  (`src/lib/db/models.ts:986-993`), per modality, checking four keys
  (providerKey, canonical id, alias, node prefix).
- `isExcludedByProviderConnections` (`catalog.ts:532-538`): hidden when the
  provider **has** connections but **none** is eligible — eligibility is
  `hasEligibleConnectionForModel` (`src/domain/connectionModelRules.ts:73-82`),
  i.e. no connection's `providerSpecificData.excludedModels` glob matches
  (`connectionModelRules.ts:53-71`, `wildcardMatch`).
- `shouldHidePaid` → `decideHidePaid` (`src/app/api/v1/models/catalogPaidFilter.ts:10-34`):
  a model survives only if `isFreeForProvider` says so. Default off.
- `shouldHideByExposure` → `isModelExposureAllowed`
  (`src/shared/utils/modelExposureList.ts:60-73`): denylist wins over allowlist,
  both support `*`/`?` globs, matched against `[modelId, "provider/model"]`.

**(e) Lifecycle.** `catalogModelPolicy.ts:9-18` composes
`isModelSelectable` (`open-sse/services/modelLifecycle.ts:287-304`) — deprecated
and shutdown models are dropped by default — with
`getModelEndpointDecision(...) !== "provider-policy"`.

**(f) `auto/*` virtual combos.** Suppressed entirely when
`settings.hideAutoCombos === true || settings.autoRoutingEnabled === false`
(`catalog.ts:359`), gated again at `catalog.ts:876-877`
(`if (hideAuto || autoCombosDisallowedForKey) break; if (blockedProviders.has("auto") || …) continue;`).

**(g) Post-filters and finalisation.** `applyCatalogPostFilters`
(`src/app/api/v1/models/catalogResponse.ts:90-…`) appends Claude effort variants
(`:125`), no-thinking variants (`:145`), and cc-discovery mirror aliases, each
re-filtered for authorisation; `?configuredOnly=true` (`catalogResponse.ts:107-112`)
restricts to `hasEligibleConnectionForModel`. `finalizeCatalogResponse`
(`catalogResponse.ts:269-…`) drops functional-gateway mirrors an API key is not
entitled to, then enriches.

### 2.3 Direct answers

- **All registry models?** No.
- **Only enabled ones?** There is no enabled bit; the equivalent is
  "provider has ≥1 active connection **and** is not in `blockedProviders`".
- **Only free ones?** Only if `hidePaidModels` is on. Default off
  (`settings.ts:253`), and the filter is deliberately scoped to the
  `PROVIDER_MODELS` + OpenRouter loops only — modality registries, combos,
  `auto/*`, synced/custom/alias-backed rows stay unfiltered (the comment at
  `catalog.ts:345-350` says so explicitly).
- **Provider health?** Partly. Health-check exclusions
  (`provider_specific_data.excludedModels`) are mirrored into the catalog at
  `catalog.ts:528-538`. Runtime `test_status` / `last_error` / `rate_limited_until`
  columns exist in the schema (`core.ts:234-241`) but are **not** consulted by the
  catalog builder — they gate dispatch, not advertisement.
- **Quota?** No quota filter in the catalog path.
- **Env key presence?** No. Credentials live in SQLite
  (`provider_connections.api_key` / `access_token`, `core.ts:228`, `:245`), not env.
  The env vars in the registry tree (`authType: "apikey"`) are only hints about
  *which header* carries a key.

---

## 3. Combo and alias ordering

### 3.1 Combo ordering — two independent rules

**Storage order.** `combos` table (`src/lib/db/core.ts:291-298`) carries
`sort_order INTEGER NOT NULL DEFAULT 0`. The read is ordered —
`src/lib/db/repositories/sqliteComboRepository.ts:122`:
```sql
SELECT id, data, sort_order, context_cache_protection FROM combos
ORDER BY sort_order ASC, name COLLATE NOCASE ASC
```
`sqlliteComboRepository.ts:29-32` reads the column back; `:34-40` writes it into
the JSON `sortOrder`. So: **`sort_order` ascending, ties broken by case-insensitive
name.** That is dashboard drag-and-drop order (`CHANGELOG.md:11182`: "Drag combo
cards by handle to reorder them in the dashboard").

**Catalog position.** `src/app/api/v1/models/catalogOrder.ts:17-21` states the
rule and `:56-77` implements it:
```
Order: combo block (owned_by === "combo") pinned first, preserving #4164; then
providers in registry precedence (OAUTH -> NOAUTH -> APIKEY canonical keys); then
unknown providers by locale-independent code-unit order. Within a group the input
order is preserved (stable) …
```
Mechanics: `modelGroupKey` (`:42-50`) returns `COMBO_GROUP` for `owned_by === "combo"`,
else `owned_by`, else the id prefix. `groupSortPriority`
(`src/shared/constants/canonicalProviderOrder.ts:44-48`) returns **-1** for the
combo group, a rank from `CANONICAL_PROVIDER_ORDER`
(`canonicalProviderOrder.ts:6-10` = `Object.keys(OAUTH_PROVIDERS)` then
`NOAUTH_PROVIDERS` then `APIKEY_PROVIDERS`), and `Infinity` for unknowns.
`sortCatalogModelsProviderGrouped` (`:56-77`) sorts by priority, breaks
unknown-vs-unknown with a UTF-16 code-unit comparator (`:33-35`), then falls
through to `a.index - b.index` — a stable sort preserving combo `sort_order`.

A combo with every target hidden is dropped (`catalog.ts:963-967`).

### 3.2 Which alias wins

`buildAliasMaps` (`src/app/api/v1/models/catalogProviderMaps.ts:20-72`) is the
whole resolution. First-wins, in four passes:
1. `Object.values(AI_PROVIDERS)` (`:25-34`) — `providerIdToAlias[providerId]` is
   set only `if (!providerIdToAlias[providerId])`, so the **first** section to
   claim an id wins. `AI_PROVIDERS` is a `Proxy` over the union of the provider
   sections (`src/shared/constants/providers.ts:394-412`).
2. `PROVIDER_ID_TO_ALIAS` both directions (`:36-49`), again `||`-guarded.
3. Any remaining `PROVIDER_MODELS` key maps to itself (`:51-55`).
4. A hardcoded `FALLBACK_ALIAS_TO_PROVIDER` safety net (`:8-18`): `ag→antigravity`,
   `cc→claude`, `cl→cline`, `cu→cursor`, `cx→codex`, `gh→github`,
   `kc→kilocode`, `kmc→kimi-coding`, `kr→kiro`.

Lookups go through `resolveCanonicalProviderId` (`:89-98`): map first, then the
supplied fallback, then the hardcoded table.

### 3.3 Id prefixing

`MODELS_CATALOG_PREFIX_MODE` (`.env.example:2137-2145`) defaults to `dual` —
each model is advertised under **both** `alias/model` and `providerId/model`,
roughly doubling the catalog. Overridable per request with `?prefix=alias|canonical|dual`
(`catalog.ts:335-339`). Optional `display_name` / `description` on cards is gated
by `MODEL_CATALOG_INCLUDE_NAMES` (default true, `.env.example:2203-2209`).

---

## 4. API keys / how OmniRoute decides a provider is usable

Credentials live in **`provider_connections`**, not env and not a
`providers.json`. Schema at `src/lib/db/core.ts:220-265`: `auth_type`,
`access_token`, `refresh_token`, `expires_at`, `api_key` (`:245`),
`id_token`, `provider_specific_data` (`:247`), `is_active` (`:227`),
`priority` (`:226`), plus health/quota columns.

The resolution path is `getProviderCredentials`
(`src/sse/services/auth.ts:1170-…`):

1. Retired providers bail first — `:1177-1187`
   (`isMicrosoftDesignerWebRetiredProviderId`, `isRuntimeRetiredProviderId`).
2. **No-auth providers get synthetic credentials, but only if not blocked** —
   `:1198-1204`:
```ts
const resolvedId = resolveProviderId(provider);
const providerMaps = [ NOAUTH_PROVIDERS, WEB_COOKIE_PROVIDERS ];
if (providerMaps.some((map) => map[resolvedId]?.noAuth)) {
  if (await isNoAuthProviderBlockedBySettings(resolvedId)) return null;
```
   `isNoAuthProviderBlockedBySettings` is at
   `src/sse/services/noAuthProviderSettings.ts:5-8`, delegating to
   `isProviderBlockedByIdOrAlias(providerId, settings.blockedProviders)`.
3. Otherwise the flow selects among DB rows by strategy, filtered by
   `is_active` and cooldown state (`rate_limited_until`, `backoff_level`).

A related per-provider switch exists — `noAuthFallbackDisabledProviders`
(`CHANGELOG.md:2277`) — which disables the synthetic anonymous credential
fallback for API-key providers declaring `anonymousFallback: true`.

There is **no** `providers.json` in the picture, and **no** env-var presence check.
The env var names in `.env.example` are for the *server's own* configuration
(dashboard auth, sync intervals, feature flags), not per-provider upstreams.

---

## 5. Gap vs artificial-route

### 5.1 What `aroute import` does today

Entry point: `crates/ar-cli/src/commands.rs:1321-1325`
(`import_config` → `import::omniroute::scan(import::upstream_tree(path))`;
`upstream_tree` is `crates/ar-cli/src/import/mod.rs:437-441`). It writes four
files unconditionally: `config.yaml`, `registry.json` (`commands.rs:1339-1343`),
`freeBudgets.json` (`:1349-1352`), `providerMeta.json` (`:1359-…`).

`scan` (`crates/ar-cli/src/import/omniroute.rs:55-…`):
- `read_tree` (`:56`), `pricing` (`:57`, `find_up` to
  `src/shared/constants/pricing`), `flat_rate_ids` (`:58`, unions
  `FLAT_RATE_PROVIDERS` / `SUBSCRIPTION_PROVIDERS` / `WEB_COOKIE_PROVIDERS` sets —
  `:1388`, `:1395`, `:1436`), `free_budgets` (`:59`).
- Per entry: `to_def` (`:70`) and `to_meta` (`:77`); flat-rate is matched against
  id **or** alias (`:78-79`); prices merged by exact id (`:81-85`).
- A redeclaration with a different base URL is **kept-first, noted** (`:86-92`);
  a provider with no resolvable base URL is **listed but noted unroutable**
  (`:95-99`). Both notes go to stderr (`:112-114`). That is a real, deliberate
  design: a gap is visible, never silent.

`combos` (`omniroute.rs:1650-1673`) folds the catalog into **one combo per
`provider/model`**, `Strategy::Priority`, single target, empty `pool`. The
comment at `:1659-1661` states the reason: *"it folds a provider catalog, not
OmniRoute's combo files, so it has no candidate pool to read and must not
fabricate one."*

`render_yaml` (`crates/ar-cli/src/import/mod.rs:327-358`) hand-assembles the
YAML so output is byte-stable.

Output shape: `crates/ar-registry/src/registry.json` — **276 providers, 1559
models**, one-for-one with OmniRoute's `REGISTRY`. `ProviderDef`
(`crates/ar-registry/src/lib.rs:323-…`) carries `base_url`, `wire_format`,
`auth`, `env_hint`, `models`, `prices`, `executor`, `auth_kind`, `flat_rate`,
`headers`. `ProviderMeta` (`crates/ar-registry/src/meta.rs:61-…`) adds `alias`,
`auth_header`, context ceiling, alternate formats, anonymous-key flag.

### 5.2 What artificial-route serves

`GET /v1/models` — `crates/ar-server/src/routes.rs:2595-2613`, cards from
`card_json` (`:2558-2577`, `owned_by` = provider, `created` = `ar_registry::BUILT_AT`).
The card set is `Config::model_cards` / `model_cards_with`
(`crates/ar-server/src/config.rs:871`, `:883`), which filters
`.filter(|p| p.is_dispatchable() && !p.upstream_model.is_empty())` (`:888`).
`is_dispatchable` (`crates/ar-server/src/exec.rs:326-350`) is a **build
capability** test — is there an outbound renderer for this wire format
(`ar_exec::outbound_wire`, `:327`), is it anonymous (`:335`), is the OAuth session
usable and is there a matching executor (`:338-349`).

### 5.3 Point-by-point

| OmniRoute behaviour | `aroute import` / `aroute serve` | Evidence |
|---|---|---|
| Static provider catalog (276 providers, ~1569 models) | **Reproduced** | `omniroute.rs:55-116`; `registry.json` = 276/1559 |
| Per-model static metadata (context, caps, executor, auth header) | **Reproduced** | `to_def` `:766`, `to_meta` `:816`, `model_fields` `:884` |
| Prices | **Reproduced** | `pricing` `:1234`, `price_rows` `:1321` |
| Flat-rate / subscription / web-cookie classification | **Reproduced** | `flat_rate_ids` `:1388-1474` |
| Free-tier budgets | **Reproduced** | `free_budgets` `:1497` |
| Short alias (`cc` → `claude`) | **Reproduced** (recorded) | `ProviderMeta::alias`, `meta.rs:69` |
| Combination constructor `buildOpenAiCompatibleRegistryEntry({...})` defaults | **Reproduced** | module doc `omniroute.rs:13-16` |
| Env-key-presence gating | **N/A** — OmniRoute has none | §2.3 |
| `blockedProviders` (id + `-search` + alias match) | **Dropped** | OmniRoute `catalog.ts:344`, `noAuthProviders.ts:19-31` |
| `is_active` connection gate (`activeAliases`) | **Dropped** | OmniRoute `catalog.ts:1057-1059`; no `provider_connections` table in ar |
| Per-connection `excludedModels` glob denylist | **Dropped** | OmniRoute `connectionModelRules.ts:53-82`; no analogue in ar crates |
| `hidePaidModels` free-tier filter | **Dropped** | OmniRoute `catalogPaidFilter.ts:10-34`, default `settings.ts:253` |
| `modelVisibilityAllowlist` / `Denylist` | **Dropped** | OmniRoute `modelExposureList.ts:60-73`, default `settings.ts:271-272` |
| Hidden models from `customModels` / `modelCompatOverrides` | **Dropped** | OmniRoute `models.ts:986-993`, `catalog.ts:444-463` |
| Lifecycle (deprecated / shutdown) | **Dropped** | OmniRoute `modelLifecycle.ts:287-304` |
| Combo rows (`combos` table) | **Dropped** — synthesised 1:1 instead | `omniroute.rs:1650-1673` |
| `sort_order` combo ordering | **N/A** — no combos imported | `sqliteComboRepository.ts:122` |
| Combo block pinned first in the listing | **Dropped** — no sorting at all | `catalogOrder.ts:56-77` |
| `dual` alias/canonical prefixing | **Dropped** — one `provider/model` id only | OmniRoute `catalog.ts:335-339`, `.env.example:2137` |
| `auto/*` synthetic combos | **Dropped** (ar has its own `Strategy`) | OmniRoute `catalog.ts:359`, `:876-877` |

A literal search for `enabled|disabled|blocked|hidden|excluded` across
`crates/**/*.rs` returns **no field, table, or predicate** — only unrelated prose
in `ar-cache` and one test comment at `omniroute.rs:1928`. artificial-route has
no notion of an operator blocking a provider or hiding a model, and by design no
persisted connection store to carry one.

---

## Import parity gap list

Ordered by how much user-visible behaviour each one changes. Each names the exact
place the code must change.

### G1 — Provider-level blocking (`blockedProviders`)
**Missing.** OmniRoute drops a provider from the catalog by id, by
`-search`-stripped id, or by alias (`noAuthProviders.ts:19-31`), applied at
`catalog.ts:1049-1054` and at five other loop sites (`:877`, `:1209`, `:1375`,
`:1460-1465`, `:1852-1856`, `:1943`).
**Needs:** a `blocked_providers: Vec<String>` setting on `ar_config::Config`
(`crates/ar-config/src/lib.rs`), a matcher mirroring `isProviderBlockedByIdOrAlias`
(next to `ProviderMeta::alias`, `crates/ar-registry/src/meta.rs:69`), and a
filter at the head of `Config::model_cards_with` (`crates/ar-server/src/config.rs:883`).
**Importer:** `crates/ar-cli/src/import/mod.rs:327-358` must not emit a blocked
provider into `config.yaml` — or better, must emit it and let the filter stand,
matching OmniRoute (a blocked provider is listed in the tree, absent from the API).

### G2 — Per-model visibility allow/deny
**Missing.** `isModelExposureAllowed` (`modelExposureList.ts:60-73`): denylist
beats allowlist, `*`/`?` globs, matched against `[modelId, "provider/model"]`.
**Needs:** two `String` lists on `ar_config::Config`, a glob matcher (**does not
exist yet** — a literal search for `glob_to_regex|wildcard_match` across
`crates/**/*.rs` returns nothing; port `src/shared/utils/globPattern.ts`), and a
filter inside `model_cards_with` (`config.rs:883`) plus the combo-card branch
that follows it.

### G3 — Free/paid filter (`hidePaidModels`)
**Missing.** `decideHidePaid` (`catalogPaidFilter.ts:10-34`) keeps a model when
`isFreeForProvider` says it is free. artificial-route *does* already import the
free-tier budget table (`freeBudgets.json`, `omniroute.rs:1497`) — so the data is
present and only the filter is absent.
**Needs:** a boolean on `ar_config::Config`; a predicate over
`FreeBudgets` in `crates/ar-server/src/config.rs:883`. Note OmniRoute's own
scope caveat (`catalog.ts:345-350`): modality registries and combos are exempt.

### G4 — Model lifecycle (deprecated / shutdown)
**Missing.** `isModelSelectable` (`modelLifecycle.ts:287-304`) drops deprecated
and shutdown ids, reading records from
`open-sse/services/modelLifecycle.ts` plus
`config/quality/model-lifecycle.json`.
**Needs:** a second generated JSON beside `freeBudgets.json`, written by
`commands.rs:1349-1352`, and a filter in `model_cards_with`.

### G5 — Combo import (`combos` table)
**Missing.** `aroute import` synthesises one one-target combo per `provider/model`
(`omniroute.rs:1650-1673`) instead of reading OmniRoute's real combos. The
deliberate reason is recorded in-code (`:1659-1661`) and is sound — but the
consequence is that an OmniRoute operator's hand-built failover chains,
`pool` benches, weights and per-combo `context_length` are simply not reproduced.
**Needs:** a `--from omniroute --combos <OmniRoute/storage.sqlite>` option, a
read of `combos.data` + `sort_order` (`sqliteComboRepository.ts:122`), and a
replacement for `combos()` that emits real rows instead of the fold.
**Also:** the LiteLLM path *does* build alias-failover combos (`mod.rs:209-212`,
`targets: BTreeMap<String, BTreeSet<String>>`) — so `aroute import --from litellm`
already has the multi-target shape the OmniRoute path lacks. Worth unifying.

### G6 — Catalog ordering
**Missing.** OmniRoute pins the combo block first, then orders providers by
registry precedence `OAUTH → NOAUTH → APIKEY` (`catalogOrder.ts:56-77`,
`canonicalProviderOrder.ts:6-10`, `:44-48`); artificial-route emits
`config.yaml` in `BTreeMap` order (`mod.rs:341-345`) and never sorts the card
list. The practical effect is cosmetic (picker grouping), but a client that
assumes the first card is the default will differ.
**Needs:** a stable provider-grouped sort in `ar-server` before
`card_json` (`routes.rs:2597-2600`), and an equivalent `CANONICAL_PROVIDER_ORDER`
in `ar-registry` derived from the imported `auth_kind`.

### G7 — `dual` id prefixing
**Missing.** OmniRoute advertises each model under both `alias/model` and
`providerId/model` (`catalog.ts:335-339`, default `dual` per `.env.example:2137`).
artificial-route emits exactly one `provider/model` id (`mod.rs:344`,
`omniroute.rs:1655`). An alias a client hardcoded (`cc/claude-sonnet-4-6`) will
not resolve.
**Needs:** either a second card per model in `model_cards_with`
(`config.rs:883`) or an alias-resolution step at `resolve()`.

### G8 — `provider_specific_data.excludedModels`
**Partial.** Per-connection glob denylist. What landed: a global `excludeModels`
denylist (`crates/ar-config/src/glob.rs`, hand-ported from
`connectionModelRules.ts:53-82`) applied in `model_cards_with`, so imported
catalogs honour an operator's glob exclusions in `/v1/models`.
**Still missing:** the *per-connection* scoping. OmniRoute keys these rules to a
connection row; artificial-route has one credential per provider, so there is
nothing to scope to — see G9. The global list is the honest analogue, not the
full feature.

### G9 — Connection/credential store parity
**Skipped, deliberately.** artificial-route reads credentials from env
(`render_yaml` writes `$AR_KEY_*`, `mod.rs:335`) plus the `ar-keys` redb store
(`crates/ar-keys/src/store.rs`). OmniRoute stores them in `provider_connections`
(`core.ts:220-265`) and *scopes visibility to what has an active row*.
**Reason:** a second credential table would duplicate what `ar-keys` already
owns, and the operator-visible effect of `is_active` is already modelled by
`CredentialStore::is_dispatchable()` — a provider with no usable key does not
appear as dispatchable and cannot win a request. That is the same guarantee with
one source of truth instead of two. **Consequence:** G1's connection-scoped half
and G8's per-connection scoping have no home, which is why neither is claimed
complete above. Revisit only if per-connection credentials are actually needed;
that is a design decision, not a port.

### G10 — Ad-hoc `openai-compatible-*` nodes (was missing; the import did not boot)
**Found by running the generated file, not by reading the code.** A combo step
names its provider by the id `provider_connections.provider` stores, and every
ad-hoc node lives in that table and nowhere else — the `open-sse/config/providers`
tree has no row for it. The importer read combos but not connections, so a config
generated from an operator who had one refused to start:
`combo target "openai-compatible-chat-f63e4c1e-…/glm-5.3-flash-abliterated"
names provider …, which is not in the registry`.
**Fixed** by `crates/ar-cli/src/import/custom.rs`: one `CustomProvider` per
distinct `provider` id, base URL read from
`provider_specific_data->baseUrl`, several keys on one node collapsing to one
entry (a node with eight keys is one endpoint, not eight providers).
`render_yaml` now writes the `custom_providers:` block **and** a `keys:` entry for
each node's `key_ref` — without the second, the loader refuses with
`references undeclared key`. Both are pinned by
`should_render_a_config_the_loader_actually_accepts`, which parses the generated
YAML back through `ar_config::Config`.

Three bugs in this one feature, all found by the same live test, all of which a
code reading had passed:
1. **The generated key had no `AR_KEY_` prefix** while every other generated key
   carried it, so the loader looked up a name the operator's `ar.env` could not
   contain. Now `env_name()` folds the id the same way, and the test pins the
   prefix.
2. **`baseUrl` was read as `base_url`.** Silent: serde matched nothing, every node
   read as base-URL-less, and the only symptom was an unroutable combo step.
   `should_read_the_camel_case_base_url_the_store_actually_spells` pins it.
3. **`is_active = 0` rows are excluded**, matching OmniRoute's own reachability
   rule; an inactive connection cannot dispatch upstream either.

### What is already correct and should not change
- The 1:1 static-catalog reproduction (276/1559) is exact and well defended.
- The `buildOpenAiCompatibleRegistryEntry` / spread / template-literal handling
  (`omniroute.rs:13-28`) is the right design.
- Reporting unrecoverable gaps by name rather than guessing (`:86-99`, `:112-114`)
  is the behaviour OmniRoute's own "short catalog is a visible defect" comments
  ask for.
