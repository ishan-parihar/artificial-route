# The config-write surface: an outline-plan for closing it

Status: **Phases 0-7 shipped, and Phase 5 with them — the plan is complete**
(`96cf513`, `9c46ff5`, `a34cd94`, `5e0a874`, `16d7cf1`, `218cb00`, `fadff81`,
`4a2f0c8`). Phase 5 was recorded here as blocked on resumable login and the
claim did not survive re-derivation (`e94caf8`): it was never blocked, and it is
now shipped. What is left of the plan is three rulings that are the operator's
rather than work: Q17 (which sections the CLI reaches, whether `mcp` ships or
the CLI grows a `commands --json` catalog, and whether `import` may update a
config in place). Written at the
operator's request, then Phase 0 (the data-loss fix) was validated and landed
before any new verb. It supersedes nothing: `docs/36` §4 step 8 specifies the
first three verb groups (8a `oauth:`, 8b `server:`, 8c `providers:` add/remove)
and this document **extends that spec to the whole `Config` struct** —
`docs/36` step 8 stops at three sections; the operator asked for the rest.

Operator ask: *"how we can enable/disable the dashboard using the aroute cli"*
was answered in `9a8821a` (the `service enable|disable` verbs). This plan is the
second half of the same intent: **an agent must be able to update every aroute
setting from the CLI, without a browser and without hand-editing YAML.**

## 1. The full inventory (measured, not assumed)

`ar-config/src/lib.rs:1056` is the ground truth. Ten top-level sections; the
column that matters is the last one — **who can write it today**.

| # | Section | Shapes | Panel writes | CLI writes | Verdict |
|---|---|---|---|---|---|
| 1 | `server:` | `host`, `port` | yes — `splice_section(&next, "server", …)` at `webui.rs:1362` | **no** | **GAP** |
| 2 | `keys:` | `name: $VAR` | yes — upserts a binding via `with_key_binding` (`webui.rs:711`, the `provider-key` route at `:343`) | `sync` (indirect, via `sync_credentials:3178`); `keys arm-gate\|mint` write the store, not the file | partial, no targeted writer |
| 3 | `providers:` | `id`, `key`, `keys` | yes — the **three-part add** (`webui.rs:1802-1815`: row + `keys:` ref + env value, no-binding refused at `:1795`) | `sync` | **GAP** |
| 4 | `custom_providers:` | `id`, protocol, `base_url`, `endpoint`, headers, `key_ref` | yes (`webui.rs:2198`) | `sync` | **GAP** |
| 5 | `combos:` | `id`, strategy, `targets`, `weights`, `pool`, `steps`, `compression`, `judge_model`, `context_length` | yes — **whole-section replace** (`webui.rs:1614`) validated by `check_combos` (`:1655`) | `import` (wholesale) | **GAP** — no single-item writer |
| 6 | `oauth:` | 13 fields per session (`OAuthSession`, `:818-907`) | **no route at all** | **nothing** | **GAP + data-loss risk** |
| 7 | `continue_on_cut:` | provider id list | no | **nothing** | **GAP** |
| 8 | `visibility:` | `blocked_providers`, `hide_paid_models`, `model_visibility_allowlist`, `model_visibility_denylist` | no | **nothing** | **GAP** |
| 9 | `provider_settings:` | `excluded_models` (globs) per provider | no | **nothing** | **GAP** |
| 10 | `limits:` | rpm, usd_micros, tokens, refuse_unpriced, model, model_tokens, spend_window | yes | **yes** (`limits set`/`clear`) | done, `b546f4c` — **amended by `912941f`: `--key default` wrote a key, not the field** |

Two findings that change the shape of this plan:

**A. `import` destroys a config it cannot represent.** *Fixed by Phase 0 — this
is the finding as it stood.* `commands.rs:2348` wrote `config.yaml` wholesale
from `render_yaml` (`import/mod.rs:353`), which emits
`keys:`, `providers:`, optionally `custom_providers:`, then `combos:` — **and
never `oauth:`**. There was no existence check at the write. So
`aroute import --out-dir <dir>` against a directory whose `config.yaml` carries
an `oauth:` block **silently deletes that block by construction.** This is the
same failure class as B-1 (the hyphen-section data loss), on a different writer,
and it is *live* — `oauth:` is the section with no other writer to restore it.
**Phase 0 fixed this before any new verb landed**, because a new `oauth add` verb
makes the loss cheaper to recover from but does not stop `import` deleting it.

**B. Sections 7, 8 and 9 are unwritable *everywhere*.** Not a CLI-vs-panel gap:
no surface at all — not the panel, not `sync`, not `import`. The catalogue-folding
`docs/` calls it "the operator's catalog-visibility choices" and
`config.yaml`'s own comments say none of them can change what a client sees
until an operator writes one. **These need a new writer, not a mirror**, so this
plan invents semantics where none exist — and says so in each phase.

