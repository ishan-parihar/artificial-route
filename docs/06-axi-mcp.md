# 06 — AXI CLI gates + MCP essential-8 (normative)

Source: `~/.agents/skills/axi` (TOON spec). Applies from P0. CI must enforce.

## CLI output (TOON on stdout, JSON inside)

* `TOON` at boundary (~40% savings). Lists default 3-4 fields `id,provider,status` + `count: N of TOTAL`. Detail via `--fields`, bodies truncated 500-1500 chars with `(truncated, TOTAL chars)` + `--full` hatch only when truncated.
* Aggregates to kill follow-ups: `checks, comments, quota%left, p95` where cheap. Empty definitive: `0 closed tasks in X`, never blank.
* `stdout` = data+errors+suggestions. `stderr` = progress/diagnostics. Exit `0` success+no-op (already-closed), `1` error, `2` usage.
* Errors structured on stdout with `help: <exact fix command>`, translated (no stack/API leak). No interactive prompts — missing flag fails with usage. Fail loud on unknown flag: name it, list valid flags inline (or `--help` block), per-subcommand sets, `--status renamed; use --state` hints. `--help` always passes.
* Content-first: bare `ar` prints `bin: ~/.../ar + one-line description + live combos/quota`, not manual. Per-command `--help` concise + 2-3 examples.
* `--version` fast path: `-v|-V|--version` bare exits 0 before command graph loads. `VERSION` in leaf builtins-only module, heavy CLI behind dynamic load. Test vs process floor, not absolute ms.
* Session: `ar setup claude|codex|opencode` installs `SessionStart` hook (explicit opt-in, idempotent, path-repair, dir-scoped, token-minimal) + generated `SKILL.md` from home view with `--check` CI stale-fail. Hook primary, skill secondary.

## MCP essential-8 (P1, `--features mcp`, default off)

Thin wrappers over same `ar-route|ar-tokens|ar-obs` fns. Transports stdio + StreamableHTTP via `rmcp`. `schemars+serde_json` schemas.

| # | Tool | Scope | Maps to |
|---|---|---|---|
| 1 | `ar_get_health` | `read:health` | lanes pressure, breakers, cache stats, `tenantKey` opaque |
| 2 | `ar_list_combos` | `read:combos` | chains + strategies + metrics opt-in |
| 3 | `ar_switch_combo` | `write:combos` | activate/deactivate, idempotent no-op exit 0 |
| 4 | `ar_check_quota` | `read:quota` | per-provider remaining + token health |
| 5 | `ar_route_request` | `execute:completions` | chat completion via routing |
| 6 | `ar_cost_report` | `read:usage` | session/day/week/month + per-provider |
| 7 | `ar_list_models` | `read:models` | catalog + capabilities + pricing |
| 8 | `ar_explain_route` | `read:health+read:usage` | provider/model/score/factors/fallbacks |

Plus `ar_tool_search` (one-line signatures, token-efficient discovery) from day one.
Deferred: `simulate_route, test_combo, best_combo_for_task, session_snapshot, cache_*, compression_*, ccr_*, web_*, skills, oneproxy_*, sync_pricing, db_health, create_combo, set_strategy, pick_fastest` — each needs RAM + scope justification.

Scopes: `read:*` wildcard reads, `*` full. `ENFORCE_SCOPES=true` default deny. Audit `blake3(input)+truncate(output,200)+tool|duration|key-id` in `redb`.
Accept: default `cargo tree` has no `rmcp`; `+mcp` idle +<25MB; agent `health->switch->route->cost` loop green.

## Rust mapping

CLI libs `thiserror`, binary `anyhow`. `#[expect(clippy::x)]` with reason. `&str/&[T]` params, no loop clones. Static dispatch hot path. `insta` goldens for TOON shapes + translator pairs.
