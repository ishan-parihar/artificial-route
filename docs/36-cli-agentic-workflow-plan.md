# 36 — The CLI as an agentic surface: the inventory, the failures, and the plan

Status: **plan, written 2026-10-20.** No code changed by this document. Built on
two read-only recon passes over the workspace at `12dda18`
(`/tmp/recon/A2-cli-inventory.md`, `/tmp/recon/A3-cli-usability.md`) plus the
operator's 2026-10-20 goal: *"audit the entire CLI surface area of aroute, for
being token/tool-calling efficient for agentic-workflows, so that agents can
directly use the CLI for updating everything without the use of manually
configuring the config files, and can guide the user through steps like login
for oauth and safely handle api keys management."*

Owner: this doc, for the CLI-surface plan. The state of every open item below
is in [`docs/28-gap-ledger.md`](28-gap-ledger.md); the order of the remaining
work across the whole project is in
[`docs/31-remaining-gaps-execution-outline.md`](31-remaining-gaps-execution-outline.md).

Companions: [`docs/06-axi-mcp.md`](06-axi-mcp.md) (**normative** for the CLI
output contract and the MCP catalog — this plan is consistent with it and §3
names where the code has drifted from it),
[`docs/34-spend-cap-audit.md`](34-spend-cap-audit.md) (why the `limits:` block
existed and why it is gone),
[`docs/35-cap-surface-removal-plan.md`](35-cap-surface-removal-plan.md) (the
`limits:` write paths this plan touches),
[`docs/37-dashboard-deployability-plan.md`](37-dashboard-deployability-plan.md)
(the browser surface the operator asked about — the panel and the CLI are the
two surfaces an agent has, and this plan is the CLI half),
[`docs/28-gap-ledger.md`](28-gap-ledger.md) (the ledger),
[`docs/31-remaining-gaps-execution-outline.md`](31-remaining-gaps-execution-outline.md)
(the execution outline),
[`docs/kb/ASKS.md`](kb/ASKS.md) (the operator's open questions — **Q9** is
directly relevant here),
[`docs/PAGE-INDEX.md`](PAGE-INDEX.md) (the index; the orchestrator links this
page, this document does not edit it).

---

## Contents