*Both findings were validated 2026-10-24 by machine extraction rather than by
reading: the ten sections were re-derived from the `Config` struct, and every
write site in the workspace was enumerated by regex over `splice_section` /
`remove_section` / `write_file`, panel and CLI separately, test modules
excluded. That pass corrected the table above — `custom_providers` has an
`endpoint` field, `Limit` has a `refuse_unpriced` field, `OAuthSession` has 13
fields not 12, and the panel does write `keys:` where the table said it did
not — and it confirmed B exactly: no writer for sections 7, 8 or 9 exists
anywhere in the workspace.*

**C. A shipped writer can be untested in its only user-facing direction.**
*Found by the post-ship validation pass, 2026-10-25 — the pass that drives every
write verb through its own round trip rather than re-reading the phase's tests.*
The defect counts above are per phase and are large; this finding is about a
*class* that survives inside a phase whose own tests are all green. Two writers
so far were only ever called on a file already in the shape being written, so the
direction nobody ran was the one the operator would type:

| verb | what the passing tests could not see |
|---|---|
| `visibility hide-paid` | a key the operator wrote **with a value** was never matched, so each write appended a duplicate; and `false` was spelled as the word, though `ModelVisibility`'s canonical form for it is absence |
| `limits set --key default` | `default` is a field of `limits:`, not an entry in `keys:`, so the named write appended a second row and the ceiling landed on neither |

**The rule this earns, and the phases already shipped now inherit it:** a writer's
test must start from the *other* state and run both directions. A test that
starts from the shape being written tests the writer's assumption about its
input, not its behaviour — and that is precisely where a shipped defect can sit
while every test in the module is green.

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

### Phase 0 — `import` must not delete a section it cannot represent *(data-loss fix)*

**Shipped 2026-10-24.** The only phase that fixes existing behaviour rather than
adding a verb, so it landed before Phase 1.

*One ruling this phase needs from the operator, flagged rather than assumed:* the
guard's stance that **an in-place import is not a supported operation at all**.
Refusing on `server:` and `limits:` — not just the unrecoverable `oauth:` — is
what makes that stance real, and it is a change to behaviour an operator may
currently be relying on. The escape hatch makes it reversible either way, but the
stance itself was a judgement call made to ship, not a decision the operator has
ruled.

`import` gained one guard and one flag:

- **Guard (default).** Before writing, parse the existing `config.yaml` (if any)
  and compute which top-level sections `render_yaml` **cannot** emit — `oauth:`
  today; anything added to `Config` later. If the existing file carries one, the
  write is **refused** with a message naming the section and the exact command
  to preserve it. A refusal is exit 1 and writes nothing: not `registry.json`,
  not `freeBudgets.json`, not the `.1` backup rotation either. Per the same law
  that B-1 established: a write that silently drops a section named by nobody is
  the failure this project refuses everywhere else.

  **Scope, measured 2026-10-24:** the unemittable set is `oauth:` *plus*
  `server:`, `limits:`, `continue_on_cut:`, `visibility:` and
  `provider_settings:`. Only `oauth:` is unrecoverable, but the guard does not
  grade — it refuses on all of them, and the repo's own `config.yaml` carries a
  `server:` block, so an import aimed at this working tree is refused by default.
  That is intended, and it follows `docs/12` §5.1: import "writes four files
  unconditionally" — it generates a fresh configuration rather than updating one
  in place, so an operator who wants an in-place change is pointing the wrong
  verb at it. The flag is how an operator says they know.
- **Escape hatch.** `--allow-section-loss <name>` proves the operator saw the
  warning, and accepts either one section name or the blanket `all`. A name that
  does not match the section at risk still refuses, so a typo is not a bypass.

The emittable set is `emittable_sections()` in `import/mod.rs`, derived from
`render_yaml` rather than from `Config`'s field list, so it follows the renderer
instead of drifting from it.

Evidence, all run:
- the pre-fix bug reproduced live: a seeded `oauth:` block went from present to
  absent with exit 0 and no warning (352 bytes -> 222, `refresh_key:` gone).
- the post-fix A/B on the same seed: exit 1, the file byte-for-byte identical
  (`cmp` clean), and no other output file written.
- `--allow-section-loss all` and `--allow-section-loss oauth` both overwrite,
  and `--allow-section-loss limits` refuses.
- negative control: removing the guard call makes the new tests fail (exit 0
  where 1 is required), so they detect the bug they claim to.

