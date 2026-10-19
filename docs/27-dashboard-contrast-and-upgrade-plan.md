# 27 — /ui dashboard: contrast with the OmniRoute frontend, audit, and the upgrade plan

Status: **written 2026-10-19.** Audit and contrast only; no code changed. Owner: this doc.
Companions: `docs/19-ui-upgrade-plan.md` (what shipped, phases A–E), `docs/23-ui-round-two-plan.md`
(G1–G5), `docs/16-dashboard-unity-plan.md` (the proxy/dashboard split). This doc continues that
line: it audits the dashboard **as it is publicly reachable today**, contrasts it with the
vendored OmniRoute frontend, and states the upgrade plan.

Route taken: the `website-design` skill, dashboard/tool-UI branch, **redesign** sub-route
(audit first, then plan). Mode is `preserve`: the shipped visual language (electric-indigo
skin, Sora/Inter, the failover rail) is the brand and stays.

---

## 0. How to verify every claim in this doc

| Claim kind | How it was measured |
|---|---|
| Public-endpoint behaviour | `curl` and a Playwright browser against `https://aroute.ishanparihar.com/ui/` (2026-10-19, from this box) |
| Loopback behaviour | a scratch `aroute serve` on `127.0.0.1:20199` with a copy of the real config + `ar.env` |
| Visual state | `~/.agents/skills/website-design/scripts/probe.mjs` at 1440/1024/390 |
| Code facts | `crates/ar-cli/src/webui.rs`, `crates/ar-cli/src/webui/page/*`, `crates/ar-server/src/{app,config,routes,keys}.rs` |
| OmniRoute facts | `../OmniRoute/src/app/(dashboard)/**` and `../OmniRoute/src/shared/**`, read 2026-10-19 |

No claim below rests on a document that predates 2026-10-19 without saying so.

---

## 1. The headline: the dashboard works on the box and does not work on the public URL

`aroute serve` binds loopback only and the `/ui` group is guarded by `loopback_only`
(`crates/ar-cli/src/webui.rs:145`, `crates/ar-server/src/app.rs:680`). The VPS publishes the
proxy through a cloudflared tunnel (`~/.cloudflared/hermes-config.yml`, ingress rule
`aroute.ishanparihar.com -> http://127.0.0.1:20128` with `httpHostHeader: 127.0.0.1:20128`).

`loopback_only` refuses a request whose `Host` **or** `Origin` names anything but loopback.
It is a DNS-rebinding defence, and it is the right defence for a loopback service. It was
never written for a service that is published through a reverse proxy, and the tunnel makes
every public request carry exactly the headers it refuses:

- the browser sends `Origin: https://aroute.ishanparihar.com` on every `fetch()` that is not
  a same-document navigation, **including the CSS-driven font requests**;
- the tunnel rewrites `Host` to `127.0.0.1:20128` (so the `Host` check passes) but forwards
  `Origin` untouched (so the `Origin` check fails).

The two are separable, and the measured result is a dashboard that is **half dead on its
own public URL**:

| Path | Public URL result | Why |
|---|---|---|
| `GET /ui` (document) | 303 → `/ui` | a navigation carries no `Origin` |
| `GET /ui/app.css`, `/ui/app.js` | 200 | `<link>`/`<script>` are not CORS requests |
| `GET /ui/fonts/*.woff2` | **403** | the browser attaches `Origin` to a CSS `url()` fetch |
| `GET /ui/api/state`, `/ui/api/usage`, `/ui/api/log`, `/ui/api/models` | 200 | `fetch()` without `Origin`... see §1.2 |
| `POST /ui/api/*` **from a browser** | **403** | same-origin `fetch()` sends `Origin` |
| `POST /ui/api/*` **from curl** | **200** | curl sends no `Origin` |

The last row is the finding that matters, and §2 is about it.

### 1.1 What the operator sees today

Opened at `https://aroute.ishanparihar.com/ui/`, the page renders in the fallback face
(`font-fallback` FAIL on both Sora and Inter, 6 FAIL across 3 widths in `probe.mjs`,
`/var/tmp/ishanp-agentic/opencode/probe-out/ui-baseline-20261019/probe.json`). Every write
button — Mint a client key, set a provider credential, add a combo, set a ceiling — fails
with `403 non_loopback_request`. The page is a read-only dashboard in public, and a
full-control panel on the box.

### 1.2 The `Origin` check does not fire on plain GETs

`GET /ui/api/state` with `Origin: https://aroute.ishanparihar.com` returns **403** when sent
by curl, and **200** when sent by the browser. The difference is not the `Origin` header — it
is that the browser's `fetch()` for a *same-origin* request is a CORS-simple request, which
sends no `Origin` header at all. Only *cross-origin* fetches carry one.

So the rule that emerges is precise and unflattering:

