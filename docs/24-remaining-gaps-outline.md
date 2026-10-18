# 24 — Remaining gaps: execution record (2026-10-18)

Status: first audit (earlier revision) and execution of its suggested order. Every
row carries a **State**: `done <sha>` (committed on `main`), `fixed (other)` (closed by
a commit from another session, verified), `open` (not started, no decision needed),
`blocked` (needs your ruling or a release), `unverified` (could not check here).

Verified at `150d27b` on a clean checkout of `HEAD`, the same three checks CI runs:

- `cargo fmt --all -- --check`: exit 0.
- `cargo clippy --all-targets --all-features --locked -- -D warnings`: exit 0.
- `cargo test --workspace --locked --no-fail-fast`: exit 0, **44 suites, 2021 passed, 0 failed**.

Blocker type: **decide** (needs your call), **code** (clear work), **time** (wall-clock),
**ops** (touches a live box or release), **probe** (needs evidence first).

Row IDs are this doc's own. A reference such as "docs/22 D5" names a docs/22 decision.

## Execution summary

Done in this pass: B1, B2, B3, B4, B5, B7 (G3), B8, G2's text fix, A3, the E1
correction in docs/05. Found closed by another session in the same window: G1 (`c3bf93a`), A4
(`813dd94`) and the `too_many_arguments` lint on `read_and_record` (`c7a841a`).

Not done, and why: A1 (release, tag and VPS deploy are outward-facing and need your
go), E1 (waits on A1), D1–D3 and G2's policy (need your ruling), C1 and C3 (the outline
makes them conditional on the log being relied on), and the probe-first and
on-demand rows (D4–D8, E2–E8), which the outline says not to start on speculation.

## A. Ship what already exists

| # | Gap | Type | State | Detail |
|---|---|---|---|---|
| A1 | Commits after tag `v0.1.16` are unreleased | ops | blocked | `Cargo.toml` is still `0.1.16`, and `v0.1.16` is the newest tag. Unreleased on `main`: `736d934` through `150d27b`. Needs a version bump, tag, release build, and the installed binary and VPS moved onto it. Not done: outward-facing. Also observed: the local gateway on `127.0.0.1:20128` is running under a new pid (704030) that started about four minutes before this check. This session did not start it. |
| A2 | The `failures` table only exists from the release that ships it | ops | open | Ships with A1. `CREATE TABLE IF NOT EXISTS failures` (`ledger.rs:49`); older rows are not backfilled, so the failed-only view starts empty. State it in the release notes. |
| A3 | Stray untracked files | code | done in working tree | Deleted `.mcp.json` (stale installer output; its `command` pointed at a temp directory that no longer exists, which broke the MCP connection every session) and `agent:/`. Kept `.token-savior-cache.json`: it appears to be written by the token-savior MCP and is regenerated, so the file is not mine to remove. Not gitignored: `.gitignore` carries another session's uncommitted edit, which this pass did not touch. |
| A4 | `docs/21` uncommitted edits | — | fixed (other) | Committed in `813dd94`. |

## B. Test and tooling

| # | Gap | Type | State | Detail |
|---|---|---|---|---|
| B1 | No handler tests for the new `/ui` endpoints | code | done `b5268c2` | `webui.rs` `http_tests`: six tests drive the router with `oneshot`. The log answers failures inside the range and filters them, a missing ledger says it is closed, `usage?days=0` and `days=99999` both answer 200, the limits writer refuses an unknown combo and writes nothing, keeps an omitted scope, clears it on `null`, and refuses a non-string. |
| B2 | The Playwright behavior script is not in the repo | code | done `83d3695` | `scripts/ui/behave.mjs`, with its run steps in the header. Run by hand against a demo `serve`, not in CI. Found and fixed a race in the script: the second send started before the first reply finished, so Send was still disabled. It now waits for Send to re-enable. Passes 13/13 against a demo server. |
| B3 | `aroute limits` cannot show or set `model` | code | done `b5268c2` | `aroute limits set --model <id>`: omit keeps, `""` clears, and an unknown combo is refused. `aroute limits` prints a `model` column. The scope check is one function, `commands::limit_scope`, that both the CLI and `/ui` call. |
| B4 | rustfmt drift | code | done `9f833bb` | 13 files, formatting only. |
| B5 | README test badge and test line stale | code | done `150d27b` | Now 44 suites, 2021 passed (the figure from the full run above). Update again when the count moves. |
| B6 | docs-check warnings | code | unverified | Located: `.git/hooks/pre-commit` runs `docs-check` in warn mode and does not gate. At this pass it reported 14 `D003` orphan pages (the earlier figure of 17 was wrong) and README-contract failures for 12 crates. Decide whether to gate it. |
| B7 | No CI gate ran fmt, clippy or tests on push | code | done `7ae39ce` | `.github/workflows/ci.yml`: a `lint` job (fmt, clippy with `-D warnings`) and a `test` job, on push to `main` and on pull request, against the pinned toolchain. Verified on `HEAD` as above. Disclosure: I committed this file before its lint step was checked. My `\| tail` hid a clippy failure, which came from another session's uncommitted `routes.rs` and not from this commit. That was fixed in `c7a841a`. |
| B8 | **new** — seven dead-code warnings in `ar-mcp` in the default build | code | done `486aa7a` | `crates/ar-mcp/src/tools.rs`: `list_combos`, `switch_combo`, `check_quota`, `route_request`, and three more `pub(crate)` helpers, unused outside the `mcp` feature. Clippy with all features is clean, and CI does not fail on them, so this is hygiene. Done: a module-level `expect(dead_code)` scoped to non-test builds without the `mcp` feature. A cfg gate would break the module's own tests in the default build. |