Tests, all in `crates/ar-cli/tests/cli.rs`:
- `import_should_refuse_when_the_target_config_carries_a_section_it_cannot_emit`
  — the consumer-visible bug, asserted on the file's bytes and the exit code.
- `import_should_allow_named_section_loss_only_when_the_name_matches` — the
  hatch is a conscious decision, so a mismatched name still refuses.
- `import_should_still_write_a_target_that_carries_no_unemittable_section` — the
  guard must not become a wall that refuses every overwrite.
- `import_should_name_every_unemittable_section_with_each_one_quoted` — a
  three-section config must quote all three. The first join was `", `"`, which
  printed `` `server, `limits:` `` and left one name unquoted; the test's negative
  control is reverting that join.

Still deferred from this phase: `render_yaml` cannot emit `oauth:`, so an
import into a directory that holds sessions still cannot carry them across.
That is the follow-up below, not a gap in the guard.

### Phase 1 — `aroute server show|set` — **SHIPPED 2026-10-24** (`9c46ff5`) *(smallest, unblocks the restart contract)

Two verbs, two fields, the panel's exact validation (`webui.rs:1343`, `:1351`;
the only `server:` splice in the tree is `webui.rs:1362`):
empty host refused, port must be `1..=65535`, and nothing else about the listen
address is configurable from this surface. `--port` is typed `u16`, so the upper
bound is clap's job and needs no hand-written check.

- `aroute server show` — one row: host, port, and whether either differs from
  the default (`127.0.0.1:20128`), because "is this the default?" is the next
  question every caller asks. Two `*_changed` columns rather than one
  `is_default` cell: which field moved is a different question from whether
  either did, and collapsing them hides the answer. Compared against
  `Server::default()`, so the default is derived from its one owner.
- `aroute server set --host H --port P` — per-field merge, so a flag not passed
  keeps what the file already carries and `set --port 20129` cannot quietly
  change the host. `splice_section(doc, "server", …)`, byte-identical
  elsewhere, plus the B-3 restart note.

Two defects in shared code, both found by running the verb rather than reading
the code, and both fixed in Phase 1:

- **`splice_section` ate the comment block after a replaced section.** A blank
  line or a column-0 comment is not a section header, so nothing stopped the
  skip, and the block documenting the *next* section was destroyed. This
  repo's own `config.yaml` carries those blocks, so a `server:` splice silently
  deleted the two comments introducing `keys:`. Indentation cannot distinguish
  them (YAML allows column-0 comments anywhere), so the skip now ends at a
  blank line or a `#`.
- **The host was written quoted.** The panel writes it through
  `serde_json::Value`, which is a JSON serializer: `127.0.0.1` came back as
  `"127.0.0.1"`. Now written raw. The loader parses both, but the written form
  is the one the operator reads back.

Test contract, all four shipped:
- `server_show_should_print_the_effective_listen_address`
- `server_set_should_write_the_listen_address_and_leave_the_rest_byte_identical`
- `server_set_should_refuse_an_empty_host_and_name_the_fix`
- `server_set_should_not_swallow_the_comment_block_after_the_section` (added
  after the comment-eating bug; its negative control reverted the fix and
  failed with "the comment block documenting the next section was eaten")

Negative controls run and confirmed to fail: the comment-preservation fix, the
empty-host refusal, the port-0 refusal, the no-field-given refusal, and the
merge-not-replace behaviour.

### Phase 2 — `aroute providers add|remove` — **SHIPPED 2026-10-24** (`a34cd94`) *(the three-part write, the panel's semantics)*

The panel's add is three writes that must land together or the config refuses to
load; the CLI reproduces that with the panel's exact ordering.

- `aroute providers add <id> --credential-file <PATH|->` — the row, the `keys:`
  reference and the `$AR_KEY_*` line in `ar.env`, beside the config, the
  convention `sync` and the panel's `Ctx::new` share. The env value is written
  **first**, because that is what publishes the credential: a config that does
  not change is not "nothing was written", and the output says which file
  moved. Two splices, not one — `providers:` gets a rendered row list and
  `keys:` gets its reference upserted — and **the credential is never an
  argument**. `--key <SECRET>` would be visible in the process table and in the
  shell's history, so the flag reads a file (or stdin, `-`), the same rule
  `keys revoke` already holds. A row already configured keeps the variable its
  own `keys:` entry names; one that points at a literal rather than an
  `$AR_KEY_*` variable is refused rather than re-pointed (`webui.rs:1795` —
  "has no AR_KEY_* binding for its credential", with the exact fix in the same
  message). `<id>` must name a registry provider; an ad-hoc endpoint is
  Phase 3's verb.