- [0. The goal, restated as a testable contract](#0-the-goal-restated-as-a-testable-contract)
- [1. The inventory: every command, and what it can reach](#1-the-inventory-every-command-and-what-it-can-reach)
- [2. The findings, severity-ranked](#2-the-findings-severity-ranked)
- [3. Where the code has drifted from `docs/06-axi-mcp.md`](#3-where-the-code-has-drifted-from-docs06-axi-mcpmd)
- [4. The plan of record](#4-the-plan-of-record)
- [5. The open operator decisions](#5-the-open-operator-decisions)
- [6. The test plan](#6-the-test-plan)
- [7. Open items carried from the recon](#7-open-items-carried-from-the-recon)
- [8. The single change to make first](#8-the-single-change-to-make-first)
- [9. The live-system red-team pass (2026-10-24)](#9-the-live-system-red-team-pass-2026-10-24)

## 0. The goal, restated as a testable contract

The operator's sentence is a capability claim about the CLI as an **agent
surface**. It has three halves, and each can be made falsifiable.

> **C1 — Reachability.** An agent can reach every config section the panel can
> reach, using a command whose name says which section it touches. A section
> that only a human can edit by hand is a failure of C1, and so is a section
> that only a browser form can edit.
>
> **C2 — Effect.** A write command's output tells the agent whether the change
> is **live now** or **needs a restart**, and it never claims live when the
> runtime is boot-built. An agent that writes and immediately probes must not
> measure a stale proxy and believe it measured the new one.
>
> **C3 — Human-in-the-loop.** Every interaction that needs a human — an OAuth
> consent, a pasted code, a token to redistribute — is expressible as two
> agent-runnable calls with a durable handle between them, so the agent's
> process can die and be resumed without the human starting over.
>
> **C4 — Cost.** Every command an agent is likely to run has a bounded output.
> An agent's question "what can I route to?" must not cost 72 KB, and an
> agent's question "is this install healthy?" must not cost a table it cannot
> narrow.
>
> **C5 — Machine-readability.** An agent can discover the command set and
> parse a command's result without English matching. Exit codes are `0/1/2`
> (`crates/ar-cli/src/main.rs:34-38`), and the error text carries a stable
> identifier a caller can branch on, not only an English sentence.

**How a plan is judged against it.** Each step in §4 names the contract clause
it closes and the test that proves it. A step that cannot name a clause is
scope creep and should not ship. A clause with no step against it is a hole in
this plan.

**The honest starting position.** C1 fails on three sections (§1.3). C2 fails
on every write command except `sync`, and the help text asserts the opposite
of the truth. C3 fails on the OAuth login (§2 B-4). C4 fails on two list
commands (§2 D-1, D-2). C5 fails on errors (§2 D-3) and on the command
catalog (§2 D-4). One of these is a **silent data-loss bug** (§2 B-1).

---

## 1. The inventory: every command, and what it can reach

### 1.1 The command spine

`crates/ar-cli/src/cli.rs:38-209` defines `pub enum Command`. Every variant is
wired to a handler in `commands::dispatch` (`crates/ar-cli/src/commands.rs:926-961`).
The `mcp` variant is `#[cfg(feature = "mcp")]` (`crates/ar-cli/src/cli.rs:186-188`,
`crates/ar-cli/src/main.rs:19-20`) and the feature is **default-off**
(`crates/ar-cli/Cargo.toml:23-25`) and **not enabled by the release build**
(`.github/workflows/release.yml:129-136`).

| Command | Read/Write | Writes to (file) | Handler |
|---|---|---|---|
| `aroute serve [--port N]` | long-running | `usage.sqlite` (ledger), `credentials.db` (revocations) | `commands.rs:929` → `serve::serve` |
| `aroute models [--fields] [--full]` | READ | — | `commands.rs:930` → `commands.rs:1008` |
| `aroute providers [--fields] [--full]` | READ | — | `commands.rs:931` → `commands.rs:1097` |
| `aroute combo [--fields] [--full]` | READ | — | `commands.rs:932` → `commands.rs:1160` |
| `aroute doctor` | READ | — | `commands.rs:933` → `commands.rs:1219` |
| `aroute run -p TEXT [-m ID]` | READ + ledger write | `usage.sqlite` usage rows | `commands.rs:935` → `serve.rs:472` |
| `aroute configure [--check]` | READ | — | `commands.rs:936` → `commands.rs:1917` |
| `aroute auth login\|logout` | **WRITE** | `credentials.db` (store rows) | `commands.rs:934` → `auth.rs:108` |
| `aroute auth status` | READ | — | `auth.rs:710` |
| `aroute keys arm-gate\|mint\|revoke` | **WRITE** | `credentials.db` (gate master row, `revoked_tokens`) | `commands.rs:948-954` |
| `aroute limits` (view) | READ | — | `commands.rs:956` → `commands.rs:616` |
| `aroute limits set\|clear` | **WRITE** | `config.yaml` `limits:` | `commands.rs:957-958` → `commands.rs:703` (`limits_set`), `commands.rs:888` (`limits_clear`) |
| `aroute import --from …` | **WRITE (whole files)** | `<out-dir>/{config.yaml,registry.json,freeBudgets.json,providerMeta.json,modelLifecycle.json}` | `commands.rs:937` → `commands.rs:1999` |
| `aroute sync [--db] [--env] [--no-restart]` | **WRITE** | `config.yaml`, `ar.env`; restarts `aroute.service` | `commands.rs:938` → `commands.rs:2073` |
| `aroute mcp [--list] [--http ADDR]` | feature-gated | `ar-mcp-audit.redb`, `credentials.db` | `commands.rs:940` (the arm is `#[cfg(feature = "mcp")]`, so a default build has no `mcp` subcommand at all) → `mcp.rs:61` |
| `aroute dashboard [--port] [--path]` | spawns a child | the child's own `$DATA_DIR` | `commands.rs:941` → `dashboard.rs:74` |
| `aroute donate` | READ | — | `commands.rs:942-947` |
| bare `aroute` (home view) | READ | — | `commands.rs:928` → `commands.rs:968` |

### 1.2 The column that matters: which config section each command can reach

The seven top-level sections are `server`, `keys`, `providers`,
`custom_providers`, `combos`, `oauth`, `limits`
(`crates/ar-config/src/lib.rs:1055-1119` — the `Config` struct fields at
`:1059`, `:1067`, `:1070`, `:1078`, `:1081`, `:1085`, `:1118`).

**The write-surface evidence.** Every config write in the workspace goes
through one of two helpers, `splice_section`
(`crates/ar-cli/src/commands.rs:2243-2274`) and `remove_section`
(`crates/ar-cli/src/commands.rs:2221-2238`). The complete list of call sites:

- **CLI** — `commands.rs:808` (`limits set`), `commands.rs:903`, `:905`,
  `:911` (`limits clear`), `commands.rs:2098` (`sync` → `combos:`),
  `commands.rs:2100` (`sync` → `custom_providers:`), `commands.rs:2150`
  (`sync` → `limits:`), `commands.rs:2418` (`sync` → `keys:`),
  `commands.rs:2419` (`sync` → `providers:`).
- **Panel** — `webui.rs:685` (`keys:`), `:1332` (`server:`), `:1348`
  (`limits:` default), `:1606` (`combos:`), `:1785` (`providers:`),
  `:2144` (`limits:`), `:2279` (`custom_providers:`), `:2345` + `:2365` +
  `:2390` (`providers:`/`custom_providers:`/`keys:` on provider removal).

`aroute import` is the exception: it does not splice, it **writes a fresh
`config.yaml`** into `--out-dir` (`commands.rs:2018`, renderer
`crates/ar-cli/src/import/mod.rs:353-398`).

| Config section | CLI verb that can reach it | Panel endpoint | Gap |
|---|---|---|---|
| `keys:` | `limits`/`import`/`sync` (whole-section rewrite). No targeted add/remove of one key row. | `POST /ui/api/provider-key`, `POST /ui/api/provider-custom` (`webui.rs:685`) | targeted key-row verb missing |
| `providers:` | **add a row: no CLI verb.** `sync` can only attach a `keys:` list to a row that already exists (`commands.rs:2410-2412` returns early when the block is empty; an unmatched dashboard provider is ignored). `import` writes a whole new file with one row per registry provider (`import/mod.rs:379-387`). **remove a row: no CLI verb.** | `POST /ui/api/provider-key` adds a row (`webui.rs:1771-1789`); `POST /ui/api/provider-remove` removes one (`webui.rs:2399`) | **GAP 3** |
| `custom_providers:` | `import` (when the source has ad-hoc nodes); `sync` (only if the dashboard has such rows, `commands.rs:2099-2106`) | `POST /ui/api/provider-custom` (`webui.rs:2279`) | targeted add missing from the CLI |
| `combos:` | `import` (whole file); `sync` (**whole-section replace** from the dashboard DB, `commands.rs:2098`) | `POST /ui/api/combos` (whole-section replace, `webui.rs:1606`) | no targeted single-combo edit anywhere |
| `limits:` | `aroute limits set` (`commands.rs:808`), `aroute limits clear` (`commands.rs:903`/`:905`/`:911`), `sync` (**rpm arm only**, `commands.rs:2150` via `import/limits::read_rpm`) | `POST /ui/api/limits` (`webui.rs:2144`), `POST /ui/api/settings` (default row only, `webui.rs:1348`) | — |
| `server:` | **no CLI verb.** `aroute serve --port` overrides at runtime and persists nothing. | `POST /ui/api/settings` (host + port only, `webui.rs:1305-1335`) | **GAP 2** |
| `oauth:` | **no CLI verb and no panel endpoint.** | none | **GAP 1** |

### 1.3 The three gaps, stated precisely

**GAP 1 — `oauth:` is hand-edit only.** There is no `splice_section(…,
"oauth")` call site anywhere in `crates/` (the complete list is §1.2, and
"oauth" is not on it). `crates/ar-cli/src/auth.rs` contains no `std::fs::write`,
no `splice`, no `write_file` — `aroute auth login` **reads** the `oauth:`
block and writes only to the credential store (`auth.rs:566` →
`persist_token`). `import::render_yaml` never emits an `oauth:` section
(`import/mod.rs:353-398` emits `keys:`, `providers:`, optionally
`custom_providers:`, then `combos:`). The panel has 14 routes
(`webui.rs:319-343`) and none of them is an oauth route. Consequence: the
operator's "guide the user through login for oauth" half of the goal cannot
even begin from the CLI, because the CLI cannot create the `oauth:` block the
login reads. **And a second, sharper consequence:** `aroute import` writes
`config.yaml` wholesale (`commands.rs:2018`) from a renderer that never emits
`oauth:` — so running `aroute import --out-dir <dir>` against a directory
whose `config.yaml` carries an `oauth:` block **silently deletes that block**,
by construction. That is the same failure class as B-1 below, on a different
writer.

**GAP 2 — `server:` is reachable only through the panel.** The only splice of
`server:` in the workspace is `webui.rs:1332`, inside
`POST /ui/api/settings`, which writes `server:` host + port only
(`webui.rs:1305-1335`; an empty host or a port of `0`/out-of-range is refused
before the file is touched, `:1312-1325`). No CLI command writes it. An agent
that must move the listen address has to drive a browser form or hand-edit.

**GAP 3 — a `providers:` row can be ADDED only by the panel.** The surgical
add is `webui.rs:1771-1789`: the row, the `keys:` reference and the
`AR_KEY_*` env line land together. On the CLI side, `sync` refuses to add a
row (`commands.rs:2410-2412`) and `import` produces a whole new file rather
than a targeted edit. The panel's `POST /ui/api/provider-remove`
(`webui.rs:2399`) is likewise the only row-level delete, and it refuses while
a combo still references the provider (`webui.rs:2436-2440`).

**What this means for the operator's goal.** "Agents can update everything
through the CLI without manually configuring the config files" is false today
in three places, and one of them (`oauth:`) is the exact half of the goal the
operator singled out ("guide the user through steps like login for oauth").

---

## 2. The findings, severity-ranked

The recon produced eleven findings. This section keeps their ranking and
gives each the agent's failure mode, the evidence, and the severity.

### B-1 — `splice_section` / `remove_section` silently DELETE any top-level YAML section whose name contains a hyphen — **BLOCKING**

**Evidence.** `is_section_header` (`crates/ar-cli/src/commands.rs:2276-2285`):

```rust
fn is_section_header(line: &str) -> bool {
    !line.starts_with([' ', '#'])
        && line.ends_with(':')
        && line[..line.len() - 1]
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}
```

`remove_section` (`commands.rs:2221-2238`) and `splice_section`
(`commands.rs:2243-2274`) both use it to find the **END** of the section
being replaced. A header like `api-keys:` is not a "section header" by that
rule, so the skip never terminates there and the section **plus everything
under it** is swallowed.

**Reproduction (orchestrator, 2026-10-20).** A config whose `api-keys:`
section follows a `limits:` section, then `aroute limits clear`, loses
`api-keys:` and its body entirely. The same holds for `aroute limits set`.
The recon measured the same shape with a synthetic `my-notes:` section.

**Agent's failure mode.** The agent asks "add a spend cap", runs
`aroute limits set --rpm 99`, and the command reports success
(`commands.rs:810-823` prints the new row). The agent then reads the config
back and a section it did not touch is gone. The agent's next act — mint a
token, point a harness at a provider, ask the panel for state — fails on a
credential that was there a second earlier, and the causal chain runs through
the *wrong* command. The write is silent: `write_file`
(`commands.rs:2596-2604`) rotates a backup and renames, so the file on disk is
smaller and there is no error.

**Blast radius — every splicing write path.** CLI: `limits set`
(`commands.rs:808`), `limits clear` (`commands.rs:903`, `:905`, `:911`),
`sync` (`commands.rs:2098`, `:2100`, `:2150`, `:2418`, `:2419`). Panel: the ten
splice sites in `webui.rs` listed in §1.2 — `keys:`, `providers:`,
`custom_providers:`, `combos:`, `limits:`, `server:` writes can each eat an
adjacent hyphenated section. Note that `aroute import` is **not** in this
blast radius: it writes a whole file rather than splicing.

**Current blast radius — LATENT, not live.** No hyphenated top-level section
exists in the repo's `config.yaml` (its top-level keys are `server`, `keys`,
`providers`, `combos` — `config.yaml:15`, `:21`, `:28`, `:95`) nor in the
VPS's `~/.config/ar/config.yaml`. And `ar_config::Config` does **not**
`deny_unknown_fields` (`crates/ar-config/src/lib.rs:1055-1119` — every field
carries `#[serde(default)]` and the struct has no `deny_unknown_fields`
attribute), so the loader happily accepts a config carrying one. A hyphenated
section is therefore *possible* today and *destructive* the moment it exists.
This is a latent data-loss bug, not a live one — and it becomes live the
moment an operator adds one section.

**The fix rule.** A top-level YAML key may contain a hyphen — `api-keys:` is
valid YAML and `serde_yaml` parses it — so the character class in
`is_section_header` must admit **every character a YAML plain key may
contain**, not a hand-picked list. Note that the current predicate is
`is_ascii_lowercase` (lowercase only) plus digits and `_`, so it also swallows
an **uppercase** section header such as `Providers:` — the class is wrong in
both directions. The minimal correct change is the one predicate at
`commands.rs:2279-2285`.

**The regression test that pins it.** A config whose **hyphenated section
follows the spliced one**, spliced, then assert the hyphenated section and
its body are **byte-identical** before and after. See §6 step 1 for the exact
test.

### B-2 — No usage / cost-report verb exists, yet the code and the MCP both name one — **BLOCKING**

**Evidence — nine stale references to a command that does not exist:**

- `crates/ar-cli/src/webui.rs:815` — *"the same rows `aroute cost-report`
  prints"*.
- `crates/ar-tokens/src/ledger.rs:368`, `:385`, `:398`, `:601`, `:1059` — five
  doc comments describing an `ar cost-report` data shape.
- `crates/ar-server/src/routes.rs:139`, `:3201`, `:4994` — three more
  references to `ar cost-report`.

The `Command` enum (`cli.rs:38-209`) has no `usage`, `cost-report` or `check`
variant; there is therefore no such subcommand for clap to dispatch, and any
attempt to run one exits 2 with `error: unrecognized subcommand 'usage'`.

**The data is reachable from the CLI's own crate.** `ar-tokens` is a direct
dependency of `ar-cli` (`crates/ar-cli/Cargo.toml:54`), and
`ar_tokens::Ledger::report(limit)` (`crates/ar-tokens/src/ledger.rs:1064`)
returns a `CostReport` whose `toon()` (`ledger.rs:406`) is the renderer the
MCP already uses (`crates/ar-mcp/src/transport.rs:546-557` calls
`crate::tools::cost_report` → `ledger.report`, `ar-mcp/src/tools.rs:476-478`).
The ledger path is already resolved once in the CLI: `serve.rs:123-126` puts
`usage.sqlite` beside the credential store and opens it at `serve.rs:127`.

**Agent's failure mode.** The agent asks "how much have we spent this week?"
The only surfaces that can answer are the browser panel
(`GET /ui/api/usage`, `webui.rs:330`) and the MCP tool `ar_cost_report`
(`ar-mcp/src/lib.rs:200`). The CLI — the surface the operator wants agents to
use — is the **only** agent surface that cannot see spend. The agent either
opens a browser or asks a human, and the spend cap it is trying to reason
about is exactly the thing it is blind to.

**Naming decision — `cost-report`, not `usage`.** The evidence is one-sided:
the code already calls it `aroute cost-report` (`webui.rs:815`), the ledger's
own type is `CostReport` (`ledger.rs:388`), the MCP tool is `ar_cost_report`
(`ar-mcp/src/lib.rs:200`), and `docs/06-axi-mcp.md:35` lists it as tool #6.
Naming the CLI verb `usage` would create a second spelling for one thing and
leave all nine existing references false. **Recommendation: `aroute
cost-report`**, with `--limit` and an optional `--key` filter. (`usage` is the
word a human reaches for, but the word the *code* reaches for is
`cost-report`, and the plan's job is to make the code's references true.)

### B-3 — `aroute limits set` writes the file and the running proxy never sees it, and the command says nothing — **BLOCKING**

**Evidence.** `ConfigHandle::watch` (`crates/ar-config/src/lib.rs:1787-1832`)
has **zero call sites** in the workspace — grepping for `.watch(` returns only
the definition's own body at `crates/ar-config/src/lib.rs:1823`. Nothing
attaches the watcher. `ar-server` builds its `Policy` **once at boot**
(`crates/ar-server/src/app.rs:396-400`) and never rebuilds it; the module
doc in `crates/ar-cli/src/webui.rs:27-29` says so plainly ("Boot-built routing
means a write changes the *file*, not the running tables"), and the panel
answers every save with `pending_restart` (`webui.rs:761`).

**`sync` gets this right; `limits set` and `import` do not.**
`commands.rs:2190-2212` is the model: it says `service: aroute.service
restarted — the synced config is live`, or `note: apply with `systemctl
--user restart aroute.service``, and it offers `--no-restart`
(`commands.rs:2195-2198`). `limits set` prints only the row summary
(`commands.rs:810-823`) and `limits clear` prints only
`limits: key "x" removed` (`commands.rs:908`).

**The help text states the opposite.** `crates/ar-cli/src/cli.rs:147-149`:
*"config.yaml is picked up live (ar-config watches it)"*. That is false —
there is no call site. An agent that reads the help, arms a spend cap with
`limits set`, and immediately probes is measuring a proxy that ignores the
cap. This is the C2 clause, and it is the one failure where the CLI actively
misleads rather than merely omits.

**The contract this plan proposes.** Every command that writes `config.yaml`
states, in its own output, that the change is **not live until the proxy
restarts**, and names the restart command. The wording is `sync`'s
(`commands.rs:2196-2198`, `:2207`, `:2210`) so there is one sentence in the
product, not three.

**Who restarts — the answer is both, and the CLI must be explicit about it.**
`aroute sync` already restarts `aroute.service` itself through
`systemctl --user restart` (`commands.rs:2200-2205`) unless `--no-restart` is
passed. For the other writers the plan's recommendation is: **the CLI offers
the same `--no-restart` switch and, by default, performs the restart itself
when it can**, falling back to the printed instruction when the restart
fails or the unit is not under systemd --user control. That makes
`limits set` one command instead of a command plus a reminder the agent has to
remember — which is the operator's whole point.

**A related falsehood worth noting.** `cli.rs:14` claims the config path is
"overridable with `--config` or `AR_CONFIG`", but there is **no `env =`
attribute anywhere in `cli.rs`**, and `ar_config::Config::load` takes an
explicit path (`crates/ar-config/src/lib.rs:1749`). `AR_CONFIG` is an
*installer* variable (`install.sh:88`), not a runtime one. The same class of
false claim as `cli.rs:147-149`, smaller blast radius; it is fixed in step 3
because it is the same edit.

### B-4 — `aroute auth login` cannot resume across a process boundary — **BLOCKING**

**Evidence.** The CLI flow is `crates/ar-cli/src/auth.rs:419-570`: build the
PKCE request → print `url: <authorize url>` (`auth.rs:484`) → **wait
in-process** on a loopback listener or one line of stdin (`auth.rs:510-516`,
`read_pasted_code` at `auth.rs:582-608`) → exchange (`auth.rs:555-564`) →
persist (`auth.rs:566`). The PKCE **verifier** lives in `request.verifier` in
process memory (`auth.rs:478`, consumed at `auth.rs:555-564`). There is no
pending-login store, no session id, no `--session`. Kill the process and the
login is unrecoverable: the human has to restart consent from zero.

The device flow (`auth.rs:359-416`) is friendlier — it prints `code:` +
`verification_uri:` and polls (`auth.rs:392-406`) — but is still bound to one
process for `args.timeout` (`auth.rs:410`).

**The MCP already has exactly the pair the CLI lacks.**

- `ar_auth_login_url` → `{provider, session_id, authorize_url, redirect_uri,
  expires_in_secs}` (`crates/ar-mcp/src/transport.rs:594-611`; the pending
  table and its `start` at `crates/ar-mcp/src/login.rs:150-196`).
- `ar_auth_complete(session_id, code_or_url)` (`crates/ar-mcp/src/transport.rs:649-651`,
  and `:656-665` for the two required arguments) — redeem, persist, verify.
- `LOGIN_TTL = 300 s` (`crates/ar-mcp/src/login.rs:45`), deliberately the
  CLI's own `--timeout` default (`crates/ar-cli/src/cli.rs:554-556`;
  `ar-mcp/src/login.rs:41-44` says the pairing is deliberate in both
  directions).

**Agent's failure mode — the human-in-the-loop handoff.** Today the agent
must hold a process open with a live stdin pipe for the whole human
round-trip. It is *possible* — pipe plus a background writer — but it is a
blocking human wait inside the agent's process: the agent's turn does not
finish until a person finishes a consent screen, and any crash, timeout, or
harness restart loses the login entirely. The agent cannot hand the task to
the human and come back.

**The shape the CLI should take, matching the MCP pair.** Two subcommands
under `aroute auth login`, sharing the `oauth:` block's session:

1. `aroute auth login --provider X --start` — builds the authorize request,
   prints `session id: <uuid>`, `url: <authorize url>`, `redirect uri: …`,
   `expires in: 300s`, persists the PKCE verifier + `state` + provider
   **on disk**, and exits 0.
2. `aroute auth login --complete --session <id> --code-or-url <s>` — reads
   the pending login, checks the TTL and the `state`, redeems, persists the
   two store rows, verifies by reading them back, exits 0.

The verifier is secret material at rest, so it must be **encrypted under
`$AR_MASTER_KEY`** like every other secret and **single-use** — the MCP's
in-memory table gets single-use for free by being a `HashMap` entry that
`take` removes (`ar-mcp/src/login.rs:204-208`); the CLI's on-disk store has to
earn it explicitly. Note the MCP's pending table is deliberately **in memory
and in-process** (`ar-mcp/src/login.rs:154-155`: "a restart drops a pending
login rather than leaving a redeemable verifier on disk"), so the CLI cannot
reuse the MCP's storage — it needs an encrypted, single-use, TTL-pruned row
of its own.

The device flow does not need splitting: it already prints the two lines a
human needs and polls. The plan's scope is the PKCE redirect path.

The `--no-browser` one-line-stdin path (`auth.rs:582-608`, `cli.rs:100-109`)
must keep working — it is the documented headless flow, and the plan keeps it
as the *default* single-process mode with the new `--start`/`--complete` pair
as the resumable alternative.

### B-5 — The `keys` subtree cannot INSPECT any minted token, and `arm-gate` with no `--key` silently OVERWRITES the master — **BLOCKING**

**No inspect verb.** `KeysCommand` (`crates/ar-cli/src/cli.rs:292-344`) has
exactly three verbs: `arm-gate`, `mint`, `revoke`. There is no `list`. And
there is **nothing to list**: `ar-keys` keeps no table of issued tokens — its
schema is `store_meta`, `credentials`, `oauth_sessions`, `quota_snapshots`,
`revoked_tokens` (`crates/ar-keys/src/store.rs:168-215`,
`crates/ar-keys/src/revocations.rs:197-213`). The only token-related table is
`revoked_tokens` (`revocations.rs:198-201`). `CredentialStore::list_names`
(`store.rs:790`) enumerates **credential names** (the `keys:` labels), not
tokens. `aroute doctor` reports a store row with only a **count**
(`commands.rs:228-241`: `"{n} readable with {m} credential(s)"`). The MCP's
`ar_auth_status` lists stored credential **names**
(`ar-mcp/src/transport.rs:612-623`). **Nothing anywhere enumerates minted
gate tokens, their key ids, their scopes, or their expiry.**

**Agent's failure mode.** The agent cannot answer "which tokens are live?",
"which one is the harness using?", "when does it expire?" — and, because
`keys revoke` requires the token itself (`--token-file`, `cli.rs:346-355`) and
the store keeps no issued-token table, **a lost token is unrevokable**. The
only lever is rotating the master, which invalidates every other token at
once. The plan's honest scope: `aroute keys list` reports what the store
holds (row names), what has been revoked (count), and the gate's own state —
an honest answer, not a fake inventory. [INFERENCE] A true inventory of
issued tokens would require a new table in `ar-keys`, which is a larger change
than this plan's scope; §5 D-4 raises it as an operator decision.

**`arm-gate` overwrites the master with no warning.** `arm_gate`
(`commands.rs:378-417`): with no `--key`, it generates a **fresh random**
master (`commands.rs:385-386`), then `store.insert(GATE_ROW, …)`
(`commands.rs:399-406`) — and `store.insert` is an upsert
(`crates/ar-keys/src/store.rs:263-266`: `ON CONFLICT(name) DO UPDATE SET
provider = ?2, envelope = ?3`). The previous master is gone. Every token
signed by it is now unverifiable.

**The CLI's own `--help` does not mention it.** The full `arm-gate` doc
comment (`cli.rs:293-306`) says only: *"generate a 32-byte master key into the
credential store, then mint and print the first client token … `--key <hex>`
stores an explicit 32-byte master instead of generating one."* There is no
mention of overwriting an existing master, of invalidating every existing
token, or of the `AR_HTTP_MASTER_KEY` desynchronisation. The blast radius is
documented **only** in `doctor`'s *failure* row (`commands.rs:1324-1325`:
"re-arm with `aroute keys arm-gate --key $AR_HTTP_MASTER_KEY`") and in project
memory — not where an agent or an operator would look before running the
command.

**The master precedence makes it worse.** The store row **beats**
`AR_HTTP_MASTER_KEY`: `serve.rs:152-160` sets `Components.master_key` from
the store, and `Components.master_key` wins at
`crates/ar-server/src/app.rs:385-391`; the MCP resolves the same order
(`crates/ar-cli/src/mcp.rs:169-182`: store row → `AR_HTTP_MASTER_KEY` →
none). So on a host where both are set, `arm-gate` without `--key` silently
makes the server and the environment disagree — **exactly the shape of the
2026-10-19 production incident** (`AR_HTTP_MASTER_KEY` and the store's
`http-gate` row must be the same 32 bytes; `arm-gate` without `--key` mints a
fresh one). This is the operator's "safely handle api keys management" half
of the goal, and the failure is silent.

### D-1 — `aroute models` is UNBOUNDED — **DEGRADING**

`model_rows` (`commands.rs:1035-1079`) appends **every model in the
compiled-in registry** after the combo targets (`commands.rs:1064-1075`,
status `known`). The registry carries **276 providers / 1,559 models**
(`crates/ar-registry/src/registry.json`).

**Measured** (recon, two-combo fixture): `count: 1559 of 1559 total`,
**71,974 bytes / 1,561 lines**; `--fields id` → 46,565 bytes / 1,561 rows;
`--fields combo` → 6,305 bytes / 1,561 rows — still 1,559 rows.

`toon::list` truncates **cells** at 1,000 chars (`crates/ar-cli/src/toon.rs:23`,
`:97-104`) but never bounds **row count** (`toon.rs:70-81` loops over every
row). `ListArgs` has only `--fields` and `--full` (`cli.rs:236-244`) — there
is no `--combo`, `--provider`, `--routable` or `--limit`. And
`count: N of TOTAL` always has `TOTAL == N`, because there is no pagination
(`toon.rs:64-67` says so explicitly: "There is no pagination at P0, so it
equals the page size"). The count line therefore tells an agent nothing beyond
"this is all of them".

**Agent's failure mode.** "Which models can I route to?" costs 72 KB — the
agent's whole context, for a question whose answer is 16 rows
(`commands.rs:1037-1063`, the combo targets). The `status` column *does*
distinguish `routable` from `known` (`commands.rs:1040-1043`), so the
information is present — but only reachable by reading and filtering 1,559
rows itself.

### D-2 — `aroute doctor` is UNBOUNDED and has no `--fields` — **DEGRADING**

`Doctor` (`cli.rs:71-77`) takes **no arguments at all**. `findings`
(`commands.rs:1250-1480`) emits one row per provider, oauth provider, auth
provider, custom provider, **key** (`commands.rs:1452-1460`), **target** and
**pool** entry, plus 5 fixed rows. `--fields` exists only on
`models`/`providers`/`combo` (`cli.rs:236-244`; validated in `check_fields`,
`commands.rs:2690-2700`).

**Measured** (recon): 12 rows / 796 bytes on the fixture; **209 rows / 9,032
bytes** with 200 `keys:` rows. The VPS has **335 `AR_KEY_*` vars**, so the
real number is higher — [INFERENCE] the VPS's row count exceeds the synthetic
209 (the recon's 200-key measurement is a floor, and the VPS also has 16
combos with multiple targets and pools).

**Agent's failure mode.** "Is this install healthy?" returns a table the
agent cannot narrow, with the `fail` rows scattered through it. `doctor` does
exit 1 when any check fails (`commands.rs:1234-1240`), so the agent learns
*that* something failed but not *which* without parsing 209 rows.

### D-3 — Errors are prose-only, and the two error streams land on different streams — **DEGRADING**

- **Exit codes are correct**: `0` success, `1` error, `2` usage
  (`crates/ar-cli/src/main.rs:34-38`; `usage_exit_code` at
  `commands.rs:2675-2682`).
- **Handled errors go to stdout** as `error: {msg}\nhelp: {fix}`
  (`main.rs:113-116`; `fail()` at `commands.rs:66-69`). Good — `docs/06:12`
  wants stdout to carry data *and* errors.
- **Clap parse errors go to stderr** (`main.rs:88-89`). The recon measured an
  unknown **subcommand** producing **0 bytes on stdout**; an unknown **flag**
  additionally gets a stdout line naming the valid set
  (`main.rs:93-98` → `unknown_flag_hint`, `commands.rs:2711-2747`), because
  clap's own text names the flag but not the valid set. So "the agent reads
  stdout" is true for handled errors and **false for one whole class**.
- **There is no error code anywhere.** `ar_config::ConfigError` is a rich enum
  — `Read`, `Expand`, `Yaml`, `EmptyApiKeys`, `UnknownKey`, `BadOAuthUrl`,
  `ZeroLimit`, `BadLimitWindow`, `Watch`, `ContradictoryOAuth`
  (`crates/ar-config/src/lib.rs:42-130`) — but `commands::load`
  (`commands.rs:90-100`) flattens it through `fail(e, help)` → `Display`
  only. The agent cannot distinguish "config does not parse" from "provider
  not configured" from "credential rejected" without English matching. The
  only structured codes in the system live on the **server**
  (`crates/ar-server/src/routes.rs`, `error_because` → `"rate_limited"`,
  `"unpriced"`) and in the **MCP** error enum
  (`crates/ar-mcp/src/lib.rs:48-137`).

### D-4 — No machine-readable command list — **DEGRADING**

`aroute --help` is clap-rendered English prose. There is **no `--json` flag
anywhere in `crates/ar-cli`** (grep for `--json|json_output|as_json` → no
matches) and **no `commands`/`list` subcommand** — the `Command` enum has
neither, so `aroute list` exits 2 with `error: unrecognized subcommand 'list'`
(and `aroute combos` → the same; the real verb is `combo`, singular).

The one machine-readable catalog, `aroute mcp --list`
(`crates/ar-cli/src/mcp.rs:220-255`, a TOON table of tool / scope /
description), is behind `#[cfg(feature = "mcp")]` (`cli.rs:186`) and the
feature is **off by default** (`crates/ar-cli/Cargo.toml:23-25`) and **not
enabled by the release build** (`.github/workflows/release.yml:129-136` runs
`cargo build --release --locked --target x86_64-unknown-linux-musl --bin
aroute` with **no `--features`**; `grep 'features' release.yml` → no
matches). **The orchestrator confirmed the deployed binary answers `error:
unrecognized subcommand 'mcp'`.** So the *shipped* binary has no
machine-readable surface at all, and `ar_tool_search`
(`crates/ar-mcp/src/lib.rs:308-318`) is reachable only over MCP.

Help text IS width-stable (the recon measured an identical `--help` md5 at
`COLUMNS=40` and `COLUMNS=100`), so it is *parseable* — but it is prose, not
a schema.

### D-5 — `aroute models|providers|combo` names a whole catalog, not what the config can route — **DEGRADING**

`model_rows` marks every registry model `known` (`commands.rs:1064-1075`),
which is the same data as D-1 seen from the answer side. `providers` and
`combo` are config-sized and fine (2 rows / 97 bytes measured). The `status`
column distinguishes `routable` / `known` / `unknown-provider` /
`unknown-model` (`commands.rs:1040-1043`), so the *signal* exists — the
*cost* of getting it does not.

### C-1 — `limits` and `auth status` are well-behaved — **COSMETIC**

`limits` (`commands.rs:616-664`): 8 columns (`commands.rs:48-57`), one row per
key plus a `default` row; measured 2 rows. It has no `--fields` (it uses
`toon::every_field`, `toon.rs:129-131`), so all 8 columns always print.
`auth status` (`auth.rs:710-763`): 7 columns, one row per declared `oauth:`
session; measured `sessions: 0 sessions found` — the definitive-empty form
(`toon.rs:54-56`). Both are bounded by config size and both carry a `count:`
line. **No action.**

---

## 3. Where the code has drifted from `docs/06-axi-mcp.md`

`docs/06-axi-mcp.md` is **normative**. The plan in §4 is consistent with it.
These are the places the code no longer matches it, and the plan names which
one it fixes and which one it records.

| # | `docs/06` says | The code does | Plan |
|---|---|---|---|
| DR-1 | `docs/06:16` — `aroute setup claude\|codex\|opencode` installs a `SessionStart` hook and generates a `SKILL.md` | **No such command exists** (grep for `setup`/`Setup` in `crates/ar-cli/src` → no matches) | Not in this plan's scope; recorded as drift. It is an `install.sh`/harness-wiring concern, not a config-section one. |
| DR-2 | `docs/06:4` — "The count is `ar_mcp::Tool::ALL.len()`; `aroute mcp --list` prints it" | The release binary has **no `mcp` subcommand** (`release.yml:129-136`, `ar-cli/Cargo.toml:23-25`; the deployed binary answers `error: unrecognized subcommand 'mcp'`) | Step 6 decides this. A CLI-side catalog that exists in **every** build removes the dependency of the catalog on the feature. |
| DR-3 | `docs/06:18` — "MCP catalog (P1, `--features mcp`, default off)" | The feature is default-off **and the release build never enables it**, so the MCP surface is not in the shipped product at all | Step 6, operator decision D-2. |
| DR-4 | `docs/06:10` — "bodies truncated 500-1500 chars with `(truncated, TOTAL chars)`" | The **body** path uses `(truncated, N chars)` (`toon.rs:117`) but the **list-cell** path uses `(truncated: N chars)` (`toon.rs:103`) — a colon where the doc has a comma | Cosmetic; the list marker deliberately avoids the comma (`toon.rs:89-95`) because a comma in a cell widens the row. Recorded, not changed. |
| DR-5 | `docs/06:10` — "`--full` hatch only when truncated" | In the **list** path `--full` only suppresses the hint line; values are **still clipped at 1,000 chars** (`toon.rs:70-84`, `clip()` is unconditional). Only the **body** path (`aroute run`) returns the untruncated text (`toon.rs:111-118`) | Step 7 decides whether `--full` means what the doc says; the honest options are "fix the list path" or "amend the doc". |
| DR-6 | `docs/06:12` — "stdout = data+errors+suggestions. stderr = progress/diagnostics" | Clap parse errors go to **stderr** (`main.rs:88-89`), and an unknown subcommand produces **0 bytes on stdout** | Step 6 adds the stdout mirror for the whole clap class. |
| DR-7 | `docs/06:13` — "No interactive prompts — missing flag fails with usage" | `aroute auth login` performs a **blocking single-line stdin read** (`auth.rs:582-608`) — an interactive prompt | Step 4 adds the resumable pair; the single-process stdin path stays as the documented headless mode, and the doc's "no prompt" clause needs an explicit carve-out for it. Recorded in step 4. |
| DR-8 | `docs/06:14` — "bare `ar` prints `bin: ~/.../ar + one-line description + live combos/quota`" | The home view (`commands.rs:968-995`) prints `bin:`, version, `config:`, `listen:`, `registry:`, the combos table and three hints — **no quota** (there is no quota reader in the CLI at all: B-2). The spelling is `aroute`, not `ar` (`crates/ar-cli/src/version.rs:22`; the `ar` binary was removed, `crates/ar-server/Cargo.toml:12-14`) | Step 2 adds the spend row to the home view; the `ar` spelling is a doc-vs-code drift recorded here (the MCP footer also says `ar mcp`, `mcp.rs:252-253`). |
| DR-9 | `docs/06:16` and `docs/06:4` — the MCP catalog is a first-class surface | `install.sh:278`, `:287`, `:291-297` register `aroute mcp` with `claude` and `codex` and print MCP wiring instructions — **for a binary that cannot serve MCP** | Step 6. Either the release ships the feature, or the installer stops advertising it. Shipping an installer that wires a transport the artifact lacks is a defect in its own right. |

---

## 4. The plan of record

Eight shippable steps. Each names the contract clause it closes, the change,
the verification, and the rollback. The order is severity-first: the two
silent-corruption bugs (B-1, then the `import`/`oauth:` interaction) before
anything that adds surface.

Every step ships as a **GitHub release**, never as a copied binary
(project memory #226): commit → bump the workspace version in `Cargo.toml`
(`Cargo.toml:7`, currently `0.1.20`) and refresh `Cargo.lock` → a
`chore(release): vN` commit → an annotated tag `vN` → push `origin main` and
the tag → `.github/workflows/release.yml` builds and publishes → install on the
VPS via `ssh racknerd`.

### Step 1 — Fix the hyphen-section data loss (B-1) — **ship first**

**Change.**
1. `is_section_header` (`commands.rs:2279-2285`): widen the key's character
   class. The rule to implement: **a line is a top-level section
   header when it is unindented, is not a comment, ends with `:`, and its
   name is a legal YAML plain key** — the predicate must accept **every
   character a YAML plain key may contain**, not a hand-picked list such as
   "lowercase letters, digits, `_`, `-`, and `.`". The current class is
   `is_ascii_lowercase` plus digits and `_`, so it wrongly rejects `-` and
   `.` — and, in the other direction, it does not recognise an **uppercase**
   header such as `Providers:` either, so the skip runs past that header too
   and swallows it as well. The minimal
   correct edit is to widen the predicate; the
   durable correct edit is to stop hand-rolling the predicate and ask
   `serde_yaml` what the top-level keys are. **Recommendation: widen the
   predicate now** (one function, one blast radius) and leave the parser
   rewrite out of scope — the parser rewrite would change how comments are
   preserved, which is a different and larger change.
2. Add the same guard to the **panel** path implicitly (it shares the
   helper — no separate change).
3. `aroute import`: the renderer already never emits `oauth:`; the plan does
   **not** change the renderer, but the plan **records** that an import into a
   directory holding a hand-written `oauth:` block deletes that block. The
   mitigation that ships with step 1 is a **pre-flight refusal**: `import`
   reads the target `config.yaml` if it exists and **warns loudly** (on
   stdout, in the `error:`/`note:` vocabulary) that a whole-file replace will
   drop the sections it does not emit (`limits:`, `oauth:`, `server:`).
   A refusal (exit 2) is the stronger form; a loud warning is the weaker and
   shippable one. **Recommendation: warning in step 1, refusal as a decision
   in §5 DEC-5.**

**Verification.**
- The regression test (§6 step 1): a config with `api-keys:` **after** the
  spliced section, spliced, then assert the hyphenated section and its body
  are byte-identical.
- The existing splice tests must still pass (`commands.rs:2908-2928`).
- End-to-end: `aroute limits clear` against a fixture config with
  `api-keys:` after `limits:`; `aroute limits set --rpm 99` against one with
  `my-notes:` after `limits:`; assert the other section survives byte-for-byte.
- Panel: one `POST /ui/api/limits` against a fixture with a trailing
  hyphenated section; assert the file keeps it.

**Rollback.** One function. Reverting the commit restores the old predicate;
the backups `config.yaml.1`…`.3` (`commands.rs:2587`, `:2638-2641`) restore
any file that was already corrupted.

**Closes.** C1 (partially — it is the precondition for every other write
step), and the D-1/C-1 integrity of every write path.

### Step 2 — Add a `cost-report` verb over the ledger (B-2)

**Change.** A new `Command::CostReport` in `cli.rs:38-209` and one handler
arm in `commands::dispatch` (`commands.rs:926-961`). The handler opens
`ar_tokens::Ledger` at the path `serve.rs:123-126` resolves (`usage.sqlite`
beside the credential store) and renders `CostReport::toon()`
(`crates/ar-tokens/src/ledger.rs:406`) — **the same renderer the MCP uses**
(`ar-mcp/src/transport.rs:546-557`), so the two surfaces cannot drift.

Three details that matter:
- **Do not create the ledger file.** `Ledger::open` creates a missing file;
  a read-only command that leaves a new sqlite file behind is a command that
  lies about what it found. Probe first, mirroring `StoreProbe::resolve`
  (`commands.rs:206-221`), and print the definitive-empty form
  (`cost: 0 rows — no usage recorded`, `ledger.rs:408`) when there is no
  ledger.
- **Options:** `--limit N` (default 50, the MCP's default at
  `ar-mcp/src/transport.rs:549`) and `--key <key-id>` to filter to one key.
- **Home view:** add a spend line to the bare `aroute` view
  (`commands.rs:968-995`), which closes DR-8's "live combos/quota" clause.

**Verification.** A golden test against a seeded in-memory ledger, asserting
the CLI's output equals `ar_cost_report`'s rows for the same ledger. Plus a
test that the verb on a host with no ledger prints the definitive-empty form
and **does not create `usage.sqlite`**.

**Rollback.** One enum variant and one dispatch arm; removing it restores the
previous surface with no other behaviour changed.

**Closes.** C1 (the CLI can now reach the spend surface the panel and the MCP
both have), C4 (bounded by `--limit`), and B-2.

### Step 3 — The "not live until restart" contract on every write command, and the false help text (B-3)

**Change.**
1. **Correct `cli.rs:147-149`.** The sentence *"config.yaml is picked up live
   (ar-config watches it)"* is false — the watcher has no call site
   (`crates/ar-config/src/lib.rs:1787-1832`). The replacement states the
   truth: the proxy builds its routing tables at boot
   (`crates/ar-server/src/app.rs:396-400`), so a config edit lands after a
   restart.
2. **Correct `cli.rs:14`** (`AR_CONFIG` is not read; `crates/ar-config/src/lib.rs:1749`
   takes an explicit path).
3. **Every write command states the effect.** `limits set`, `limits clear`,
   `import` and `sync` each print one line naming the restart, using
   `sync`'s existing wording verbatim (`commands.rs:2196-2198`, `:2207`,
   `:2210`) so there is exactly one sentence in the product:
   - *`service: aroute.service restarted — the config is live`*, or
   - *`note: apply with `systemctl --user restart aroute.service` (or your
     deployment's restart)`*.
4. **Offer the restart.** `limits set`, `limits clear` and `import` gain the
   same `--no-restart` switch `sync` has (`cli.rs` `SyncArgs`), and by
   default perform `systemctl --user restart aroute.service` themselves
   (`commands.rs:2200-2205` is the pattern to copy), falling back to the
   printed instruction when the restart fails. **This is the "who restarts"
   answer: the CLI restarts when it can, and says so when it cannot.**
5. The panel already answers every save with `pending_restart`
   (`webui.rs:761`); the CLI's wording should match the panel's.

**Verification.**
- A test that `limits set` on a live config prints the restart line (and, with
  `--no-restart`, prints the instruction instead of restarting).
- A doc review for `cli.rs:14` and `cli.rs:147-149` — no test, because the
  claim is prose.
- An end-to-end check against a running proxy: `limits set` → restart → the
  proxy's behaviour changes; **without** the restart it does not. The second
  half is the assertion that the notice was not lying.

**Rollback.** Two `println!` lines and one doc-comment fix per command; the
`--no-restart` default flip is the only behavioural part and it is one
`if`.

**Closes.** C2, B-3, and DR-8's doc half.

### Step 4 — A resumable OAuth login in the CLI (B-4)

**Change.** Two subcommands under `aroute auth login`, matching the MCP pair
exactly:

- `aroute auth login --provider X --start` — build the PKCE request
  (`auth.rs:478`), print `session id:`, `url:`, `redirect uri:` and
  `expires in: 300s` (the TTL is the CLI's own `--timeout` default,
  `cli.rs:554-556`, matching `ar_mcp::LOGIN_TTL`, `ar-mcp/src/login.rs:45`),
  persist the verifier + `state` + provider **encrypted under
  `$AR_MASTER_KEY`**, and exit 0.
- `aroute auth login --complete --session <id> --code-or-url <s>` — load the
  pending login, enforce the TTL and the `state` check (the same check
  `read_pasted_code` does, `auth.rs:582-608`), redeem (`auth.rs:555-564`),
  persist the two store rows (`auth.rs:566`), verify by reading them back
  (`report_login`, `auth.rs:295-318`), and exit 0.

Storage requirements the MCP does **not** have to meet (its pending table is
in-memory and in-process by design, `ar-mcp/src/login.rs:154-155`): the
verifier is secret material at rest, so it must be encrypted under
`$AR_MASTER_KEY` like every other secret, **single-use**, and **TTL-pruned**
at read time (300 s). A session id is a v4 UUID, matching the MCP's choice
(`ar-mcp/src/login.rs:186`).

Keep the current single-process flow intact: `--no-browser` and the
one-line-stdin path (`auth.rs:582-608`) stay the default headless mode, so
nothing that works today breaks.

**The human-in-the-loop handoff, spelled out.** The agent runs:

```
aroute auth login --provider codex --start
# → session id: 3f2a…  url: https://…  expires in: 300s
```

then **hands `url` to the human** and **ends its turn** — the process exits,
the verifier is on disk, and nothing is lost if the agent's process dies.
The human completes consent anywhere and pastes the redirected URL (or a bare
code) back to the agent. The agent then runs:

```
aroute auth login --complete --session 3f2a… --code-or-url "https://…?code=…&state=…"
```

and gets exit 0 plus the armed/failed verdict. The whole round trip is now
two agent calls with a durable handle between them — C3 satisfied, and the
operator's "guide the user through steps like login for oauth" met.

**Verification.** Start a login, kill the process, complete it from a **new**
process with the session id; assert the verifier is single-use (a second
`--complete` with the same id fails) and that it expires at 300 s. Plus: the
existing `--no-browser` stdin path still passes.

**Rollback.** New subcommands and a new on-disk store; removing them restores
the current single-process flow untouched. The store file is additive — a
host that never uses `--start` never writes one.

**Closes.** C3, B-4, and the `oauth:` half of C1 (step 8 still owes the
`oauth:` block itself).

### Step 5 — A `keys` inspect surface, and an `arm-gate` that cannot silently overwrite (B-5)

**Change.**
1. **`aroute keys list`** — report, honestly:
   - the credential **row names** the store holds (`CredentialStore::list_names`,
     `ar-keys/src/store.rs:790`),
   - the OAuth sessions the store knows (`oauth_sessions`,
     `ar-keys/src/store.rs:180-189`),
   - the revocation table's count (`ar-keys/src/revocations.rs:168`,
     `:208`),
   - the gate's own state: armed / not armed / armed-but-unreadable
     (`commands.rs:1321-1352`).
   It must **not** print tokens — there is no table of issued tokens to read,
   so a token list would be a fabrication. The output says so explicitly:
   *"minted tokens are not recorded; a lost token cannot be revoked"*.
2. **`arm-gate` refuses to overwrite an existing `http-gate` row without
   `--force`.** When it does overwrite (with `--force`), it prints exactly
   what it invalidated: *"the previous master is replaced; every token signed
   by it stops working"*. The `--key` path stays available — an operator
   bringing the store into line with `AR_HTTP_MASTER_KEY` is doing exactly
   that on purpose — but it goes through `--force` too, and `doctor`'s repair
   text (`commands.rs:1324-1325`) is updated to name the new flag.
3. **`arm-gate`'s own `--help`** (`cli.rs:293-306`) states the consequence in
   the command's own words: that omitting `--key` generates a **new** master,
   that an existing master is **replaced** unless `--force` is given, that
   every token signed by the old master stops verifying, and that the store
   row **beats** `AR_HTTP_MASTER_KEY` in the resolution order
   (`serve.rs:152-160` → `ar-server/src/app.rs:385-391`; the MCP resolves the
   same order, `crates/ar-cli/src/mcp.rs:169-182`).

**Verification.**
- `arm-gate` twice: the second **refuses** without `--force`; with `--force`
  it overwrites and prints the invalidation warning.
- `keys list` on a seeded store: the row names appear; no token material
  appears; the "minted tokens are not recorded" line appears.
- `doctor`'s repair text names `arm-gate --force --key …`.

**Rollback.** The `arm-gate` refusal is one `if`; `keys list` is one new
subcommand. Both are additive.

**Closes.** C1 (the `keys` subtree can now be inspected), and the "safely
handle api keys management" half of the operator's goal.

### Step 6 — A machine-readable output mode, a command catalog, and the release-feature decision (D-4, DR-2, DR-3, DR-9)

**Change.**
1. **`aroute commands --json`** — a catalog derived from the clap tree
   (`Cli::command()` is already reachable; `commands.rs:2712` walks it for
   `unknown_flag_hint`). Emit `{name, about, args:[{name, takes_value,
   default, possible_values}], subcommands}`. Add `--search <term>`, the CLI
   twin of `ar_tool_search` (`ar-mcp/src/lib.rs:308-318`).
2. **`--json` on the list verbs** — `models`, `providers`, `combo`, `doctor`,
   `limits`, `auth status`, and the new `cost-report`. **This extends
   `docs/06:8-13`, which is normative** — the doc's own heading is "TOON on
   stdout, JSON inside", and a CLI JSON mode widens it. So step 6 **must
   amend `docs/06`** to state the new rule: TOON stays the default; `--json`
   is the machine-readable opt-in. The amendment is part of the step, not a
   follow-up.
3. **Error codes.** A small closed set riding the existing error line —
   `config_unreadable`, `config_unparseable`, `var_unresolved`,
   `unknown_key`, `gate_not_armed`, `gate_master_unreadable`, `store_absent`,
   `field_unknown`, `not_routable` — mapped onto `ar_config::ConfigError`'s
   existing variants (`crates/ar-config/src/lib.rs:42-130`) and
   `ar_keys::KeyError`. Printed as `error: <code>: <message>` + `help: <fix>`
   on stdout (`commands.rs:66-69`). **The code is added to the existing line,
   not substituted for it**, so the many `help:` assertions keep holding; the
   exit-code contract stays `0/1/2` only (`main.rs:7`, `:34-38`).
4. **The clap error stream.** An unknown **subcommand** currently produces
   0 bytes on stdout (`main.rs:88-89`). The plan mirrors the unknown-flag
   behaviour (`main.rs:93-98`, `commands.rs:2711-2747`) for the whole clap
   class, so "the agent reads stdout" is true for every error.
5. **The release-feature decision.** See §5 DEC-2. The **plan's
   recommendation**: ship a CLI-side catalog (`aroute commands --json`) that
   exists in **every** build, and **do not** enable the `mcp` feature in the
   release build in this step. The reason is the accept gate in
   `docs/06:60` (*"default `cargo tree` has no `rmcp`; `+mcp` idle +<25MB"*)
   and the release-flow discipline in project memory #226: changing the
   release artifact's dependency set is a decision with its own risk, and the
   CLI catalog removes the *reason* for the decision in the first place. If
   the operator rules that the MCP must ship, the change is one `--features
   mcp` in `release.yml:131-135` **plus** the `install.sh` wiring becomes
   correct (DR-9), **plus** the RAM budget is re-measured against the
   `docs/06:60` gate.
6. **`install.sh`** must stop advertising a transport the artifact may not
   have (DR-9, `install.sh:278`, `:287`, `:291-297`): either the feature ships
   (DEC-2 yes) or the installer prints no MCP wiring.

**Verification.** A test asserting every `Command` variant appears in the
catalog; a test asserting `limits set`'s arg list matches the source; a test
asserting each `ConfigError` variant maps to its code; a test asserting an
unknown subcommand produces a stdout line.

**Rollback.** The catalog is one new subcommand; `--json` is one flag per
command; the error code is additive to an existing line. The release-feature
change is a one-line workflow edit and the one item here with a non-trivial
blast radius — hence §5 DEC-2.

**Closes.** C5, D-3, D-4, DR-2, DR-6, DR-9.

### Step 7 — Filters and pagination for the unbounded list commands (D-1, D-2, D-5, DR-5)

**Change.**
1. **`models`:** `--combo <id>`, `--provider <id>`, `--routable`, and
   `--limit N`. The **default** becomes the combo-target rows only — the
   models this config can actually route (`commands.rs:1037-1063`) — with
   `--all` restoring the full registry (`commands.rs:1064-1075`). The
   default change is a **breaking output change** for any downstream script;
   it is announced in the release notes and the alternative (keeping the
   default and requiring `--routable`) is the conservative variant the
   operator may prefer. **Recommendation: change the default** — the current
   default spends 72 KB to answer a question whose answer is 16 rows, and an
   agent's context is the scarce resource.
2. **`doctor`:** `--fields` (on the `check,status,detail` column set,
   `commands.rs:59`) and `--status fail` so an agent can ask for just the
   broken rows. `doctor` already exits 1 on any failure
   (`commands.rs:1234-1240`).
3. **Pagination.** `toon::list` already emits `count: N of TOTAL` and the
   code says the shape will not change when pagination lands (`toon.rs:64-67`).
   The plan adds `--limit`/`--offset` to the list verbs and makes `TOTAL` the
   **full** row count rather than the page size.
4. **DR-5:** decide whether `--full` means what `docs/06:10` says. The
   honest options are "fix the list path so `--full` returns the whole cell"
   or "amend the doc to say the list path clips at 1,000 chars and `--full`
   only suppresses the hint". **Recommendation: amend the doc** — the
   1,000-char cell clip is deliberate (`toon.rs:19-23`) and the marker
   already tells the reader how much was cut.

**Verification.** Measure `aroute models` before and after; assert the
routable set is a subset of the full set and the full set is still reachable
via `--all`; assert `count: N of TOTAL` has `TOTAL != N` under `--limit`.
The default-columns test (`commands.rs:2753-2763`) must keep passing and
must **not** be extended to `doctor` (its column set is not
`DEFAULT_FIELDS`-shaped).

**Rollback.** Additive flags; the one breaking part is the `models` default,
reverted by one line.

**Closes.** C4, D-1, D-2, D-5, and the `count: N of TOTAL` clause of `docs/06:10`.

### Step 8 — The three unreachable config sections (GAP 1, GAP 2, GAP 3)

One decision each, with the semantics spelled out.

**8a — `oauth:` (GAP 1).** The operator's goal names OAuth explicitly, so
the `oauth:` block cannot stay hand-edit-only.
**Recommendation: add `aroute oauth` as a small verb group** — but only
after step 4, because step 4 is what makes the block worth creating
programmatically.

- `aroute oauth list` — one row per declared session: `provider`,
  `authorization_url`, `token_url`, `client_id`, `redirect_uri`,
  `refresh_key`, `client_secret_key` — **placement only, never a secret**
  (the block carries no secrets by design; `config.yaml:57-85` documents
  that).
- `aroute oauth add --provider X --authorization-url … --token-url …
  [--client-id …] [--redirect-uri …] [--refresh-key …] [--client-secret-key …]`
  — appends one row to the `oauth:` block.
- `aroute oauth remove --provider X` — removes that row.

**Semantics that must be pinned:** the block is a **list of placements**, one
per provider id (`ar-config/src/lib.rs:817-821`); `authorization_url` and
`token_url` are **never guessed** from the provider id (a guessed auth
endpoint is an invented wire format — `config.yaml:64-67` says so); a
`refresh_key` must name a row under `keys:` or the session cannot renew
(`config.yaml:61-63`); a `client_secret_key` names a third `keys:` row
(`config.yaml:77-79`); and a `redirect_uri`, when declared, must be `https`
or a loopback `http` address (`config.yaml:73-76`). The writer must go
through `splice_section`, which means **step 1's fix is a precondition**.

**8b — `server:` (GAP 2).** The panel writes `host` + `port` only
(`webui.rs:1305-1335`). **Recommendation: add `aroute server show` and
`aroute server set --host H --port P`** — two verbs, the same two fields, the
same validation (empty host refused; port in `1..=65535`), the same
`splice_section` path. The listen address is a first-class operator concern
and the CLI is the surface an agent drives. Note that changing the listen
address is a **restart** action, so the step-3 restart contract applies to
it too.

**8c — a targeted `providers:` add (GAP 3).** **Recommendation: add
`aroute providers add <id> [--key <name>]` and `aroute providers remove
<id>`**, mirroring the panel's semantics exactly, because the panel's
semantics are already the tested ones:

- **add** — the row, the `keys:` reference and the `AR_KEY_*` env line land
  together (`webui.rs:1771-1789`). A row with no `$AR_KEY_*` binding is
  refused (`webui.rs:1761-1768`).
- **remove** — refuses while any combo still references the provider
  (`webui.rs:2436-2440`), and drops the `keys:` entries the removed rows
  alone held (`webui.rs:2366-2390`).

The CLI must not invent different semantics from the panel's; two editors for
one data set is how the two drift.

**Verification per sub-step.** A round-trip test per verb: add → the section
is spliced in and the rest of the file is byte-identical → remove → the file
is back. Plus: `oauth add` for a provider with a missing `refresh_key` is
refused with the exact fix command; `providers add` with no env binding is
refused; `providers remove` with a referencing combo is refused.

**Rollback.** Three new verb groups; each is additive and each writes through
the existing splice helper.

**Closes.** C1 — the contract clause that opens the plan.

---

## 5. The open operator decisions

### DEC-1 — Should the CLI reach EVERYTHING the panel can?

**Recommendation: yes, for the config sections; no, for the panel's
browser-only conveniences.**

The operator's goal is explicit — *"agents can directly use the CLI for
updating everything without the use of manually configuring the config files"* —
and the three gaps in §1.3 are exactly the places where "everything" is
currently false. The panel is the browser surface; the CLI is the agent
surface. Where the panel has a *semantic* the CLI lacks (the `providers:`
add's three-part write, the removal's reference check), the CLI should
**adopt the panel's semantics**, not invent new ones — the panel's are the
tested ones.

The exceptions, where the panel should stay the only surface:

- **The panel's own gate and token flow** (`webui/auth.rs:290-292` admits
  `write:*` or `execute:completions`; the paste-a-token field and the
  `?token=` first paint). A browser can carry a credential a CLI cannot
  reach safely; the CLI's `keys mint` (`commands.rs:506-530`) is the
  machine path and needs no panel twin.
- **The panel's live view** of the running proxy (`/ui/api/state`,
  `/ui/api/resilience`, `/ui/api/health`) — that is `aroute serve`'s own
  surface, and the CLI's answer is `aroute doctor` on the config side.

**What this decision costs:** steps 8a–8c are three verb groups, and step 2
is what makes the panel's usage pane reachable from the CLI too. The panel
keeps every endpoint it has; nothing is removed.

### DEC-2 — Should the `mcp` feature ship in the release binary?

**Recommendation: no, not in this plan. Ship `aroute commands --json`
instead.**

Facts: the feature is default-off (`crates/ar-cli/Cargo.toml:23-25`), the
release build passes **no `--features`** (`.github/workflows/release.yml:129-136`),
and the **deployed binary answers `error: unrecognized subcommand 'mcp'`** —
so the MCP surface is not in the shipped product. `docs/06:60` states the
accept gate: *"default `cargo tree` has no `rmcp`; `+mcp` idle +<25MB;
agent `health->switch->route->cost` loop green."*

**The cost of shipping it:** the release artifact gains `rmcp` and its
transitive tree (already resolved in `Cargo.lock` at 3.5.0), so the binary
grows and its idle RAM has to be re-measured against the `<25MB` gate; the
release flow changes (project memory #226 treats the artifact as a
disciplined thing); and `install.sh:278`, `:287`, `:291-297` would finally
be telling the truth (DR-9).

**The cost of not shipping it:** the CLI catalog is the *only* machine-readable
surface in the shipped binary — which is exactly what step 6 builds, and it
needs no feature flag, no SDK, and no RAM budget.

**The recommendation's reasoning:** the *purpose* of shipping the MCP is to
give agents a machine-readable surface. A CLI-side catalog gives them that
without changing the artifact's dependency set. If the operator's real reason
for wanting the MCP is to **use the MCP tools** (`ar_check_quota`,
`ar_explain_route`, `ar_switch_combo`) from a harness, then shipping it is
the right answer — but that is a *different* goal from "agents can drive the
CLI", and it should be ruled separately. **This decision is cheap to defer
and expensive to get wrong**: it is the one item in this plan that changes
what is published.

**Amended 2026-10-24 by the live-system red-team pass (§9.4).** The
recommendation is unchanged, but the alternative is now *measured* rather than
assumed: the feature compiles cleanly under `--features mcp`, the default tree
still contains no `rmcp` (the `docs/06:60` gate survives), `aroute mcp --list`
prints the 12 registered tools plus `ar_tool_search` (13 rows), and a real stdio
`initialize` handshake returns
`protocolVersion 2024-11-05` with `serverInfo` and the full catalog. So the
cost of shipping is bounded and known: one `--features mcp` in
`release.yml:131-135`, the `install.sh` wiring becomes truthful (DR-9), and two
numbers still owe a release-artifact measurement against the gate — the
artifact's size and its idle RAM against the +<25 MB cap. `release.yml` was
deliberately left unchanged by that pass so this decision stays the
operator's.

### DEC-3 — What should the spend verb be called?

**Recommendation: `aroute cost-report`.**

Nine code sites already say `aroute cost-report` / `ar cost-report`
(`webui.rs:815`; `ar-tokens/src/ledger.rs:368`, `:385`, `:398`, `:601`,
`:1059`; `ar-server/src/routes.rs:139`, `:3201`, `:4994`); the type is
`CostReport` (`ar-tokens/src/ledger.rs:388`); the MCP tool is
`ar_cost_report` (`ar-mcp/src/lib.rs:200`); `docs/06:35` lists it as tool #6.
Naming it `usage` would leave all nine false and create a second spelling
for one thing. The word an agent reaches for is "usage", so the `--help`
should carry that word in prose (*"spend and token usage since the last
restart of the ledger"*), while the command's name stays the one the code
uses.

### DEC-4 — Should `arm-gate` refuse to overwrite an existing master without an explicit `--force`?

**Recommendation: yes — refuse without `--force`, and print the invalidation
warning when `--force` is given.**

The blast radius of an accidental overwrite is every live token on the box,
and the precedence makes it worse: the store row **beats**
`AR_HTTP_MASTER_KEY` (`serve.rs:152-160` → `ar-server/src/app.rs:385-391`),
so the overwrite does not merely rotate the master — it **desynchronises the
server from the environment**, which is the 2026-10-19 incident shape.

**The counter-argument, and why it does not win.** `arm-gate` is currently
idempotent-ish, and re-running it to re-arm a broken store is a legitimate
operator move — `doctor`'s own repair text says *"re-arm with `aroute keys
arm-gate --key $AR_HTTP_MASTER_KEY`"* (`commands.rs:1324-1325`). A refusal
that blocked that would break the documented repair. The `--force` flag
resolves it: the repair path becomes `aroute keys arm-gate --force --key
$AR_HTTP_MASTER_KEY`, and step 5 updates `doctor`'s text accordingly. **The
refusal must apply to the `--key` path too** — an operator bringing the store
into line with the env is replacing a master, and the replacement should be
explicit.

### DEC-5 — Should `aroute import` refuse to overwrite a `config.yaml` that carries sections it cannot emit?

**Recommendation: warn loudly now (step 1); refuse as a default later, with
`--force` to override.**

`import::render_yaml` (`import/mod.rs:353-398`) emits `keys:`, `providers:`,
optionally `custom_providers:`, `combos:` — and **never** `limits:`,
`oauth:` or `server:`. `aroute import` writes `config.yaml` wholesale
(`commands.rs:2018`). So importing into a directory whose config carries an
`oauth:` block deletes it silently. That is the same class of silent
destruction as B-1, on a different writer, and the operator's goal says
explicitly: *"without the use of manually configuring the config files"* —
which is only true if the hand-written parts survive the machine-written
ones.

**Recommendation:** step 1 adds a loud warning naming the sections that
would be dropped; a later release makes the refusal the default with
`--force` to override, because a refusal-by-default changes what
`aroute import` does for every existing user.

### DEC-6 — Should the `models` default change to the routable set?

**Recommendation: yes** (step 7), with `--all` as the escape hatch. This is
listed here because it is a **breaking output change**, and the operator
should know the cost is being paid. The conservative alternative — keep the
full catalog as the default and require `--routable` — is safe but leaves the
72 KB default in place, which is the thing C4 is about.

### DEC-7 — Should `ar-keys` keep a table of issued tokens?

**Recommendation: no, not in this plan. `aroute keys list` reports what the
store actually holds; a true inventory is a separate, larger change.**

`ar-keys` keeps no table of issued tokens today: its schema is `store_meta`,
`credentials`, `oauth_sessions`, `quota_snapshots` and `revoked_tokens`
(`crates/ar-keys/src/store.rs:168-215`,
`crates/ar-keys/src/revocations.rs:197-213`). The consequence is that a
**lost** token is unrevokable — `keys revoke` needs the token itself
(`cli.rs:346-355`) and the store keeps no record of what it issued.

Adding an issued-token table would fix that, and it would make `keys list`
able to answer "which tokens are live, for which key id, until when?" — but
it is **not** a CLI-surface change: it is a new table in a security crate, it
needs its own migration and its own retention policy (a table of token ids
with expiry is a list of live credentials), and it is a different review
than the one this plan carries. Step 5's `keys list` therefore reports the
honest inventory — row names, OAuth sessions, revocation count, gate state —
and says in its own output that minted tokens are not recorded and a lost
token cannot be revoked. **If the operator wants the true inventory, that is
DEC-7 and it needs its own plan.**

---

## 6. The test plan

The project's convention (visible in `crates/ar-cli/src/commands.rs:4215-4251`,
`crates/ar-cli/src/webui.rs:3457-3461`,
`crates/ar-cli/src/import/limits.rs:118-122`): the test name reads
`verb_should_outcome_when_condition`, there is **one assert**, and — the part
that matters — there is a **regression test that catches the
consumer-visible bug**, not merely a unit test of the changed function.

Tests live next to the code they test, in the crate's own `#[cfg(test)]`
module. Nothing in this plan adds a test framework.

### Step 1

| Test | Where | Asserts |
|---|---|---|
| `splice_should_leave_a_hyphenated_section_after_the_spliced_one_byte_identical` | `commands.rs` tests | the `api-keys:` section and its body are byte-identical before and after |
| `remove_section_should_leave_a_hyphenated_section_after_the_removed_one_byte_identical` | `commands.rs` tests | same, for `remove_section` |
| `limits_clear_should_keep_a_hyphenated_section_that_follows_the_limits_block` | `commands.rs` tests | the **consumer-visible** bug: the file after `aroute limits clear` still contains the other section and its body |
| `import_should_warn_when_the_target_config_carries_a_section_it_cannot_emit` | `commands.rs` tests | the warning names `oauth:` |

The regression test that matters is the **third**: it drives the *command*, not
the helper, because the bug the operator would experience is "a command I ran
deleted a section I did not name".

### Step 2

| Test | Where | Asserts |
|---|---|---|
| `cost_report_should_print_the_ledger_rows` | `commands.rs` tests | the rows match a seeded in-memory ledger |
| `cost_report_should_match_the_mcp_cost_report_rows` | `commands.rs` tests | the CLI's rows and `ar_cost_report`'s rows are the same for the same ledger |
| `cost_report_should_not_create_a_ledger_file_when_none_exists` | `commands.rs` tests | no `usage.sqlite` appears after the read |

### Step 3

| Test | Where | Asserts |
|---|---|---|
| `limits_set_should_print_the_restart_notice` | `commands.rs` tests | the output names the restart command |
| `limits_set_should_print_the_restart_command_instead_of_restarting_when_no_restart_is_given` | `commands.rs` tests | `--no-restart` prints the instruction and does not restart |
| (doc review, no test) | `cli.rs:14`, `cli.rs:147-149` | the prose no longer claims a live watcher |

### Step 4

| Test | Where | Asserts |
|---|---|---|
| `login_complete_should_redeem_a_login_started_by_another_process` | `auth.rs` tests | start, **kill**, complete from a new process with the session id → armed |
| `login_complete_should_refuse_a_session_that_was_already_completed` | `auth.rs` tests | the verifier is single-use |
| `login_complete_should_refuse_a_session_older_than_the_ttl` | `auth.rs` tests | 300 s, the CLI's own `--timeout` default |
| `login_should_still_accept_a_pasted_code_on_stdin` | `auth.rs` tests | the existing headless path still works |

### Step 5

| Test | Where | Asserts |
|---|---|---|
| `arm_gate_should_refuse_to_overwrite_an_existing_master_without_force` | `commands.rs` tests | the second `arm-gate` refuses |
| `arm_gate_should_say_what_it_invalidated_when_forced` | `commands.rs` tests | the warning names the invalidated tokens |
| `keys_list_should_name_the_store_rows_without_printing_a_token` | `commands.rs` tests | the row names appear and no token material does |
| `doctor_should_name_the_force_flag_in_the_repair_text` | `commands.rs` tests | the repair text is current |

### Step 6

| Test | Where | Asserts |
|---|---|---|
| `commands_should_list_every_command_variant` | `commands.rs` tests | the catalog has one entry per `Command` variant |
| `commands_should_report_the_args_of_a_nested_subcommand` | `commands.rs` tests | `limits set`'s flags match the source |
| `config_error_should_map_to_a_stable_code` | `commands.rs` tests | one `ConfigError` variant → one code |
| `an_unknown_subcommand_should_print_a_line_on_stdout` | `main.rs` tests | stdout is not empty for an unknown subcommand |
| `json_should_render_the_same_rows_as_toon` | `commands.rs` tests | the two renderings carry the same data |

### Step 7

| Test | Where | Asserts |
|---|---|---|
| `models_should_default_to_the_routable_rows` | `commands.rs` tests | the default is a subset of `--all` |
| `models_all_should_reach_the_whole_registry` | `commands.rs` tests | `--all` has every registry row |
| `doctor_should_print_only_the_failing_rows_when_status_is_fail` | `commands.rs` tests | the filtered table has the `fail` rows and no others |
| `list_should_report_a_total_larger_than_the_page` | `toon.rs` tests | `count: N of TOTAL` has `TOTAL != N` under `--limit` |

### Step 8

| Test | Where | Asserts |
|---|---|---|
| `oauth_add_should_append_a_row_and_leave_the_rest_byte_identical` | `commands.rs` tests | the file's other sections are unchanged |
| `oauth_add_should_refuse_a_provider_with_no_authorization_url` | `commands.rs` tests | the refusal names the exact fix |
| `server_set_should_write_the_listen_address` | `commands.rs` tests | `server:` carries the host and port |
| `providers_add_should_write_the_row_the_key_ref_and_the_env_line_together` | `commands.rs` tests | the three-part write the panel performs |
| `providers_remove_should_refuse_while_a_combo_references_the_provider` | `commands.rs` tests | the refusal matches the panel's |

---

## 7. Open items carried from the recon

These are the recon's `CLAIMS_TO_VERIFY` rows that this plan depends on or
touches. They are **open**, not settled.

1. **The hyphen-deletes-section bug against a current build.** The recon's
   reproduction used the stale `target/release/aroute` (`0.1.17`). The
   orchestrator re-reproduced it on 2026-10-20, and `is_section_header`
   (`commands.rs:2279-2285`) is unchanged since `05e2a06` (2026-10-09), but a
   plan author should re-run the reproduction against a **fresh build** before
   committing to the fix. *Unchanged by this plan; step 1's regression test
   supersedes it.*
2. **Whether `splice_section` replacing *every* occurrence of a duplicated
   header is a bug** (`commands.rs:2249-2254` pushes `rendered` each time it
   sees the header). Untested. *Not addressed by step 1; the plan should not
   widen step 1 to cover it without a separate decision.*
3. **Whether attaching `ConfigHandle::watch` is in scope.** `cli.rs:147-149`
   claims the config is picked up live; the code says otherwise; **no ADR
   explains the disagreement**, and the panel already answers every save with
   `pending_restart` (`webui.rs:761`). **This plan takes the doc-fix side**
   (step 3) and does **not** propose wiring the watcher, because
   `ar-server` builds its `Policy` once at boot (`ar-server/src/app.rs:396-400`)
   and a live reload would reset the rpm token buckets — a spend-control
   regression. The operator's ruling is still wanted; the related open
   question is **Q9** in `docs/kb/ASKS.md` ("Should the /ui panel be able to
   restart the proxy after a write?"), and `docs/28-gap-ledger.md` **B5**
   carries the same item.
4. **Whether the `aroute cost-report` references indicate a verb that was
   planned and never built**, rather than one that was dropped. `docs/21`,
   `docs/28` and `docs/kb/ASKS.md` were searched for the decision and **none
   records it**. *Step 2 treats the absence as a plain gap; the search result
   is recorded here so the next reader does not repeat it.*
5. **Whether the second `arm-gate` genuinely invalidates the first token
   end-to-end.** The master row is replaced (`commands.rs:399-406` into
   `store.rs:263-266`) and a new token is minted, but the old token has not
   been run against the gate to confirm the 401. *Step 5's test should assert
   it.*
6. **`doctor`'s row count on the real VPS config.** The recon's 209 rows used
   200 synthetic keys; the VPS has 335 `AR_KEY_*` vars and 16 combos. The real
   number is likely higher. *Measure before sizing step 7's `--fields` work.*
7. **Whether `ar_switch_combo`'s in-process semantics are intentional**
   (`ar-mcp/src/transport.rs:500-509`). The MCP's "active combo" is
   per-MCP-process and does not touch the running proxy's routing; the CLI's
   `run --model` is per-call. Neither matches the data plane. *Not addressed
   here; it is an MCP-surface question, not a CLI one — but it is why this
   plan does **not** try to make the CLI and MCP "consistent" on combo
   selection.*
8. **`aroute mcp --list` as the model for a CLI catalog.** §5 DEC-2. The
   recon notes that shipping the `mcp` feature changes the release artifact's
   dependency set, so it needs sign-off against the release-flow discipline
   in project memory #226. *Unchanged; carried into DEC-2.*
9. **The `ar` vs `aroute` spelling.** `cli.rs:185` and `mcp.rs:252-253` still
   print `ar mcp`; no `ar` binary exists (`crates/ar-server/Cargo.toml:12-14`
   removed it). **Unresolved in the repo.** *Not in this plan's scope; a
   doc-and-help-text cleanup.*
10. **`AR_CONFIG` does not select the config file.** The recon's grep found no
    `env =` attribute; this plan re-confirmed it (no `env =` in `cli.rs`;
    `ar-config/src/lib.rs:1749` takes an explicit path). *Step 3 corrects the
    doc comment rather than adding the env override — the env override is a
    separate decision, because an env var that silently changes which config
    a command edits is a hazard, not a convenience.*
11. **A re-login overwrites the existing token rows** rather than creating a
    second pair (`ar-keys/src/store.rs:263-266` is the same upsert).
    *Not addressed by this plan; it is correct behaviour (the old token stays
    valid until the exchange succeeds) and only needs documenting.*
12. **`aroute sync` cannot add a `providers:` row.** `commands.rs:2410-2412`
    returns early only when the `providers:` block is *empty*; when it is
    non-empty, a dashboard connection for a provider the config does not have
    is simply ignored. *Step 8c adds the targeted add, so this gap closes
    there — but the rule "sync never adds a row" must survive that change, or
    the two writers disagree about who owns a `providers:` row.*
13. **`aroute import` overwrites a whole `config.yaml` and silently drops
    `limits:` / `oauth:` / `server:`.** The recon derived this from the
    renderer (`import/mod.rs:353-398`) and the write (`commands.rs:2018`); the
    empirical check is to import into a directory holding a full config and
    diff. *§5 DEC-5 depends on it.*
14. **The `key_caps` table has no production writer.** `set_cap` is called
    only from a test in `ar-mcp/src/tools.rs` and from `ar-tokens`' own tests,
    so the engine-side cap store is empty in production — consistent with
    `docs/34-spend-cap-audit.md:43`. *Not a CLI-surface fact, but it is why
    `cost-report`'s totals and the panel's budget pane can disagree; noted
    so step 2's golden test is written against a seeded ledger, not against
    production state.*
15. **`aroute dashboard` writes only to `$DATA_DIR`.** The child's sqlite
    appears under `~/.config/ar/dashboard-data`, never in aroute's config
    (`dashboard.rs:113`, `:140-149`). *Not a CLI-surface fact; it is why
    `sync`'s dashboard DB is a separate thing from `config.yaml` and why
    `sync` is the one bridge between them.*

---

## 8. The single change to make first

**Step 1: widen `is_section_header`'s character class so it accepts every
character a YAML plain key may contain — `-` first among them
(`crates/ar-cli/src/commands.rs:2279-2285`), with the byte-identical
regression test.**

It is first because it is the only finding that **destroys data silently**,
and because it is a precondition for every write path the rest of this plan
adds — steps 3, 4 and 8 all write through `splice_section`. Fixing it after
those steps land means the new verbs inherit the bug.

---

## 9. The live-system red-team pass (2026-10-24)

Everything above is read from source at `12dda18`. This section is the same CLI
audited against the **running** system, which found things a static pass cannot:
what the commands cost in tokens, what the daemon costs in RAM, and what breaks
when an agent actually tries to drive it.

### 9.1 What the tokens cost (measured against the live config)

| Verb | Bytes | ~Tokens |
|---|---|---|
| `models` | 72,913 | **18,228** |
| `configure` | 31,660 | 7,915 |
| `doctor` | 31,446 | 7,861 |
| `providers` | 6,028 | 1,507 |
| `combo` | 638 | 159 |

`aroute models` spends **18k tokens** to answer "what can I route?", and D-1's
fix (step 7) is what makes that cheap. `--fields` already works today and is the
only live lever: `models --fields model` costs **27 tokens** against that 18,228.

G3 is now the second lever, and it is the one that does not lose information.
Measured on the repo fixture (the operator's live config is not reachable from
CI, so these are the reproducible numbers; the shape of the win is the same
one, since the 18k is almost entirely catalog rows `--routable` deletes):

| Call | Bytes | What it returns |
|---|---|---|
| `models` | 71,974 | all 1,563 rows, catalog included |
| `models --routable` | **169** | the 3 this config routes |
| `models --combo default` | 131 | one combo's targets |
| `models --provider openai` | 715 | one provider's models |
| `doctor --limit 3` | 280 | 3 of 12 checks, `TOTAL` still 12 |

So `models --routable` is **~425x smaller** than the default, against the
18,228-token baseline above. The catalogue rows are the cost, and
`--routable` is the filter that removes exactly them.

`--limit`/`--offset` are on every list verb, and `--status` on `doctor`. Two
corrections came out of running these rather than reasoning about them, and
both are load-bearing:

1. **`--routable` must not filter on `status == "routable"`.** That string means
   "provider AND model are both in the compiled-in registry". A config whose
   providers are all custom filters to **0 models** — measured — which is the
   worst possible answer, because it reads as "nothing routes". A combo target
   that cannot currently route is listed with the status that says why. The
   structural test is "does this row have an owning combo", i.e. column 3 is
   not `-`.
2. **`--status` is a `ValueEnum`, not a `String`.** A free string that matched
   nothing would print `0 checks` and look like a clean config. A typo now
   exits **2** naming `ok, skip, fail`, the same contract as `--fields bogus`.

And the invariant the filters must never break: **the exit code is computed
before any filter**. `aroute doctor --status ok` on a config with failures
exits **1** and still prints `N check(s) failed` — proven by hand against a
build that counted the printed rows, which exits **0** and launders a broken
config through a build gate. All three G3 tests have negative controls; the
table of injected bugs is in the commit.

### 9.2 What the daemon costs (measured, two systemd user units)

| Component | RSS | Peak (VmHWM) | Threads |
|---|---|---|---|
| aroute proxy (Rust, pid 1103) | 44 MB | 160 MB | 26 |
| dashboard launcher (Rust) | 4 MB | 6 MB | 1 |
| dashboard child `omniroute v16.3.8` (Node) | 873 MB | **959 MB** | 13 |

Two things follow. First, the unit comment claims "roughly 500 MB RSS"; the
measured peak is ~960 MB, so it **understates by ~2x** and should be corrected.
Second, an agent that has only `aroute` cannot observe or manage any of this —
there is **no lifecycle verb in the command spine** (§1.1 lists `serve, models,
providers, combo, doctor, run, configure, auth, keys, limits, import, sync,
dashboard, donate` and nothing that starts, stops or restarts a service).
`aroute dashboard` launches a foreground server; it cannot stop the running one.
So the two operations the red-team pass actually performed are only reachable
through a second tool:

```sh
systemctl --user status   aroute-dashboard   # observe
systemctl --user disable --now aroute-dashboard  # verified: freed ~960 MB of RSS
systemctl --user enable  --now aroute-dashboard  # restored it, serving in 7 s
```

This is the two-tool gap DEC-1 touches for config sections but not for
*processes*, and it should be ruled with it.

### 9.2.1 Two unit comments that are wrong, and one unit defect whose owner is NOT the repo

Three systemd claims were made while auditing; checking them changed two, and one
of those would have sent a fix at the wrong file:

- **The "roughly 700 MB" comment understates the measured footprint by ~2x.** The
  dashboard unit's comment (`install.sh:628-630`) says the Node child "holds
  roughly 700 MB RSS"; the live peak was **959 MB**. This one *is* in the repo
  and is a fair thing to correct.
- **The `StartLimitIntervalSec` misplacement is NOT in `install.sh`.**
  `install.sh:866-891` emits `Restart=on-failure` / `RestartSec=5s` in
  `[Service]` and contains **no `StartLimit*` key at all**. The installed
  `~/.config/systemd/user/aroute.service` does carry
  `StartLimitIntervalSec=300` + `StartLimitBurst=5` inside `[Service]` — both
  belong in `[Unit]`, which is why systemd logs
  `Unknown key 'StartLimitIntervalSec' in section 'Service'` on every boot — and
  it says `Restart=always` with a comment, *"a mission-critical service silently
  missing while systemd reported success"*, that appears **nowhere in the
  repository**: no script, workflow or doc contains that string. **That unit was
  hand-edited on this machine.** So the defect is real, but its owner is a
  hand-maintained local file, and "fix install.sh" would have edited a file that
  was never wrong. Recorded so the next pass does not repeat the attribution.

### 9.3 The 15.4-second `/healthz` (the finding worth the whole pass)

The dashboard returned **HTTP 200 after 15,427 ms** while its own journal said
`event loop stalled 45.6s` and `GET /healthz event-loop lag 91738ms (HTTP 200 is
not healthy; busy != ready)`.

CPU attribution decided it: **99% kernel time** (utime 131 ticks vs stime 65,941)
and 501,889 major page faults. That is thrash, not a JS hot loop — the host sat
at ~5.2 GB available while two other tenants ran 12 GB python3 jobs. A
`disable --now` + `enable --now` cycle restored it to **23 ms**, so the cause was
host memory pressure, not aroute code.

The agent-facing consequence is the real finding: **`/healthz` returning 200 was
actively misleading.** An agent polling it would have concluded "healthy" from a
process that could not serve a request in under a minute. Whatever step 6 ships
as the machine-readable health surface must report **latency**, not just status,
or it repeats this failure.

### 9.4 The MCP question, settled by building it (this is what DEC-2 needs)

DEC-2 asks whether to ship the `mcp` feature. The red-team pass built the feature
and ran the real thing, so the decision no longer has to be made on inference:

| Check | Result |
|---|---|
| default `cargo tree -e no-dev` contains `rmcp` | **none** (docs/06:60 gate stays green) |
| `--features mcp` pulls `rmcp` | **3.5.0**, present |
| `--features mcp` compiles | **yes** (7m24s, clean) |
| `aroute mcp --list` | **12 registered tools + `ar_tool_search`** (13 rows printed) + `next:` hints |
| real stdio `initialize` handshake | **`protocolVersion 2024-11-05`**, `serverInfo {name: ar, version: 0.1.21}`, full catalog |
| negative control (installed 0.1.21) | still `error: unrecognized subcommand 'mcp'` |

So the capability is real, it builds, and the accept gate survives it. Two
numbers still owe a measurement against the gate: the artifact's size and its
idle RAM footprint (`docs/06:60` caps the latter at +<25 MB). Those are
release-artifact measurements, not developer-machine ones — which is exactly why
this pass records the answer without changing `release.yml`.

**Corrections this pass made to its own draft**, all from running the check:
the catalog is **13** tools, not 12; and the defect is not "the default feature
should be on" (that would break the gate) but "**no build path in the repo
enables the feature at all**" — `release.yml` passes no `--features` and
`install.sh` never invokes cargo, so the `claude mcp add aroute` wiring that
`install.sh` prints cannot work from any published artifact.

The recommendation of DEC-2 stands as written (ship `commands --json` first);
the point of this section is that the alternative is now measured rather than
assumed, and that **shipping it is a one-line `release.yml` edit plus an idle-RAM
measurement**, not an unknown.
