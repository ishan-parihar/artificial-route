# Page index — every active doc, the question it owns

| File | Status | Owns |
|---|---|---|
| [`00-overview.md`](00-overview.md) | LIVE | Artificial Route — Overview — One OpenAI-compatible endpoint http://127.0.0.1:20128/v1 serving 300+ providers. P0 ships lean-routi… |
| [`02-port-from-omniroute.md`](02-port-from-omniroute.md) | LIVE | 02 — Port as reference from OmniRoute — Base: ../OmniRoute/. Do NOT copy TS. Re-implement logic in Rust, keep constants.… |
| [`03-crates-and-deps.md`](03-crates-and-deps.md) | LIVE | 03 — Crates and deps — Edition 2024, rust-version 1.90, pinned rust-toolchain.toml. Profiles: release lto=true codegen-unit… |
| [`04-subsystems.md`](04-subsystems.md) | LIVE | 04 — Subsystems (hardened specs) — lite |
| [`05-roadmap.md`](05-roadmap.md) | LIVE | 05 — Roadmap P0-P6 — Gate every phase: cargo test --release + cargo clippy --all-targets --all-features --locked -- -D wa… |
| [`06-axi-mcp.md`](06-axi-mcp.md) | LIVE | 06 — AXI CLI gates + MCP catalog (normative) — Twelve tools today: the original eight plus four browser-login tools. The count is armcp::Tool::ALL.… |
| [`07-blockers.md`](07-blockers.md) | LIVE | Remaining blockers: plan — Written 2026-10-05 against the ported free-stack / small-stack on origin/main (daddb0b). Every numbe… |
| [`07-parity-closeout.md`](07-parity-closeout.md) | LIVE | 07 — Parity closeout plan (waves A–O) — Recorded before any closeout code lands, per AGENTS.md §3 ("update docs when scope changes, before c… |
| [`11-web-ui-port-plan.md`](11-web-ui-port-plan.md) | LIVE | 11 — Web UI port plan (OmniRoute dashboard → `artificial-route`) — Read-only recon of OmniRoute's web UI, to decide whether (and how) it ports into artificial-route. S… |
| [`12-import-parity-audit.md`](12-import-parity-audit.md) | LIVE | 12 — Import parity audit: OmniRoute "visible and enabled" vs `aroute import` — Read-only investigation. Every claim below carries a file:line into ../OmniRoute (TypeScript) or thi… |
| [`14-web-ui-port-plan.md`](14-web-ui-port-plan.md) | LIVE | Web UI port — the compiled-dashboard approach (implemented) — Status: shipped. aroute dashboard runs a rebranded, fully-compiled OmniRoute web UI as a child Node… |
| [`15-policy-security-layer.md`](15-policy-security-layer.md) | LIVE | 15 — Policy & security layer upgrade plan — Import the worthy agentgateway mechanisms into aroute, and nothing else. Every row below was verifie… |
| [`16-dashboard-unity-plan.md`](16-dashboard-unity-plan.md) | LIVE | 16 — Dashboard config unity: the split, the fix plan, the skin plan — Status: plan (the §4 rebrand closeout is implemented and live). Companion to docs/14 (the compiled-d… |
| [`19-ui-upgrade-plan.md`](19-ui-upgrade-plan.md) | LIVE | 19 — /ui upgrade plan: close the gap to the omniroute frontend — Status: Phases A+B+C+D+E shipped 2026-10-17 — the staged roadmap below is complete. Open follow-ups… |
| [`20-model-attribution-plan.md`](20-model-attribution-plan.md) | LIVE | 20 — Per-model dispatch attribution (diagnosis gap) — Status: implemented 2026-10-17, shipped 0.1.16. Triggered by "which member of free-stack is hallucin… |
| [`21-remaining-gaps.md`](21-remaining-gaps.md) | LIVE | 21 — Remaining implementation gaps (register, 2026-10-17) — Status: register, not plan. Each row names its authority (the doc that owns the decision) so this fi… |
| [`22-stream-and-failover-repair.md`](22-stream-and-failover-repair.md) | LIVE | 22 — Stream and failover repair (2026-10-18) — Status: implemented 2026-10-18 (B1, B2, B3, B5, B6, T3, T5). B4, B7, T2 and the operator rows are op… |
| [`23-ui-round-two-plan.md`](23-ui-round-two-plan.md) | LIVE | 23 — /ui round two: the gaps A–E left open — Status: G1–G5 implemented 2026-10-18, uncommitted (see "Outcome" at the end). Written after docs/19… |
| [`24-remaining-gaps-outline.md`](24-remaining-gaps-outline.md) | LIVE | 24 — Remaining gaps: execution record (2026-10-18) — Status: first audit (earlier revision) and execution of its suggested order. Every row carries a Sta… |
| [`25-red-team-register.md`](25-red-team-register.md) | LIVE | 25 — Red-team register (2026-10-18) — Status: fixes applied on 2026-10-18 in the working tree; not committed, not deployed. The register w… |
| [`26-benchmarks-parity-gaps.md`](26-benchmarks-parity-gaps.md) | LIVE | Benchmarks, parity and known gaps — Measured numbers, the feature comparison against OmniRoute and agentgateway, and the open gaps. Move… |
| [`audit-notes.md`](audit-notes.md) | LIVE | Audit notes — `classify_refresh` vs the reference taxonomy — Row-for-row cross-check of arexec::oauth::classifyrefresh against ../OmniRoute/open-sse/services/err… |
| [`RECORDS.md`](RECORDS.md) | LIVE | Records — decisions (AD) and regressions (RG) — Generated zones below are written by genome rec emit; never hand-edit them. Topic → governing record… |
| [`RUNBOOK.md`](RUNBOOK.md) | LIVE | Runbook — exact commands, environment traps |
| [`2026-10-01-AUDIT-REPORT.md`](audit/2026-10-01-AUDIT-REPORT.md) | LIVE | Audit Report: artificial-route vs OmniRoute vs agentgateway — That third row is why there are two lists. CARVEOUTTERMINALSTATUS holds (401, "invalidclient") and f… |
