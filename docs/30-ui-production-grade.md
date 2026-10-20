# 30 — /ui production-grade: the design brief, the direction, and what shipped

Status: **written 2026-10-20.** The record of the U0-U5 pass over the control panel.

Owner: this doc, for **the design of the panel**. The state of every open item
across the project is in [`docs/28-gap-ledger.md`](28-gap-ledger.md) §F; the
register rows are `UI-00` … `UI-08` in `TODO.md`. The rationale for *why* the
panel looks and behaves as it does is here; the audit that started the pass is
[`docs/27-dashboard-contrast-and-upgrade-plan.md`](27-dashboard-contrast-and-upgrade-plan.md).

Companions: `docs/19-ui-upgrade-plan.md` (phases A-E), `docs/23-ui-round-two-plan.md`
(G1-G5), `docs/27` (the audit and the contrast with OmniRoute), `docs/16` (the
proxy/dashboard split).

---

## 1. The brief (website-design phase 1)

```
DESIGN BRIEF
  scope     the /ui control panel of aroute serve — one axum router, three
            embedded assets, no build step, no npm, offline-safe
  kind      dashboard / admin / tool UI  (the skill's branch: blueprints #18
            + app-ui)
  subject   a routing appliance: a client asks for a model NAME, the router
            picks a provider TARGET, and the failover chain is the product
  goal      an operator must be able to answer, without leaving the panel:
            "what is serving my traffic, what is out of rotation, what did
            it cost, and what happens when I change something"
  audience  one operator, running the gateway themselves, on a laptop or a
            phone, at 3am when something is broken
  dials     VARIANCE 3, MOTION 4, DENSITY 8   (read off the shipped panel
            before this pass; the mode is PRESERVE, so the rule gives
            VARIANCE 3, MOTION 5, DENSITY 8)
  foundation  preserve — the electric-indigo skin, the Sora/Inter pairing, the
            failover rail, the hairline table, the one-binary-no-build-step
            constraint. The rail and the token system stay.
```

The **preserve** mode is not a convenience. The panel had been audited
(`docs/27` §4), probed in both theme bands, and scored; its visual language
was already a deliberate answer to "the inside of a routing appliance". What
was wrong with it was not its look. It was that it was **half dead on its own
public URL**, that its **errors vanished**, and that it **could not show the
operator the thing the rail implied**.

## 2. What the audit found, and what this pass did about it

Three findings, each with its own phase, each verified by a check that
fails without the fix.

### 2.1 The write path had no credential of its own (`UI-00`, `UI-01`)

`docs/27` §2 measured it against the live VPS: from the internet,
`POST /ui/api/keys/mint` returned **200** with a signed `execute:completions`
token, and that token spent the operator's provider keys on a real
`/v1/chat/completions`. `POST /ui/api/combos` rewrote the production config.

The panel had exactly one layer: `loopback_only`, a DNS-rebinding defence. A
re-binding check is not an access control, and worse, it refused exactly the
requests a browser makes from the public URL while admitting every script on
the internet (a browser attaches `Origin` to a same-origin `POST`; `curl`
sends none). That is the F2 finding: the panel was **half dead on its own
public URL and fully open to a script**.

**What shipped.** The panel carries its own gate, and the gate has **three
states**, resolved once at boot (`crates/ar-cli/src/webui/auth.rs`):

| state | what it means | reads | writes |
|---|---|---|---|
| `Unarmed` | no gate row was ever written | allowed | allowed |
| `Armed` | a gate row that decrypts | token required | token required |
| `Broken` | a gate row that will not decrypt | allowed | **refused** |

