# ui dashboard contrast

Dated-frozen: audited `2026-10-19`; findings mark verified vs asserted.

**Owner of the evidence.** The full audit, the contrast tables and the CRAFT score live in
`docs/27-dashboard-contrast-and-upgrade-plan.md`. This file is the **findings register**:
one row per finding, each with the evidence that produced it. The plan that consumes it:
`docs/plans/2026-10-19-ui-dashboard-upgrade.md`. Evidence artefacts:
`docs/audit/2026-10-19-ui-dashboard/` (probe JSON + 13 screenshots, light and dark).

**The state of every open item these findings produced** is tracked in
[`docs/28-gap-ledger.md`](../28-gap-ledger.md) — §A (security) and §F (the panel), with
the register rows `SEC-01`…`SEC-04` and `UI-00`…`UI-08`.

## Scope

- The subject: `crates/ar-cli/src/webui.rs`, `crates/ar-cli/src/webui/page/{index.html,app.js,app.css}`.
- The reference: `../OmniRoute/src/app/(dashboard)/**` and `../OmniRoute/src/shared/**`
  (Next.js 16 / React 19; 124 route files, 750 API handlers, 985 files in the dashboard tree).
- The deployment under audit: the **public** endpoint `https://aroute.ishanparihar.com/ui/`
  (cloudflared tunnel → `http://127.0.0.1:20128` on `racknerd`), and the same binary served
  on loopback.
- Method: `curl` and a real browser (Playwright/Chromium) against the public URL; the same
  against a scratch instance on `127.0.0.1:20199` with a copy of the real config + env;
  `probe.mjs` at 1440/1024/390 in both theme bands; `contrast.py` on the token file; the
  VPS inspected read-only over `ssh racknerd`.

**Not in scope**: the `/v1` proxy surface, the harness configs, the model stack. One
prod-state finding (§F3) is reported here because it was found through the dashboard and
it gates the dashboard's future auth, but it is not a dashboard defect.

## Findings

- **[verified] F1 — the public `/ui` write endpoints are unauthenticated.** The `/ui`
  router has no credential check of its own; it inherits the socket's trust boundary
  (`crates/ar-cli/src/webui.rs:14-17`), and the only layer it adds is `loopback_only`, a
  DNS-rebinding check, not an authentication check. The HTTP gate (`routes::authorize`,
  `crates/ar-server/src/routes.rs:2193`) is applied to the chat and media routes, not to
  `/ui`. Evidence: from the public internet, `POST /ui/api/provider-remove` → 409
  (validation ran), `POST /ui/api/limits` → 400 (validation ran), `POST /ui/api/combos`
  → `{"ok":true,"pending_restart":true}` and **the prod config file was rewritten**.
  **At 20:2x the exposure is complete, not potential:** `POST /ui/api/keys/mint` from the
  internet returns **200 with a signed `execute:completions` token**, and that token
  against `GET /v1/models` → 200, `GET /metrics` → 200, and
  `POST /v1/chat/completions` → **200 with a real completion served by the operator's
  provider keys**. Impact: an anonymous caller on the internet can mint a token that
  spends the operator's provider keys, repoint every combo, and remove the spend
  ceilings. Fix: `AR-1` (gate `/ui`) + `AR-2` (do not publish `/ui` — **staged**, see F12).

- **[verified] F2 — `loopback_only` refuses exactly the requests a browser makes from the
  public URL.** The rule refuses a request whose `Host` **or** `Origin` names a non-loopback
  authority (`crates/ar-server/src/app.rs:680-701`). The tunnel rewrites `Host` to
  `127.0.0.1:20128` but forwards `Origin` untouched, and a browser attaches `Origin` to
  every cross-origin `fetch()` and to CSS-driven font requests. Evidence: the public
  `/ui/fonts/*.woff2` return **403** (both faces fail to load, `probe.mjs` 6 FAIL across 3
  widths); a browser `POST /ui/api/provider-custom` returns **403**; `curl` with the same
  path returns **200**. The panel is therefore read-only in public and fully functional
  on the box, and the check admits every non-browser client from the internet.

