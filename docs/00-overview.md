# Artificial Route — Overview

> Minimal-RAM Rust port of OmniRoute proxy core. CLI + file config only. No web UI.

## Vision

One OpenAI-compatible endpoint `http://127.0.0.1:20128/v1` serving 300+ providers.
P0 ships **lean-routing** (4 strategies, ~80% traffic) at <50MB RSS idle.
P2 reaches **full-strategies** parity (19 strategies + 16-factor `auto/*`).
Agent-native operation via optional `ar-mcp` (12 tools, default off).

## Non-goals (explicit)

* No Next.js dashboard, no Electron, no PWA, no i18n UI (42 locales dropped).
* No ONNX / MobileBERT / OmniGlyph / quantumLock / adaptive ML compression in P0.
* No xDS control plane, no HBONE, no SPIFFE.
* No cloud bundle sync, no BigQuery export, no gamification, no plugins marketplace.
* No OAuth browser-session providers in P0 (claude-web, gemini-web, codex-web). API-key only first.
* No full 110-tool MCP in P0 — a 12-tool catalog, feature-gated (see `06-axi-mcp.md`).

## Routing parity law

P0 `priority|round-robin|cost-optimized|lkgp` is **lean-routing, not done**.
Call it done only after P2: all 19 strategies + `auto/*` virtual factory +
`simulate_route` dry-run + `explain_route` trace. See `02-port-from-omniroute.md`
deferred table and `05-roadmap.md` gates.

## RAM budget

| State | Target RSS | Measured (2026-10-03) |
|---|---|---|
| idle, no traffic | <35MB | **~120MB** — exceeded, and not by request state: `/proc/PID/status` on the release binary reads ~110MB `RssAnon` (the `jemalloc` arenas the d24ea9a parity program's dependency set brought in) against ~10MB `RssFile`. The number was ~11MB on the v0.1.1 binary, so the budget row predates the growth; either tune the allocator (`narenas`, decay) or restate the budget — the wave A–O closeout added no resident per-request state beyond one sqlite handle. |
| 1 streaming chat | +<3MB | unchanged — streams relay un-buffered |
| 20 heavy `/v1/responses` concurrent | <400MB, no FATAL | not re-measured; wave D's one-buffer-per-non-stream-reply is bounded by the provider's reply size, typically KBs |
| cache full 32MB | bounded, `quick_cache` sharded | unchanged |
| `+mcp` feature on, idle | +<25MB (rmcp tree) | unchanged |

Enforced by: `jemalloc` decay 5s, `Strng` interning, `BufList` zero-copy,
`AssertSize::<4K>` on futures, bounded log channel 128k lines lossy.

## Source map

* Copy directly: `agentgateway/crates/{core,http,pool,llm,agentgateway-app}` — see `01-copy-from-agentgateway.md`.
* Port as reference: `OmniRoute/open-sse/{config,translator,executors,services}` — see `02-port-from-omniroute.md`.
* Crates: see `03-crates-and-deps.md`. Subsystems: `04-subsystems.md`. Phases: `05-roadmap.md`.
* CLI+MCP ergonomics: `06-axi-mcp.md` (normative AXI gates + the tool catalog).

## Entry points (read in this order)

1. `../agentgateway/architecture/configuration.md`
2. `OmniRoute/src/app/api/v1/chat/completions/route.ts` (329 lines)
3. `OmniRoute/open-sse/translator/registry.ts + formats.ts`
4. `OmniRoute/open-sse/executors/default.ts + base/`
5. `OmniRoute/src/domain/pipeline.ts` (clean portable unit)
6. `OmniRoute/open-sse/mcp-server/README.md` (control-plane catalog)
