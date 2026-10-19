# ui dashboard upgrade

Dated-frozen: published `2026-10-19`. A plan that changes is a new plan (or declare it
in genome.toml `core.living_plans` and keep a 'Resume here' section).

**Owner of the gates. The owner of the rationale is `docs/27-dashboard-contrast-and-upgrade-plan.md`
§6** — that doc says *why* and *in what order*; this one says *what must pass*. Do not
duplicate a gate here that already lives in the code; cite the file.

**The state of every open item, across the whole project, is in
[`docs/28-gap-ledger.md`](../28-gap-ledger.md)** — this plan is the panel's slice of it
(ledger §F), and the register rows are `UI-00` … `UI-08`.

The subject: `crates/ar-cli/src/webui.rs` and `crates/ar-cli/src/webui/page/*`. The two
shipped UI plans this continues: `docs/19-ui-upgrade-plan.md` (phases A–E) and
`docs/23-ui-round-two-plan.md` (G1–G5). The audit that produced this plan:
`docs/27-…` §1–§4, with the record in `docs/audit/2026-10-19-ui-dashboard-contrast.md`.

## Mechanism

One phase at a time, in the order below. Each phase ends with its own gate and its own
commit. **U0 is not a UI phase and it is the only one that is time-critical**: it closes an
unauthenticated write path to the production gateway.

| phase | what changes | gate |
|---|---|---|
| **U0** | `AR-2` (cloudflared ingress: `/ui` → 404; `/v1` untouched) + `AR-3` (the `/ui` write handlers refuse when the gate master is unreadable) + the credential-store fix (`AR_CRED_STORE` set, gate re-armed with `--key "$AR_HTTP_MASTER_KEY"`, the duplicate store deleted) | `POST /ui/api/*` from the public internet → **404**; `aroute doctor`'s gate check **fails** when the store is present but unreadable; `/v1/models` with the operator's token → **200**; the two `credentials.db` files → **one** |
| **U1** | `AR-1`: the `/ui` router gets the same bearer gate `/v1` uses; a paste-a-token field; `sessionStorage`; `Referrer-Policy: no-referrer`; a `?token=` first-paint fallback | an unauthenticated `/ui` request → **401**; the panel works in a real browser over the public URL with a token; `probe.mjs` **0 FAIL** at 1440/1024/390 in **both** theme bands |
| **U2** | the four data states per view; errors that persist until dismissed; a write that re-reads the file and compares; the `--amber-ink` contrast fix (`docs/27` §4.4) | each of loading / empty / error / populated rendered and screenshotted; a failed write shows a **persistent** error; `--amber #e0900f` is never used as text on `--panel #ffffff` (target ≥ 4.5:1) |
| **U3** | the operator's missing views: resilience (breaker + cooldown state), provider health/expiry, per-combo metrics, budgets and quota windows | every figure comes from the ledger or the resolver, **not** from a guess; `probe.mjs` 0 FAIL ×3 widths |
| **U4** | a settings pane over the yaml: `server`, `limits.default`, timeouts, model discovery, auth mode | byte-exact splices with round-trip tests; a wrong value is refused **naming the field** |
| **U5** | craft: the h1 to 28–32px (clears the `type-scale` warn); the table buttons to 44px; the clipped footer paths get a `title` + a copy button; a reviewed light band | `probe.mjs` **0 FAIL** ×3 widths ×2 themes; `contrast.py` **0 FAIL**; CRAFT **≥ 16/20** with no item at 0 |
| **U6** | only then: compare-mode playground, a request-detail drawer, a timeline | — |

**U0 and U1 are not interchangeable.** U0 is configuration and a code change to the write
handlers; U1 is a release. U0 alone closes the hole. U1 makes the panel safe to publish
deliberately. Do U0 first, always.

## Gates / success criteria

(fixed before the run)

Every phase, in full:

1. `cargo clippy --all-targets --all-features --locked -- -D warnings` — clean.
2. `cargo test --workspace --locked` — green.
3. `cargo build --release` — the binary that the probe runs is `target/release/aroute`,
   **never** `~/.local/bin/aroute` (the installed one lags until a release).