- `aroute providers remove <id>` — refuses while any combo still references the
  provider (`webui.rs:2452` closure, `:2468` conflict; the same set
  `doctor` computes), and drops the `keys:` entries the removed row alone held.
  A dead provider fails loudly; a dangling one fails late, which is why the
  refusal exists.

Four defects found by running the verbs, all fixed with them:

- **The panel's reader could not open a config the CLI had to.** `load(cli)`
  expands every `$VAR` and refuses on a missing one, so `providers add` refused
  on a config whose *other* keys were unexported — which is exactly the config
  an operator edits *because* a credential is missing. `load_edit`
  (`commands.rs:124`) parses unexpanded, the same shape as `webui.rs`'s
  `load_cfg`, and both write verbs use it.
- **The remove invented a section the operator never had.**
  `render_custom_section` emits `custom_providers:` even for an empty row list,
  so an unconditional splice wrote it into a file that had none.
- **Every write left an extra blank line.** The renderers end with `\n`, and
  pushing that into a line list and joining with `\n` put two blank lines after
  the spliced section. Fixed once in `splice_section` (`commands.rs:3524`), so
  every writer is covered.
- **A no-op write dirtied the file.** The renderers emit block style where a
  hand-written file uses flow style, so re-adding an already-present provider
  re-rendered and reordered. `means_the_same` (moved from `webui.rs` to
  `commands.rs`, so the panel and the CLI share one owner) gates both writes.

Three helpers moved from `webui.rs` to `commands.rs` — `raw_keys`,
`with_key_binding`, `splice_remove_provider`, `set_env_var` and
`means_the_same` — because a rule about writing the config has one answer, and
two copies could drift. `commands.rs` is where the other writers already live.

Test contract, all six shipped:
- `providers_add_should_write_the_row_the_key_ref_and_the_env_line_together`
- `providers_add_should_refuse_an_id_the_registry_does_not_know`
- `providers_add_should_work_on_a_config_whose_other_keys_are_unexported`
- `providers_remove_should_refuse_while_a_combo_references_the_provider`
- `providers_remove_should_drop_the_keys_row_only_it_held` — the negative case
  for a shared row, which also asserts no `custom_providers:` is invented
- `splice_section_does_not_add_a_blank_line_after_the_replaced_section`

Negative controls run and confirmed to fail: the registry check, the combo
reference check, the `load_edit` (expanding) parse, the `custom_providers`
guard, and the blank-line trim.

### Phase 3 — `aroute custom-providers add|remove` — **SHIPPED 2026-10-24** (`5e0a874`) *(an endpoint that is not in the registry)*

