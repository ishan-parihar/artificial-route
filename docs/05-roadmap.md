# 05 — Roadmap P0-P6

Gate every phase: `cargo test --release` + `cargo clippy --all-targets --all-features --locked -- -D warnings` + RAM check.
AXI gates in `06-axi-mcp.md` apply from P0 (`TOON`, `--fields`, `--version` fast path, no prompts).

* **P0 spine lean-routing (wk1-2):** vendor `ar-core/http/pool/llm/cli-bootstrap`, `File` mode, `POST /v1/chat/completions` single provider stream + `GET /v1/models` SWR. 4 strategies only. Accept: 1 provider e2e + idle RSS + `TOON` list output.
* **P1 tokens+cache+keys (wk3-4):** BPE exact/heuristic 50k, `NormalizedUsage`, ledger + budget caps + `$0` flat-rate, exact cache 32MB + idempotency, keys v2 + JWT scopes + revoke, 3 lanes + RPM leases. Accept: over-budget 402, stolen token expires, heavy lane no starvation.
* **P2 full-strategies (wk5):** remaining 15 strategies, `auto/*` virtual factory + scoring, `simulate_route` + `explain_route`, Anthropic/Ollama/Responses inbound, `pipeline.ts` port. Accept: P0 label removed — `auto/*` live, routing trace correct. This is "done" for routing.
* **P3 compress (wk6):** `lite+rtk+caveman` + `planResolution` + `hardBudget` dial. Accept: `eval:compression` fidelity vs savings reported.
* **P4 cli+mcp-essential (wk7):** `serve|models|providers|combo|doctor|run|configure` AXI-clean + `ar-mcp --features mcp` 12-tool catalog wrapping same fns + `tool_search` + scopes + hashed audit. Accept: `cargo tree` default has no rmcp; agent `health->best_combo->route` loop works.
* **P5 guard+obs (wk8):** PII+injection 2-stage, secrets redact bidi, redacted audit, BigQuery off. Accept: red-team corpus + p95 guard <5ms.
* **P6 modality+import (wk9+):** embeddings full, vision/audio/video + `/v1/ocr`, live discovery, vector P1 `usearch`, `import --from omniroute|litellm`. Accept: 12h soak RSS flat.

Out: full 110-tool MCP, xDS, HBONE, UI, ONNX, cloud sync. Each needs proposal + RAM budget to re-enter.