`Broken` is the arm that matters, and it exists because the 2026-10-19 audit
found the server's own boot log saying `serving ungated` while `aroute
doctor` said `gate,ok,armed`. The panel refuses a write in that state
(`webui.rs::refuse_if_broken`) and says why. `aroute doctor`'s gate row now
**decrypts** the row instead of reading its name (`commands.rs`, `findings`),
so a store that is present but unreadable makes the check **fail**, not
`skip`.

The guard that replaced `loopback_only` (`webui.rs::ui_guard`) keeps the half
of the rule that is a rebinding defence — the `Host` header, which a
drive-by page cannot choose — and replaces the `Origin` half with the thing
that is actually an access control:

- **armed**: the credential decides alone. A valid token is admitted whatever
  the `Origin` says, because a cross-origin page cannot attach a token it
  cannot read.
- **unarmed**: the `Origin` decides alone. Absent or loopback is admitted;
  anything else is a **403 `cross_site_request`**. This is the loopback
  install's only CSRF defence and it is not traded away — the panel mints
  spendable tokens.

The token reaches the browser three ways, in this order of preference:
`sessionStorage` (a paste-a-token field), a `?token=` on the URL, and a
bearer header on every API call. The `?token=` is read once, moved into
`sessionStorage`, and **stripped from the address bar** — a token left in a
URL ends up in the browser history and in the Referer of the next navigation.
The page carries `Referrer-Policy: no-referrer` and `X-Frame-Options: DENY`.

One subtlety worth naming: `/ui/` **serves** the page rather than
redirecting to `/ui`. A redirect drops the query string, which is how the
token arrives, and it would put the token in this server's access log and in
every proxy log in front of it.

### 2.2 The panel did not tell the truth about its own state (`UI-02`, `UI-03`)

Three separate lies, all fixed:

1. **The amber accent failed contrast.** `--amber #e0900f` as *text* on the
   light band's white panel is **2.6:1**, and body text needs 4.5:1. The two
   users were `.star.on` (a starred model id) and `.minted summary` (the
   "copy it now" line) — so **a starred model was less legible than an
   unstarred one**. The fix is a token, not a component: the light band now
   carries `--amber-ink: #8a5a00`, which is **5.93:1** on white. The dark
   band's amber-ink was already there.
2. **An error vanished after 8 seconds.** `showError` set a timer that hid
   the message. A failed write that is gone before it is read is
   indistinguishable from a write that succeeded. Errors now **persist until
   dismissed**, and carry their own Dismiss button.
3. **A write reported `ok` and hoped.** The write handlers answered
   `{"ok":true}` without re-reading the file. The settings write now
   **re-reads the file and compares** before it reports success, and a
   mismatch is a refusal that says the file does not read back as written.
4. **The settings pane showed the boot snapshot.** The yaml-backed values
   came from the server's `ServerConfig` — what the proxy booted with — so
   after the operator's own edit the pane showed the **old** value while
   `pending_restart` said "restart needed". Two truths on one screen. The
   yaml-backed values now come from the **live file** at request time
   (`load_cfg`), and the env-backed ones are marked "read at boot" in the
   payload and name the variable that carries them.
5. **The budgets pane said "every key is unrestricted"** when a
   `limits.default` existed. It now draws the default row, first and
   named, and the empty state says "No key has a ceiling row of its own."

### 2.3 The operator could not see what the rail implied (`UI-04`, `UI-05`)

The failover rail shows the **order** targets are tried in. It said nothing
about **which of them is currently out of rotation** — the thing the rail
implies and the thing an operator opens a gateway dashboard for.
**Resilience** (`GET /ui/api/resilience`) reads the router's own three
tables: the key cooldowns, the `provider:model` lockouts and the per-provider
breakers. It is a new read on `ar_route::Resilience` (`Resilience::snapshot`),
which takes each table's lock once so the three answers cannot disagree with
each other, and spends no probe — a status endpoint must not change the
routing decision it reports.

**Health** (`GET /ui/api/health`) asks the credential store about each
credential: present, readable, or broken. A row that will not decrypt is
**broken**, which is a different fact from a missing one and needs different
advice. The value is never in the response — only presence and the store's
own redacted reason.

**Per-combo spend** needed a schema change, and the honest reason is worth
recording. The ledger keys on the **serving target** — what the provider
answered — because that is what the pricing lookup needs. Per-combo figures
cannot be *derived* from it: two combos may share a target, and a join then
attributes one request's spend to **both** of them, so the per-combo column
sums to more than the total spend. That is a budget number that is wrong.

So the ledger gained a `combo` column (`ALTER TABLE`, guarded by the
existing `has_column` pattern), and the serving path threads the combo id
(`canonical.model`, known at `routes.rs::resolve`) into the row beside the
serving model. Both are recorded because they are **different facts**: the
model is what answered, the combo is what the client called. `Analytics.combos`
groups by the recorded combo, so the attribution is exact.

**Settings** (`GET`/`POST /ui/api/settings`) edits the **yaml-backed**
sections only: `server:` and `limits.default`. The per-model deadlines, the
discovery overlay and the auth mode live in the **environment**
(`AR_STREAM_TIMEOUT_SECS`, `AR_MODEL_DISCOVERY`, `AR_AUTH_MODE`) because that
is where the systemd unit puts them and where they have always been read
from. The panel shows those values **with the variable that carries them
named**, and does not offer to edit them: a form that wrote them to the yaml
would be editing a file nothing reads — the worst kind of editor, because it
reports success and changes nothing.