Mirrors `provider_custom` (`webui.rs:2091`, not `:2198` as first written — the
handler's line moved). A custom node is the operator's way to add an endpoint
without a rebuild, so an agent needs it as much as a registry provider.

- **Shipped as `aroute providers add-custom`, not a new verb.** A separate
  `custom-providers` command would be a second place to learn the same
  semantics, and the row already goes to `custom_providers:` in the file the
  `providers` verbs own; `aroute providers remove` already reaches it, which is
  the sentence that decided it. `--id X --protocol P --base-url U
  [--endpoint E] [--header K=V]… [--credential-file PATH|-]` writes the row, the
  `keys:` reference and the `$AR_KEY_*` env line, in the panel's order.
- `--credential-file` rather than `--key <SECRET>`, the Phase 2 rule: an
  argument is visible in the process table and the shell's history. A **new**
  row must carry one (a keyless node fails every request); an **edit** of an
  existing row may omit it, which is how an operator moves a base URL without
  re-pasting a secret they already store.
- Collision with a compiled-in catalog id is refused by
  `ar_registry::Registry::merge`, and the verb checks `registry()` directly
  rather than `catalog`, which merges the config's own custom rows — checking
  the merged catalog makes an existing custom row collide with itself and
  refuses every edit of one.

Four defects found by the round trip, all in shared code:

- **`providers remove` refused a row `add-custom` had just written.** It
  checked only `providers:`, so an ad-hoc node was unremovable from the CLI.
- **The remove left an empty `custom_providers:` header behind.** The renderer
  emits the header for an empty row list, so "no rows left" and "an empty
  section" are not the same thing. `empty_custom_section` drops the header and
  its rows and keeps the comment block below; `remove_section` skips to the
  next header and would eat **62 lines** of documentation (proven by negative
  control: 63 lines deleted) — the Phase 0 data-loss shape rearmed by a new
  caller.
- **The remove wrote a `providers:` section into a config that carried none.**
  A splice of an absent section *inserts* it, so the guard is the same one, on
  the other list.
- **Every `keys:` write re-sorted the operator's own rows.**
  `with_key_binding` rendered through a `BTreeMap`; `raw_keys_in_order` walks
  serde_yaml's insertion-ordered `Mapping` instead. `sync` and the removal's
  orphan-drop use the same walk, and `import::render_keys_section` — the sorted
  spelling, never right for a write — is gone.

Two `splice_section` invariants restored while testing, both pre-existing: the
trailing newline is the file's, not the writer's (the repo's config deliberately
has none, so every write reported a whole-file diff), and an inserted section
goes **below** the blank line and comment block that introduce the next one,
rather than flush against them.

Test contract, all shipped:
- `providers_add_custom_should_write_the_row_the_key_ref_and_the_env_line`
- `providers_add_custom_should_refuse_a_registry_id`
- `providers_add_custom_should_refuse_an_absolute_endpoint`
- `providers_add_custom_should_refuse_a_new_row_with_no_credential`
- `providers_add_custom_should_keep_the_binding_the_row_already_names`
- `custom_providers_round_trip_must_come_back_byte_identical`
- `custom_providers_remove_the_last_row_must_take_the_section`

Negative controls run and confirmed to fail: the `providers:` check, the empty
section, `remove_section` in its place (63 lines), and the unguarded
`providers:` splice.

### Phase 4 — `aroute combo` single-item writers — **SHIPPED 2026-10-25** (`7a87077`) *(the routing table, one row at a time)*

`check_combos` (measured at `webui.rs:1564`, not the `:1655` the draft said) is
the validation both surfaces must share — id legal (1-64 of `a-z0-9._-`,
unique), every target's provider known (registry or `custom_providers:`), every
strategy one this build routes **or one already in the file** (a save must
round-trip a config it did not write). Extracted into `commands.rs` (not
`ar-cli` at large) so the panel re-exports it and the CLI writes call one
function; two copies is how they drift.

- `aroute combo add <id> --strategy S --targets a,b,c` — validates, then
  appends one row. `a@2` declares a weight, and the map form is what is
  written. Duplicate id refused.
- `aroute combo remove <id>` — refused while the id is referenced as another
  combo's target, pool entry, step or judge (a wider set than the draft's
  "step or judge": `combo_names` covers every place a combo can name one).
- `aroute combo target add|remove --combo X --target T` — the surgical edit.

**What changed from this draft, and why.** The draft says "splices one row into
`combos:`". A splice cannot. Round-tripping the repo's real `config.yaml`
against a whole-section splice produced two defects that reading the source
could not:

1. **The operator's own notes moved.** The repo's `combos:` carries a blank
   separator and a four-line comment introducing its second row. Rendering the
   section from the parsed config emitted both in canonical order, so the
   comment that documents `cheap` ended up *below* it. The panel's wholesale
   save does this (`webui.rs:1522`) and it is acceptable there; a verb asked to
   add one row has no business reflowing the other two. The writers are
   therefore row-level: `append_combo_row` / `remove_combo_row` place or drop a
   row's own lines and copy every other byte, and
   `add_target_line` / `remove_target_line` do the same for one target.
   `import::render_combo_block` is the one owner of a row's shape, used by the
   wholesale renderer and the row writers alike.
2. **`splice_section` duplicated rows.** It ended its skip at the first blank
   line or column-0 comment, reasoning that both are unindented and so belong
   to the next section. The repo's `combos:` carries a blank separator *inside*
   the section, so every splice stopped there and the rows after it survived
   below the rendered block: a three-row table read six. Now a look-ahead
   (`rest_is_another_section`), which ends the skip only where the next line of
   substance is column-0 — so a note that introduces the *next* section still
   survives, and the existing blank-line test still pins that.

Test contract, all six written against the repo's own `config.yaml` through
`include_str!`:
- `combo_add_then_remove_is_a_byte_identical_round_trip`
- `combo_target_add_then_remove_is_a_byte_identical_round_trip`
- `combo_writers_preserve_the_files_own_trailing_newline` (the Phase 3 contract,
  restated: `combo add` is the third writer to reach this file)
- `splice_consumes_a_section_body_that_carries_a_blank_separator` (defect 2)
- `splice_consumes_a_column_zero_comment_between_two_rows` (defect 2's other
  half, and the record of why the row writers exist)