4. `node ~/.agents/skills/website-design/scripts/probe.mjs <url> --widths 1440,1024,390
   --scheme light` **and** `--scheme dark` — **0 FAIL**.
5. `python3 ~/.agents/skills/website-design/scripts/contrast.py --css
   crates/ar-cli/src/webui/page/app.css` — **0 failing pairs**.
6. A behaviour script in a real browser (Playwright) over the **scratch** instance at
   `/var/tmp/ishanp-agentic/opencode/ar-webui-demo`, not against prod.

**Never** run a write-path test against the prod config. `POST /ui/api/combos` is a
whole-section replace and the YAML renderer normalises list style, so a "no-op" save still
rewrites the file (measured: `targets: [x]` → a two-line block). A write test compares
**semantics**, not bytes. This bit once already — see `docs/27` §2.2.

**Never** copy `ar.env` between machines. It carries `$AR_MASTER_KEY` and
`$AR_HTTP_MASTER_KEY`; the local file has no master keys and overwriting the VPS's makes
its credential store undecryptable (the 2026-10-19 prod incident).

## Cost

| phase | effort | blast radius |
|---|---|---|
| U0 | small (one ingress rule, one handler guard, one env line + re-arm) | **the operator's** — it re-arms the gate, so every harness's token must still work afterwards |
| U1 | medium (a new auth layer on a router group, a token field, a first-paint path) | medium — a new release to the VPS, and the panel's entry flow changes |
| U2 | medium (a state machine per view) | none — read and write behaviour only |
| U3 | large (four new data surfaces, some needing new ledger/resolver queries) | none — new read paths |
| U4 | large (new yaml splicers) | **the config file** — byte-exact splices, round-trip tests, a refused bad value |
| U5 | small (tokens and sizes) | none — visual only |
| U6 | large | none |

## Resume here

- **Next phase: U0.** Nothing in U0 depends on a code release except `AR-3`.
- U0's `AR-2` is a **cloudflared config change on the VPS** (`~/.cloudflared/hermes-config.yml`)
  followed by a **root** `systemctl restart cloudflared` — the operator's step, not the
  agent's. Back the file up first (there are already two `.bak` files there).
- The store fix needs the operator's ruling on **which store is canonical**
  (`docs/27` §7.2). The evidence says the store **beside the config**
  (`~/.config/ar/credentials.db`) must win, because that is the one `aroute serve` opens.
- After U0, re-run the audit probes and confirm the public `/ui` is 404 while `/v1` is 200.

## Outcome — 2026-10-20 (U0-U5 shipped)

The record of what shipped, with the evidence, is
[`docs/30-ui-production-grade.md`](../30-ui-production-grade.md). The state of
every open item is [`docs/28-gap-ledger.md`](../28-gap-ledger.md) §F; the
register rows are `UI-00` … `UI-08` in `TODO.md`.

**Shipped:** U0's `AR-3` (the write handlers refuse on a broken gate) and
U0's doctor check (a store that is present but unreadable now makes the check
**fail**); U1's whole gate (a three-state panel gate, the credential on
`/ui/api/*`, the paste-a-token field, `sessionStorage`, the `?token=` first
paint stripped from the URL); U2's four states, persistent errors, and the
write that re-reads; U3's four new views; U4's settings pane over the
yaml-backed sections; U5's craft pass.

**Still the operator's:** U0's `AR-2` (the cloudflared ingress 404) — staged
on the VPS, needs a **root** `systemctl restart cloudflared`. Until it runs,
the public `/ui` is **gated** rather than **open**, which is a different
thing and a smaller one. `UI-08` (which credential store is canonical) is
also the operator's ruling.

**The gates, as measured:** clippy clean; **2251** workspace tests green;
**20** probe runs (ten views x two bands x three widths) at **0 FAIL, 0
warn**; `contrast.py` 0 failing pairs; the anti-slop detector **24/24**; the
browser behaviour checks **20/20** unarmed and **10/10** armed; CRAFT
**18/20** with no item at 0.

**U6 is not started** and stays out of scope until U0's ingress block is the
operator's and the operator wants it.

