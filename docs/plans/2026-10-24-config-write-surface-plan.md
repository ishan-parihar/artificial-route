# The config-write surface: an outline-plan for closing it

Status: **outline-plan, written 2026-10-24 at the operator's request.** No code
from this document is implemented yet. It supersedes nothing: `docs/36` §4 step 8
specifies the first three verb groups (8a `oauth:`, 8b `server:`, 8c `providers:`
add/remove) and this document **extends that spec to the whole `Config` struct**
— `docs/36` step 8 stops at three sections; the operator asked for the rest.

Operator ask: *"how we can enable/disable the dashboard using the aroute cli"*
was answered in `9a8821a` (the `service enable|disable` verbs). This plan is the
second half of the same intent: **an agent must be able to update every aroute
setting from the CLI, without a browser and without hand-editing YAML.**

## 1. The full inventory (measured, not assumed)

`ar-config/src/lib.rs:1056` is the ground truth. Ten top-level sections; the
column that matters is the last one — **who can write it today**.

| # | Section | Shapes | Panel writes | CLI writes | Verdict |
|---|---|---|---|---|---|
| 1 | `server:` | `host`, `port` | yes (`webui.rs:1305-1335`, host+port only) | **no** | **GAP** |
| 2 | `keys:` | `name: $VAR` | no (mint only) | `sync`, `import` (wholesale) | partial, no targeted writer |
| 3 | `providers:` | `id`, `key`, `keys` | yes — the **three-part add** (`webui.rs:1771-1789`) | `sync` | **GAP** |
| 4 | `custom_providers:` | `id`, protocol, `base_url`, headers, `key_ref` | yes (`webui.rs:2198`) | `sync` | **GAP** |
| 5 | `combos:` | `id`, strategy, `targets`, `weights`, `pool`, `steps`, `compression`, `judge_model`, `context_length` | yes — **whole-section replace** (`webui.rs:1614` + `check_combos:1655`) | `import` (wholesale) | **GAP** — no single-item writer |
| 6 | `oauth:` | 12 fields per session (`OAuthSession`, `:818-895`) | **no route at all** | **nothing** | **GAP + data-loss risk** |
| 7 | `continue_on_cut:` | provider id list | no | **nothing** | **GAP** |
| 8 | `visibility:` | `blocked_providers`, `hide_paid_models`, `model_visibility_allowlist`, `model_visibility_denylist` | no | **nothing** | **GAP** |
| 9 | `provider_settings:` | `excluded_models` (globs) per provider | no | **nothing** | **GAP** |
| 10 | `limits:` | rpm, usd_micros, tokens, model, model_tokens, spend_window | yes | **yes** (`limits set`/`clear`) | done, `b546f4c` |

Two findings that change the shape of this plan:

**A. `import` destroys a config it cannot represent.** `commands.rs:2347` writes
`config.yaml` wholesale from `render_yaml` (`import/mod.rs:353`), which emits
`keys:`, `providers:`, optionally `custom_providers:`, then `combos:` — **and
never `oauth:`**. There is no existence check at the write. So
`aroute import --out-dir <dir>` against a directory whose `config.yaml` carries
an `oauth:` block **silently deletes that block by construction.** This is the
same failure class as B-1 (the hyphen-section data loss), on a different writer,
and it is *live* — `oauth:` is the section with no other writer to restore it.
**Phase 0 fixes this before any new verb lands**, because a new `oauth add` verb
makes the loss cheaper to recover from but does not stop `import` deleting it.

**B. Sections 7, 8 and 9 are unwritable *everywhere*.** Not a CLI-vs-panel gap:
no surface at all — not the panel, not `sync`, not `import`. The catalogue-folding
`docs/` calls it "the operator's catalog-visibility choices" and
`config.yaml`'s own comments say none of them can change what a client sees
until an operator writes one. **These need a new writer, not a mirror**, so this
plan invents semantics where none exist — and says so in each phase.

## 2. What this plan deliberately does not do

- **No `combos` whole-section writer from a file.** The panel's
  whole-section replace is a browser affordance (it holds the list client-side).
  A CLI verb that replaces the section from a file argument invites an agent to
  rewrite the whole routing table to change one target. Phase 4 is
  **single-item only**.