## C. Request log and scoping

| # | Gap | Type | State | Detail |
|---|---|---|---|---|
| C1 | Failures are only logged for the four chat dialects | code | fixed | The JSON media routes (`embeddings`, `images/generations`, `ocr`) and the two audio routes now write their failures to the failure log through the same `note_failure` the chat path uses. Audio rows carry the `?model=` they asked for. Test: `a_failed_media_request_lands_in_the_failure_log_with_the_model_asked_for`. |
| C2 | A stream that fails after it started is not a failure row | decide | open | Unchanged. The response is already `200`. |
| C3 | The log shows the newest 200 rows with no paging | code | open (conditional) | `LOG_LIMIT = 200` (`webui.rs`), no `before` parameter. Conditional, as C1. |
| C4 | Failure rows carry no provider | decide | open | `ledger.rs` writes `'-'` for provider on failure rows. Unchanged. |
| C5 | `Limit.model` scopes *when* a row applies, not *what it counts* | decide | open | Unchanged. docs/23 G3 records the choice. The `--model` help text now says so. |
| C6 | Trust model of `/ui` | probe | fixed (other + this pass) | `aroute serve` passes `public: false` (`serve.rs`), so `/ui` is loopback-only. The open question moved to G2. |

## D. Stream and failover decisions (docs/22)

| # | Gap | Type | State | Detail |
|---|---|---|---|---|
| D1 | docs/22 D8 / B4 — pre-commit silence, up to 110s per attempt | decide | blocked | `STREAM_START_TIMEOUT_CAP` is 110s (`crates/ar-exec/src/lib.rs:145`). Fixing it changes what clients see on failure. Needs your ruling. |
| D2 | docs/22 D10 / B10 — two stalls exceed the model deadline | decide | partly fixed (other) | `c3bf93a` and `c7a841a` made the model-aware JSON 504 win over the bare 504, so the empty body is gone. The two-stall case is not fixed: a second 110s start cap still runs past the 120s model deadline. Needs the ruling in D1. |
| D3 | docs/22 D9 / B7 — failover reports the last provider's status | decide | blocked | Pinned by contract tests. Needs your ruling. |
| D4 | docs/22 T3, streamed arm — empty or degenerate 200 | probe | open | Needs a measured corpus before any threshold. |
| D5 | docs/22 D5, repeat detection | code | open | `docs/22-stream-and-failover-repair.md:73` says it is not implemented. It waits on D4's corpus. |
| D6 | T5, streamed Gemini and tool calls | code | open | `TODO(#p1-tools)`, `canonical.rs:218`. Not started. |
| D7 | Live `cline` stream not re-probed after docs/22 | probe | open | Needs a live request through the released build. |
| D8 | T2, cline envelope unwrap | probe | open | Waits for an OAuth cline session that streams `{success,data}`. |

## E. Roadmap and parity