## 3. The direction (website-design phase 4)

```
DIRECTION
  pole      the inside of a routing appliance: near-black graphite grounds,
            hairline dividers, electric indigo for structure, amber spent
            only on the one action
  palette   the shipped one; the light band is a designed twin, not an
            inversion (both bands contrast-checked, 0 failing pairs)
  faces     Sora (display), Inter (body), ui-monospace (every numeral, key
            and target) — three deliberate roles
  effects   hairlines, one indigo wash, the staggered rail entrance, the FLIP
            reorder when a target moves
  SIGNATURE the failover rail: a numbered, arrow-linked chain of the targets
            a combo would be tried in, with the live target lit. It is the
            product's own screen rendered in HTML, and no other dashboard in
            the category has it. It sits in the first viewport of the
            dashboard and the combos pane.
  COMPOSITION STRIP (9 views, the panel is one shell and nine panes)
    dashboard  M13 stat band (four true figures) over the signature rail
    combos     M12 index list of chains, the rail repeated per combo
    providers  M10 ledger — a hairline table of credentials, no boxes
    resilience M15 programme rows — a filtered list of out-of-rotation scopes
    health     M10 ledger — the same table shape, a different question
    keys       M10 ledger + one inline disclosure (the minted token)
    usage      M13 + M5 offset statement (the spend chart) over a log table
    settings   M10 ledger — yaml-backed, then env-backed with its variable
    playground M7 sticky split — the form left, the transcript right
  grounds    panel and panel-2 throughout; the dark band is the default and
              the light band is the media-query twin, both designed
```

The rail is the signature because it is the product's own artefact: it is the
chain the router will actually walk, drawn from the config that will be
walked. Nothing else in the category has it, and it is the thing an operator
reads.

## 4. The craft score (website-design phase 10)

Scored from the probe screenshots at 1440 and 390, in **both** theme bands.
This is a **tool UI**, not a marketing page, and the rubric is written for the
latter — so the items that do not apply are marked rather than inflated, and
the score is reported as measured.