- **No new secret handling.** Every credential the plan touches stays a **name**
  (`$VAR` / `$AR_KEY_*` / a `keys:` row). No verb in this plan reads, writes or
  prints a secret value, matching `docs/06`'s "never print a credential" gate.
- **No `--now` on anything.** Every write prints the restart note; nothing
  restarts the proxy itself. The write and the state change stay two decisions
  (`9a8821a` established this split).
- **No semantic invention where the panel already has one.** For 3, 4 and 5 the
  plan *adopts* the panel's semantics, because the panel's are the tested ones
  and two editors for one data set is how they drift.

## 3. The phases, dependency-ordered

Each phase is independently shippable and independently revertible. The order is
by risk, not by section size: the destructive-writer fix first, the destructive
verbs last, the unwritable-everywhere sections at the end where they can be
reconsidered without blocking anything.

### Phase 0 — `import` must not delete a section it cannot represent *(data-loss fix, ships first)*

The only phase that fixes existing behaviour rather than adding a verb.

`import` gains one guard and one flag:

- **Guard (default).** Before writing, parse the existing `config.yaml` (if any)
  and compute which top-level sections `render_yaml` **cannot** emit — `oauth:`
  today; anything added to `Config` later. If the existing file carries one, the
  write is **refused** with a message naming the section and the exact command
  to preserve it. A refusal is exit 1 and writes nothing, per the same law that
  B-1 established: a write that silently drops a section named by nobody is the
  failure this project refuses everywhere else.
- **Escape hatch.** `--allow-section-loss <name>` (or `--force`) proves the
  operator saw the warning. Without a flag there is no silent path.

Test contract (one per outcome, project naming):
- `import_should_refuse_when_the_target_config_carries_a_section_it_cannot_emit`
  — the consumer-visible bug: `import --out-dir <dir>` over a config with an
  `oauth:` block leaves that block **byte-for-byte intact**.
- `import_should_carry_the_oauth_block_over_when_it_can_represent_it` — the
  follow-up once `render_yaml` can emit it, so the guard is not a permanent wall.
- negative control: remove the guard, the first test fails on a mutated file.

### Phase 1 — `aroute server show|set` *(smallest, unblocks the restart contract)*

Two verbs, two fields, the panel's exact validation (`webui.rs:1305-1335`):
empty host refused, port must be `1..=65535`, and nothing else about the listen
address is configurable from this surface.

- `aroute server show` — one row: host, port, and whether either differs from
  the default (`127.0.0.1:20128`), because "is this the default?" is the next
  question every caller asks.
- `aroute server set --host H --port P` — `splice_section(doc, "server", …)`,
  byte-identical elsewhere, plus the restart note (the listen address is read at
  boot).

Test contract:
- `server_show_should_print_the_effective_listen_address`
- `server_set_should_write_the_listen_address_and_leave_the_rest_byte_identical`
- `server_set_should_refuse_an_empty_host_and_name_the_fix`

### Phase 2 — `aroute providers add|remove` *(the three-part write, the panel's semantics)*

The panel's add is three writes that must land together or the config refuses to
load; the CLI must reproduce that atomically.

- `aroute providers add <id> [--key <name>]` — writes, in one
  `splice_section` pass backed by one `render_providers_section`:
  1. the `providers:` row,
  2. the `keys:` row it references,
  3. the `AR_KEY_<NAME>` line in the env file the service loads.
  A row with no binding is **refused** (`webui.rs:1761-1768`) — a provider with
  no credential fails at the first request, which is a worse answer than an
  upfront refusal. `<id>` must name a registry provider (`aroute providers` is
  how an agent discovers the ids), not a custom endpoint — Phase 3 is where a
  custom endpoint goes.
- `aroute providers remove <id>` — refuses while any combo still references the
  provider (`webui.rs:2436-2440`, the same set the CLI already computes for
  `doctor`), and drops the `keys:` entries the removed row alone held
  (`webui.rs:2366-2390`). A dead provider fails loudly; a dangling one fails
  late, which is why the refusal exists.

Test contract:
- `providers_add_should_write_the_row_the_key_ref_and_the_env_line_together`
- `providers_add_should_refuse_a_row_with_no_credential_binding`
- `providers_add_should_refuse_an_id_the_registry_does_not_know`
- `providers_remove_should_refuse_while_a_combo_references_the_provider`
- `providers_remove_should_drop_the_keys_row_only_it_held` — the negative case
  for a shared row: removing one of two providers that share a `keys:` entry
  must **not** take the row.

