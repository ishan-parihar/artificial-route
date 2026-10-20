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
| [`01-copy-from-agentgateway.md`](01-copy-from-agentgateway.md) | LIVE | 01 — Copy directly from agentgateway — Base: ../agentgateway/. License Apache-2.0. Pin to commit before vendoring. Method: path dependencie… |
| [`27-dashboard-contrast-and-upgrade-plan.md`](27-dashboard-contrast-and-upgrade-plan.md) | LIVE | 27 — /ui dashboard: contrast with the OmniRoute frontend, audit, and the upgrade plan — Status: written 2026-10-19. Audit and contrast only; no code changed. Owner: this doc. Companions: d… |
| [`28-gap-ledger.md`](28-gap-ledger.md) | LIVE | 28 — The gap ledger: every OPEN bug, decision and implementation gap, in one place — Status: consolidated 2026-10-19. Owner: this doc, for the state of every open item. Companions: TODO… |
| [`29-routing-quality.md`](29-routing-quality.md) | LIVE | Routing quality: the audit and the fixes — Dated-frozen: written 2026-10-19. The state of every item below is in [docs/28-gap-ledger.md](28-gap… |
| [`30-ui-production-grade.md`](30-ui-production-grade.md) | LIVE | 30 — /ui production-grade: the design brief, the direction, and what shipped — Status: written 2026-10-20. The record of the U0-U5 pass over the control panel.… |
| [`31-remaining-gaps-execution-outline.md`](31-remaining-gaps-execution-outline.md) | LIVE | 31 — Remaining gaps: the execution outline — Status: written 2026-10-20. Owns the **order and shape** of the remaining work; the ledger owns the state. Six phases… |
| [`32-per-model-token-budgets.md`](32-per-model-token-budgets.md) | LIVE | 32 — Per-model token budgets: a ceiling that names the model it stopped — Status: built 2026-10-20. The design, the measurements and the test plan for the per-target spend arm… |
| [`33-vps-limits-config.md`](33-vps-limits-config.md) | LIVE | 33 — The VPS limits configuration, and why each number is that number — Status: written 2026-10-20. The measured burn per target on `racknerd` and the ceilings chosen from it… |
| [`34-spend-cap-audit.md`](34-spend-cap-audit.md) | LIVE | 34 — The spend-cap surfaces on the VPS, and why the `limits:` block was there — Status: audited 2026-10-20. Every surface that can refuse a request for spend, measured; and the finding that the block was an audit fixture, not an operator decision… |
| [`2026-10-19-skill-redundancy-audit.md`](audit/2026-10-19-skill-redundancy-audit.md) | LIVE | Skill redundancy audit — 2026-10-19 — The library is large but the aroute and harness clusters were the fragmented ones: 27 aroute-named s… |
| [`2026-10-19-ui-dashboard-contrast.md`](audit/2026-10-19-ui-dashboard-contrast.md) | LIVE | ui dashboard contrast — Dated-frozen: audited 2026-10-19; findings mark verified vs asserted.… |
| [`STATE.md`](handoff/STATE.md) | LIVE | State — Current snapshot only (no history: that is the worklog). The first ## block is what genome wake show… |
| [`ASKS.md`](kb/ASKS.md) | LIVE | Operator questions and rulings — genome q add "<question>" · genome q rule Q1 "<ruling>" --applied <owner path>. A ruling is done onl… |
| [`CLOSED.md`](kb/CLOSED.md) | LIVE | CLOSED — tried and rejected — Append-only. An entry names the item, the verdict, the measurement that killed it (a path), and the… |
| [`WORKLOG.md`](kb/WORKLOG.md) | LIVE | Worklog — Append-only: genome log "<entry>", one per unit of work. - 2026-10-18 swap: live aroute replaced 5c0… |
| [`feed.md`](kb/feed.md) | LIVE | Agent feed — Append-only. genome post "msg" --wf <wf> --to <agent>; genome feed --to <you>.… |
| [`2026-10-19-provider-native-discovery.md`](plans/2026-10-19-provider-native-discovery.md) | LIVE | Provider-native model discovery (priority over models.dev) — Status: implemented + deployed 2026-10-19 (commit cc264ae). Operator ruling: /v1/models windows must… |
| [`2026-10-19-racknerd-public-endpoint.md`](plans/2026-10-19-racknerd-public-endpoint.md) | LIVE | Plan: aroute on racknerd behind a public endpoint — Status: complete (2026-10-19). Local-only proxy removed; all harnesses use https://aroute.ishanparih… |
| [`2026-10-19-ui-dashboard-upgrade.md`](plans/2026-10-19-ui-dashboard-upgrade.md) | LIVE | ui dashboard upgrade — Dated-frozen: published 2026-10-19. A plan that changes is a new plan (or declare it in genome.toml… |
| [`RG-001-the-ui-panel-is-published-without-a-credential-of-its-own.md`](regressions/RG-001-the-ui-panel-is-published-without-a-credential-of-its-own.md) | LIVE | RG-001: the /ui panel is published without a credential of its own — Immutable once numbered: a correction is a new record that supersedes this one.… |
