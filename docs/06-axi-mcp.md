# 06 — AXI CLI gates + MCP catalog (normative)

Twelve tools today: the original eight plus four browser-login tools. The count
is `ar_mcp::Tool::ALL.len()`; `aroute mcp --list` prints it.

Source: `~/.agents/skills/axi` (TOON spec). Applies from P0. CI must enforce.

## CLI output (TOON on stdout, JSON inside)

* `TOON` at boundary (~40% savings). Lists default 3-4 fields `id,provider,status` + `count: N of TOTAL`. Detail via `--fields`, bodies truncated 500-1500 chars with `(truncated, TOTAL chars)` + `--full` hatch only when truncated.
* Aggregates to kill follow-ups: `checks, comments, quota%left, p95` where cheap. Empty definitive: `0 closed tasks in X`, never blank.
* `stdout` = data+errors+suggestions. `stderr` = progress/diagnostics. Exit `0` success+no-op (already-closed), `1` error, `2` usage.
* Errors structured on stdout with `help: <exact fix command>`, translated (no stack/API leak). No interactive prompts — missing flag fails with usage. Fail loud on unknown flag: name it, list valid flags inline (or `--help` block), per-subcommand sets, `--status renamed; use --state` hints. `--help` always passes.
* Content-first: bare `ar` prints `bin: ~/.../ar + one-line description + live combos/quota`, not manual. Per-command `--help` concise + 2-3 examples.
* `--version` fast path: `-v|-V|--version` bare exits 0 before command graph loads. `VERSION` in leaf builtins-only module, heavy CLI behind dynamic load. Test vs process floor, not absolute ms.
* Session: `aroute setup claude|codex|opencode` installs `SessionStart` hook (explicit opt-in, idempotent, path-repair, dir-scoped, token-minimal) + generated `SKILL.md` from home view with `--check` CI stale-fail. Hook primary, skill secondary.

## MCP catalog (P1, `--features mcp`, default off)

Thin wrappers over same `ar-route|ar-tokens|ar-obs|ar-exec` fns. Transports stdio + StreamableHTTP via `rmcp`. `schemars+serde_json` schemas.

**Status:** stdio is wired (`aroute mcp`, `--features mcp`); StreamableHTTP is deferred, and the
`Scope` column below is each tool's *need* — a *grant* is a subset of the ten scope bits
(`read:*`, `write:*`, `execute:*`, `*:health`, `*:combos`, `*:quota`, `*:usage`, `*:models`,
`*:completions`, `*`), set per process in `AR_MCP_SCOPE`. Those spellings are compositions of
the bits, so accepting both would give one scope two names.

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
| 9 | `ar_auth_login_url` | `read:*` | PKCE authorize URL + pending session id + redirect_uri |
| 10 | `ar_auth_complete` | `write:*` | redeem a pasted redirect URL *or* bare code, persist, verify |
| 11 | `ar_auth_status` | `read:*` | pending logins + stored credential row *names* |
| 12 | `ar_auth_logout` | `write:*` | forget a provider's access + refresh rows |

Plus `ar_tool_search` (one-line signatures, token-efficient discovery) from day one.
Deferred: `simulate_route, test_combo, best_combo_for_task, session_snapshot, cache_*, compression_*, ccr_*, web_*, skills, oneproxy_*, sync_pricing, db_health, create_combo, set_strategy, pick_fastest` — each needs RAM + scope justification.

### The four login tools (9–12)

The remote half of `aroute auth login|status|logout`, over OmniRoute's
`inAppLoginService` order — **url → complete → persist → verify** — with Playwright replaced by
a person on another device and stdin replaced by a second tool call. Splitting at exactly one
point is what makes it work from a headless VPS; what crosses the gap is the PKCE verifier,
held server-side keyed by a random v4 session id, single-use, and pruned at the CLI's own
300 s `--timeout` default (`ar_mcp::LOGIN_TTL`). An authorize URL and a `redirect_uri` are
public by construction — that is *why* the reads need only `read:*` — so no tool output
carries a verifier, a code or a token; `ar_auth_complete` reports armed/failed plus row
*names*. A bare code is accepted as well as a whole redirect URL, held to the same `state`
check. A `write:*`-less host refuses 10 and 12; a narrow grant reaches 9 and 11.

Scopes: `read:*` wildcard reads, `*` full. `ENFORCE_SCOPES=true` default deny. Audit `blake3(input)+truncate(output,200)+tool|duration|key-id` in `redb`.
Accept: default `cargo tree` has no `rmcp`; `+mcp` idle +<25MB; agent `health->switch->route->cost` loop green.

## Rust mapping

CLI libs `thiserror`, binary `anyhow`. `#[expect(clippy::x)]` with reason. `&str/&[T]` params, no loop clones. Static dispatch hot path. `insta` goldens for TOON shapes + translator pairs.
