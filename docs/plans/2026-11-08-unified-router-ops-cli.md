# Unified router-ops CLI: folding seven vendored scripts into `aroute`

**Status: plan, written 2026-11-08.** No code changed by this document.
**Tracking: ledger `F2` family, `docs/36` §4.**

Companions: [`docs/36-cli-agentic-workflow-plan.md`](../36-cli-agentic-workflow-plan.md)
(the CLI as an agent-facing surface — this plan is the harness half of it),
[`docs/28-gap-ledger.md`](../28-gap-ledger.md) (the state of every open item),
[`docs/06-axi-mcp.md`](../06-axi-mcp.md) (normative for CLI output contract),
[`docs/40-project-closing-outline.md`](../40-project-closing-outline.md)
(the closing order).

---

## 0. The problem, in one paragraph

Seven shell and Python scripts live in `~/.claude/skills/router-ops/scripts/`
and are installed onto `PATH` by that skill's `install.sh`. They own the
harness surfaces around aroute: the static model lists, the MCP server
registry, the fixed model slots, the gate token, the readiness doctor, and the
guard against `aroute sync` destroying the routing table. They are the *only*
thing that keeps those surfaces correct, and they are outside the repository
that owns the thing they are configuring. Every one of them is a second
implementation of a fact aroute already knows, and two of them reach into a
*third* skill to learn it.

The scripts are not wrong. They are green. The problem is that aroute
publishes facts and the scripts re-derive them, so the two can disagree, and
when they do the disagreement is silent.

## 1. What the scripts own today

| Script | Language | Owns | Authority it reads |
|---|---|---|---|
| `harness-models-sync` | Python | opencode, jcode, pi static model lists | `GET /v1/models` (live) |
| `harness-sync` | Python | MCP servers in omp, opencode, jcode, codex, pi, agy | `config/harness/mcp-servers.yaml` |
| `harness-model-select` | Python | Claude Code and Codex fixed model slots | live aroute streaming routes |
| `combos-guard` | Bash | the `combos:` block in `config.yaml` | a canonical snapshot beside the config |
| `router-ops-doctor` | Bash | "can I work on the router here" | five config files |
| `gate-token.sh` | Bash | the gate JWT in five harness configs | `~/.config/ar/racknerd-gate-token.jwt` |
| `endpoint-bench` | Python | latency/TTFT measurement | the live endpoint |

Verified 2026-11-08: `router-ops-doctor` 13/13, `harness-models-sync --check`
no drift, all five static lists carry exactly the 13 streaming lanes with
correct windows and zero decision leakage.

## 2. The seven gaps, each with the evidence for it

**G1 — Only Codex is a first-class `aroute <harness>` citizen.**
`aroute codex` writes a catalog, a profile, a provider block and a daemon
restart. Nothing does the equivalent for opencode, jcode, pi, omp, claude or
omo. The other five are configured by a vendored Python script.

**G2 — `/v1/models` publishes no `max_output_tokens`.**
Measured live 2026-11-08 against `https://aroute.ishanparihar.com/v1/models`:
18 cards, and **every one has `max_output_tokens: None`**. The field exists in
the code (`crates/ar-server/src/routes.rs:4323`, `ModelCard::max_output_tokens`)
and is written when it is known, but no card knows it. The harnesses therefore
*guess* — opencode 128000, pi 32000, omo 64000 — and they disagree, and a
wrong ceiling is worse than a missing one: the harness sizes a request off it
and the upstream refuses a request it would have accepted.