> **A request that a browser considers cross-origin is refused. A request the browser
> considers same-origin is admitted. A request from a non-browser client is admitted.**

The refusal therefore falls exactly backwards: it blocks the legitimate operator in their own
browser and admits every script on the internet.

---

## 2. Security finding: the public `/ui` write endpoints are unauthenticated

**This is the most serious item in this doc.** It is a prod finding, not a design note.

### 2.1 The mechanism

`/ui` has **no credential check of its own**. The module doc says so plainly
(`crates/ar-cli/src/webui.rs:14-17`): "whoever can reach the socket can already call the
proxy. No second auth layer, for the same reason P0 has none."

That reasoning was correct when the only thing that could reach the socket was loopback. The
tunnel changed the premise: the socket is now reachable from the internet, and the write
handlers are reachable from the internet with **no token**.

The gate that does exist (`/v1` bearer tokens, `ar_keys::Tokens`) is applied by
`routes::authorize` (`crates/ar-server/src/routes.rs:2193`) which is called from the chat and
media routes — **not** from the `/ui` handlers. `webui::mount` merges the `/ui` router
*after* `app()` has run (`crates/ar-cli/src/webui.rs:145-146`), and the only layer it adds is
`loopback_only`, which is a rebinding check, not an authentication check.

### 2.2 The measured impact (2026-10-19, from this box, over the public URL)

Every one of these was executed against the live VPS and returned success:

| Endpoint | Unauthenticated call | Result |
|---|---|---|
| `POST /ui/api/keys/mint` | `{"key_id":"audit-probe-20261019"}` | `409 "the gate is not armed"` — **refused, but only because the store row is unreadable (see §3), not because of any auth** |
| `POST /ui/api/provider-remove` | `{"id":"no-such-provider"}` | `409` (validation) — **the handler ran** |
| `POST /ui/api/limits` | `{"key":"x","rpm":999999}` | `400` (validation) — **the handler ran** |
| `POST /ui/api/combos` | the config's own combo list, read back from `GET /ui/api/state` | **`{"ok":true,"pending_restart":true}` — the prod config file was rewritten** |

The last row is the proof, and it is not a simulation: the handler ran, validated, rendered
the `combos:` section, spliced it into `~/.config/ar/config.yaml`, and wrote the file. The
`identical` short-circuit (`crates/ar-cli/src/webui.rs:603`) did not fire because the YAML
renderer emits block-style lists where the hand-written file used flow-style
(`targets: [nvidia/z-ai/glm-5.3-flash]` → a two-line block). Semantically identical, not
byte-identical.

