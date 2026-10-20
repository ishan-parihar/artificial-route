# 31 — Remaining gaps: the execution outline (2026-10-20)

Dated-frozen: published `2026-10-20`. A plan that changes is a new plan.

> **This doc owns the ORDER and the SHAPE of the remaining work. It does not own
> the state.** The state of every open item is in
> [`docs/28-gap-ledger.md`](../28-gap-ledger.md), and the register rows are
> `UI-00` … `UI-08`. Read the ledger before acting on anything here: several
> states moved on 2026-10-20 (A1, A2, A7, C6, F1 were all re-measured against
> the **live** endpoint, not the code).
>
> **What changed on 2026-10-20:** the panel work
> ([`docs/30-ui-production-grade.md`](../30-ui-production-grade.md)) closed the
> panel's code gaps and, in doing so, **measured** the remaining ones against the
> running VPS. That measurement is what produced this outline: three gaps that
> the ledger had described as "code" turned out to be **operations**, and one
> gap that had no CLI at all turned out to be the reason a 30-day credential
> cannot be withdrawn.

## The shape of the remaining work

There are **nine** open items. They are not nine pieces of work of the same
size, and treating them as one queue is what produced a release that sat
**23 hours** behind `main` (the `v0.1.17` tag is 2026-10-19 07:16; the repo
itself is three weeks old, not months). They fall into four shapes, and the
shape dictates who acts and in what order:

| shape | rows | who | blocking? |
|---|---|---|---|
| **Ops — the operator's hands, minutes** | A1, A2, C6 | operator | **yes — this is the live exposure** |
| **Code — a defined change, no ruling needed** | A7, A8, A5, F8, UI-08 | agent | no, but A7 is the lever for A2 |
| **Decide — needs the operator's ruling first** | B1, B3, B5, B2 | operator | B1 blocks A5 and B5 |
| **Probe — needs evidence, not code** | E1, E2, C3, C4, D2 | agent, after a release | waits on the release |

**Every item below says who owns it.** "agent" means an agent can execute it
end to end from the repo. "operator" means it needs a hand on the VPS, a
credential, or a ruling — an agent can prepare it but not finish it. "blocked"
means the item is real and named but cannot start; the text says on what.

### The ordering constraints that are not obvious

Four of these are **orders, not tasks**, and a flat list of the same items
would get them executed wrongly:

1. **A1 → A2 is an order, not two tasks.** Restarting the tunnel closes the
   **door**; rotating the master kills the **keys already handed out**.
   Rotating first leaves a window where the door is still open, so a fresh
   30-day token can still be minted — and the rotation then kills it too,
   having rotated for nothing. Restart first, then rotate.
2. **A7, A8 and A6 are carried by the release (C1), not done before it.**
   They are code in the same binary as the panel's gate, so they cannot ship
   ahead of it. Phase 1 (the release) therefore comes **before** Phase 2 and
   Phase 3's server-side work — but **after** Phase 0, because Phase 0 needs no
   release and the exposure is live now.
3. **A8 depends on A7's mechanism.** Both need a per-request store read. Doing
   A7 first means that read is already built, opened once and shared.
4. **A5's general form depends on B1's ruling.** If `/ui` is never public, the
   general `loopback_only` change is moot and only the panel's copy matters.

**The one thing that is true today and was not true yesterday:** the exposure is
**live and it is not a code problem**. Measured 2026-10-20 against
`https://aroute.ishanparihar.com`:

```
POST /ui/api/keys/mint   (no credential)  ->  200 + an execute:completions token
that token on POST /v1/chat/completions   ->  200, real provider, billed tokens
```

The deployed binary is the **2026-10-19 20:44** build — the pre-fix one — so the
public panel has **no gate at all**. Everything in the "Code" row below is worth
doing and none of it is urgent, because the code is already written and the
problem is that it is **not running**.

---

## Phase 0 — Close the exposure (operator, minutes, do this first)