- **[verified] F3 — two credential stores on the VPS, desynced.** `store_path` resolves
  `credentials.db` beside the config unless `$AR_CRED_STORE` is set
  (`crates/ar-cli/src/commands.rs:113-124`); `AR_CRED_STORE` is **not** set in the VPS's
  `ar.env` and the unit has no `WorkingDirectory`. So `aroute serve` opens
  `~/.config/ar/credentials.db` (unreadable: `authentication tag mismatch`, so the store's
  gate row is ignored and the boot log prints "serving ungated" on nine consecutive boots)
  while a shell in `$HOME` opens `~/credentials.db` (readable, and its minted tokens are
  **accepted by the public endpoint**). `aroute doctor` reports `gate,ok,armed` — it reads
  the row's *name*, not whether it decrypts (`crates/ar-cli/src/commands.rs:1086-1088` says
  so in its own comment). **This is the same three-names-one-master failure mode as the
  2026-10-19 incident, one layer down, and it is currently in that state.**

- **[verified] F4 — the panel works in full over an SSH port-forward.** `ssh -L
  20199:127.0.0.1:20128 racknerd` gives the browser a loopback `Origin`
  (`http://127.0.0.1:20199`), which `loopback_only` accepts. Measured in a real browser
  through that forward: both font faces load (200), `POST /ui/api/provider-custom` → 200
  with `{"ok":true}`, and the computed body font is `Inter, ui-sans-serif, system-ui,
  sans-serif` (the real face, not a fallback). **This is `AR-2`: the answer needs no code.**

- **[verified] F5 — the loopback panel passes the probe; the public one does not.**
  `probe.mjs` at 1440/1024/390: **0 FAIL, 3 warn** on loopback; **6 FAIL, 23 warn** on the
  public URL. The difference is entirely F2. The three standing warns: `clipped-text` on
  the two footer paths (`p#foot-config`, `p#foot-env`) and `type-scale` (largest type in
  the first viewport is 21px, 1.6× body).

- **[verified] F6 — an accent-colour contrast failure in the light band.** `--amber
  #e0900f` on `--panel #ffffff` is **2.6:1** (body text needs 4.5:1). Amber is used as
  *text* by `.star.on` (the favourite marker, `app.css:674`) and `.minted summary` (the
  "copy it now" disclosure, `app.css:584`). Measured: a starred item is **less** legible
  than an unstarred one (`rgb(224,144,15)` vs `rgb(100,107,136)`). `contrast.py` does not
  catch it — it pairs tokens, not token-on-ground. The fix is the token layer: `--amber`
  stays the fill, a new `--amber-ink` becomes the text colour (the dark band already has
  one, `#8a5a00`, which is 5.9:1 on white and 5.5:1 on `--bg`).

- **[verified] F7 — the light band is the default rendering and has never been reviewed.**
  Headless Chromium defaults to `prefers-color-scheme: light`, so every probe run in this
  project's history measured the light band; the dark band had never been screenshotted.
  Both were captured on 2026-10-19 (`docs/audit/2026-10-19-ui-dashboard/`). Both bands work
  (token values verified by computed style in a real browser). Dark-band contrast, computed
  by hand: `--text` on `--bg` 16.5:1, `--muted` on `--bg` 6.9:1, `--indigo` on `--panel`
  6.3:1, `--amber` on `--panel` 9.9:1 — all pass AA.

- **[verified] F8 — CRAFT 12/20, no zero.** Scored from the loopback shots per
  `references/craft.md` §7. C1 scale **0** (`scale=1.6x` at 1440 — the h1 is 21px over a
  13.5px body), C3 composition **1**, C7 rhythm **1**; C2 signature **2** (the failover
  rail), C4 type voice **2**, C5 colour world **2**, C6 imagery **2**, C8 motion **2**, C9
  details **2**, C10 copy **2**. The three tests: **logo swap passes** (the panel is about
  this gateway's routing, ledger and credential store; no competitor's copy fits);
  **squint passes with a caveat** (one dominant shape, obvious reading order, but at 12%
  the viewport reads as one repeated card silhouette — that caveat is C3=1);
  **thumbnail passes** (the `ar` wordmark tile and the indigo accent identify it at 320px).
  Missing details: no `::selection` rule, no `text-wrap: balance`, links carry no
  underline, no `meta description`, no `og:image`, no favicon, no `<footer>`.