- `check_combos_accepts_the_repos_own_config` (the extraction's own proof)

Verified: add/remove, target add/remove and the weighted map form are each
byte-identical round trips whose diff is the new row and nothing else; five
refusals fire with their own messages and none writes a file; the repo's
`config.yaml` was never opened for writing and the running services were never
restarted.

### Phase 5 — `aroute oauth list|add|remove` *(the operator's stated goal)* — **BLOCKED, MEASURED 2026-10-25**

`docs/36` 8a specified these; the operator's ask makes them the phase the whole
plan is for. `OAuthSession` has 13 fields (`ar-config/src/lib.rs:818-853`), of
which seven are commonly written. The verb writes **placement only, never a
secret** — the block carries no secrets by design.

- `aroute oauth list` — one row per session: `provider`, `authorization_url`,
  `token_url`, `client_id`, `redirect_uri`, `refresh_key`, `client_secret_key`.
- `add --provider X [--authorization-url U] [--token-url U] [--client-id I]
  [--redirect-uri U] [--refresh-key name] [--client-secret-key name] [--scope S]`
  — appends one row. Rules pinned, not invented: `authorization_url` and
  `token_url` are **never guessed** from the provider id (a guessed endpoint is
  an invented wire format); a `refresh_key` must name a `keys:` row or the
  session cannot renew and `ar doctor` says so; a `client_secret_key` names a
  third row; a `redirect_uri`, when given, must be `https` or a loopback `http`.
- `remove --provider X` — one session per provider id, so the key is the id.

**Amended 2026-10-25, after measuring: the phase is NOT blocked, and the
paragraph below mis-read its own evidence.** Re-derived from the tree before
amending, because the claim it replaces was carried through three summaries
without being re-checked:

- `session_for` (`auth.rs:123-134`) **refuses** a login whose config has no
  `oauth:` block for the provider, so `auth login` needs an `oauth:` row
  *already written* before it can run. `oauth add` is login's prerequisite,
  not its dependent: the dependency runs the opposite way from what the
  paragraph below says.
- `refresh_key` is a **declaration the operator supplies** in that row —
  `client_secret_key` and `token_url` are declared the same way — and
  `persist_token` reads it from the parsed config, never from the token. The
  refusal it raises is correct for a session that declares no name and gets a
  refresh token back, and it is a *validation* rule the new verb must apply
  (one of the plan's own test contracts), not a blocker.
- `auth.rs` writes no config file at all. Nothing in the login flow persists a
  row, so no "resumable login" work is a precondition for a writer that
  appends one.

So the phase is **shippable now**, and what it needs from the operator is a
ruling on two shape questions rather than a code dependency: whether
`oauth add` may write a row with no `refresh_key` when the provider renews
(the plan's own test contract says refuse), and whether `client_id` is
required or left to the operator's own judgement.

**Shipped, `4a2f0c8`.** The blocker paragraph below was wrong and the phase
was never blocked; the amendment above the divider says how. Thirteen tests,
all five of the phase's own test contracts among them, plus the two controls the
round trips earned:

| what broke running it | what the writer did |
|---|---|
| `append_row` inserted where a line *was* an entry | a section whose only row had continuations had no entry line that ended a row, so the loop walked past the section's end and the write produced nothing — which the verb reported as `unchanged` |
| the no-section path wrote the row at column zero | not a list under a header, so the file did not load |

The fix for the first is a rule the other phases also needed and did not have:
**a row is its entry plus its continuation lines, so "where does the row end" is
a property of the next line's indent, never of this line's shape.**

--- The original measurement, superseded by the paragraph above ---

**Why it is blocked, measured rather than assumed.** The plan's own ordering note
says this comes after step 4, the resumable OAuth login. `auth.rs` has no
resumable path: `aroute auth login` is single-shot with a `--timeout`, and
`AuthLoginArgs` carries no resume state. The blocker is not cosmetic.
`persist_token` (`auth.rs:265-288`) **refuses** a session that declares no
`refresh_key` when the provider returned a refresh token — correctly, since
without one the session dies on its first 401. So an `oauth add` that omits
`refresh_key` writes a row the operator's own login cannot complete, and the
verb would be handing them a config that looks finished and is not.

That is a dependency, not an argument against the phase: it is the reason to do
step 4 first, and it names the one field the new verb must not let pass.

**Test contract, unchanged:**
- `oauth_add_should_append_a_row_and_leave_the_rest_byte_identical`
- `oauth_add_should_refuse_a_refresh_key_that_names_no_keys_row`
- `oauth_add_should_refuse_a_non_loopback_http_redirect_uri`
- `oauth_list_should_never_print_a_secret_value` — same gate as `keys list`.
- `oauth_add_should_require_a_refresh_key_when_the_provider_renews` — new, from
  the `persist_token` finding above.

### Phase 6 — `aroute visibility` *(the section nothing wrote, part 1)* — **SHIPPED 2026-10-25**

Four fields in `ModelVisibility`. None has a writer anywhere, so the semantics
are this plan's to choose — and the choice is **list-shaped and reversible**,
because the field's own doc says the block is the reason nothing is
irreversible: a blocked provider stays in `registry.json`, it is the *listing*
that drops it, so there is nothing to re-add.

**Amended by the post-ship validation pass** (`de6bd63`). Driving every write
verb through its own round trip instead of trusting the phase's tests found that
`hide-paid` had never been editable at all. Two defects, both in the direction
nobody had exercised, and both invisible to the phase's own tests because every
test called the writer on a section whose key was already in the shape being
written:

| what broke | what the writer did |
|---|---|
| the key was matched against a bare `key:` header | a key the operator wrote *with a value* was never found, so every write appended a second copy beside the first — a duplicate-key config the loader refuses |
| `false` was written as the word | `ModelVisibility` serialises `false` as the key's absence (`skip_serializing_if = "Not::not"`), so spelling it left a byte the operator never put there, and the round trip that should restore the file could not |

The generalisation the other phases now inherit: **a writer's tests must run the
verb in both directions on a file that starts in the *other* state.** A test
that starts from the shape being written tests the writer's assumption, not its
behaviour.

- `aroute visibility show` — the four fields as one row, with `default` for the
  unset ones so a caller cannot confuse "unset" with "empty on purpose".
  Spelled `visibility` with no verb too, the way `aroute server` reads.
- `aroute visibility block --provider X` / `unblock` — `blocked_providers` add
  or remove, idempotent, both reporting **no-op** rather than an error when the
  state already matches (the `keys list` convention).
- `aroute visibility hide-paid true|false` — the boolean
  (`hide_paid_models`), default `false`, i.e. unset means the unfiltered catalog.
- `aroute visibility allow|deny --glob G` — the two glob lists, validated by
  compiling the glob once so an illegal pattern is refused here rather than at
  the first `/v1/models`.

**What the plan said, and what measured differently.**

`visibility:` is a map of lists, not a list of rows, so the Phase 4 row writers
do not reach it and two new ones are added beside them: `append_list_entry` and
`remove_list_entry`. Same discipline, other shape — one entry is placed at the
end of the list that exists and every other byte is copied, so `block` then
`unblock` restores the file byte-for-byte. Measured on the repo's `config.yaml`
(no trailing newline) and on the `oauth:`/`visibility:`/`mcp:` fixture the
Phase 4 invariant test uses, where the other sections survive untouched.

Four defects the round trip found that reading did not, each pinned by a control
that fails without its fix:

1. **`section_depth` never matched a section.** It compared a bare name against a
   header line that carries its colon, so the first `block` on any config added a
   *second* `visibility:` header and the file failed to load with a duplicate
   key. The scan's own predicates were right, which is why the headless tests
   were needed to see it.
2. **A new key landed in the last section, not its own.** The tail path appended
   to the end of the document, so an allow-list written into a config whose last
   section was `mcp:` ended up under `mcp:`. The fix is `section_tail`, and it is
   the same rule `splice_section` already uses for a missing section.
3. **A leading-`*` glob breaks the file.** `- *-search` is a YAML *alias*, so the
   config failed to load with "unknown anchor" — the matcher's glob syntax and
   YAML's scalar syntax collide exactly there, and the writer is the only place
   that can see it. `yaml_entry` quotes the values that need quoting and leaves
   the rest plain.
4. **`hide-paid` was a flag, not a value.** The plan spells `hide-paid
   true|false`; clap parses a bare `bool` positional as a toggle, so the arity
   and the action are named explicitly.

The plan's glob rule was, read literally, unenforceable: the matcher compiles
anything, so there is no illegal pattern to catch. What is actually checkable is
narrower and the verb does that — an all-whitespace entry, which
`normalized()` drops at serve time and would therefore look written while
changing nothing, is refused. Recorded rather than implemented as written.

**Test contract, as written plus the four above:**

- `block_creates_the_section_when_none_exists`
- `block_appends_inside_the_existing_list`
- `blocking_the_same_provider_twice_is_a_no_op_that_says_so`
- `a_list_entry_write_leaves_the_rest_of_the_document_alone`
- `a_flow_list_is_refused_rather_than_rewritten`
- `a_new_key_lands_inside_its_own_section_not_the_last_one`
- `a_leading_star_glob_is_quoted_so_the_config_still_loads`
- `a_value_that_needs_no_quoting_is_written_plain`

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

**Shipped, `fadff81`.** The prediction held: Phase 6's `owner`-shaped writers
carried over, and this phase found six more defects that reading the code would
not have caught. All six run through the same fact — a flat list has no key
line, so every arithmetic derived from one is wrong — and all six are now
pinned by tests whose mutation controls have actually been run:

| what broke | what the writer did |
|---|---|
| the entry's indent was derived from a list key that does not exist | a flat list's entry sat four under its header, not two |
| `scan_nested_entry` never opened the list for `owner = None` | a second `add` wrote a duplicate instead of reporting `unchanged` |
| the section-drop predicate counted only keys | removing one of two entries read the survivor as empty and dropped the section, with the `mcp:` after it |
| `append_nested_list_entry` had no flat-list arm | an entry was pushed onto the next section's header and swallowed it |
| `create_nested_list` skipped `yaml_entry` | a leading-`*` glob reached the file unquoted and the loader refused it as an unknown anchor |
| the new section's separator took the file's own terminator | a file with no trailing newline got `port: 9000continue_on_cut:`, which is not YAML |

The amendment to the cross-cutting rules below is the same one Phase 6 made:
the rule "a writer that touches one row uses a row-level edit" now has a
second half that only running the verb produced — **a flat list under a
section is the `owner = None` case, and every depth in that path must be
derived from the section, never from a key**. The tests are the enforcement;
the prose is only the map.

## 4. Cross-cutting, applies to every phase

1. **One writer module.** `splice_section` + `write_file` already exist
   (`commands.rs:2978`, `:3366`) and B-1 fixed the predicate. Phase 0 onward adds
   `render_*_section` siblings beside `import`'s existing ones; the rule is that
   a new writer reuses `splice_section` rather than open-coding YAML.

   **Amended by Phase 4.** `splice_section` owns a *whole section*, and a
   whole-section write cannot represent a section it did not parse: the
   operator's own blank separators and inline notes are not in the config
   model, so re-rendering moves them. For `combos:` — a list of rows, where the
   operator's notes live *between* the rows — the writers are therefore
   row-level (`append_combo_row`, `remove_combo_row`, `add_target_line`,
   `remove_target_line`) and the section-level splice is reserved for the panel's
   wholesale form save. The rule is now: a writer that touches one row uses a
   row-level edit; a writer that replaces a table may splice. `visibility:` and
   `provider-settings:` (Phases 6-7) are single-value sections, so the splice
   stays right for them.
2. **The restart note, every write.** The line `9a8821a` added to `limits set`
   is the contract; each new write prints it. A write that does not name the step
   that makes it live is the B-3 failure.
3. **`--json` on every read.** `models`/`providers`/`doctor`/… already have it;
   `server show`, `oauth list`, `visibility show` and the rest are reads and take
   it from the start, per `docs/06`'s machine-readable rule.

   **Amended closing the phase** (`1e7f90c`). The rule was satisfied on paper and
   broken in practice: the flag was declared on each verb's own args struct,
   which clap reads only *before* the subcommand, so `aroute providers list
   --json` — the spelling this plan's own examples use — answered `unrecognized
   argument '--json'` on `server`, `providers`, `combo`, `visibility` and both
   Phase 7 reads. Three of the six shipped in earlier phases. The field existed
   and a read of the struct said it worked, which is exactly why no test caught
   it. The rule now reads: **`--json` is `global` on the parent, so it parses on
   either side of the subcommand, and a parse test covers every read in both
   positions.**
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
- The second half of the `import` half of the Q17 finding: Phase 0 has landed,
  so no path destroys a section any more. What is still open is the first half —
  the CLI reaching every section, which is what Phases 1-7 build.

## 8. Open findings, recorded by what found them

**A row-level writer must compare bytes, not configs.** `combo add` used
`means_the_same`, which compares *parsed* configs — and `Config` models four of
the top-level sections a real file can carry. On a config holding `oauth:`,
`visibility:` or `mcp:`, both parses are equal, so a row was written and then
discarded as "unchanged": the operator's add did nothing and printed success.
The four combo verbs now compare bytes, and
`combo_add_lands_on_a_config_carrying_sections_no_writer_represents` holds the
shape. The whole-section writers keep `means_the_same`, which is right for them:
a re-render can legitimately re-spell the same configuration, and a one-row edit
cannot.

**A row-level writer must also insert at a flush header boundary.**
`append_combo_row` reached for its boundary through a blank line or comment, so a
`combos:` written flush against `oauth:` accepted no row at all — the same
silent no-op by a different route. Both were found by probing a config that
carries sections no writer represents, which is the config the Phase 0 guard was
written for and which the repo's own file does not happen to contain.

**`guard_against_section_loss` still runs only on `import`.** Phase 0's finding,
unchanged and still true: every other write path is unguarded against a target
carrying a section its own renderer cannot represent. The two findings above are
why that matters for the row writers specifically — the guard is one mechanism,
and the byte-comparison is the other. It remains the operator's call whether the
guard is widened to every writer or whether byte-equality (which is strictly
stronger for a row-level edit) is accepted as the guarantee.
