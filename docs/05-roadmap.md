# 05 — Roadmap P0-P6

**Status (2026-10-04):** P0–P4 shipped in v0.1.0/v0.1.1; P5's guard/obs
shipped as crates — `ar-guard` is wired into the request path
(`ar-server/src/text.rs:32`), and `ar-obs`'s `Metrics` now has an in-tree
consumer: it is the **routing half** of `/metrics`, observed from both reply
arms — `read_and_record` for buffered replies, `UsageTee::observe` for streamed
ones, which previously reached only the ledger. `ar-server/src/metrics.rs`
keeps the five transport counters the `ar_obs` outcome taxonomy cannot express
(throttled, failover, upstream attempts, catalog refresh). Two renderers, one
document. `ar-obs`'s `TraceWriter` and `AuditLedger` are now consumed too
(`Components::obs_dir`, armed by `AR_OBS_DIR`): the trace line carries the full
request figure, the audit row only the bounded tuple `ar_keys::AuditLine` can
type. Both halves start together and the trace channel is lossy by contract, so
the write path never slows a request; `/metrics` still renders with obs off.
P6's media family landed under
the parity closeout (`docs/07-parity-closeout.md`, wave C: embeddings,
transcriptions, image-generations, OCR) and `/v1/audio/translations` landed
afterwards, closing the modality scope; `import --from omniroute|litellm` shipped
in v0.1.0 (`5cccd13`). The parity-closeout waves A–O are
complete. **One row is still open: the 12-hour soak run itself** — the harness is
`scripts/soak.sh`, a 3-minute proof is recorded, and a 12h run is in flight
(relaunched post-`6fde400`, CSV `/tmp/ar-soak4.csv`, harness pid 293551), but 12
hours is not yet claimed. Two prior attempts died before acceptance and are
recorded in `docs/02`'s soak row, not as acceptance: one operator-killed at
54 min, one host-wide SIGBUS at 1h52m that also felled unrelated processes.
Landed since the last register update (`6a55836`):
server-side live discovery (`DiscoveredCatalog` as a `ModelCatalog` impl, unioned
over config, opt-in via `AR_MODEL_DISCOVERY`, default off — the scheduler is a TTL
tick, **not** the reference's recorded 24h sweep, so that drop is only partially
reversed) and `quota-share-fair`'s persisted deficit map (`DeficitMap`, keyless of
any combo id, winner pays 1, converges on the weight ratio).
The phase gates below still apply to anything a P-phase takes on.

Gate every phase: `cargo test --release` + `cargo clippy --all-targets --all-features --locked -- -D warnings` + RAM check.
AXI gates in `06-axi-mcp.md` apply from P0 (`TOON`, `--fields`, `--version` fast path, no prompts).

* **P0 spine lean-routing (wk1-2):** vendor `ar-core/http/pool/llm/cli-bootstrap`, `File` mode, `POST /v1/chat/completions` single provider stream + `GET /v1/models` SWR. 4 strategies only. Accept: 1 provider e2e + idle RSS + `TOON` list output.
* **P1 tokens+cache+keys (wk3-4):** BPE exact/heuristic 50k, `NormalizedUsage`, ledger + budget caps + `$0` flat-rate, exact cache 32MB + idempotency, keys v2 + JWT scopes + revoke, 3 lanes + RPM leases. Accept: over-budget 402, stolen token expires, heavy lane no starvation.
* **P2 full-strategies (wk5):** remaining 15 strategies, `auto/*` virtual factory + scoring, `simulate_route` + `explain_route`, Anthropic/Ollama/Responses inbound, `pipeline.ts` port. Accept: P0 label removed — `auto/*` live, routing trace correct. This is "done" for routing.
* **P3 compress (wk6):** `lite+rtk+caveman` + `planResolution` + `hardBudget` dial. Accept: `eval:compression` fidelity vs savings reported.
* **P4 cli+mcp-essential (wk7):** `serve|models|providers|combo|doctor|run|configure` AXI-clean + `ar-mcp --features mcp` 12-tool catalog wrapping same fns + `tool_search` + scopes + hashed audit. Accept: `cargo tree` default has no rmcp; agent `health->best_combo->route` loop works.
* **P5 guard+obs (wk8):** PII+injection 2-stage, secrets redact bidi, redacted audit, BigQuery off. Accept: red-team corpus + p95 guard <5ms.
* **P6 modality+import (wk9+):** embeddings landed; transcriptions/image-gen/`/v1/ocr` landed (wave C); remaining in scope: live discovery (`modelDiscovery.ts+reactiveModelSync.ts`,
P1), `import --from omniroute|litellm`. The earlier "vector P1 `usearch`" phrase
is dropped: no reference surface carries it. Accept: 12h soak RSS flat —
harness is `scripts/soak.sh` with its stub upstream and config alongside
(proof run recorded: 3 minutes, healthz clean, RSS flat ~26MB); the 12-hour
run itself is one command and is not yet claimed.

Out: full 110-tool MCP, xDS, HBONE, UI, ONNX, cloud sync. Each needs proposal + RAM budget to re-enter.