### Phase 3 — `aroute custom-providers add|remove` *(an endpoint that is not in the registry)*

Mirrors `provider_custom` (`webui.rs:2198`). A custom node is the operator's
way to add an endpoint without a rebuild, so an agent needs it as much as a
registry provider.

- `add --id X --protocol P --base-url U [--header K=V]… [--key-ref name]` —
  writes the `custom_providers:` row and its `AR_KEY_*` env line. `--key-ref`
  names the `keys:` row it resolves against, the same way `import` renders one
  (`import/mod.rs:370-377`).
- `remove --id X` — same reference check as Phase 2, same shared-`keys:` rule.
- Collision with a compiled-in catalog id is refused by
  `ar_registry::Registry::merge`; the verb surfaces that refusal rather than
  writing a row `ar serve` will reject at boot.

### Phase 4 — `aroute combo` single-item writers *(the routing table, one row at a time)*

`check_combos` (`webui.rs:1655`) is the validation both surfaces must share —
id legal (1-64 of `a-z0-9._-`, unique), every target's provider known (registry
or `custom_providers:`), every strategy one this build routes **or one already
in the file** (a save must round-trip a config it did not write). The plan
extracts it into `ar-cli` so the panel and the CLI call one function; two copies
is how they drift.

- `aroute combo add --id X --strategy S --targets a,b,c` — validates, then
  splices one row into `combos:`. Duplicate id refused.
- `aroute combo remove --id X` — refused while the id is referenced as another
  combo's step or judge (the same late-failure argument as Phase 2).
- `aroute combo target add|remove --combo X --target T` — the surgical edit:
  repointing one combo rather than rewriting the section, validating the target
  the same way.

Test contract:
- `combo_add_should_reject_a_target_the_catalog_does_not_know`
- `combo_target_add_should_splice_one_row_and_leave_the_section_byte_identical`
- `combo_remove_should_refuse_while_another_combo_steps_through_it`
- `check_combos_should_accept_a_round_trip_of_the_fixture` — already exists at
  `webui.rs:2618`; it must keep passing after the extraction.

### Phase 5 — `aroute oauth list|add|remove` *(the operator's stated goal)*

`docs/36` 8a specified these; the operator's ask makes them the phase the whole
plan is for. `OAuthSession` has 12 fields, of which only four are commonly
written. The verb writes **placement only, never a secret** — the block carries
no secrets by design (`config.yaml:57-85`).

- `aroute oauth list` — one row per session: `provider`, `authorization_url`,
  `token_url`, `client_id`, `redirect_uri`, `refresh_key`, `client_secret_key`.
- `add --provider X [--authorization-url U] [--token-url U] [--client-id I]
  [--redirect-uri U] [--refresh-key name] [--client-secret-key name] [--scope S]`
  — appends one row. Rules pinned, not invented (`config.yaml:61-79`):
  `authorization_url` and `token_url` are **never guessed** from the provider id
  (a guessed endpoint is an invented wire format); a `refresh_key` must name a
  `keys:` row or the session cannot renew and `ar doctor` says so; a
  `client_secret_key` names a third row; a `redirect_uri`, when given, must be
  `https` or a loopback `http`.
- `remove --provider X` — one session per provider id, so the key is the id.

**Ordering note:** `docs/36` 8a says this phase comes **after step 4** (the
resumable OAuth login), because step 4 is what makes the block worth creating
programmatically. That dependency still holds; Phase 5 is listed last of the
five verb phases only to respect it.

Test contract:
- `oauth_add_should_append_a_row_and_leave_the_rest_byte_identical`
- `oauth_add_should_refuse_a_refresh_key_that_names_no_keys_row`
- `oauth_add_should_refuse_a_non_loopback_http_redirect_uri`
- `oauth_list_should_never_print_a_secret_value` — same gate as `keys list`.

### Phase 6 — `aroute visibility` *(the section nothing writes, part 1)*