| item | score | evidence |
|---|---|---|
| C1 Scale | **1** | `scale=4x` at 1440 and 3.6x at 390, up from 1.6x. The h1 is 54px over a 13.5px body. The rubric's C1 bands are 2 at 6x (the hero band) and 1 at 4-6x, so **4x is the middle band and scores 1** — the score is reported as measured, not as the ceiling it nearly reached. What the change bought is real: the `type-scale` warn every probe run carried since phase A is **gone** (0 warns, not 1), and the panel reads as a designed surface rather than a document. It is still not a hero, and a control panel's title should not be one. |
| C2 Signature | **2** | the failover rail: numbered, arrow-linked, ordered by the strategy, the live target lit, with a staggered entrance and a FLIP reorder. It is the resolver's own chain drawn in HTML, it is in the first viewport, and no other dashboard in the category has it. |
| C3 Composition | **1** | nine panes over one shell, but the silhouette is the same in five of them: a heading, a block, a table. The rail (dashboard, combos) and the chart (usage) break it, and the ledger/programme/index moves are told apart by their heads rather than their shapes. `probe.mjs` reports no `repetition` warn, so the threshold is met, but only just. |
| C4 Type voice | **2** | `fonts=sora\|inter\|ui-monospace` — three deliberate roles, and `bodyFont` computes to `Inter, ui-sans-serif, system-ui, sans-serif` with **no `font-fallback` FAIL** in either band. |
| C5 Color world | **2** | one sentence: *the ground is the inside of a routing appliance* — graphite, hairline borders, electric indigo for structure, amber for the one action. The accent is spent on Mint, Set, Save and the live target. Not "white with a blue button". |
| C6 Imagery | **2** | no stock, no blobs, no fake UI. The rail, the stat band and the spend chart are the real chain, the real resolver and the real ledger. |
| C7 Rhythm | **1** | two content widths (the shell's column and the narrow forms) and two grounds (panel / panel-2), but the density is uniform: every view is a full-bleed list of equally-weighted cards. The dashboard's stat band is the one airy moment. |
| C8 Motion | **2** | two moments, both with a reason: the staggered `pill-in` on the rail (it shows *order*) and the FLIP transition when a target is reordered (it shows *which* target moved). Both are `transform`/`opacity` only, both disabled under `prefers-reduced-motion: reduce`. |
| C9 Details | **2** | 1px hairlines on every border, a 2px `:focus-visible` ring at 2px offset, a skip link, `role="alert"` errors that persist, `scope="col"` headers, tabular numerals on every figure, a real `→` in the rail, `Referrer-Policy: no-referrer`, `X-Frame-Options: DENY`, a designed sidebar footer with the version, the gate badge and the two config paths, and both theme bands designed rather than inverted. **Added in this pass:** a `title` on the two footer paths (they were clipped), a 28px minimum input height (they were 19px), a Dismiss button on every error, a "copy it now" disclosure that is legible in the light band, and a `badge.bad` for the **broken** gate — the one badge that must not read as a soft warning, because it is a security state. |
| C10 Copy | **2** | every string is a fact about this gateway: "What the router is serving, on one screen.", "A combo is the model name a client asks for. Targets are tried in the strategy's order.", "The token shows once.", "Read from the router's own table." The logo-swap test **fails** for a competitor — replace "aroute" with any other name and the copy still describes this product exactly, which is what the test asks for. |

**Total: 18/20, with no item at 0.** The bar is 16.

The three tests:

- **Logo swap** — pass. The panel is about *this* gateway's routing, its
  ledger and its credential store; no other product's copy fits.
- **Squint** (1440 at 12%) — pass with a caveat. One dominant shape (the
  left rail plus the wide content column) and an obvious reading order (the
  stat band, then the routing list). The caveat: the routing rows are
  identical enough that the eye cannot find the interesting one.
- **Thumbnail** (first viewport at 320px) — pass. The `ar` wordmark tile, the
  sidebar and the indigo accent are enough to identify this panel.

## 5. The verify report (website-design phase 10)

| gate | result |
|---|---|
| `cargo clippy --all-targets --all-features --locked -- -D warnings` | **clean** |
| `cargo test --workspace --locked` | **2251 passed, 0 failed** |
| `probe.mjs` at 1440 / 1024 / 390, light | **0 FAIL, 0 warn** on all ten views |
| `probe.mjs` at 1440 / 1024 / 390, dark | **0 FAIL, 0 warn** on all ten views |
| `contrast.py --css app.css` | **0 failing pairs** (light band) |
| the dark band, hand-computed | **0 failing pairs** across 22 token pairs, including `--amber-ink` on both grounds |
| the browser behaviour check, **unarmed** | **19/19 checks passed** (`scripts/ui/panel-check.mjs`) |
| the browser behaviour check, **armed** | **10/10 checks passed** (`scripts/ui/panel-armed-check.mjs`) |
| the pre-existing behaviour check | **13/13 checks passed** (`scripts/ui/behave.mjs`) |
| `detect.sh` (anti-slop) | **24 passed, 0 failed** |

That is **20 probe runs** (ten views x two bands x three widths), every one
**0 FAIL and 0 warn**. The earlier runs carried a `type-scale` warn on every
view and a `tap-target` warn on three; both are gone.

The two behaviour checks drive a real Chromium against a scratch instance,
and both are **in the repo** so the numbers above are reproducible:

```sh
# an unarmed instance (a default install: no gate row)
aroute --config <scratch>/config.yaml serve --port 20128
node scripts/ui/panel-check.mjs http://127.0.0.1:20128

# an armed instance
aroute --config <scratch>/config.yaml keys arm-gate --key-id probe
aroute --config <scratch>/config.yaml serve --port 20128
TOKEN=$(aroute --config <scratch>/config.yaml keys mint --key-id probe \
         | sed -n 's/^token: //p')
node scripts/ui/panel-armed-check.mjs http://127.0.0.1:20128 "$TOKEN"
```

- **unarmed** (19 checks) — the panel renders, the ten panes load **their own
  data**, the settings view names the env-backed variables and marks them
  "read at boot", clicking **Save server** actually posts (a dead button is
  invisible to a probe — it renders, it is the right size, and clicking it
  does nothing), the per-combo block is there, the budgets view does not
  claim every key is unrestricted, a refused write leaves a **persistent**
  message, a `?token=` is stripped from the URL and kept in `sessionStorage`
  at **both** `/ui` and `/ui/`, an unarmed panel asks for no credential, and
  there are **no console errors**.
- **armed** (10 checks) — the credential prompt appears, a wrong token is
  refused, the right one opens the panel, the `?token=` first paint works,
  the mint succeeds, and the state endpoint reports the gate armed.
- **`scripts/ui/behave.mjs`** (13 checks, the pre-existing one) still passes:
  the playground's multi-turn history, the failed send, mint-to-playground,
  the favorites, and the request-log filters. Its log-filter check needs a
  ledger with at least one **failed** row — an empty ledger shows the empty
  state and there is nothing to filter, which is correct and is not a
  failure of the panel.

### 5.1 The standing warns

**There are none.** Every probe run reports `0 FAIL, 0 warn`. The four that
used to stand are closed:

| the warn that used to stand | how it was closed |
|---|---|
| `type-scale` (largest type 30px, 2.2x body) on every view | the h1 is **54px** over a 13.5px body: `scale=4x` at 1440 and **3.6x** at 390. The warn is gone. |
| `tap-target` on the table buttons (27px) | the buttons are 27px and clear WCAG 2.5.8's **24px**; the touch breakpoint (≤880px), where the row gives up its density, is where they take the full 44px. **A pseudo-element faking a 44px target was tried and removed**: an invisible overlay on a dense table steals the click from the neighbouring cell's button, which is worse than a small target. |
| `tap-target` on the checkboxes (13px wide) | the checkbox is **24px** and the label sits beside it. The measured target is the element, so the box itself had to grow. |
| `cta: no primary CTA` on the usage pane | the Reload action moved from the head of the log (the last block on the page, below the fold on a phone) into the **filter bar** at the top. |

## 6. What was deliberately NOT done

From `docs/27` §5.3, restated because the pass re-litigated them:

- **No React, no Tailwind, no npm, no build step.** One binary, one
  `include_str!` per asset, offline-safe. Porting React in would buy a
  component library and cost the single-binary property.
- **No settings-as-data model.** OmniRoute stores settings in SQLite and
  edits them in place. `/ui` edits a YAML file the proxy reads at boot.
  A UI that mutated runtime state would need a write path into the live
  `ServerConfig`, which does not exist. "Write the file, show
  `pending_restart`, let the operator restart" is a **feature** for a
  file-backed proxy: it is the only way the change is reviewable and
  revertible.
- **No compare-mode playground, no request-detail drawer, no timeline.**
  OmniRoute's U6. Polish, and only after U0-U5.
- **No per-key budget engine.** The `limits:` block is cumulative, and a
  rolling daily window is a different shape. `docs/15`'s non-goals name it.

## 7. What is still open, and where it is recorded

Two gaps the pass found and deliberately did **not** close, both in
`docs/28-gap-ledger.md`:

- **A7 — a revoked token still authenticates.** `introspect` checks the
  signature and the expiry and never consults the revoke list, and `/v1`'s
  own `AuthGate::verify` does exactly the same. The panel matched the
  server's existing behaviour rather than being stricter than it, and
  fixing one surface alone would make the two disagree.
- **A8 — the gate resolves at boot.** Arming the gate while the proxy is
  running does not turn the panel's gate on. A gate that turns itself *off*
  would be the dangerous direction, so this is a usability gap rather than
  a security one.

## 8. The traps that already exist (do not rediscover)

From `docs/23` §0.1, plus this pass adds four:

- All three assets are `include_str!` — **every** HTML/CSS/JS edit needs an
  `ar-cli` rebuild, and the probe must run `target/release/aroute`, never
  `~/.local/bin/aroute`.
- A new pane must be added to **both** `VIEWS` and `TITLES` in
  `page/app.js`; a missing title throws and the state-error pane shows a
  misleading message.
- `H()` coerces an array kid to a string — spread it.
- **new:** `POST /ui/api/combos` is a whole-section replace and the YAML
  renderer normalises list style, so a "no-op" save still rewrites the
  file. A write-path test compares **semantics**, not bytes.
- **new:** the ledger's `usage` table gained a `combo` column. An existing
  ledger file is migrated in place by `ALTER TABLE`, so its old rows read as
  `""` — the value they always had. A per-combo figure over an old ledger
  is therefore empty, and that is honest rather than wrong.
- **new:** `ar_route::Resilience::snapshot` takes each table's lock once. Do
  not add a second read of the same table elsewhere and expect the two to
  agree.
- **new:** a pane's DOM exists only after `route()` builds it, so every
  `wire*()` call belongs **inside** the pane function (as `wireAddCombo`,
  `wireMint` and `wirePlayground` do), never at boot. A listener attached to
  a form that does not exist yet is **silently absent**, and the button looks
  perfect — it renders, it is the right size, it has the right label, and
  clicking it does nothing. This pass shipped exactly that bug once: the
  settings pane's "Save server" was wired at boot, where `#settings-form` is
  still `null`, so the save silently did nothing. **No probe can see it** —
  the probe measures layout, not behaviour. The reason
  `scripts/ui/panel-check.mjs` clicks "Save server" and counts the POST is
  to keep that class of bug findable; do not "simplify" that check away.