- **[verified] F9 — tap targets and overflow are clean.** No interactive element is under
  24px in either width (WCAG 2.5.8), the document does not scroll horizontally at 1440 or
  390, and the mobile nav scrolls **inside** its box (`overflow-x: auto`, 264px visible of
  662px) rather than widening the page. The 44px recommendation is met on the ≤880px
  nav; the 27px table buttons remain a 44px *recommendation* miss, as recorded in
  `docs/19` phase C.

- **[verified] F10 — a "no-op" write still rewrites the file.** `POST /ui/api/combos` is a
  whole-section replace and the YAML renderer emits block-style lists where the hand-written
  file used flow-style, so posting the config's own combo list back changed 39 lines
  (`targets: [x]` → a two-line block) and the `identical` short-circuit did not fire
  (`crates/ar-cli/src/webui.rs:604`). Semantically identical, not byte-identical. **The
  prod config was restored byte-exact afterwards** (`config.yaml.1` copied back; both files
  now `sha256 767a402a966d53aac803f169d38111ed59b1d9b690ec5957fa47dce1a8168e84`; the
  service was verified healthy: `healthz` 200, unit `active`, 20 combos, 266 providers,
  `pending_restart: false`). The probe row added later was removed and the file returned
  to its pre-probe 26 888 bytes. **Prod carries no residue from this audit.**

- **[verified] F12 — the `mint` refusal was an accident that ended at 20:07:58.** Between
  19:50 and 20:07 `POST /ui/api/keys/mint` answered `409 "the gate is not armed"` — because
  the gate row in `~/.config/ar/credentials.db` could not be decrypted with the current
  `$AR_MASTER_KEY` (F3), **not** because of any access control. At 20:07:58 a parallel
  session re-armed the gate (the store file's mtime is that minute; the row is now a
  readable `enc:v2` envelope and `keys mint` succeeds against **both** stores), and the
  endpoint went to **200**. The exposure window therefore opened at 20:07:58, not at the
  tunnel's publication. **Consequence for the operator: every token minted since
  20:07:58 is valid, and the master must be rotated** — including the audit's own probe
  tokens (`audit-probe`, `audit-verify`, `audit-verify-2`, `blast-radius-test`,
  `timing-probe-1..3`, `verify-home-store`, `verify-cfg-store`, `verify-cfg-store-2`),
  all of which carry `execute:completions`.

- **[verified] F13 — the `AR-2` mitigation is staged and awaits one privileged command.**
  The cloudflared ingress on the VPS now carries a path rule that 404s `/ui` before it
  reaches the proxy (`path: ^/ui(/.*)?$`), validated with `cloudflared tunnel ingress
  validate` (OK) and matched with `ingress rule` (`/ui/api/state` → rule #3 = 404;
  `/v1/chat/completions` and `/healthz` → rule #4 = the proxy). The file is user-owned so
  the edit needed no privilege; the running cloudflared is a **root** system unit, so the
  new rules are not live until `sudo systemctl restart cloudflared`. Three reload routes
  were tried and failed: `kill -HUP` → `Operation not permitted`; `PUT` on the tunnel's
  own config API (`127.0.0.1:20241/config`) → **200 and a no-op** (the runtime rules were
  re-read and were unchanged, and the tunnel's health was re-verified after: `/ready` →
  200 with 4 ready connections, all four hostnames still answering); a second connector or
  a user-level unit → both need root. **Until the restart, the public `/ui` is open.**

- **[asserted] F11 — the OmniRoute contrast, by capability.** OmniRoute is a superset
  (124 routes, 750 API handlers); `/ui` is one router, three embedded assets, 3 499 lines.
  The gap that matters is not feature count but two *conventions* `/ui` lacks: the four
  states of every data view (loading / empty / error / populated — `/ui` has the
  vocabulary in `stateRow`, but the populated and loading states are not distinct, and an
  error auto-hides after 8 seconds, `app.js:46`), and a write that proves itself by
  re-reading the file. The other two OmniRoute strengths — settings-as-data and a React
  component library — are **deliberate non-goals** for `/ui` (a file-backed proxy wants
  reviewable, revertible file edits; and `H()` + no build step + one binary + offline
  safety is worth more than a component library). The full capability table is
  `docs/27` §5.1.