This phase is **operations, not code**, and it does not need a release. It is
the whole of the difference between an open panel and a closed one. Every step
is the **operator's** — an agent can run the checks but the restart and the
rotation need the VPS.

| step | command | acceptance |
|---|---|---|
| **0.1** back up the ingress config | `cp ~/.cloudflared/hermes-config.yml ~/.cloudflared/hermes-config.yml.bak-$(date +%s)` | the backup exists (there are already three `.bak` files) |
| **0.2** validate the rules | `cloudflared --config ~/.cloudflared/hermes-config.yml tunnel ingress validate` | `OK` |
| **0.3** confirm the rule matches | `... tunnel ingress rule https://aroute.ishanparihar.com/ui/api/keys/mint` | matches `path: ^/ui(/.*)?$` → `http_status:404` |
| **0.4** **apply them** | `sudo systemctl restart cloudflared` | the tunnel restarts (it is a **root** unit: `kill -HUP` and the admin API are no-ops — C6) |
| **0.5** verify from **outside** | `curl -o /dev/null -w '%{http_code}' https://aroute.ishanparihar.com/ui/api/keys/mint` → **404** | and `GET /v1/models` without a token → **401**, with a token → **200** |
| **0.6** **rotate the master** | the RUNBOOK sequence (`docs/RUNBOOK.md`, "The gate has one master, three names") | the tokens minted in the window stop working |

**0.6 is not optional and it is not a repeat of 0.4.** Restarting cloudflared
closes the **door**; it does not touch the **keys already handed out**. The
panel's mint defaults to `--ttl-days 30` (`--ttl-days 0` never expires), so the
tokens minted through the open panel are good for **30 days**, and there is
**no per-token revocation** (A7): `Revocation` and `Tokens::revoke` exist but no
production path opens the store and no CLI subcommand calls them. **The master
rotation is the only lever.** Rotate **after** the restart, so the window is
closed before the keys die.

Rotation invalidates every harness token, so the three harness configs and
`~/.config/ar/racknerd-gate-token.jwt` need the new one
(`docs/RUNBOOK.md` names all four).

**Phase 0 gate:** the public `/ui` answers **404**; `/v1` still answers; the
tokens minted before the rotation answer **401**.

---

## Phase 1 — Ship what is already written (agent, then operator)

The panel work is **committed and unpushed**. The release scope is **39
commits** ahead of the `v0.1.17` tag (`git rev-list --count v0.1.17..HEAD`;
the tag is annotated, so it resolves to the commit `5045a3b` `chore(release):
v0.1.17`, not to the tag object's own hash), of which **8** are not yet pushed
to `origin/main`. That number is the release's blast radius, not this
session's: the unreleased range starts at `0bbaa22` (the installer and the
default config) and includes the provider-discovery work, not only the panel.
**Steps 1.1-1.4 are the agent's; 1.5-1.7 are the operator's**, because the
release publishes an artefact and the install touches the VPS.

| step | action | gate |
|---|---|---|
| **1.1** | `git push origin main` | the commits are on the remote |
| **1.2** | bump `Cargo.toml` to `0.1.18`, refresh `Cargo.lock` | `cargo build --locked` is clean |
| **1.3** | `chore(release): v0.1.18` commit, then the annotated tag `v0.1.18` | `git tag -l v0.1.18` |
| **1.4** | `git push origin main v0.1.18` | the tag is on the remote |
| **1.5** | `.github/workflows/release.yml` builds and publishes | the musl asset is on the release |
| **1.6** | install on the VPS from the **release asset** | `aroute --version` reports `0.1.18` |
| **1.7** | re-verify the panel | the public `/ui` now needs a token: an unauthenticated `/ui/api/state` → **401** |
**Never scp a locally built binary** (ledger C1). The local release build
links glibc 2.39 and the VPS has glibc 2.36; the 2026-10-19 incident came from
doing exactly that. `docs/RUNBOOK.md` has the install recipe, including the
`sha256sum` comparison trap.