Four fields in `ModelVisibility`. None has a writer anywhere, so the semantics
are this plan's to choose — and the choice is **list-shaped and reversible**,
because the field's own doc says the block is the reason nothing is
irreversible: a blocked provider stays in `registry.json`, it is the *listing*
that drops it, so there is nothing to re-add.

- `aroute visibility show` — the four fields as one row, with `default` for the
  unset ones so a caller cannot confuse "unset" with "empty on purpose".
- `aroute visibility block --provider X` / `unblock` — `blocked_providers` add
  or remove, idempotent, both reporting **no-op** rather than an error when the
  state already matches (the `keys list` convention).
- `aroute visibility hide-paid true|false` — the boolean
  (`hide_paid_models`), default `false`, i.e. unset means the unfiltered catalog.
- `aroute visibility allow|deny --glob G` — the two glob lists, validated by
  compiling the glob once so an illegal pattern is refused here rather than at
  the first `/v1/models`.

### Phase 7 — `aroute provider-settings` + `aroute continue-on-cut` *(the section nothing writes, part 2)*

- `aroute provider-settings exclude --provider X --glob G` / `include` — one
  provider's `excluded_models`, the per-provider counterpart to Phase 6's
  catalog-wide lists. Same glob validation, same idempotence.
- `aroute continue-on-cut add|remove --provider X` — the provider id list that
  may take over a cut streamed reply. Empty means "no continuation anywhere", so
  the verb must print the whole list after each edit: a list an agent cannot
  read back is one it cannot reason about.

Both are tiny writers over one field each, and both are the same shape as
Phases 6's verbs — so this phase is small, and it is last because if Phases 6's
semantics prove wrong these inherit the fix rather than the other way round.

## 4. Cross-cutting, applies to every phase

1. **One writer module.** `splice_section` + `write_file` already exist
   (`commands.rs:2977`, `:3365`) and B-1 fixed the predicate. Phase 0 onward adds
   `render_*_section` siblings beside `import`'s existing ones; the rule is that
   a new writer reuses `splice_section` rather than open-coding YAML.
2. **The restart note, every write.** The line `9a8821a` added to `limits set`
   is the contract; each new write prints it. A write that does not name the step
   that makes it live is the B-3 failure.
3. **`--json` on every read.** `models`/`providers`/`doctor`/… already have it;
   `server show`, `oauth list`, `visibility show` and the rest are reads and take
   it from the start, per `docs/06`'s machine-readable rule.
4. **Round-trip tests everywhere.** Each writer: apply → the target section is
   spliced and **every other section is byte-identical** → reverse → the file is
   back to its original bytes. That assertion is the one that catches a
   whole-file rewrite masquerading as a splice.
5. **Cost.** Every verb in this plan answers in <200 tokens. The token table
   (`docs/36` §9.5) is the measurement of the existing surface; this plan adds
   to it.

## 5. Risks, and how each is handled

| Risk | Handling |
|---|---|
| An agent rewrites the routing table badly | single-item writers only (no whole-section `combos` verb) |
| A write silently drops a section | Phase 0's guard, plus the byte-identical round-trip test per verb |
| CLI and panel drift | extract `check_combos`, adopt the panel's refusals verbatim, cite the panel's `file:line` in each phase |
| A guessed secret or endpoint is written | every semantic is pinned to a `config.yaml` comment or a refusal that exists today |
| Live config damaged mid-plan | every test drives the verb through `empty_dir`-style temp dirs; the fixture config is never the target; each phase is revertible by deleting a verb group |
| Boot state changed by a write | nothing in this plan touches systemd; `service enable|disable` already owns that and Phase 0-7 write only the YAML + env file |

## 6. Rollback

Every phase is additive verb groups over `splice_section`; reverting a phase is
deleting its `match` arm and its args struct. Phase 0 is the one phase that
modifies existing behaviour, and its rollback is removing one guard — no
migration, no state. `import`'s escape-hatch flag stays as the record that the
operator chose to lose a section.

## 7. Closes

- **Q17 / DEC-1** — the CLI reaching every config section. The operator asked for
  this plan, so the direction is signalled; the sub-decisions (single-item vs
  whole-section writers, whether Phases 6-7 invent semantics or wait for a design)
  are still the operator's to rule.
- The second half of the `import` half of the Q17 finding: once Phase 0 lands,
  the CLI can update every section **and no path destroys a section**.
