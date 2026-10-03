# 05 — Roadmap P0-P6

**Status (2026-10-03):** P0–P4 shipped in v0.1.0/v0.1.1; P5's guard/obs
shipped as crates — `ar-guard` is wired into the request path
(`ar-server/src/text.rs:32`), while `ar-obs` is a shipped library with **no
in-tree consumer**: its `Metrics`/`TraceWriter`/`AuditLedger` are not called by
`ar serve`, which serves its own four counters from `ar-server/src/metrics.rs`.
Both crates exist and are tested; only the guard is on the request path.
P6's media family landed under
the parity closeout (`docs/07-parity-closeout.md`, wave C: embeddings,
transcriptions, image-generations, OCR) and `/v1/audio/translations` landed
afterwards, closing the modality scope; `import --from omniroute|litellm` shipped
in v0.1.0 (`5cccd13`). The parity-closeout waves A–O are
complete. Three rows are still open and none is claimed: server-side live
discovery — the models.dev overlay ships in `ar-registry` and `ar import` uses
it, but the running server's `/v1/models` is still config-static, and wiring it
reverses a recorded drop of the scheduler — `quota-share-fair`'s persisted
deficit map, and the 12-hour soak run itself: the harness is `scripts/soak.sh`
and a 3-minute proof run is recorded, but 12 hours is not. One more that is not
a P-phase row: `ar-obs` is a shipped, tested library with no in-tree consumer.
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