**Phase 1 gate:** the deployed binary is `0.1.18`; the panel's own gate is
armed; the ingress block and the gate are **both** in place, so the panel is
reachable again but credentialed.

---

## Phase 2 — Per-token revocation (code, no ruling needed)

**This is the change that makes A2 survivable next time.** Today a stolen token
is good for its whole lifetime and the only remedy is rotating the master,
which invalidates every legitimate token at once. The machinery is already
built and tested — it is simply not reachable.

**2.1 — the store, and where it lives.** *(agent)* The revocation table is a
`jti` → `exp` row, and it goes **in the credential store's sqlite**, not in a
separate redb file.

That is a correction to what this outline said first, and the reason is
concrete. `Revocation` is a **redb** table, and redb takes an **exclusive**
`flock(LOCK_EX | LOCK_NB)` on its file
(`redb-2.6/src/tree_store/page_store/file_backend/unix.rs:37`) and returns
`DatabaseAlreadyOpen` when it cannot. So a redb revocation file would mean the
running proxy holds an exclusive lock on it, and `aroute keys revoke` — which
must run **while the proxy is up**, because that is when a token needs
revoking — could not open it. `ar-keys/src/revoke.rs:35-37` already says so:
"handing a second handle to the same file is a footgun (redb takes an
exclusive lock, so it would fail at runtime rather than compile time)".

sqlite is already the credential store's format (`crates/ar-keys/src/store.rs`),
`rusqlite` is already a dependency, and it supports **concurrent readers with
one writer** — which is exactly the access pattern: the server reads a `jti`
per request, and the operator's `aroute keys revoke` writes one row.

The `jti` → `exp` row is a natural fit beside the store's other tables, and
putting it in the same file settles the question this outline previously
deferred ("where the store lives is the decision"): **one file, one path, one
master.** The store's path is `commands::store_path`
(`crates/ar-cli/src/commands.rs:113`), which honours `$AR_CRED_STORE` and
otherwise resolves beside the config.

- The cap stays: 4096 live entries, and the write is **refused** at the cap
  with nothing expired rather than evicting a live entry. Evicting a live
  revocation un-revokes a stolen token, and the existing test
  `never_drops_a_live_entry_to_make_room` pins that.
- The store's journal mode is left at SQLite's default rollback journal
  (`store.rs:100-103`), deliberately: WAL buys concurrent-reader throughput
  and costs two sidecar files next to ones an operator must keep out of git.
  The revocation read is one point lookup, so the default is enough — and
  **the cost is measured** (2.4), not assumed.
- `Tokens::verify` now takes `Option<&Revocation>`'s sqlite equivalent, so a
  deployment with no store is a supported configuration rather than a
  forced-open failure. With no store, a revoked token is good for its whole
  lifetime, and the row says so.

**2.2 — the server consults it.** *(agent)* Both gates must, **together**:

- `/v1`: `AuthGate::verify` (`crates/ar-server/src/keys.rs:253`) calls
  `introspect` and never checks the list. It must call
  `Tokens::verify(token, need, &revocations)`, which checks the list *before*
  the scope.
- `/ui`: `webui/auth.rs::check` has the same shape and must change with it.
- **Doing one without the other is worse than doing neither**: the two surfaces
  would disagree about the same token, and a token revoked in the panel would
  still work on `/v1`.

**2.4 — the cost is one point read per request.** *(agent)* A sqlite point
lookup on a `jti` primary key is cheap, but it is on the hot path of every
`/v1` request, so **the number is measured, not assumed**.