| # | Gap | Type | State | Detail |
|---|---|---|---|---|
| E1 | T1 — 12-hour soak against a released binary | time | blocked (on A1) | Earlier text said three attempts died for non-ar reasons. That was wrong. docs/05 records two attempts that ended before acceptance (one operator-killed at 54 min, one host-wide SIGBUS at 1h52m). docs/05 said a run was in flight with pid 293551; neither that process nor `/tmp/ar-soak4.csv` exists. docs/05 is corrected in `5d7d8e8`. Launch only after A1, detached, killed by PID, CSV outside `/tmp`. |
| E2 | `bedrock` and `gitlab-duo` cannot route | code | open | README "Known gaps": 2 of 276 providers. On request only. |
| E3 | `grok-cli` authenticates but does not dispatch | code | open | README "Known gaps". On request only. |
| E4 | `kilocode` dispatch wire not ported | code | open | README "Known gaps". On request only. |
| E5 | Anthropic-wire custom providers refuse at dispatch | code | open | README "Known gaps", including `zenmux-free`. On request only. |
| E6 | Multi-provider `auto/*` pools fall back to config order | code | open | README: bare `auto` maps to `least-used` since 0.1.12. Scoring engages for literal `auto/*` names only. |
| E7 | MCP control plane is stdio-only | decide | open | No HTTP transport owner or request. |
| E8 | Registry snapshot lags OmniRoute | ops | open | `aroute doctor` warns past 7 days. docs/21 T8 records a second scheduled sweep as rejected, so refresh cadence is an operations choice. |

## F. In-code seams (`TODO(#…)`)

| Seam | Where | State | Note |
|---|---|---|---|
| `#p1-admission` | `crates/ar-server/src/keys.rs:28` | open | Lane control is not wired. |
| `#p1-cancel` | `crates/ar-server/src/exec.rs:855` | open | Per-request cancellation token. |
| `#p1-config-public` | `crates/ar-server/src/config.rs` | text corrected (`6a50d7f`) | The TODO no longer refers to `ar serve --public`. It now says it is blocked on the G2 ruling. |
| `#p1-tools` | `crates/ar-translate/src/canonical.rs:218,232` | open | Canonical tool-call type. Overlaps D6. |
| `#p0-align` | `crates/ar-exec/src/lib.rs:865`, `crates/ar-translate/src/translate.rs:351` | open | `auth_header` / `auth_prefix` on `ProviderDef`. |
| `#P0-translate`, `#P0-route` | `crates/ar-core/src/traits.rs` | closed | The unused `ar-core` seam traits (all `todo!()`) were deleted. The live seams are `ArExec` and `ArTranslate` in `crates/ar-route/src/contract.rs`. |
| `#F-MED-2-device` | `crates/ar-keys/src/store.rs:423` | open | `poll_state` column is absent. |

## G. Found by the audit

| # | Gap | Type | State | Detail |
|---|---|---|---|---|
| G1 | Uncommitted fixes in the working tree | decide | fixed (other) | Committed by another session as `c3bf93a`, with the docs/22 note. The `?e` log change went in with it. |
| G2 | A routable bind is impossible through `aroute serve`, and the refusal text pointed at a knob that does nothing | decide | text fixed (`6a50d7f`); policy blocked | No shipped path reads `public`. `ServerConfig::from_env` is the only reader, and it is test-only. The three refusal texts now say what the build does. **Default taken, your call to reverse:** the build stays loopback-only. Open question: may a routable bind be allowed while the bearer gate is armed? |
| G3 | No CI gate ran clippy, fmt or tests on push | code | done `7ae39ce` | See B7. |

## Suggested order from here

1. ~~B8~~ done.
2. **Rulings, in one session**: D1–D3 (the two-stall case is the same question as D1), and G2 (routable bind with an armed gate).
3. **A1 release**, once you say to cut it: version, tag, build, the local binary and the VPS. A2 goes into the release notes.
4. **E1 soak** on the released binary, started right after A1 and left alone. It is wall-clock only.
5. **C1 and C3** if the request log will be relied on. C2, C4 and C5 only on demand.
6. **D4–D8, E2–E8, F** as evidence or requests arrive. Do not start them on speculation.

## Questions that need you

- A1: cut `0.1.17` now, and deploy the VPS as well? Both are outward-facing, so I have not done either.
- G2: keep the build loopback-only (the current state), or allow a routable bind while the bearer gate is armed?
- D1–D3: rule on the start cap, the deadline and the failover status. The outline lists the options.
- C1/C3: do you rely on the request log? If yes, those two go next.
- B6: gate `docs-check` on push, or leave it advisory?

## Not gaps (unchanged from docs/21)

`/metrics` label cardinality cap, `x-ar-model` carrying the client spelling, trace-channel
lossiness and redb's exclusive lock, mid-stream failover after content flowed, and
pipeline/fusion semantics stay closed.
