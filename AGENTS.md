# AGENTS.md — Artificial Route territory rules

Fresh agent: read `docs/00-overview.md` then `01..05` in order before touching code.

## 1. What this is

Minimal-RAM Rust LLM proxy. CLI + file YAML only. Sources: copy `../agentgateway/crates/{core,http,pool,llm,agentgateway-app}`,
port-reference `../OmniRoute/open-sse/*`. Never invent provider wire formats — read `ar-llm/conversion/*` + `docs/02*` first.

## 2. Rust disciplines (rust-best-practices, enforced)

* Borrow don't clone: `&str/&[T]` params, `Cow` ambiguous, `Copy` <=24B. No `clone()` in loops; iterators, no stray `collect`.
* Errors: `Result` + `?`, no `unwrap/expect` outside `#[cfg(test)]`. Libs `thiserror`, binary `anyhow`.
* Perf: `--release` benches, `clippy::perf`, `AssertSize::<4K>` on new futures, `BufList` zero-copy bodies, `Strng` for names.
* Lints: `cargo clippy --all-targets --all-features --locked -- -D warnings` must pass. `#[expect(lint)]` with reason only.
* Dispatch: generics static on hot path; `dyn` only heterogeneous lists; box at boundary.
* State: `TypeState` for connections (`Disconnected->Connected` only sends). `Send/Sync` audit on shared state.
* Docs: `//` why, `///` what/how, `#![deny(missing_docs)]` on libs, `TODO(#issue)` links.
* Tests: `verb_should_outcome_when_condition`, one assert, doc-tests, `insta` goldens for translator.
* Benches (ch.10): dev/test/synthetic split, `provisional:` on fitted consts, 3x `criterion+black_box`, record load/CPU/SHA, generator no-overwrite, CI only `test+clippy`, pinned toolchain.

## 3. How to work here

* 2+ steps -> `todowrite` atomic todos `[WHERE] [HOW] to [WHY] - expect [RESULT]`, one `in_progress` at a time.
* Delegate by domain: `visual-engineering` never (no UI), `ultrabrain` hard logic, `deep` research+impl, `quick` single-file typo only. Load relevant skills in `load_skills`.
* Subagent prompts must have TASK/EXPECTED/TOOLS/MUST-DO/MUST-NOT-DO/CONTEXT. Vague <5 lines = rejected.
* Explore/librarian always `run_in_background=true` parallel; never re-grep same scope yourself; end turn and wait for `<system-reminder>`, then `background_output`. Cancel individually, never `all=true`.
## 5. File map

`docs/00-overview` vision+parity-law, `01-copy*` vendor+`ar-mcp` gate, `02-port*` TS->Rust + deferred-parity table,
`03-crates*` deps+why, `04-subsystems*` hardened specs incl. MCP hooks, `05-roadmap*` P0-P6 gates, `06-axi-mcp*` normative AXI+MCP-8.
Update docs when scope changes, before code. Never call P0 routing "done" — P2 is done.