**Measured** (release build, 500 live entries, 2000 lookups — the bench is
`crates/ar-keys/tests/keys.rs::the_revocation_lookup_stays_under_its_budget`,
following the repo's own `ar-guard` precedent of `std::time` over `criterion`):

| p50 | p95 | p99 | budget |
|---|---|---|---|
| 11.2 µs | 12.3 µs | 17.6 µs | 1 ms at p95 |

The budget is deliberately loose. The lookup is a point read on a primary key
and should be microseconds; the budget exists to catch an order-of-magnitude
regression (a full table scan, a synchronous fsync), not to police the last
digit.

**2.3 — the panel's mint default.** *(agent)* `--ttl-days 30` is the current default and
`--ttl-days 0` mints a token that **never expires**. A panel that mints
30-day credentials by default is a panel that hands out long-lived secrets to
whoever reaches it. The panel's mint should default to the **CLI's shortest
defensible life** and say so on the page.

**Landed:** the panel's default is **7 days**, the field is on the page with
its own label, and `0` (never expires) has to be typed. The lifetime is
bounded at 365 days and validated **before** the store is consulted, so a bad
input is named as a bad input rather than as a missing store.

**Phase 2 gate:** `aroute keys revoke <token>` exists; the revoked token
answers **401** on **both** `/v1` and `/ui`; the bench shows the lookup's cost;
the panel's mint default is shorter and says so. — **all met, 2026-10-20.**

- `aroute keys revoke --token-file <path>` writes the token's **session id**
  into a `revoked_tokens` table in the credential store's own sqlite file.
- **Both** surfaces refuse a revoked session, and both are **tested**:
  `ar-cli`'s `a_revoked_token_should_be_refused_by_the_panel` and
  `ar-server`'s `refuses_a_token_that_was_revoked`. The `/v1` test asserts the
  **reason string**, so the revocation path is pinned as distinct from a
  generic rejection.
- The lookup is **p95 12.3 us** (p50 11.2, p99 17.6) against a 1 ms budget,
  with 500 live entries. The number is *lock acquisition plus one indexed
  point query, uncontended* — under concurrency the `Mutex` is the ceiling,
  not sqlite.
- The panel's mint defaults to **7 days**, says so on the page, and the
  lifetime is bounded at a year.

**One thing the gate did not ask for, and the phase needed anyway:** the
revocation list keys on a **session id**, not a `jti`. A minted token is a
pair — a 15-minute access token and a 30-day refresh token — and **the refresh
half is itself a valid bearer credential** that the same gate accepts with
the same scopes. Revoking only the access half would have left a 30-day
credential live, which is the outcome the command exists to prevent. The two
halves share a `sid`; the list keys on it; one revoke kills the session. The
expiry written is the **later** of the two halves', so the entry is not swept
while the other half is still good.

---

## Phase 3 — The remaining code gaps (agent, in this order)

**3.1 — A8: the panel's gate is resolved once at boot.** *(agent)* `GateState` is built
in `serve` and held in an `Arc` for the process's life, so arming the gate
while the proxy runs does not turn the panel's gate on, and breaking the
master does not turn it `Broken`.

- The **safe** direction is already the one in place: a panel that stays
  unarmed for the life of the process is safer than a gate that turns itself
  off. So this is a **usability** gap, not a security one — and it is the same
  shape as the boot-built routing the panel already reports as
  `pending_restart`.
- The fix is to re-resolve per request, which costs a store read per request —
  the same cost as 2.2, and **the two can share one mechanism**: one
  `Arc<Revocation>`-shaped store handle, opened once, consulted per request.
- **Do it after 2.2**, not before. Doing 2.2 first means the per-request store
  read is already built and paid for.

**3.2 — A5: `loopback_only` refuses what a browser sends.** *(agent, after B1)* The rule refuses a
request whose `Host` **or** `Origin` names a non-loopback authority
(`crates/ar-server/src/app.rs:680-701`). The tunnel rewrites `Host` to
loopback but forwards `Origin`, so a browser's cross-origin `fetch()` and its
CSS-driven font requests are refused while a script's silence is admitted.

- The panel solved this for itself (`webui.rs::ui_guard`): the `Host` check is
  kept and the `Origin` check is replaced by the credential, with the `Origin`
  rule firing only on an **unarmed** panel, where it is the loopback install's
  only CSRF defence.
- **The same shape should move to `loopback_only`**: ignore `Origin` when
  `Host` is loopback, because `Host` is the header a rebinding page cannot
  choose. This is the same reasoning, one layer down, and the panel's version
  is the worked example.
- **Blocked on B1's ruling** (should `/ui` be public at all?). If the answer is
  "never public", A5's general form is moot and only the panel's copy matters.

**3.3 — F8: a "no-op" write still rewrites the file.** *(agent)* `POST /ui/api/combos` is
a whole-section replace and the YAML renderer emits block-style lists where the
hand-written file used flow-style, so posting the config's own combo list
changed 39 lines and the `identical` short-circuit (`webui.rs:1253` and three
siblings) never fired.

- The fix is a **semantic comparison before the write**: parse both documents
  and compare the parsed `Config`, rather than comparing the rendered bytes.
  `Config` derives `PartialEq` (`crates/ar-config/src/lib.rs:1047`), and `Combo`
  derives `PartialEq` too (`:489`), so the comparison is available.
- The bonus is that the comparison is **the** fix for the write-path tests:
  compare semantics, and "saving the same thing" stops dirtying the config.
- Note `Combo` derives `PartialEq` but **not** `Eq`, and `Config` derives
  `PartialEq` but **not** `Eq` — so the comparison is `==` on `PartialEq`, and
  no `Hash` is needed.

**3.4 — UI-08: rule the canonical credential store on the VPS.** *(operator, folded into Phase 0.6)* Two stores,
one `http-gate` row each, different envelopes: `~/.config/ar/credentials.db`
(what `serve` opens, because `store_path` resolves beside the config) and
`~/credentials.db` (what a shell whose CWD is `$HOME` opens).

- **This is the operator's ruling** (which file is canonical), and it is
  **cheap to make unambiguous**: set `AR_CRED_STORE` explicitly in
  `~/.config/ar/ar.env` to the chosen path, then delete the other file. The
  variable already exists (`commands::STORE_PATH_VAR`, `commands.rs:104`) and
  is honoured by `store_path` (`:113-124`).
- **Do it as part of Phase 0.6** — the rotation is already touching
  `ar.env`, and setting `AR_CRED_STORE` in the same edit means the store is
  unambiguous from the moment the new master lands.

**3.5 — A6: the MCP HTTP transport has no bearer gate.** *(agent)* `aroute mcp --http`
serves StreamableHTTP at `/mcp`, refuses a routable listener
(`crates/ar-mcp/src/transport.rs:885`), and relies on rmcp's loopback
`Host` allow-list. The loopback bind and the scope guard are the only controls.

- Lower exposure than A1 (stdio is the default), but it is the same law
  (RG-001): a route that changes state needs a credential.
- The fix is the same shape as the panel's: a bearer check on the HTTP
  transport, using the **same** `Tokens` engine the rest of the system uses, so
  one master governs all three surfaces.

**3.6 — D3: the Responses `function_call` item and four renderers.** *(agent)*
Tool declarations, `tool_choice` and tool history already cross every dialect
that has them. Two pieces remain: the inbound **Responses `function_call`
item** is refused (`UnsupportedPart`, a 400 for the whole request), and the
**Ollama, cursor, clova and kiro** renderers emit neither declarations nor
calls.

- The code is in `crates/ar-translate/src/translate.rs`, and it is the same
  shape as the work that already landed: a canonical `ToolCall` in the middle
  and a per-dialect renderer at the edge.
- The inbound `function_call` is the **request** side, so it is the harder
  half: the canonical form has to be produced from a Responses item that the
  other dialects never send.
- This is the one Phase 3 item that is **not** a gate or a control. It is
  ordinary feature work, and it is scheduled here only because the ledger
  keeps it open. If Phase 0-3.5 is done and this is not, the panel work is
  still complete — this row is a separate feature, not part of the exposure.

**Phase 3 gate:** the panel's gate is live (armed or broken, seen per request);
`loopback_only` ignores `Origin` when `Host` is loopback; a no-op write leaves
the file byte-identical; one credential store exists and `AR_CRED_STORE` names
it; `/mcp` needs a bearer; a Responses `function_call` item is accepted rather
than refused.

---

## Phase 4 — The decisions (operator, and they gate Phase 3)

These are **ruling-shaped**, not code-shaped. They are listed here because two
of them unblock Phase 3 work.

| row | the question | what it unblocks |
|---|---|---|
| **B1** | Should `/ui` be reachable from the internet at all? | **A5's general form** (3.2). The blast radius differs by two orders of magnitude between "SSH-forward only" and "publish it, gated". The 2026-10-20 measurement is evidence for the "gated" answer, not for "never". |
| **B3** | May a routable bind be allowed while the gate is armed? | the `public` config seam (`TODO(#p1-config-public)`), and whether Phase 1's panel should ever be published |
| **B5** | Should the panel be able to restart the proxy after a write? | the `pending_restart` banner's usefulness. `ConfigHandle::watch` exists in `ar-config` but is not attached (docs/16), so a restart needs that wiring or a systemd call. Neither is needed for correctness. |
| **B2** | The start cap, the model deadline and the failover status | the stall failure (D2, E3) — waits on `docs/24` D1–D3 |

**Phase 4 gate:** the rulings are recorded in `docs/28` (the ledger owns the
state) and the affected rows move.

---

## Phase 5 — The probes (agent, after Phase 1)

These need a **released binary** and mostly need a live provider. They are not
code, and the ledger's own rule is that they wait on C1.

| row | the probe | needs |
|---|---|---|
| **C3** | the 12-hour soak against a released binary | Phase 1 done; `scripts/soak.sh`, detached, killed by PID, CSV outside `/tmp` |
| **E1** | re-probe the live `cline` stream | a released binary and a live cline key |
| **E2** | the cline `{success,data}` envelope unwrap | an **OAuth** cline session that streams the envelope — the API-key row streams plain OpenAI SSE, so this is not reachable with the rows that exist |
| **C4** | the registry snapshot lags OmniRoute's rotation | an operations decision: a weekly `aroute import`, or accept the warning |
| **D2** | repeat detection of an empty/degenerate 200 | a corpus that does not exist yet — the 2026-10-18 probe of `free-stack` returned 0 of 4, all four 504 `upstream_timeout`. **The open failure is the stall (B2), not the empty 200.** |

**Phase 5 gate:** each probe's result is recorded in the ledger with the
evidence, not just a status word.

---

## Phase 6 — The rows this outline does NOT schedule

The twenty-nine open ledger rows fall into two groups. **Twelve are
scheduled** in Phases 0–5 above. **Six more** (A2, A7, C6, E1, E2, F8) are
discussed in the phases above because other work depends on them, and are
listed in Phase 6 with the rest so nothing is left implicit. **Eleven** are
named only here, each with why it is not scheduled. A row that is silently
absent is the failure mode this doc exists to prevent.

**Three rows are "partly built" rather than open** — A4, D3 and D4 — and all
three are named above: A4 in Phase 3.4, D3 in Phase 3.6, D4 in Phase 6.1.

### 6.1 — Provider-dependent: no credential, no work

These are **not** code gaps. Each is built, unit-tested and refused only
because the account has no key. Nothing an agent does moves them.

| row | what it needs | who |
|---|---|---|
| **D6** | a working `zenmux-free` key — inference is locked for this account (403 `access_denied api_key_source: payg`), and the account action is on zenmux's side | operator |
| **D7** | a Bedrock API key + region, and a GitLab Duo token. Both wires are built with unit tests and an axum mock; neither has been called against its provider | operator |
| **D4** | the `grok-cli` reference **on disk**, and a login. The Responses wire, the Build identity headers and the body repair are ported; the four remaining items are not, and live dispatch is unproven until a login exists | operator |
| **E1**, **E2** | see Phase 5 — both wait on a released binary, and E2 additionally on an **OAuth** cline session (the API-key row streams plain OpenAI SSE, so the envelope is not reachable with the rows that exist) | operator + release |

### 6.2 — Wire work that needs a decision first

| row | why it is not scheduled |
|---|---|
| **D8** | a custom `anthropic-compatible` node is **accepted and named by `doctor`, then refused at dispatch**. The name-then-refuse shape is deliberate — the node is real, the wire is not. The open question is whether to implement the wire or keep the refusal, and that is a decision, not a task |
| **D5** | `kilocode` has no mechanism declared: its `oauth/` row is a `fail` whose fix names the YAML for **either** mechanism. The device flow and anonymous dispatch are ported, and the reference has no kilocode-specific executor, so there is no further wire to port. It waits on an operator's YAML choice |
| **D9** | `poll_state TEXT NULL` is not in the credential store (`crates/ar-keys/src/store.rs:423`, the `TODO(#F-MED-2-device)` seam). It is small, but it is part of the **device flow**, which is only reachable with a provider that has one — and no provider currently declares a device mechanism (D5) |

### 6.3 — Operator rulings that are already answered in practice

| row | where the answer already lives |
|---|---|
| **B4** | `Limit.model` scopes **when** a row applies, not **what** it counts. The choice is recorded in `docs/23` G3, the `--model` help text says so, and the ledger row says the same. It is open only because the ledger has no way to record "settled by precedent" — **not** because it is undecided |
| **A4** | scheduled, as Phase 3.4. The operator's ruling is which file is canonical; the code half (`AR_CRED_STORE` honoured by both paths) is done |

### 6.4 — Operations and policy

| row | why it is not scheduled |
|---|---|
| **C2** | the `failures` table only exists from the release that ships it (`CREATE TABLE IF NOT EXISTS failures`, `crates/ar-tokens/src/ledger.rs:49`), and older rows are not backfilled. It **ships with Phase 1** — it needs a release note, not a task |
| **C5** | `docs-check` runs in warn mode and does not gate. The orphan count is **0** and the README rubric, the outward boundary, the per-package README contract and the genome boundary all pass. Making it blocking is an operator decision (docs/24 B6), and it is a **policy** choice, not a gap in the tree |

### 6.5 — Not a gap

| row | why |
|---|---|
| **E4** | `glm-5.3`'s heavy-reasoner trait (`max_tokens < ~1500` → empty content) is a **known trait, not a defect**. Recorded in `AROUTE_BUGS.md` and `docs/07`. The ledger says explicitly: **do not re-open it as one** |

---

## What is deliberately NOT in this outline

- **A "no-op" write that touches the file** (F8) is here, but the *bytes* are
  not the goal. The goal is that saving the same thing does not dirty the
  config. A cosmetic byte-preservation pass would be the wrong fix.
- **A per-provider `expired` view** is not here: the registry has no expiry
  field, so the Health view reports what exists (present, readable, broken)
  and does not invent an expiry.
- **Charts.** The website-design skill is explicit that charts are out of its
  scope, and the panel has no charting dependency to extend.
- **A React/Next port of the panel.** The stack is vanilla JS over one embedded
  CSS and one embedded JS file, with no build step, because the panel must
  work on an offline VPS.

## The gate on this document

Before any row here is closed, the ledger row must carry the **evidence** —
the command and its output, not a status word. That rule is why A1, A2, A7, C6
and F1 all changed on 2026-10-20: they were re-measured against the live VPS
instead of the code, and four of them turned out to say something different
than they had said for two months.