**G3 — `/v1/models` does not mark stream-vs-decision.**
The card carries `id`, `context_length`, `input_image`, `owned_by`,
`ar_upstream_model`. Nothing says whether the card answers a chat request.
So `harness-models-sync` reaches into `~/.agents/skills/decision-models/scripts/decide.py`,
`importlib`-loads it, and calls `get_decision_models()` — with a hardcoded
fallback set of seven ids — to decide what to *exclude*. That is a third
skill's private Python API, called by path, with a silent `except Exception:
pass` around it.

The signal aroute needs already exists in its own config: a provider whose
`endpoint` is `/v1/decisions`, `/v1/systemone` or `/api/alpha/decisions` is a
decision provider, and a combo that routes to one is not a chat lane.
`crates/ar-cli/src/codex.rs` already computes exactly this to exclude decision
lanes from the Codex catalog.

**G4 — Discovery requires the gate token, re-minted per harness on rotation.**
`/v1/models` is behind the gate. Every harness therefore needs a token, and
`gate-token.sh` owns the fan-out: opencode and pi interpolate the canonical
file, but omp, jcode and claude take a literal value, so the script owns three
inline copies. A stale copy in `~/.claude/settings.json` makes every Claude
session 401 with "this token is not valid", which reads as a gateway fault
rather than credential drift.

**G5 — No MCP-server-list surface from aroute.**
`config/harness/mcp-servers.yaml` is already *in this repository*, and
`harness-sync` is a vendored copy of the reader for it. The registry is
versioned with the code but the reader is not, so the two can drift and the
drift is invisible.

**G6 — `aroute sync` is a live footgun, guarded only by an external script.**
`aroute sync` rewrites the `combos:` and `custom_providers:` sections of
`config.yaml` from the dashboard's own SQLite, which holds only the 3 combos
the web UI knows about. The routing config holds 18. Running it therefore
destroys 15, including `large-stack`, which every harness is pinned to.
Nothing in the repo warns you; the proxy just serves 3 lanes afterwards. This
happened during the 2026-10-23 minimax migration and cost several restore
cycles. `combos-guard` is the only thing standing between an operator and a
silent 15-lane loss, and it is a vendored Bash script outside the repo.

**G7 — `omo` and `claude` are unowned by any sync.**
`grep -rn "omo" harness-models-sync harness-sync harness-model-select` returns
nothing. `omo` carries a static model list that nothing writes. Claude Code is
written by `harness-model-select` (model slots) and `gate-token.sh` (token),
but its *model list* is owned by nobody — it relies on
`CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1` being set.

## 3. The shape of the answer

One principle, and every decision below follows from it:

> **aroute publishes facts. The harness tooling reads them. Nothing re-derives
> a fact aroute already knows.**

That inverts the current arrangement. Today the scripts are the authority for
harness state and they talk to aroute over HTTP to recover what aroute already
knows. In the target arrangement aroute is the authority, exposes the missing
facts on its own surface, and the harness tooling is a thin renderer.

### 3.1 The command surface

```
aroute harness <harness>          # configure one harness (codex, opencode, jcode, pi, omp, claude, omo)
aroute harness list                # every harness, its state, and whether it is in sync
aroute harness check               # report drift across every harness, change nothing
aroute harness apply               # write every harness from aroute's facts
aroute harness select <harness> <model>   # set the active model for a fixed-slot harness
aroute gate push|verify|show       # fan the gate token out to every consumer
aroute doctor --harnesses          # the readiness check, folded into the existing doctor
```

`aroute codex` stays as it is: it is the one harness whose configuration is
genuinely different enough (a catalog file, a provider block, a profile
layer, a daemon restart) that a shared renderer would be a worse fit than a
purpose-built command. It becomes the *first* member of the family rather than
the only one, and `aroute harness codex` is an alias for it.

### 3.2 Closing each gap

| Gap | Fix | Where |
|---|---|---|
| G1 | `aroute harness <name>` for every harness; the shared renderer owns the four static-list formats | new `crates/ar-cli/src/harness/` |
| G2 | publish `max_output_tokens` on every card, from the operator's `providerMeta.json` first and discovery second | `crates/ar-server/src/models.rs`, `routes.rs` |
| G3 | publish a structural `lane` marker on every card (`"chat"` / `"decision"`), computed from the provider's endpoint | `crates/ar-server/src/models.rs` |
| G4 | `aroute gate push` owns the fan-out; the canonical file stays the one source | `crates/ar-cli/src/gate.rs` |
| G5 | `aroute harness mcp list` reads the registry *in this repo* | `crates/ar-cli/src/harness/mcp.rs` |
| G6 | move the guard into `aroute sync` itself: refuse when the DB's combos would drop lanes the config has, unless `--force` | `crates/ar-cli/src/commands.rs` |
| G7 | own `omo` and `claude` explicitly, in the same table as the other five | `crates/ar-cli/src/harness/` |

### 3.3 The card shape

The published card grows two fields. Both are facts aroute already knows and
already uses internally, so neither needs new configuration.

```json
{
  "id": "large-stack",
  "object": "model",
  "context_length": 1048576,
  "max_output_tokens": 128000,
  "lane": "chat",
  "input_image": true,
  "owned_by": "large-stack",
  "ar_upstream_model": "nvidia/minimaxai/minimax-m3.1-flash"
}
```

`lane` is `"chat"` for a combo whose targets all serve chat, `"decision"` for
one whose targets are decision endpoints, and `"mixed"` when it serves both —
a mixed lane is published but flagged, because a harness that cannot route by
lane should leave it out.

`max_output_tokens` is emitted only when it is known, with the same rule
`card_json` already applies to every other optional figure: a number there is
a real one, an absent one is an absence, never a guess. The harnesses then
stop guessing 128000/32000/64000 and start reading the truth, and a harness
that finds no figure keeps its current behaviour (omit) rather than inventing
one.

### 3.4 The guard, in-repo

`aroute sync` currently splices the dashboard's combos over the config's. The
in-repo guard is a pre-flight, not a wrapper:

> Before writing, compare the combos the DB would produce with the combos the
> config has. If the DB's set is a strict subset, refuse, name the lanes that
> would be lost, and point at the canonical snapshot. `--force` overrides.

This is the same mechanism `combos-guard` implements today, with the
difference that it runs *inside* the command that does the damage, so it
cannot be skipped by forgetting to wrap it in a script. The canonical snapshot
stays beside the config; `aroute sync --restore-combos` reads it.

The refusal is a refusal, not a warning. A warning is what the current
arrangement effectively has, and it is not enough.

## 4. What the Codex work taught us about the harness design

The Codex fix (2026-11-08) is the first member of this family, and it is
worth reading as a rehearsal. Three findings shape the harness design, and
each one is a mistake a generic renderer would repeat.

**A harness config is more than a model list.** Three separate keys had to be
written for `codex` to work through the router, and each one failed on its
own:

| Key | What happens without it |
|---|---|
| `model_catalog_json` | Codex does not recognise the lane: `Model metadata for 'large-stack' not found` |
| `model_provider = "aroute"` | the request goes to OpenAI's own backend, which answers every router model name with `The 'large-stack' model is not supported when using Codex with a ChatGPT account` |
| `web_search = "disabled"` | the router refuses the whole request: `unsupported tool type "web_search": this build does not run it` |

The second and third are the dangerous ones, because each error names
something that is not the cause. The account error reads like a plan problem
and is not one: the same error comes back for a model name that does not
exist anywhere. The tool error names a tool the user never asked for.

**A renderer must know what the harness does with a missing figure.** The
catalog's `supports_search_tool: false` was believed sufficient, and it is
not: measured against the live router, the same request is refused with the
key absent and answered with the key present. The lesson for the harness
renderer is that *a field the router publishes is not the same as a field the
harness reads*. Each harness needs its own facts, and the renderer has to
know which ones.

**A client that caches the config needs a restart, and the router cannot
guess that.** Codex 0.167 runs an `app-server daemon` that outlives the CLI
and reads `model_catalog_json` once, at startup — upstream documents the key
as "applied on startup only". The catalog on disk was correct and the `/model`
picker still disagreed with it, because the daemon had started before the
catalog was written. `aroute codex` now restarts the daemon, because the
daemon is the only thing that can load a new catalog.

Every other harness has its own version of this: a cache, a long-lived
process, a config read once at startup. The harness renderer must own *that*
surface too, or a "successful" write will leave the harness running the old
configuration and nothing will say so.

## 5. What is deliberately NOT in this plan

**No new transport.** The harness tooling runs on the operator's workstation
and talks to the router over the same `/v1` surface it already uses. The
gate token is the only credential involved, and `aroute gate push` owns it.

**No new crate.** The harness renderer belongs in `ar-cli`, next to `codex.rs`.
It is a renderer over `serde_json` and `serde_yaml`, both already depended on.

**No change to the catalog schema.** The Codex catalog stays Codex's own
schema, cloned from the installed Codex. That is the only way it survives a
Codex upgrade.

**No port of `endpoint-bench`.** It measures latency and TTFT, which is a
benchmark, not a configuration surface. It stays a script until there is a
reason it must not.

## 6. The sequence

**Phase 1 — publish the missing facts.** `max_output_tokens` and `lane` on
every card. Nothing consumes them yet, so this is additive and safe, and it
is the phase that removes G2 and G3 at the source.

**Phase 2 — the guard.** Move `combos-guard` into `aroute sync`. This is the
highest-value phase and the smallest: it converts a silent 15-lane loss into
a refusal, and it needs no new surface.

**Phase 3 — the harness renderer.** `aroute harness <name>` for the four
static-list formats (opencode, jcode, pi, omp), reading `/v1/models` with the
new fields. `aroute harness check` reports drift; `apply` writes.

**Phase 4 — the remaining harnesses.** `claude` and `omo`, which nobody owns.
Claude's model slot is a fixed key rather than a list, so it lands with
`harness-model-select`, which becomes `aroute harness select`.

**Phase 5 — the gate and the doctor.** `aroute gate push|verify|show`, and
the readiness check folded into `aroute doctor --harnesses`.

**Phase 6 — retire the vendored scripts.** The skill's `install.sh` stops
installing them and the skill's SKILL.md points at the `aroute` verbs. The
skill keeps what is genuinely skill-shaped (the decision-model workflow, the
VPS procedures); it loses what is genuinely a CLI.

## 7. The test plan

Every phase carries its own acceptance, and the whole plan is measured
against the same seven checks the doctor runs today, which are all green at
the time of writing and must stay green:

1. `aroute doctor --harnesses` — 13/13 local, 14/14 on the VPS.
2. `aroute harness check` — no drift on any harness.
3. All five static lists carry exactly the 13 streaming lanes, with the
   windows aroute publishes and zero decision lanes.
4. `aroute sync` against the dashboard DB is **refused**, naming the lanes it
   would have lost.
5. A real MCP tool dispatch through Codex against `large-stack` — the
   acceptance this repo already uses, and the one that catches a catalog or
   provider regression that a config diff cannot.
6. `cargo test --release --workspace` and
   `cargo clippy --all-targets --all-features --locked -- -D warnings` clean,
   `Cargo.lock` unchanged.
7. The published card carries `lane` and `max_output_tokens`, and the
   harnesses read them rather than guessing.

## 8. The open decisions

**D1 — does `lane` go on the OpenAI-shaped card, or on a router-specific
sidecar?** The card shape is OpenAI's, and `lane` is not an OpenAI field. A
strict client might reject an unknown field. Every current consumer is
lenient, and the router already publishes `ar_upstream_model`, which is also
not an OpenAI field, so the precedent is established. This plan puts `lane`
on the card, and the operator can rule otherwise.

**D2 — is `max_output_tokens` the right name for the harness's ceiling?**
OpenAI's `/v1/models` does not publish it, but OpenAI's API does accept it as
a request parameter, so the name is not misleading. The alternative is
`ar_max_output_tokens`, consistent with `ar_upstream_model`, which is uglier
but unmistakably not an OpenAI field. `ar_max_output_tokens` is the safer
choice if any consumer is strict about unknown fields.

**D3 — does the renderer own `~/.claude/settings.json`?** Claude Code's
config is shared with the rest of the Claude tooling, and editing it from a
router CLI is a wider blast radius than editing a dedicated model list. The
plan says yes, because Claude is currently unowned (G7), but the operator may
prefer Claude to stay outside the router's reach.

**D4 — what is the migration story for a host where the scripts have already
drifted?** `aroute harness apply` is authoritative and overwrites. The plan
does not propose a merge, because the scripts have no canonical state worth
merging with: the router's published facts are the truth, and the local
copies are the thing that drifted.