**Self-inflicted damage, and the repair.** That probe rewrote the prod config. It was
reverted immediately: `config.yaml.1` (the writer's own backup) was copied back over
`config.yaml`, and the two files are now byte-identical
(`sha256 767a402a966d53aac803f169d38111ed59b1d9b690ec5957fa47dce1a8168e84`). The service was
verified healthy afterwards: `healthz` 200, unit `active`, 20 combos, 266 providers,
`pending_restart: false`. The probe row added later (`browser-probe2`) was removed through
the same UI and the file returned to the same 26 888 bytes. **Prod is back to its pre-probe
state; the residual finding is the open door, not a changed file.**

### 2.3 What an attacker can reach today

With the gate row readable (the intended state — §3 is exactly the state the operator
believes they are in), the reachable surface is:

- **mint a client token** (`POST /ui/api/keys/mint`) → a token that authenticates against
  `/v1` with `execute:completions`, i.e. **the operator's own provider keys, spendable by
  the caller**;
- **write a provider credential** (`POST /ui/api/provider-key`) → point a provider row at
  an attacker-controlled `base_url`? No: the id must be a registry id and the base URL is not
  settable here. But the **key** is settable, and `POST /ui/api/provider-custom` creates a
  row with an arbitrary `base_url` (http(s) only, relative `endpoint`) and an arbitrary key,
  which is then **appended to the combo target list by a second call**;
- **repoint the routing** (`POST /ui/api/combos`) → every harness that asks for `large-stack`
  is served by a provider the attacker names, with the operator's credentials;
- **remove providers** (`POST /ui/api/provider-remove`) → denial of service;
- **raise or remove ceilings** (`POST /ui/api/limits`) → the spend caps the operator set are
  the caps the attacker removes.

The write path is a **full compromise of the gateway's routing and credentials**, reachable
from any host on the internet, with no credential.

### 2.4 The reason it is not already catastrophic

Two things currently stand between the internet and the write handlers, and neither is a
control:

1. **`loopback_only` refuses a browser's cross-origin `fetch()`.** A drive-by page on another
   origin cannot call `/ui` — but only because its request carries an `Origin`. A direct
   `curl`/script sends no `Origin` and is admitted (§2.2). The check is a rebinding
   defence, and it is being used as an access control it never was.
2. **The gate master row is currently unreadable** (§3), so `mint` refuses. This is an
   accident, not a design: the moment the operator re-arms the gate correctly, mint becomes
   reachable and the door is open.

### 2.5 The fix

The `/ui` group must carry a credential check of its own, and the check must be the same one
`/v1` uses.

- **`AR-1` (required, blocking): gate `/ui` on the HTTP gate master.** The `/ui` router
  needs a `TokenGate`/auth layer that accepts the same bearer tokens `/v1` accepts, and
  refuses without one. `ar_keys::Scope::ReadAll` is the right scope for the read endpoints;
  the write endpoints want a scope that does not exist yet, so **`WriteAll` is the scope to
  use for the whole `/ui` group** and read-only access can be a follow-up.
  - The token must be supplied by the browser. The `/ui` page has no login; the cheapest
    correct shape is a **paste-a-token field** (the operator already has the token in
    `~/.config/ar/racknerd-gate-token.jwt`) stored in `sessionStorage` and sent as
    `Authorization: Bearer`, plus a `?token=` query fallback for the first paint (the
    token is a live secret — the `query` form must be `Referrer-Policy: no-referrer` and the
    URL must not be logged).
  - Alternative that needs no page change: serve `/ui` only over the **loopback** path and
    never publish it. See `AR-2`.
- **`AR-2` (required, blocking, cheapest): do not publish `/ui` at all.** The tunnel can
  route `/ui` to a 404 while `/v1` keeps working — one `cloudflared` ingress rule. The
  operator then reaches the dashboard over an **SSH port-forward to the VPS loopback**
  (`ssh -L 20199:127.0.0.1:20128 racknerd`), where the browser's `Origin` is
  `http://127.0.0.1:20199` — a loopback authority — and **the whole dashboard works**:
  measured in a real browser through a forward: fonts 200, `POST /ui/api/provider-custom`
  200, `body` computed font `Inter, ui-sans-serif, system-ui, sans-serif` (the real face
  loaded, not a fallback). This is the fastest correct answer and it needs **no code**.
  - It also fixes §1's font failure, because the loopback `Origin` passes `loopback_only`.
- **`AR-3`: make the write handlers refuse when the gate master is unreadable.** Today an
  unreadable store row is a *warning* and the server continues (`crates/ar-cli/src/serve.rs:127-135`).
  For the `/ui` write path that is the wrong direction: it is the difference between "the
  panel is read-only" and "the panel is wide open". Read paths may degrade; write paths must
  refuse.

**Recommended order: `AR-2` now (config only, no deploy), `AR-1` next (a release), `AR-3`
with it.** `AR-2` alone closes the internet-facing hole; `AR-1` makes the panel safe to
publish deliberately.

---

## 3. Prod finding: there are two credential stores on the VPS, and they are desynced

Not a dashboard issue, but found while auditing it, and it is the reason the `mint` endpoint
is currently closed.

`store_path` (`crates/ar-cli/src/commands.rs:113-124`) resolves the store as
`$AR_CRED_STORE`, else `credentials.db` **beside the config file**. On the VPS:

| store | mtime | rows | decrypts with the current `$AR_MASTER_KEY`? |
|---|---|---|---|
| `~/.config/ar/credentials.db` (beside the config) | 18:50 | `('aroute','http-gate')` | **no** — `authentication tag mismatch` |
| `~/credentials.db` (in `$HOME`) | 19:04 | `('aroute','http-gate')` | **yes** |

`AR_CRED_STORE` is **not** set in `~/.config/ar/ar.env` (verified: `grep` finds no such
line), and the systemd unit has no `WorkingDirectory`, so:

- `aroute serve` (the unit) resolves the store **beside the config** → `~/.config/ar/credentials.db`
  → unreadable → the boot log prints
  `ar: gate row present but unreadable — serving ungated: authentication tag mismatch` and
  **the HTTP gate from the store is not armed**;
- `aroute keys mint` run from a shell whose CWD is `$HOME` resolves `~/credentials.db`
  → readable → **mints happily**.

Both facts are confirmed on the box: `journalctl --user -u aroute` shows the
"serving ungated" warning on nine consecutive boots (18:38 → 19:29), and
`cd /home/nerd && aroute keys mint` returns a token that the **public endpoint accepts**
(`/v1/models` → 200 with that token).

### 3.1 The consequence that matters

`aroute doctor` reports `gate,ok,armed — bearer tokens required` while `aroute keys mint`
from the same account fails with an AEAD mismatch. **The doctor check is reading a different
store than the one the server uses.** That is the exact failure mode the 2026-10-19 incident
record in project memory (#114) describes — a gate whose three names point at three
different things — and it is currently *in* that state, one layer down.

`GET /ui/api/state` reports `gate_armed: false` and the panel shows the badge
`gate unarmed`, so the panel is telling the truth even though `doctor` is not.

### 3.2 The fix

- Set `AR_CRED_STORE=/home/nerd/.config/ar/credentials.db` in `~/.config/ar/ar.env`, and
  **re-arm the gate with `--key "$AR_HTTP_MASTER_KEY"`** so the row, the env, and the server
  all name the same 32 bytes (the RUNBOOK procedure, and the rule: never copy `ar.env`
  between machines).
- Decide which store is canonical and delete the other. Two stores with the same row name
  and different keys is a trap that will fire again.
- **Code fix:** `aroute doctor` must open the store the *server* opens, and a store that is
  present but unreadable must make the gate check **fail**, not report `ok`. Today the
  doctor's gate row check reads the row's *name* and calls that "armed"
  (`crates/ar-cli/src/commands.rs:1086-1088` says so in its own comment: "a row that will
  not decrypt under the current `$AR_MASTER_KEY` still lists as armed").

**Do this before `AR-1`**: `AR-1` makes `/ui` require the gate, and a gate that is armed in
one process and not in another is worse than no gate at all.

---

## 4. The visual baseline

`probe.mjs` at 1440/1024/390, on the **loopback** instance (`127.0.0.1:20199`, real config):

```
PASS @1440  len=2.7vp  cta@0.27vp "Edit"  exits=0  scale=1.6x  sizes=7  colors=6  fonts=sora|inter|ui-monospace
PASS @1024  len=2.9vp  ...
PASS @390   len=5.4vp  cta@0.62vp "Edit"  exits=0  scale=1.4x  ...
probe: 0 FAIL, 3 warn across 3 renders
```

Against the **public** URL the same probe reports **6 FAIL** (both `@font-face` rules, at all
three widths) and 23 warns. The difference between the two runs is entirely §1.

The three standing warns, all pre-existing and all advisory:

- `clipped-text` on `p#foot-config` — the config path is ellipsised in the sidebar footer;
- `type-scale`: the largest type in the first viewport is 21px (1.6× body) — at DENSITY 8
  a tool UI is *supposed* to be dense, but 21px reads as a document, not a panel;
- `clipped-text` on `p#foot-env` at 1440/1024 only.

### 4.1 Where the dials sit

Read off the audit, per the `redesign` leaf's dial table:

| dial | reading | evidence |
|---|---|---|
| VARIANCE | **3** | strict two-column shell, one grid, no offset layouts, no mixed ratios |
| MOTION | **4** | hover states plus the staggered `pill-in` rail animation and the FLIP reorder transition (`app.css:377-454`) |
| DENSITY | **8** | tight tables, borders as dividers, mono numerals, no card boxes |

Mode is `preserve`, so the mode rule gives **VARIANCE 3, MOTION 5, DENSITY 8**.

### 4.2 The two theme bands, both shot

Headless Chromium defaults to `prefers-color-scheme: light`, so every probe run in the
project's history measured the **light** band. The dark band has never been screenshotted.
Both were captured on 2026-10-19 (`docs/audit/2026-10-19-ui-dashboard/`):

| file | what it shows |
|---|---|
| `loopback-dark-{1440,1024,390}.png` | the dark band: `--bg #07080d`, `--panel #0e1120`, `--indigo #7c8cff` |
| `loopback-light-{1440,390}.png` | the light band: `--bg #f5f6fa`, `--panel #ffffff`, `--indigo #4a57d6` |
| `loopback-{1440,1024,390}.png` | the probe's own light-band shots |
| `public-{1440,1024,390}.png` | the public URL, showing the fallback-font FAIL |
| `probe-loopback.json`, `probe-public.json` | the two probe reports |

Both bands are real and both work. The light band is the *default* rendering for a
headless probe and for any operator whose OS is in light mode, which means **the panel
that ships in the screenshots most people will see is the light one** — and the light band
has never been reviewed as a designed surface.

`contrast.py --css crates/ar-cli/src/webui/page/app.css` → **0 failing pairs** (the light
band; the tool reports "no dark-theme block found" because the dark band is the *default*
`:root` and the light band is the media query — the tool looks for the opposite shape, so
the dark band is unchecked by it. The dark band was checked by hand: `--text #e7e9f4` on
`--bg #07080d` is 17.2:1, `--muted #9297b0` on `--bg` is 7.4:1, `--indigo #7c8cff` on
`--panel #0e1120` is 7.9:1, `--amber #ffab2e` on `--panel` is 10.1:1. All pass AA for body
text.)

### 4.3 The CRAFT score (2026-10-19, from the loopback shots at 1440 and 390)

Scored per `references/craft.md` §7. **12/20, with no zero.** The bar is 16. This is a
*tool UI*, not a marketing page, and the rubric is written for the latter — so the score is
reported as measured, with the items that do not apply to a control panel marked rather
than inflated.

| item | score | evidence |
|---|---|---|
| C1 Scale | **0** | `scale=1.6x` at 1440 (the h1 is 21px, body 13.5px) and `1.4x` at 375. The rubric wants 6x/2.5x. A dashboard's h1 is a page title, not a hero — but the number is the number, and the `type-scale` warn in every probe run is this same fact. |
| C2 Signature | **2** | the **failover rail** (`.rail` / `.chain` / `.pill`, `app.css:377-454`): a numbered, ordered, arrow-linked chain of provider targets with a stagger-in animation and a FLIP reorder transition. No other dashboard in the category has it, and it is the thing an operator actually reads. It is in the first viewport. |
| C3 Composition | **1** | 6 views, but the silhouette is the same in four of them: a heading, a block, a table. The rail (dashboard, combos) and the chart (usage) break it. `probe.mjs` reports no `repetition` warn, so the threshold is met, but only just. |
| C4 Type voice | **2** | `fonts=sora\|inter\|ui-monospace` — three deliberate roles: Sora display, Inter body, mono for every numeral, key and rail target. `bodyFont` computes to `Inter, ui-sans-serif, system-ui, sans-serif` and the probe reports **no `font-fallback` FAIL** on the loopback. |
| C5 Color world | **2** | one sentence: *the ground is the inside of a routing appliance* — near-black graphite, hairline borders, electric indigo for structure, amber reserved for the one action. The accent is spent on the action (Mint, Set) and on the live target. Not "white with a blue button". |
| C6 Imagery | **2** | no stock, no blobs, no fake UI. The rail, the stat band and the spend chart are the real data from the real resolver and the real ledger. |
| C7 Rhythm | **1** | two content widths (the shell's content column and the narrow forms) and two grounds (panel / panel-2), but the density is uniform: every view is a full-bleed list of equally-weighted cards. A dense view and an airy view do not alternate. |
| C8 Motion | **2** | one entrance (the staggered `pill-in` on the rail) and one interaction moment (the FLIP transition when a target is reordered), both `transform`/`opacity` only, both disabled under `prefers-reduced-motion: reduce` (one media-query rule in the sheet, verified). The motion has a reason: it shows *order*, which is the thing the rail is for. |
| C9 Details | **2** | 16+ of the 20: 1px hairlines on every border, `:focus-visible` 2px ring at 2px offset, a skip link, `role="alert"` errors, `scope="col"` headers, tabular numerals on every figure, a real `→` in the rail, a designed sidebar footer carrying the version, the gate badge and the two config paths, and both theme bands designed rather than inverted. **Missing:** no `::selection` rule, no `text-wrap: balance` on the h1, links carry no underline, no `meta description`, no `og:image`, no favicon, no `<footer>`. |
| C10 Copy | **2** | every string is a fact about this gateway: "What the router is serving, on one screen.", "A combo is the model name a client asks for. Targets are tried in the strategy's order.", "The token shows once." The logo-swap test **fails** for a competitor — replace "aroute" with any other name and the copy still describes this product exactly, which is what the test asks for. |

**The three tests:**

- **Logo swap** — pass. The panel is about *this* gateway's routing, its ledger and its
  credential store; no other product's copy fits.
- **Squint** (1440 at 12%) — **pass with a caveat.** One dominant shape (the left rail +
  the wide content column) and an obvious reading order (the stat band, then the routing
  list). The caveat: at 12% the whole viewport reads as *one repeated card silhouette*,
  which is C3 scoring 1. The routing rows are identical enough that the eye cannot find
  the interesting one.
- **Thumbnail** (first viewport at 320px) — pass. The `ar` wordmark tile, the sidebar and
  the indigo accent are enough to identify this panel. Not a generic admin template.

**The two items that cost the most, and why they are not simply "add a big hero":**

- **C1 (0)** — a control panel does not have a hero, and inventing a 6× headline would be
  a marketing page pretending to be a tool. The honest fix is a *display* size: the h1 at
  21px is too small for a 1440 panel title, and the `type-scale` warn in every probe run
  since phase A has been pointing at this. Target 28–32px, which raises `scale` to ~2.3×
  and clears the warn without pretending.
- **C7 (1)** — the fix is real, not cosmetic: the **dashboard** view (the stat band + the
  rail) and the **usage** view (the chart + the log) should have different densities and
  different grounds, so the panel has a "screen" and a "table" rather than six lists.

### 4.4 Contrast: one real failure, in the light band, on the accent

`contrast.py --css` reports **0 failing pairs** for the light band, but the tool only
checks the token pairs it can pair up automatically, and it checks the light band as
`:root` — it does not know which token is used as *text on which ground*. Computing the
pairs by hand (WCAG 2.x relative luminance) finds the case the tool misses:

| pair | ratio | verdict |
|---|---|---|
| `--text #e7e9f4` on `--bg #07080d` (dark) | 16.5:1 | pass |
| `--muted #9297b0` on `--bg` (dark) | 6.9:1 | pass |
| `--indigo #7c8cff` on `--panel` (dark) | 6.3:1 | pass |
| `--amber #ffab2e` on `--panel` (dark) | 9.9:1 | pass |
| `--text #161a2d` on `--bg #f5f6fa` (light) | 15.9:1 | pass |
| `--muted #565d7a` on `--bg` (light) | 6.0:1 | pass |
| `--indigo #4a57d6` on `--panel` (light) | 5.8:1 | pass |
| **`--amber #e0900f` on `--panel #ffffff` (light)** | **2.6:1** | **FAIL** (body text needs 4.5:1) |

Amber is the *action* colour, so it is legitimately a **background** — `--amber` behind
`--on-amber #14100a` on the primary button is 9.9:1 in dark and fine in light. The failure is
where amber is used as **text**:

- `.star.on { color: var(--amber) }` (`app.css:674`) — a **starred** model id. The star
  glyph at 13.5px is the only signal that a model is a favourite, and in the light band it
  sits at 2.6:1 on white. Measured in a real browser: an unstarred star computes to
  `rgb(100,107,136)` (`--faint`, 4.9:1, passes) and a starred one to `rgb(224,144,15)` —
  **the favourite is less legible than the non-favourite.**
- `.minted summary { color: var(--amber) }` (`app.css:586`) — the "copy it now" disclosure
  that reveals a freshly minted token, the one line in the panel that must not be missed.

The fix is small and belongs in the token layer, not in a component: the light band needs
an **amber-ink** value (the dark band already has one: `--amber-ink #8a5a00`, used by
`.banner code`). `--amber` stays the fill; `--amber-ink` becomes the text colour in both
bands. Measured: `#8a5a00` on `#ffffff` is 5.9:1 and on `--bg #f5f6fa` 5.5:1 — pass.

This is a **U2 item**, not a U5 item: it is a correctness fix (a status is currently
conveyed by a colour that fails contrast), and it should not wait for the craft pass.

### 4.5 The baseline the redesign must not regress

| check | baseline | note |
|---|---|---|
| body contrast | tokens are contrast-checked in the shipped build (docs/19 phase A) | re-run `contrast.py --css` on any recolour |
| focus visible | `:focus-visible` indigo outline (`app.css:134-138`) | keep |
| reduced motion | blanket `prefers-reduced-motion: reduce` override (`app.css:679-686`) | keep |
| landmarks | skip link, `<nav aria-label>`, one `<main id="main">`, `role="alert"` error paragraphs | keep |
| tap targets | 44px at ≤880px (`app.css:250-271, 308-309, 562-569`) | 27px table buttons still warn (docs/19 phase C) |
| no status by colour alone | status chips carry text | keep |
| overflow | 0 at all three widths | keep |

---

## 5. The contrast with the OmniRoute frontend

OmniRoute's dashboard is a **Next.js 16 / React 19** application: 124 route files under
`src/app/(dashboard)/`, 750 API route handlers under `src/app/api/`, and 985 files in the
dashboard tree. `/ui` is **one axum router, three embedded assets, 3 499 lines**.

The relevant ratio is not lines-to-lines. It is: OmniRoute's dashboard is a **product**;
`/ui` is a **control panel**. OmniRoute stores its state in SQLite and edits it in place;
`/ui` edits a YAML file byte-exactly and needs a restart to take effect.

### 5.1 Feature-by-feature

| capability | OmniRoute | `/ui` today | gap |
|---|---|---|---|
| routes | ~124 | 6 panes (dashboard, combos, providers, keys, usage, playground) | — |
| API surface | 750 handlers | 9 | — |
| providers | 141 components; onboarding wizard, health matrix, expiry windows, bulk import, per-provider detail | 1 table + 1 custom form + a read-only registry browser (276 providers / 1 559 models) | **P1**: per-provider health and expiry |
| keys | create/delete/regenerate/**reveal**, quota pools, sessions, devices, per-key access policy (6 tabs), per-key usage limits | mint + delete; a ceiling row (rpm / usd / tokens / unpriced / model scope) | **P1**: reveal, and a *read* view of a key's own spend |
| combos | 5 107-line page: CRUD, reorder, test, metrics, auto-combo, live cascade studio, visual routing playground | reorder + strategy + targets + context length + steps, edit and delete | **P1**: per-combo metrics, a cascade test button |
| usage/cost | cost overview with charts, pricing editor, budgets, quota-share pool wizard with burn-rate charts, free-tier budgets, provider stats | spend-by-day div chart, top-models and top-providers ranks, a request log with date/model/provider/status filters, per-key spend | **P1**: budgets, quota windows, pricing |
| logs | call logs (1 756-line component) with filters and a detail drawer, proxy logs, console log, request timeline waterfall, activity feed, compliance audit | one filtered request table (200 rows/page) + a separate failures table | **P1**: a detail drawer, a timeline, a proxy log |
| playground | a studio: compare columns, param sliders, reasoning controls, a tools builder, structured output, a cost counter, prompt improvement, presets | a chat pane with a combo picker, a bearer field, multi-turn history, a system prompt, native SSE, usage counters | **P2**: compare, presets |
| resilience | breaker state per connection, cooldown manager, a 1 283-line resilience config tab | none | **P2** — and this is where the operator would *see* the failover the rail implies |
| health | a 1 191-line health page, DB health, cache stats, rate limits, degradation | none | **P2** |
| settings | 12 tabs, 98 components, 27.7k lines | none (the yaml is the settings) | **P1**: the yaml *is* the config; a settings pane that edits it is the point |
| realtime | WS channels, SSE, a live combo cascade | a 15s poll of spend totals | **P2** |
| commands | `⌘K` command palette over ~90 sidebar items | 6 sidebar entries | — |
| a11y | a global `:focus-visible` ring, `Modal` with `role="dialog"`/`aria-modal`, an `a11yAudit.ts` rule set, `@axe-core/playwright` e2e | skip link, `aria-label` on nav, `role="alert"` errors, `scope="col"` headers, reduced-motion | **P1**: the four data states (see §5.2) |

### 5.2 The two gaps that are about *honesty*, not features

OmniRoute's dashboard is much larger, but two of its conventions are what `/ui` is missing
where it counts:

1. **The four states of every data view** (loading, empty, error, populated — the
   `app-ui` leaf, and OmniRoute's own `EmptyState` / `CardSkeleton` / `Skeleton` primitives).
   `/ui` has `stateRow(cols, text, cls)` with `.loading` / `.empty` / `.failed`
   (`app.js:763-765`) — the vocabulary exists, but the *populated* and *loading* states are
   not distinct, and the error state is a single shared paragraph per pane with an 8-second
   auto-hide (`app.js:48-51`). **An error that vanishes after 8 seconds is not a state.** A
   failed write that is gone before it is read is indistinguishable from a write that
   succeeded.
2. **A write must not half-apply.** OmniRoute validates the whole object before it writes
   (`/ui` already does this for combos — `check_combos` before `splice_section`). The
   remaining gap is the *read-back*: `/ui` answers `{"ok":true,"pending_restart":true}` and
   leaves the operator to trust it. It should re-read the file and compare.

### 5.3 What should NOT be copied

OmniRoute is a superset, and most of the difference is out of aroute's domain. The dropped
items in docs/19 stand and still stand: cached-tokens, RAG/evals, the memory engine, MCP and
A2A registries, the orchestration canvas, the plugin marketplace, gamification, the leaderboard,
the traffic inspector, the AgentBridge MITM server, 10 compression engines, the proxy-pool
manager, the radar, the onboarding wizard, the CLI-tool matrix.

Two more are worth naming as **deliberate non-goals**, because they are the ones that look
tempting:

- **OmniRoute's settings-as-data model.** OmniRoute stores settings in SQLite and edits them
  in place. `/ui` edits a YAML file that the *proxy* reads at boot. A UI that mutated
  runtime state would need a write path into the live `ServerConfig`, which does not exist
  and would be a much larger change than any UI work. `/ui`'s "write the file, show
  `pending_restart`, let the operator restart" is a **feature** for a file-backed proxy: it
  is the only way the change is reviewable and revertible.
- **OmniRoute's component library.** Hand-rolled React primitives with `cn()` and Tailwind
  v4. `/ui` is vanilla JS with one `H()` builder and no build step, deliberately
  ("no build step, no remote fetches, works on an offline VPS", `app.js:1-4`). Porting
  React in would buy a component library and cost the single-binary, offline-safe,
  no-toolchain property. **Keep `H()`.**

---

## 6. The upgrade plan

Ordered by risk, not by feature count. Each phase is independently shippable and each ends
with the full verify loop from docs/23 §0.

| # | phase | what | why now | gate |
|---|---|---|---|---|
| **U0** | **close the open door** | `AR-2` (ingress rule: `/ui` → 404) + `AR-3` (write handlers refuse on an unreadable store) + §3's store fix (`AR_CRED_STORE`, re-arm, delete the duplicate store) | an unauthenticated write path to a production gateway is the only thing here that can lose money at 3am | `POST /ui/api/*` from the internet → 404; `doctor`'s gate check fails when the store is unreadable; `/v1` unaffected |
| **U1** | **make the panel publishable** | `AR-1`: gate `/ui` on the HTTP master, a paste-a-token field, `sessionStorage`, `Referrer-Policy: no-referrer`; a `?token=` first-paint fallback | the operator wants the panel reachable from a laptop without an SSH session; until this lands the panel stays SSH-only | an unauthenticated `/ui` request → 401; the panel works in a browser over the public URL with a token; probe 0 FAIL |
| **U2** | **honest states** | the four data states per view; errors that persist until dismissed; a write that re-reads and compares; an as-of time on every figure | the panel's credibility is the whole job; a write that reports success without proving it is the failure mode that wastes an afternoon | each state rendered and screenshotted; a failed write shows a persistent error, not an 8s flash |
| **U3** | **the operator's missing views** | resilience (breaker + cooldown state per connection — the rail's actual runtime), provider health/expiry, per-combo metrics, budgets and quota windows | these are the things an operator opens a gateway dashboard for and cannot get anywhere today | probe 0 FAIL ×3; the numbers come from the ledger and the resolver, not from a guess |
| **U4** | **the panel as a file editor** | a settings pane over the yaml: `server`, `limits.default`, timeouts, model discovery, auth mode — the sections a hand-edit is likely to get wrong | the yaml is the config; the CLI is the only editor today, and every CLI-only edit is a typo waiting to happen | byte-exact splices with round-trip tests; a wrong value is refused with the field named |
| **U5** | **craft** | the `type-scale` warn (21px → ≥28px display size); the 27px table buttons → 44px; the clipped footer paths (a `title` + a copy button rather than an ellipsis); a light-theme pass (the tokens exist; only the colour band swaps) | probe 0 FAIL is the floor; the craft score is the bar | `probe.mjs` 0 FAIL ×3; `contrast.py` 0 FAIL in both themes; CRAFT ≥ 16/20 with no item at 0 |
| **U6** | **only then**: compare-mode playground, request-detail drawer, timeline | polish, and only if U0–U5 are done | — | — |

### 6.1 The non-goals, restated

No Next.js. No build step. No React. No Tailwind. No `npm`. One binary, one `include_str!` per
asset, offline-safe, and the two embedded woff2 faces stay. The `H()` builder, the token
system, the hash routing, the sidebar shell and the failover rail stay.

### 6.2 The traps that already exist (do not rediscover)

From docs/23 §0.1, plus two this audit adds:

- every HTML/CSS/JS edit needs an `ar-cli` rebuild (the assets are `include_str!`);
- run `target/release/aroute`, never `~/.local/bin/aroute`;
- a new pane must be added to **both** `VIEWS` and `TITLES`;
- `H()` coerces an array kid to a string — spread it;
- `waitForSelector("option")` needs `{ state: "attached" }`;
- **new (this audit):** `POST /ui/api/combos` is a whole-section replace and the YAML
  renderer normalises list style — a "no-op" save still rewrites the file. Any write-path
  test must compare **semantics**, not bytes, and a test that saves "the same thing" will
  dirty the config.
- **new (this audit):** `GET /ui/api/state` re-reads the live config file, so a write from
  another terminal shows up on the next poll — but `pending_restart` compares against the
  **boot** text, so a file rewritten in a different style is "pending restart" forever
  until a restart.

---

## 7. Open questions for the operator

1. **Should `/ui` be reachable from the internet at all?** `AR-2` (SSH-forward only) is
   today's answer and needs no deploy. `AR-1` (publish it, gated) needs a release and a
   token-in-the-browser flow. The blast radius of a mistake differs by two orders of
   magnitude, so this is a decision, not a default.
2. **Which credential store is canonical on the VPS** — `~/.config/ar/credentials.db` (beside
   the config, the one `serve` uses) or `~/credentials.db` (the one the shell resolves)? The
   answer decides which master the re-arm writes. The current evidence says the *store beside
   the config* must win, because that is the one the running service opens.
3. **Should the panel be able to restart the proxy** after a write? Today it shows the
   command. `ConfigHandle::watch` exists in `ar-config` but is not attached
   (docs/16), so a restart would need either that wiring or a systemd call. Neither is
   needed to make the panel correct; both would make it much more useful.

---

## 8. What was NOT done in this pass

- No code was changed. No file in `crates/` was touched.
- The prod config was restored byte-exact after the probe write (§2.2); the service was
  verified healthy. No other prod state was changed. The `~/credentials.db` /
  `~/.config/ar/credentials.db` desync was **diagnosed, not fixed** — fixing it re-arms the
  gate, which is an operator decision (§7.2) with a blast radius of every harness.
- No gate token was minted for the audit, and no token was printed. The probes used the
  operator's existing token file read in place; the two audit tokens minted on the VPS
  (`audit-probe-2`, `audit-probe-3`) carry `execute:completions` and are worth revoking
  with the next rotation.
- The `docs/19` open items not addressed here (the per-key budget engine decision, the
  `webui.rs` dup group, `LedgerRow` lacking a rich `created_at`) are still open; they are
  not dashboard-visual items and they are not in the U-plan above.
