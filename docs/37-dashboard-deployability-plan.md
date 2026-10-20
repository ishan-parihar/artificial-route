# 37 — Dashboard deployability: can the compiled dashboard live on the `aroute` hostname?

Status: **plan** (written 2026-10-20). Owner: this doc. No code was changed to
write it, and no build, test, formatter or linter was run.

Companions: [`16-dashboard-unity-plan.md`](16-dashboard-unity-plan.md) — the
existing unification plan; this doc **extends** it and names where the new
evidence supersedes it. [`27-dashboard-contrast-and-upgrade-plan.md`](27-dashboard-contrast-and-upgrade-plan.md)
— the feature contrast and the `/ui` audit. [`34-spend-cap-audit.md`](34-spend-cap-audit.md)
— the spend-cap surfaces and the operator's "no caps" ruling.
[`35-cap-surface-removal-plan.md`](35-cap-surface-removal-plan.md) — the plan
that removes the write paths, including the `aroute sync` rpm carry this doc's
control **C9** depends on.
[`36-cli-agentic-workflow-plan.md`](36-cli-agentic-workflow-plan.md) — the
CLI's agent-facing gaps; the panel and the CLI are the two surfaces an agent
has, and this doc's recommendation puts more weight on the panel.
[`30-ui-production-grade.md`](30-ui-production-grade.md) — the panel's design
direction. [`14-web-ui-port-plan.md`](14-web-ui-port-plan.md) — why the
dashboard is a vendored artifact.

Evidence base: the two recon reports `/tmp/recon/A4-dashboard.md` (the compiled
dashboard, read from the dist bundle) and `/tmp/recon/A5-panel.md` (the Rust
`/ui` panel). Every code fact below carries a `path:line`; every compiled-bundle
fact carries its chunk name. Anything not directly observed is marked
`[INFERENCE]`.

---

## 1. The question, and the answer in one line

The operator's stated goal (2026-10-20): *"the lack of a web-dashboard surface
area is a limitation; I want to reach a dashboard from the `aroute`
hostname."*

**Answer: not safely, not today, and not as a path mount.** The compiled
dashboard's own gateway tier ships **open by default**
(`REQUIRE_API_KEY` `defaultValue:"false"`, chunk `_0wzure5._.js`), so publishing
it publishes an anonymous, unledgered, unlimited spend surface over the
operator's provider keys — outside aroute's gate, outside aroute's ledger, and
against the operator's 2026-10-20 "no spend budgets" ruling
(`docs/34-spend-cap-audit.md:141-154`).

The recommendation this plan makes is in §8. In short: **the panel is the
dashboard**; the compiled dashboard is a local operator tool until
`docs/16` §6-B lands. If the operator insists on a hostname surface in the
meantime, the only safe shape is a **separate hostname behind Cloudflare
Access** (recon Option 2), never a path mount on the `aroute` hostname.

---

## 2. The two UIs, honestly

| | **the Rust `/ui` panel** | **the compiled Next.js dashboard** |
|---|---|---|
| code | `crates/ar-cli/src/webui.rs` + `crates/ar-cli/src/webui/` | `dashboard/dist` — a compiled Next.js standalone, vendored from OmniRoute |
| routes | **20 routes / 21 handlers** — `webui.rs:319-343`: 20 router entries (6 static + 14 API), `settings` having both GET and POST | | **122 dashboard pages** under `/(dashboard)/dashboard/…`; **885** app routes (`app-path-routes-manifest.json`); **726 API handlers** (the `/api/` entries in `app-path-routes-manifest.json`; `find .build/next/server/app/api -name route.js` → 726); 201 OpenAPI paths |
| where it runs | **inside the proxy process**, merged onto the proxy router (`webui.rs:372` `.merge(proxy)`) — same socket and port `:20128` as `/v1` (`crates/ar-server/src/app.rs:655-708`) | **a separate Node process** on its own port **:20149** (`crates/ar-cli/src/cli.rs:224` `DEFAULT_DASHBOARD_PORT`), spawned by `aroute dashboard` (`crates/ar-cli/src/dashboard.rs:74-135`) |
| its gateway | none — it *is* the proxy's UI | **a second full LLM gateway**: rewrites `/v1/:path*` → `/api/v1/:path*` (chunk `[root-of-the-server]__1xc1lr1._.js`, `_originalRewrites`); `/v1/models` answers 200 on :20149 (`docs/16:17`) |
| store | the live config: `~/.config/ar/config.yaml` + `ar.env` (`$AR_KEY_*`) | its own SQLite at `~/.config/ar/dashboard-data/storage.sqlite` — the directory is `dashboard.rs:25` (`DEFAULT_DATA_SUBDIR = ".config/ar/dashboard-data"`), and the filename is spelled out by `crates/ar-cli/src/commands.rs:2078` (`dir.join("dashboard-data").join("storage.sqlite")`); provider keys encrypted `enc:v1:` (`crates/ar-cli/src/commands.rs:2446-2451`) |
| its client keys | **JWTs signed by the same master the proxy verifies** (`crates/ar-keys/src/token.rs:483-488`; verify at `:378-400`, `:440-457`) — the panel's minted token works against `/v1` | **`sk-<machine>-<id>-<crc>`** (chunk `src_shared_utils_apiKey_ts_0tq07a2._.js`) — validates only against the dashboard's own `api_keys` table; the proxy answers **401** (`crates/ar-server/src/keys.rs:366-384`) |
| writes | **byte-exact yaml/env splices into the live config** (`splice_section` at `webui.rs:685`, `:1332`, `:1348`, `:1606`, `:1785`, `:2144`, `:2279`, `:2345`, `:2365`, `:2390`), with a `pending_restart` banner (`webui.rs:1371`, `:1617`, `:1818`, `:2155`, `:2311`, `:2458`) | edits its own SQLite in place; the proxy only sees it after `aroute sync` (`docs/16:40-53`) |
| runtime deps | **no Node, no npm, no build step, one binary** (`webui.rs:1-13`; `docs/30:36`) | Node ≥ 18 required (`install.sh:443-448`); ~700 MB RSS while serving (`install.sh:519-520`) |
| auth | its own three-state gate, resolved live per request (`crates/ar-cli/src/webui/auth.rs:97-119`; `webui.rs:215-220` → `livegate.rs:191-232`): `Unarmed` (Origin check only), `Armed` (token required), `Broken` (reads allowed, **writes refused** — `webui.rs:650-656` `refuse_if_broken`) | two independent tiers: a **management** tier (session cookie / `management_key` / `manage`-scope key) and a **gateway** tier (`clientApiPolicy`) that is **anonymous by default** (chunk `[root-of-the-server]__1xc1lr1._.js`) |

### 2.1 What the panel lacks

From the recon's capability list (`/tmp/recon/A5-panel.md` §4): no OAuth login
surface (the flow exists only in the CLI and MCP — `crates/ar-mcp/src/lib.rs:164-171`);
no key **revocation** endpoint and no "which key ids exist" view (minted tokens
are stateless JWTs, `crates/ar-keys/src/store.rs:168-215`); no edit of
`weights`/`pool`/`steps`/`compression`/`judge_model` on a combo (round-tripped
only, `page/app.js:408-432`); no edit of env-backed settings (`webui.rs:1272-1285`);
no restart control (the panel shows the command, `page/index.html:47-51`); no
per-provider health/expiry matrix, no key reveal, no cost charts, no WS/SSE
realtime, no command palette (`docs/27:511-526`).

### 2.2 What the dashboard lacks

Everything that makes the panel the operator's window on the **actual proxy**:
the live config, the live resilience tables (`webui.rs:951-988`), the proxy's
own usage ledger with per-combo attribution (`webui.rs:854-856`), the credential
store's health, OAuth session status, provider quota snapshots — none of which
exist in the dashboard's store. The dashboard's own charts show **its own DB**,
which on the VPS does not exist at all (`docs/34:46`). And its gateway bypasses
the proxy entirely: the `sk-` keys are not JWTs, so a dashboard-minted key is
**401 at :20128** (`/tmp/recon/A4-dashboard.md` §2c).

### 2.3 The honest summary

The dashboard is a **product**; the panel is a **control panel**
(`docs/27:505-507`). The dashboard is bigger, prettier, and has a second
gateway. The panel is the only one of the two that touches the thing the
operator actually runs.

---

## 3. The three hard blockers

### B1 — No sanctioned delivery path (the artifact problem)

**Evidence.** `dashboard/dist` is 1.6 GiB (`du -sh`), gitignored
(`.gitignore:39`), 25,866 files under `.build`. It is built from a **sibling
checkout**: `dashboard/rebrand-dist.sh:30` `OMNI_ROOT="${OMNI_ROOT:-$AR_ROOT/../OmniRoute}"`,
`:31-32` copies `$OMNI_ROOT/.build/next/standalone` → `$AR_ROOT/dashboard/dist`.
The release workflow publishes **exactly one binary** plus `SHA256SUMS`
(`.github/workflows/release.yml:169-176` packaging, `:206-217` publishing), and
the installer downloads **only** that binary (`install.sh:565-574`). Copying the
dist by hand is the scp-of-artifacts pattern the project's release ledger
forbids (ledger C1 / memory #226).

**The orchestrator's verification — and how it changes the blocker.**
`install.sh` **already accepts a dashboard tarball**:

- `install.sh:150-160` — `--dashboard-src` requires `--with-dashboard`
  (`:150-152`), and the source must be a `*.tar.gz`/`*.tgz` **or** a directory
  containing `server.js` (`:155-160`).
- `install.sh:437-479` `install_dashboard()` — Node ≥ 18 check (`:443-448`),
  source resolution (`:450-462`), and the copy with an **atomic swap**:
  unpack into `${DASH_DEST}.new` (`:465-471`), assert `server.js` present
  (`:472`), `mv "$DASH_DEST" "${DASH_DEST}.old"` (`:474`), `mv "${DASH_DEST}.new" "$DASH_DEST"` (`:475`), remove `.old` (`:476`).
- `install.sh:481-537` `install_dashboard_unit()` — writes
  `aroute-dashboard.service` with `ExecStart=$TARGET dashboard --port $DASH_PORT`
  (`:512`) and `EnvironmentFile=$ENV_FILE` (`:513`), **not enabled** at install
  time (`:526-530`).
- Node **v22.22.2 is installed on the VPS** (orchestrator measurement).

**So B1 is not "there is no way" — it is "there is no way that is *sanctioned* yet".
** The missing piece is a **release-attached artifact plus installer
verification**, not a capability. §4 specifies it.

### B2 — The dashboard's own auth ships OPEN at the gateway tier

**Evidence, verbatim.** The feature-flag definition in the compiled chunk:

```
{key:"REQUIRE_API_KEY",label:"Require API Key",description:"Require an API key
for all incoming requests",descriptionI18nKey:"featureFlagRequireApiKeyDescription",
category:"security",defaultValue:"false",type:"boolean",requiresRestart:!1,
warningLevel:"caution"}
```
— chunk `_0wzure5._.js` (identical definition in `_12-leig._.js`).

The resolver `isRequireApiKeyEnabled()` = DB override > `process.env.REQUIRE_API_KEY`
> `defaultValue` (chunk `_12-leig._.js`, function `n`/`a`); the boolean parse
accepts `"true"`, `"1"`, `"yes"`.

The gateway policy is `clientApiPolicy` (chunk `[root-of-the-server]__1xc1lr1._.js`,
`"clientApiPolicy",0,{routeClass:"CLIENT_API",…}`): it extracts a
Bearer/`x-api-key`/`x-goog-api-key` credential, validates it against the
dashboard's own `api_keys` table, and **falls through to
`allow({kind:"anonymous",id:"local"})` whenever `REQUIRE_API_KEY` is disabled**.

**Consequence.** Publishing the dashboard publishes `POST /api/v1/chat/completions`
(and `/v1/chat/completions`, `/chat/completions`, `/responses`, `/embeddings`,
`/models` — the `_originalRewrites` set) **anonymously**, dispatched to the
dashboard's own `provider_connections` — i.e. the operator's provider keys, over
the second gateway, **outside aroute's ledger, outside aroute's `limits:`, outside
aroute's gate**. The dashboard's compiled docs say it in the app's own words
(chunk `_1-76pds._.js`): *"Falls through to anonymous only when the effective
REQUIRE_API_KEY feature flag is disabled."*

This is a **spend** surface, not just a read surface: the dashboard holds its own
copy of the provider credentials (`enc:v1:` in `provider_connections.api_key`,
`crates/ar-cli/src/commands.rs:2446-2451`), and `aroute sync` deliberately will
not materialise that ciphertext into `ar.env` (`:2451-2457`) — so the two stores
drift silently. It also re-creates the `limits:` block the operator removed
(`docs/34:151-154`; `crates/ar-cli/src/import/limits.rs:35-38` →
`crates/ar-cli/src/commands.rs:2126-2151`) — but only when a dashboard DB with
active keys carrying `max_requests_per_minute > 0` exists (recon
`CLAIMS_TO_VERIFY` #6).

**The env vars that close it.** The recon's §B2 listed the accepted schema; I
read it directly out of the compiled chunk
`[root-of-the-server]__16g15nu._.js` (the zod object `l`):

```
NODE_ENV, DATA_DIR, JWT_SECRET, API_KEY_SECRET, INITIAL_PASSWORD,
AUTH_COOKIE_SECURE, PRICING_SYNC_ENABLED, OMNIROUTE_DISABLE_BACKGROUND_SERVICES,
CLOUD_URL, NEXT_PUBLIC_CLOUD_URL, OMNIROUTE_PUBLIC_BASE_URL, OMNIROUTE_BASE_URL,
BASE_URL, NEXT_PUBLIC_BASE_URL, CONDUCTOR_HUB_URL, CONDUCTOR_SPOKESPERSON_URL,
CONDUCTOR_HUB_TOKEN, CONDUCTOR_ORCHESTRATOR_TOKEN, OMNIROUTE_PORT, API_PORT,
DASHBOARD_PORT
```

**Two facts I established that the recon marked unknown:**

1. **The schema is NOT strict.** The chunk contains **zero** `.strict()` and zero
   `.passthrough()` calls, and the object is a plain `n.z.object({...})` — so
   **an unknown key does NOT fail `safeParse`**. The recon's warning
   (`CLAIMS_TO_VERIFY` #2) that "an unknown key would fail `safeParse` and
   exit(1)" is **wrong** on the strictness point: `REQUIRE_API_KEY` is **not**
   in the accepted schema, but setting it does **not** trip the validator.
   It is read directly from `process.env` by the feature-flag resolver
   (chunk `_12-leig._.js`), so `REQUIRE_API_KEY=true` in the unit's environment
   **is** honoured.
   **This supersedes recon `CLAIMS_TO_VERIFY` #2** — the empirical confirmation
   step is still worth doing, but it is a smoke test, not a schema question.
2. **`enforceWebRuntimeEnv` really does `process.exit(1)`** — chunk
   `[root-of-the-server]__16g15nu._.js`:
   `if(!t.valid){ … r.error("Fix the environment and restart the server.") … process.exit(1) }`.
   So a **malformed** value for a *known* key kills the dashboard at boot. That
   is the risk the plan must design around: `OMNIROUTE_PUBLIC_BASE_URL` must be
   a valid `http(s)://` URL (the schema's `m` type is
   `z.string().url().refine(starts with http:// or https://)`), and
   `OMNIROUTE_PORT`/`DASHBOARD_PORT` must match `/^\d+$/` and be 1–65535.

**The secret requirements** (chunk `[root-of-the-server]__16g15nu._.js`,
`validateSecrets`): `JWT_SECRET` minLength 32, **not required**;
`API_KEY_SECRET` minLength 16, **required**. Both are auto-generated and
persisted at startup when unset (chunk
`src_instrumentation-node_ts_1c--sr1._.js`: *"JWT_SECRET auto-generated and
persisted (random 64-char secret)"*, *"API_KEY_SECRET auto-generated and
persisted (random 64-char hex secret)"*), but the auto-generation runs in the
**instrumentation hook**, which is *after* the env validation — hence the recon's
open item #3 remains: the ordering on a **fresh** `DATA_DIR` with no DB is not
confirmed by reading. **The plan sets both explicitly** and thereby removes the
dependency on the auto-generation ordering entirely.

**The full env set the plan mandates** (this is the "accepted schema" the
operator must stay inside; anything outside it is simply ignored, but anything
*inside* it with a bad value kills the boot):

| var | value | why |
|---|---|---|
| `DATA_DIR` | `/home/nerd/.config/ar/dashboard-data` | the dashboard's own store; must match `crates/ar-cli/src/dashboard.rs:25` so `aroute sync` finds the same DB (`commands.rs:2078`) |
| `JWT_SECRET` | `openssl rand -base64 48` | signs the dashboard session cookie (30 d, chunk `[root-of-the-server]__1xc1lr1._.js` `mintDashboardSessionToken`) |
| `API_KEY_SECRET` | `openssl rand -hex 32` | HMAC secret for the `sk-` key CRC — **required, minLength 16** |
| `INITIAL_PASSWORD` | a strong operator password | makes `setupComplete` true and forces the management tier to demand auth; without it the panel tier is exempt for bootstrap (chunk `src_18fjw2y._.js`) |
| `AUTH_COOKIE_SECURE` | `true` | the session cookie must not travel over plaintext |
| `OMNIROUTE_PUBLIC_BASE_URL` | `https://<dashboard-host>` | the origin the CSRF/origin check accepts (chunk `[root-of-the-server]__1xc1lr1._.js` `validateBrowserMutationOrigin`) |
| `REQUIRE_API_KEY` | `true` | **closes B2**; read from `process.env` by the feature-flag resolver (chunk `_12-leig._.js`) |
| `OMNIROUTE_TRUST_PROXY` | `none` (default) unless the tunnel is confirmed to forward `x-forwarded-*` | the trusted-forwarded-origin branch of the origin check is enabled only when this is not `"none"` (chunk `[root-of-the-server]__1xc1lr1._.js` `getTrustProxyMode`) — and, per §7, turning it on without confirming the tunnel's header behaviour is exactly the wrong move |

`OMNIROUTE_BASE_PATH` is **not** in the accepted schema (confirmed: zero
occurrences in chunk `[root-of-the-server]__16g15nu._.js`) — consistent with the
recon's finding that the Next `basePath` is baked at **build** time
(`dashboard/dist/server.js:12` `basePath:""`; `.build/next/required-server-files.json:85`
`"basePath": ""`; `NEXT_PUBLIC_OMNIROUTE_BASE_PATH:""`), not read from the
environment at runtime.

### B3 — A second gateway, no routing path, and a fail-open locality check

1. **A second gateway on a second port with no route to it.** `:20149` is a
   separate process; the aroute router has **no** `nest`/`any` route and no
   upstream client (`crates/ar-server/src/app.rs:633-684` builds a fixed router
   ending in `.fallback(routes::not_found)`); a repo-wide grep for
   reverse-proxy capability in `crates/` returns only prose
   (`crates/ar-cli/src/webui.rs:347-348`). The design intent is explicit:
   *"The child is a sibling process, not a reverse-proxied route, so the `/v1`
   proxy and the UI each own their socket and neither can fail the other"*
   (`crates/ar-cli/src/dashboard.rs:7-9`).
2. **The `basePath` is baked empty**, so a path mount needs a **rebuilt dist**
   (`dashboard/dist/server.js:12`); `NEXT_PUBLIC_OMNIROUTE_BASE_PATH` is
   *"inlined at build time"*. The upstream docs (chunk `_1-76pds._.js`) say the
   value "is baked during `docker build`".
3. **The peer-locality fail-open.** The launcher stamps
   `x-omniroute-peer-ip` / `x-omniroute-via-proxy` / `x-omniroute-client-ip`
   (`dashboard/peer-stamp-launcher.cjs:103-120`); `hasProxyHopHeader`
   (`peer-stamp-launcher.cjs:62-75`) sets the `via-proxy` marker **only** when
   the request carries an `x-forwarded-*`, `x-real-ip`, `forwarded` or `via`
   header **and** the TCP peer is a trusted proxy (`peer-stamp-launcher.cjs:55-57`
   `isTrustedProxyPeer` = loopback or private LAN). `classifyStampedPeerLocality`
   (chunk `[root-of-the-server]__1xc1lr1._.js`) returns `"remote"` whenever the
   via-proxy marker is set and the stamped peer is not itself remote.
   **If the tunnel forwards no such header, every public request is classified
   `loopback`**, which opens the LOCAL_ONLY route tier and the `!s` branch of
   `isAuthRequired` (chunk `_08k0o4x._.js`) — a fail-open **no ingress rule can
   see**. Note `cf-connecting-ip` is **not** in the recognised list, and that is
   what cloudflared normally sends.
   `[INFERENCE]` — cloudflared's default ingress behaviour is to add
   `CF-Connecting-IP` and `X-Forwarded-For`; if `X-Forwarded-For` is present the
   marker *would* be set. **This must be measured, not assumed** — see §7.

---

## 4. Resolving B1: how a 1.6 GiB artifact becomes a release asset

This is the concrete engineering B1 requires. It is a **release-pipeline**
change, not a runtime change.

### 4.1 The artifact

```
# on the build machine (which has ../OmniRoute and the Node toolchain)
cd dashboard/dist
tar --numeric-owner --owner=0 --group=0 -czf ../dashboard-dist-v${VERSION}.tar.gz .
sha256sum ../dashboard-dist-v${VERSION}.tar.gz > ../dashboard-dist-v${VERSION}.tar.gz.sha256
```

**Size.** The dist is 1.6 GiB. Measured compression on a representative
subtree: `.build/next/static` is 86 MiB and `tar | gzip -6` yields **26.6 MB**
— a ratio of ~0.31. `[INFERENCE]` Applying a similar ratio to the whole tree
gives a tarball in the **~450–550 MB** range. The dominant term is
`.build/next/server` (1.1 G), which is mostly minified JS and *does* compress;
`node_modules` (284 M) contains binaries (`.node` files) that compress poorly.
The plan must not assume a number: **measure the real tarball and record its
size in the release body** before deciding whether to publish it as one asset or
to split it.

**GitHub release asset limit:** 2 GB per file — a ~500 MB tarball is inside it,
but it makes the release page heavy and the download long. `[INFERENCE]`
Two options, both acceptable: publish one `dashboard-dist-vN.tar.gz` +
`.sha256`, or publish a **split** archive (`tar | split -b 400M`) with a
concatenation step. **Prefer the single file** unless the measurement exceeds
~800 MB.

### 4.2 The release workflow

`.github/workflows/release.yml` gains one job, gated on the tag, that:

1. Checks out the repo **with the sibling OmniRoute checkout present** — or,
   better, takes the dist as a **workflow artifact uploaded by a separate
   "build-dist" workflow** run on the build machine. The latter keeps the
   release workflow free of the OmniRoute dependency and keeps the 1.6 GiB out
   of the git checkout.
2. Creates the tarball as in §4.1 and writes `dashboard-dist-v${VERSION}.tar.gz.sha256`.
3. Attests the tarball with the same provenance mechanism the binary uses
   (`actions/attest-build-provenance`, `.github/workflows/release.yml:201-204`)
   — the checksum alone only proves the bytes match a file published beside them.
4. Publishes both files alongside the binary (`:215-217` `files:` list grows by
   two entries).

### 4.3 The installer's verification step

Today `install.sh` verifies the **binary** and nothing else (`:574` downloads
`SHA256SUMS`; `:582-592` the `sha256sum -c`/`shasum` compare, refusing on
mismatch or on a missing tool; `:594` prints `checksum verified`; `:601-606` the
"dynamically linked" refusal; `:611-617` the optional `gh attestation verify`). **The dashboard tarball needs the same
three-part treatment**, and the plan adds it:

```sh
# NEW: verify the dashboard tarball before it is unpacked
DASH_URL="$BASE/dashboard-dist-v${VERSION}.tar.gz"
curl -fsSL -o "$TMP/dashboard.tar.gz" "$DASH_URL"
curl -fsSL -o "$TMP/dashboard.tar.gz.sha256" "$DASH_URL.sha256"
# sha256sum -c / shasum compare, exactly as the binary does (:582-592)
# refuse: "checksum mismatch for the dashboard tarball — the dashboard was not installed"
```

The refusal must happen **before** `install_dashboard()` unpacks anything, so a
corrupt or substituted tarball never reaches `${DASH_DEST}.new`. The atomic
swap the installer already performs (`install.sh:464-476`) then guarantees that
a failed unpack leaves the previous dashboard intact: the old tree is only moved
to `.old` **after** the new tree is unpacked and asserted to contain `server.js`
(`:472`).

**The installer's existing weakness to close:** `install_dashboard()` today
accepts a local path only; the *download* of the tarball does not exist yet.
The plan's change is to make `install.sh` fetch the release-attached tarball
when `--dashboard-src` is not given, i.e. the same pattern as the binary
(`:565-573` tries two candidate asset names and dies if neither exists).

### 4.4 Native-module portability — the real risk in the tarball

`keytar` ships **no prebuilds**: the installed dist has
`dashboard/dist/node_modules/keytar/build/Release/keytar.node` only — a
**locally compiled, machine-built** artifact (there is no `prebuilds/`
directory). It will not necessarily load on the VPS.

`better-sqlite3`, by contrast, **does** ship prebuilds —
`dashboard/dist/node_modules/better-sqlite3/prebuilds/` contains
`darwin-arm64.node`, `darwin-x64.node`, `linux-arm64.node`,
`linuxmusl-arm64.node`, `linuxmusl-x64.node`, `linux-x64.node`,
`win32-arm64.node`, `win32-x64.node` (verified by listing the directory).

**Therefore the tarball must be built for the TARGET platform**, and the plan
says so explicitly:

- The dist's native modules must be compiled for **linux-x64** on the build
  machine (or the prebuilt `linux-x64.node` used for `better-sqlite3`), **not**
  carried over from a macOS build machine. The prebuilds directory shows the
  upstream project ships both platforms, so the build machine can produce a
  linux-x64 tree without a VPS-side toolchain.
- `keytar` must be **rebuilt for linux-x64** on the build machine
  (`npm rebuild keytar --build-from-source` in the dist) so the packaged
  `keytar.node` is the linux-x64 one. Otherwise the VPS must run
  `npm rebuild keytar` — which needs a compiler and `libsecret` headers on the
  box, a new VPS dependency.
- **Verification before the swap**: boot the extracted tree on the target and
  require the dashboard to start. `[INFERENCE]` The cheapest proof is the
  installer's own pattern for the binary (`install.sh:632-640`: run
  `--version` on the staged artifact *before* it replaces the working one) —
  the analogous check for the dashboard is to start the extracted tree on a
  scratch port and require a 200 from its health route. `[INFERENCE]` the exact
  health path must be confirmed empirically; the recon's Option 1 verification
  uses `http://127.0.0.1:20149/healthz`, which is **unconfirmed** — flag it.

**The order of operations that makes B1 safe:**

1. Build the linux-x64 dist on the build machine; rebuild `keytar` there.
2. Tarball + sha256 + attestation.
3. Publish on the release.
4. Installer downloads, **verifies the checksum**, unpacks to `${DASH_DEST}.new`,
   asserts `server.js`, **boots the new tree and checks it answers**, and only
   then does `mv .old` / `mv .new` (`install.sh:472-476`).
5. If step 4's boot check fails, the installer **aborts with the previous
   dashboard still in place** and says so.

### 4.5 What B1 does NOT buy

A delivery path does not make publishing safe. B2 and B3 are untouched by
§4. **Delivering the dashboard is a precondition for the *local* tool and for
the subdomain option; it is not authorisation to publish it.**

---

## 5. The serving options (recon's ranking, intact)

The recon's five options, ranked lowest risk first. The ranking is preserved;
each option gets the orchestrator's added note.

### Option 1 — Publish nothing; SSH-forward the dashboard

`ssh -L 20149:127.0.0.1:20149 racknerd`.
*Changes:* none to the code; one operator habit.
*Blast radius:* zero — the dashboard's own loopback bind
(`crates/ar-cli/src/dashboard.rs:111` `.env("HOSTNAME","127.0.0.1")`) is the
trust boundary, exactly as `docs/16:103-108` P4 assumes.
*Could break:* nothing; the SSH port-forward is the same trust model the
current `/ui` access uses.
*Verify:* `curl -s http://127.0.0.1:20149/healthz` through the tunnel (the
health path itself is **[INFERENCE]**, to be confirmed); `ss -ltn | grep 20149`
shows `127.0.0.1` only.
**Added note:** this option still requires B1 — the dashboard must be on the VPS
to be forwarded to. §4 is its prerequisite.

### Option 2 — Separate hostname + Cloudflare Access, no path mount

`dashboard.ishanparihar.com` → `http://127.0.0.1:20149`, wrapped in a
Cloudflare Access policy, plus `REQUIRE_API_KEY=true` and
`OMNIROUTE_PUBLIC_BASE_URL=https://dashboard.ishanparihar.com` in the
dashboard unit's environment.
*Changes:* DNS record; one ingress rule; an Access app; the env vars in §B2's
table.
*Blast radius:* confined to the dashboard hostname — `/v1` on the aroute
hostname is untouched, so a mistake here cannot expose the proxy.
*Could break:* (i) needs the dashboard installed on the VPS → **B1**;
(ii) `REQUIRE_API_KEY=true` breaks the dashboard's own UI playground unless its
own keys are used; (iii) the **`via-proxy` locality question** (§7) must be
settled first; (iv) `httpHostHeader` must be **omitted** for this rule so the
app sees the public host, or the browser-write origin check 403s.
*Verify:* `curl -I https://dashboard.ishanparihar.com/` from outside → 302 to
the Access login; an anonymous `POST /api/v1/chat/completions` → **401**; the
UI's own API calls still work from a browser.

### Option 3 — Path mount on the aroute hostname (`/dash`), same origin

*Changes:* rebuild the dist with `OMNIROUTE_BASE_PATH=/dash` (baked at build
time, `dashboard/dist/server.js:12`) — a **build-machine artifact change**, so
it is a *different* tarball from §4's, and the release must carry both or the
option must be re-cut per release; a new ingress rule **before** the catch-all;
`OMNIROUTE_PUBLIC_BASE_URL=https://aroute.ishanparihar.com/dash`.
*Blast radius:* **the highest of the three** — the dashboard's second gateway
would live on the same origin as `/v1`, so one Access/ingress mistake exposes
the proxy too.
*Could break:* everything in Option 2 plus the base-path rebuild, plus a
`/dash` vs `/v1` path collision at the edge unless the dashboard is mounted at
a prefix, plus the same loopback-locality question.
*Verify:* `/dash/_next/*` asset URLs resolve; the login page renders; a browser
write succeeds; an anonymous gateway POST 401s.

### Option 4 — Build a reverse-proxy mount into `ar-server` (`/ui/dashboard` or `/dash`)

*Changes:* new code in `crates/ar-server/src/app.rs` — an `any` route plus a
`reqwest`/hyper client to `127.0.0.1:20149`, with path-prefix rewrite and
header hygiene (`reqwest` is already a dependency, `crates/ar-server/Cargo.toml:42`).
*Blast radius:* the **proxy** gains a failure domain — a hung dashboard child
could stall proxy requests, which `crates/ar-cli/src/dashboard.rs:7-9`
explicitly designed against.
*Could break:* the proxy's 120 s request timeout and 50 MiB body limit
(`crates/ar-server/src/app.rs:71`, `crates/ar-server/src/config.rs`) now apply
to a dashboard asset stream; the dashboard's Next streaming responses need care.
*Verify:* an integration test that the mount proxies `/dash/_next/*` and that
`/v1/chat/completions` is unaffected when the dashboard is down.
**Only worth it if the operator insists on one origin AND B1 is solved.**

### Option 5 — Do nothing to the dashboard; finish `docs/16` §6-B

*Changes:* serve a thin UI from the axum router; delete `dashboard/`, the
rebrand pipeline, `sync`, and the second gateway.
*Blast radius:* large but **eliminates** the class rather than mitigating it —
no second auth surface, no second credential store, no second gateway, no
`enc:v1:` problem, no 1.6 GiB artifact to deliver.
*Could break:* the operator loses the parent's UI polish (drag-drop ordering,
in-browser OAuth — the CLI auth flow covers it — usage charts, catalog
browsing) (`docs/16:188-189`); ~2–5 focused days per `docs/16:181`.
*Verify:* the migration already exists — `aroute import --combos storage.sqlite`
plus the sync env materialisation (`docs/16:185-186`), run once, then archive
the DB. `aroute import --from omniroute --combos <path>` is the flag surface
(`crates/ar-cli/src/cli.rs:590-610`; `crates/ar-cli/src/import/omniroute.rs:148-151`),
and `aroute sync --db ~/.config/ar/dashboard-data/storage.sqlite`
(`crates/ar-cli/src/cli.rs:170`) is the env-materialisation path.

### 5.1 The tension, stated honestly

The operator wants the dashboard reachable from the `aroute` hostname. The recon
found the path mount (`/dash`) requires a **rebuilt dist** with
`OMNIROUTE_BASE_PATH=/dash` and puts a **second gateway on the same origin as
`/v1`** — so one mistake exposes both — while a subdomain needs **DNS plus a
Cloudflare Access app** and a **separate hostname**.

These are not equally good ways to satisfy the same wish:

- **`/dash` on the aroute hostname** satisfies the wish literally, but it is the
  one option whose blast radius includes `/v1`. It also needs a *second*
  artifact (a base-path rebuild) that §4's release pipeline would have to carry,
  doubling the release's asset weight and creating a permanent "which dist is
  this?" question.
- **A subdomain** satisfies the wish in substance ("I can reach a dashboard
  from a browser on my domain") without co-hosting the second gateway with the
  proxy. It costs DNS and an Access app — both one-time, operator-driven
  actions, not code.

**Recommendation: Option 2 (subdomain + Access), and never Option 3.** If the
operator's actual requirement is literally "the `aroute` hostname", say so
plainly to them and let them choose Option 3 **with** the §6 control set in
force — but the plan's default remains Option 2.

---

## 6. The security control set — what must be TRUE before the dashboard is reachable from the aroute hostname

This is the section the operator acts on. **Every row is a precondition.** If
any row is FALSE, the dashboard does not get an ingress rule.

| # | Control | How to verify it |
|---|---|---|
| C1 | **`REQUIRE_API_KEY=true` is in effect** for the dashboard process. The gateway tier refuses an unauthenticated completion. | `curl -s -o /dev/null -w '%{http_code}' -X POST http://127.0.0.1:20149/api/v1/chat/completions -H 'content-type: application/json' -d '{"model":"x","messages":[]}'` → **401** (not 200). Repeat against the public hostname. Also read the effective value: the flag resolver (chunk `_12-leig._.js`) reads DB override **first**, so confirm no DB row overrides it. |
| C2 | **`INITIAL_PASSWORD` is set** and `setupComplete` is true, so the management tier demands auth for a remote peer. | `curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:20149/api/settings/require-login` → not an unauthenticated 200. Confirm the DB row (`settings` table, `requireLogin` = true). |
| C3 | **The default management password is not in force.** The well-known default is `"CHANGEME"` (chunk `_1k-gd0d._.js`) and is refused off-loopback (chunk `_0ajtddf._.js`). | Log in from a **non-loopback** address and confirm a 403 with the "still set to the well-known default" text — i.e. the check fires. Then change it and confirm login succeeds. |
| C4 | **The tunnel forwards an `x-forwarded-*` / `x-real-ip` / `forwarded` / `via` header**, so the dashboard classifies internet traffic as `remote` and the LOCAL_ONLY tier is closed. | `curl -sv https://<host>/api/health/ping 2>&1 | grep -i 'x-forwarded\|x-real-ip\|cf-connecting'` at the **origin** — read the headers the origin receives. Then confirm the dashboard's own view: the `via-proxy` marker must be set (`peer-stamp-launcher.cjs:113-114`). **Until this is measured, no ingress rule exists.** |
| C5 | **`OMNIROUTE_TRUST_PROXY` is `none`** (the default), and `OMNIROUTE_PUBLIC_BASE_URL` is set to the dashboard's **public** origin so the browser-origin check passes without trusting forwarded headers. | `grep OMNIROUTE_TRUST_PROXY` in the unit's environment → `none`; `grep OMNIROUTE_PUBLIC_BASE_URL` → the exact public origin. A browser write from the public origin → **not 403 `INVALID_ORIGIN`**. |
| C6 | **The dashboard's credential store is NOT the provider keys.** The dashboard must hold no `provider_connections.api_key` rows, or must hold them under a `STORAGE_ENCRYPTION_KEY` the operator controls and rotates. Otherwise publishing it publishes a second copy of the provider keys (`enc:v1:`). | `sqlite3 ~/.config/ar/dashboard-data/storage.sqlite "select count(*) from provider_connections where api_key like 'enc:v1:%';"` → **0**, or the operator accepts and owns that copy explicitly. |
| C7 | **The dashboard's gateway cannot spend the operator's money anonymously.** If C1 is FALSE, the only remaining control is that the dashboard holds no working provider credentials (C6) and/or that the ingress rule blocks `/api/v1/*`, `/v1/*`, `/chat/*`, `/responses/*`, `/embeddings/*` outright. | `curl` each rewritten path from outside → **404** at the edge (ingress-blocked) or **401** (auth-required). Note `public/openapi.yaml` lists 201 paths; the four rewritten families are the spend surface. |
| C8 | **The dashboard's own `sk-` keys are worthless against the proxy.** They must never be pasted into a harness config as a `/v1` bearer. | `curl -s -o /dev/null -w '%{http_code}' https://aroute.ishanparihar.com/v1/models -H 'Authorization: Bearer sk-…'` → **401** (expected, and load-bearing). |
| C9 | **No `limits:` block is re-created.** `aroute sync` carries the dashboard's `api_keys.max_requests_per_minute` into `limits:` (`crates/ar-cli/src/import/limits.rs:35-38` → `crates/ar-cli/src/commands.rs:2126-2151`). | After any `aroute sync`, `grep -A2 '^limits:' ~/.config/ar/config.yaml` → **absent**. The condition is precise: it fires only if the dashboard DB exists **and** has active keys with `max_requests_per_minute > 0` (recon `CLAIMS_TO_VERIFY` #6). |
| C10 | **The dashboard unit does not inherit the proxy's master keys.** `install.sh:513` writes `EnvironmentFile=$ENV_FILE` — aroute's own `ar.env`, which carries `AR_HTTP_MASTER_KEY` and `AR_MASTER_KEY`. The Node child does not need them; it should not have them. | `systemctl --user show aroute-dashboard -p Environment` → the master keys are **absent**. This requires a separate environment file for the dashboard unit — a real installer change. |
| C11 | **The ingress rule for `/ui` (the panel) and the rule for the dashboard do not overlap**, and the dashboard rule is ordered **before** the aroute catch-all. | Read `~/.cloudflared/hermes-config.yml`; confirm the rule order; then `sudo systemctl restart cloudflared` (ingress is read at **startup** — `docs/RUNBOOK.md:40-41`) and re-probe. |
| C12 | **The dashboard is bound to loopback only.** | `ss -ltn | grep 20149` → `127.0.0.1:20149` only. Enforced by `crates/ar-cli/src/dashboard.rs:111`; confirm it is not overridden by an env in the unit. |

---

## 7. Open items — the first one decides the whole plan

### O1 — **Does cloudflared forward an `x-forwarded-*` header to the dashboard?** (FIRST — this decides whether publishing is safe at all)

The recon called this *"the single most dangerous unknown in this report"*, and
it is. The mechanism, from the source:

- `peer-stamp-launcher.cjs:62-75` `hasProxyHopHeader` recognises header names
  starting with `x-forwarded-`, plus `x-real-ip`, `forwarded`, `via`.
- `peer-stamp-launcher.cjs:55-57` `isTrustedProxyPeer(ip)` = loopback **or**
  private LAN address — the cloudflared hop from the VPS's own loopback bind
  qualifies.
- `peer-stamp-launcher.cjs:113-114` sets the `via-proxy` marker only when
  `hasProxyHopHeader(headers) && isTrustedProxyPeer(ip)`.
- `classifyStampedPeerLocality` (chunk `[root-of-the-server]__1xc1lr1._.js`)
  returns `"remote"` when the marker is set and the stamped peer is not itself
  remote.

**If the marker is never set, every public request is classified `loopback`**,
which opens the LOCAL_ONLY route tier and the `!s` branch of `isAuthRequired`
(chunk `_08k0o4x._.js`) — and **no ingress rule can see that**. `cf-connecting-ip`
is not in the recognised list, and it is what cloudflared normally sends.

**How to settle it:** read `~/.cloudflared/hermes-config.yml` on the VPS
(ssh `racknerd`), and `curl -sv` through the tunnel to a route that echoes
the request headers it receives.

**Partially settled, 2026-10-20, by the orchestrator.** The ingress config was
read (`~/.cloudflared/hermes-config.yml`): the `aroute.ishanparihar.com` rule
sets **only** `originRequest.httpHostHeader: 127.0.0.1:20128` and **no**
`originRequest` header injection, and the other three rules set only
`noTLSVerify` / `http2Origin`. Cloudflared is **2026.7.3**. So the tunnel
itself adds no proxy-hop header of its own — the header must come from
Cloudflare's edge.

[INFERENCE] The prior is **strongly favourable**: Cloudflare's edge adds `X-Forwarded-For`
(and `CF-Connecting-IP`) to every proxied request — the vendor's own
documentation, "Cloudflare HTTP headers" (`developers.cloudflare.com/fundamentals/reference/http-request-headers/`),
lists both under *Request headers* as headers Cloudflare "adds" to the origin
request. `hasProxyHopHeader` recognises any header whose name starts with
`x-forwarded-` (`peer-stamp-launcher.cjs:62-75`), so `X-Forwarded-For` alone
sets the `via-proxy` marker and the dashboard classifies the peer as
**remote**. The trap the recon named is narrower than it looked: the header
that matters is `X-Forwarded-For` (which Cloudflare always sends), **not**
`CF-Connecting-IP` (which the launcher does not recognise).

**It is nevertheless unmeasured, and the measurement needs the operator.**
The orchestrator could not complete it: the tunnel process is **root-owned**
(`ps` → `root … cloudflared --no-autoupdate tunnel --config …/hermes-config.yml
run`; `kill` → "Operation not permitted"), and `sudo` requires a password.
The proxy does not log request headers, the nginx access log is root-only, and
the cloudflared journal contains no header detail at the default log level.
A temporary ingress rule pointing at a header-echo origin was prepared and
the config was restored **byte-identical** (verified by `diff`) after the
attempt — the probe rule was never live.

**The one command that settles it** (needs `sudo systemctl restart cloudflared`,
because ingress is read at startup):

```bash
# 1. a header-echo origin on the VPS
python3 -c 'import http.server,json
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(s):
        b=json.dumps({"h":dict(s.headers)}).encode()
        s.send_response(200); s.send_header("content-length",str(len(b))); s.end_headers(); s.wfile.write(b)
    def log_message(s,*a): pass
http.server.HTTPServer(("127.0.0.1",20399),H).serve_forever()' &
# 2. a temporary ingress rule for  ^/__o1probe$  ->  http://127.0.0.1:20399
sudo systemctl restart cloudflared
# 3. the measurement
curl -s https://aroute.ishanparihar.com/__o1probe | python3 -m json.tool
# 4. restore the ingress config and restart again
```

**The answer that matters:** does the echoed header map contain `X-Forwarded-For`
(yes ⇒ the `via-proxy` marker is set ⇒ the peer is classified **remote** ⇒
the LOCAL_ONLY tier stays closed)? If the map contains only `CF-Connecting-IP`
and no `x-forwarded-*`, the marker is never set and **the dashboard must not
be published at all** until the launcher is patched to recognise
`cf-connecting-ip` (a one-line change at `peer-stamp-launcher.cjs:62-75`).

headers, reading the headers **the origin receives**. Then boot the dashboard
once and confirm the `via-proxy` marker is set.

**Until O1 is answered, the answer to the operator's question is "not yet".**

### O2 — Is `REQUIRE_API_KEY` honoured in practice?

The recon could not confirm it was in the accepted env schema. **I resolved the
schema question:** the zod object in chunk `[root-of-the-server]__16g15nu._.js`
is not `.strict()` and has no `.passthrough()`, so unknown keys are **ignored,
not fatal**; `REQUIRE_API_KEY` is read straight from `process.env` by the
feature-flag resolver (chunk `_12-leig._.js`) and is **not** in the schema.
**Still to do empirically:** boot the dashboard with `REQUIRE_API_KEY=true` and
confirm (a) the process does **not** exit(1) — `enforceWebRuntimeEnv` only
validates the *known* keys, chunk `[root-of-the-server]__16g15nu._.js` — and
(b) an anonymous gateway POST now 401s.

### O3 — Do the `docs/16` numbers still match this dist?

`docs/16:165` says "726 `route.js` handlers across 105 namespaces" and 149 page
routes; the recon measured **885 app routes / 122 dashboard pages / 726 API
handlers / 201 OpenAPI paths** on this dist. `docs/16`'s numbers came from
upstream source, not a built tree. The §6-A rejection must be restated in the
**measured** terms — it is stronger, not weaker. This plan uses the measured
numbers throughout.

### O4 — The legacy `sk-omni-` row

`docs/16` (memory #69) mentions "one legacy `sk-omni-`" key in the dashboard's
`api_keys` table. The recon found only the `sk_omniroute` → `sk_artificial`
**placeholder sentinel** (chunk `_00bwjsj._.js`) and the
`sk-<machine>-<id>-<crc>` mint format (chunk
`src_shared_utils_apiKey_ts_0tq07a2._.js`); the legacy row was not located in
the bundle. Confirm against a live `storage.sqlite` before relying on it.

### O5 — The `keytar` rebuild

`keytar` has only a locally compiled `build/Release/keytar.node` and **no
prebuilds directory**, while `better-sqlite3` ships `linux-x64` and
`linuxmusl-x64` prebuilds. The tarball must be built for linux-x64 (§4.4), or
the VPS must gain a compiler plus `libsecret` headers. Unresolved until the
tarball is built and booted on the target.

### O6 — The `docs/16` §6-B estimate

"2–5 focused days" and "10–15 endpoints" are the author's, not measured. But
the panel **already exposes 14 API routes / 15 handlers** (`webui.rs:329-342` — state,
usage, log, models, resilience, health, budgets, settings, combos, provider-key,
provider-custom, provider-remove, keys/mint, limits) covering state/usage/log/
models/resilience/health/settings/combos/providers/keys/limits. **The thin
dashboard's backend is largely already there.** `[INFERENCE]` The remaining
§6-B work is mostly *presentation* (a richer page) plus the endpoints the panel
genuinely lacks (OAuth login, key revoke, key list). This materially shortens
§6-B's scope.

### O7 — The dashboard's health endpoint path

The recon's Option 1 verification uses `http://127.0.0.1:20149/healthz`. The
dashboard's public route set (chunk `_08k0o4x._.js`) includes `/api/health/ping`,
`/api/monitoring/health`, `/api/health` (GET/HEAD/OPTIONS) — `/healthz` is
**not** among them. The verification command must use a path that actually
exists; `/api/health/ping` is the likely one. `[INFERENCE]`, to be confirmed.

---

## 8. The two-panels decision — decisive position

**Position: converge on the panel. Do NOT keep maintaining two UIs. Treat the
compiled dashboard as a temporary local operator tool, and do not publish it to
the aroute hostname.**

### 8.1 What `docs/16` already decided, and what this plan does with it

`docs/16:164-171` **explicitly REJECTS** option A (reimplement the dashboard's
API in Rust): 726 route handlers across 105 namespaces plus 149 page routes, a
moving target that changes with every upstream pull. `docs/16:173-189` names
option B — an aroute-native thin dashboard served from the axum router, deleting
the vendored stack entirely — as *"the real integration"*, estimating 2–5
focused days and listing what it deletes: `dashboard/` (1.6 GiB), the rebrand
pipeline, the `sync` verb, the second gateway on :20149, the ~700 MB Node child,
the `enc:v1:` problem, and the update-dist treadmill.

**The new evidence strengthens §6-B and does not contradict it:**

1. The §6-A rejection is now **measured, not argued**: 885 routes, 122 dashboard
   pages, 726 API handlers, 201 OpenAPI paths, 198 migrations, and an auth
   model (peer-stamp, bootstrap token, CSRF/origin, `enc:v1:`) that is not
   implementable in a week. §6-A is dead.
2. The §6-B case is now **stronger than when it was written**, because the
   compiled bundle reveals that the second gateway is **anonymous by default**
   (`REQUIRE_API_KEY` `defaultValue:"false"`, chunk `_0wzure5._.js`). The
   problem §6-B deletes is not "a second gateway to be labelled" (§3-P4,
   `docs/16:103-108`) — it is **an unauthenticated spend surface over the
   operator's provider keys, outside aroute's gate and ledger**, which the
   operator's 2026-10-20 ruling (`docs/34:141-154`) explicitly rejects.
3. §6-B's scope is now **smaller than estimated**: the panel already implements
   14 of the endpoint routes §6-B would need (`webui.rs:329-342`; 15 handlers
   counting `settings` GET and POST separately).

**Where this plan supersedes `docs/16`:** §3-P4 ("decide the second gateway —
default: label, don't neuter", `docs/16:103-108`) is **superseded**. Labelling
a gateway that admits anonymous completions is not a decision; the only options
are *close it* or *delete it*. This plan's answer: close it with
`REQUIRE_API_KEY=true` **if and only if** the dashboard stays installed, and
delete it when §6-B lands.

**Where this plan extends `docs/16`:** §7 (B1) is new — `docs/16` assumed the
dashboard could be installed on a machine that already had the dist. §4
specifies the release-attached tarball that makes the delivery *sanctioned*.

### 8.2 The reasoning, briefly

- **One gateway is a correctness property, not a preference.** Two gateways over
  one set of provider keys means two spend paths, two ledgers, two auth models.
  aroute's ledger, gate and spend accounting are the product's core value; the
  dashboard bypasses all three.
- **The dashboard cannot be the operator's window on the proxy.** It reads a
  different store; its charts show traffic that never happened; its keys do not
  work at `/v1`. Publishing it gives the operator a *second* view of a *different*
  system and the impression of control over the first.
- **The panel is already the surface the operator needs.** 21 routes, live
  config edits, tokens the proxy accepts, the real playground posting to the
  real `/v1` on the same origin (`page/app.js:1688`), the live resilience
  tables, the proxy's own ledger with per-combo attribution (`webui.rs:854-856`),
  and a three-state gate that resolves **live** rather than at boot
  (`webui.rs:215-220` → `livegate.rs:191-232`).
- **The panel is already deployed and reachable** — over SSH-forward, which is
  the *only* access path the recon's Option 1 needs and the only one that
  carries zero blast radius.

### 8.3 The migration consequence

If the operator agrees to converge:

1. **Migrate the config out of the dashboard's DB, once:**
   `aroute import --from omniroute --combos ~/.config/ar/dashboard-data/storage.sqlite`
   (`crates/ar-cli/src/cli.rs:590-610`; `crates/ar-cli/src/import/omniroute.rs:148-151`),
   then `aroute sync --db ~/.config/ar/dashboard-data/storage.sqlite` for the env
   materialisation (`crates/ar-cli/src/cli.rs:170`; `commands.rs:2073-2157`).
   Then **archive** the DB (`docs/16:185-186`).
2. **Do not import the dashboard's `api_keys` as aroute client keys.** That is
   §3-P3 (`docs/16:87-92`), still open, and it is *not* required for the
   convergence — aroute's own `aroute keys mint` already issues tokens the proxy
   accepts.
3. **Fill the panel's real gaps** — the ones the recon named: OAuth login
   (recon Option 4 of the panel report), key revoke / key list (recon Option 5),
   and the §6-B presentation work.
4. **Delete** `dashboard/`, `rebrand-dist.sh`, `patch-artificial.sh`,
   `scrub-sponsors.cjs`, `peer-stamp-launcher.cjs`, `update-dist.sh`,
   `enable-onnx.sh`, the `sync` verb, and the `aroute-dashboard.service` unit.
5. **Do not run `aroute sync` after the deletion** — it would recreate the
   `limits:` block from the archived DB (C9).

### 8.4 What this plan recommends the operator do **now**

1. **Do not add an ingress rule for the dashboard.** Not on `/dash`, not on a
   subdomain, until O1 is answered and the §6 control set is green.
2. **Do publish the `/ui` panel** if the operator wants a browser surface on the
   `aroute` hostname — the panel is the safe surface. Its gate is
   `Armed` on the VPS (measured: an anonymous `POST /ui/api/keys/mint` without a
   token → **401** `credential_rejected`,
   `crates/ar-cli/src/webui/auth.rs:237-277`), and the tunnel's
   `path: ^/ui(/.*)?$ -> 404` block is the only thing standing in the way
   (removing it plus `sudo systemctl restart cloudflared` — the admin API and
   `kill -HUP` are no-ops). **Caveat, measured:** `?token=` over a public URL
   puts the token in the tunnel's access log and in browser history before the
   panel strips it (`page/app.js:60-69`); prefer pasting the token into the
   panel's credential field (`page/index.html:55-65`).
3. **Keep the dashboard as a local tool** over SSH-forward (recon Option 1) —
   which needs B1 solved first (§4).
4. **Answer O1** (one `curl -sv` and one file read) — it is the cheapest
   measurement in this document and it gates everything else.

---

## 9. Test and verification plan

### 9.1 Per-control verification (offline / on the box, before any ingress rule)

| control | check | pass |
|---|---|---|
| C1 | anonymous `POST http://127.0.0.1:20149/api/v1/chat/completions` | `401` |
| C1 | read the effective flag: no DB override of `REQUIRE_API_KEY` (chunk `_12-leig._.js` reads the DB first) | DB has no override row |
| C2 | `GET /api/settings/require-login` unauthenticated | not an unauthenticated 200 |
| C3 | login attempt from a non-loopback address with the default password | 403 with the "well-known default" message (chunk `_0ajtddf._.js`) |
| C4 | `curl -sv` through the tunnel; read the origin-side headers | an `x-forwarded-*` / `x-real-ip` / `forwarded` / `via` header is present |
| C4 | boot the dashboard, read the stamped headers server-side | `via-proxy` marker = `1` |
| C5 | unit environment: `OMNIROUTE_TRUST_PROXY=none`, `OMNIROUTE_PUBLIC_BASE_URL=https://<host>` | both present |
| C5 | a browser write from the public origin | not `403 INVALID_ORIGIN` |
| C6 | `sqlite3 storage.sqlite "select count(*) from provider_connections where api_key like 'enc:v1:%';"` | `0`, or explicitly owned |
| C7 | anonymous POST to each of `/api/v1/chat/completions`, `/v1/chat/completions`, `/chat/completions`, `/responses`, `/embeddings` | `401` (or `404` at the edge) |
| C8 | `GET https://aroute.ishanparihar.com/v1/models` with a dashboard `sk-` bearer | `401` |
| C9 | `grep -A2 '^limits:' ~/.config/ar/config.yaml` after a sync | absent |
| C10 | `systemctl --user show aroute-dashboard -p Environment` | no `AR_HTTP_MASTER_KEY`, no `AR_MASTER_KEY` |
| C11 | read `~/.cloudflared/hermes-config.yml`; rule order; restart cloudflared; re-probe | the dashboard path reaches :20149, `/ui` still 404s, `/v1` still 401s |
| C12 | `ss -ltn \| grep 20149` | `127.0.0.1:20149` only |

### 9.2 Per-artifact verification (the §4 delivery path)

1. `sha256sum -c dashboard-dist-vN.tar.gz.sha256` on the build machine.
2. On the VPS, after the installer downloads: the installer prints
   `checksum verified` (mirroring `install.sh:594`) **before** unpacking.
3. After the swap: `ls ~/.config/ar/dashboard/server.js` exists; `du -sh` matches
   the published size.
4. `node -e "require('/home/nerd/.config/ar/dashboard/node_modules/keytar')"` →
   **no throw**. This is the native-module check that the tarball's portability
   rests on (§4.4). `[INFERENCE]` — a direct require is the cheapest proof that
   the packaged `keytar.node` loads on this host; a dashboard boot is the
   stronger check and should be the acceptance gate.
5. `systemctl --user start aroute-dashboard`; the unit reaches `active` and
   stays there (no `Restart=on-failure` loop — `install.sh:517`).
6. `curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:20149/api/health/ping`
   → 200. (The path itself is O7 — confirm before using it.)
7. `journalctl --user -u aroute-dashboard -n 200 | grep -i 'STARTUP'` — the
   startup banner must show **no** "Invalid web runtime environment" (chunk
   `[root-of-the-server]__16g15nu._.js` `enforceWebRuntimeEnv`) and no
   `exit(1)`.

### 9.3 Live check list — the order to run them in

1. `ssh racknerd 'grep -A20 "ingress:" ~/.cloudflared/hermes-config.yml'` — **O1**,
   and the current state of the `/ui` block.
2. `ssh racknerd 'node --version; which node'` — confirm Node ≥ 18 (the
   orchestrator measured v22.22.2).
3. `ssh racknerd 'ss -ltn | grep -E "20128|20149"'` — what is listening.
4. `ssh racknerd 'systemctl --user list-units "aroute*"'` — the units in play.
5. `ssh racknerd 'ls ~/.config/ar/'` — is there a `dashboard-data/`? (There should
   not be — `docs/34:46`.)
6. `curl -s -o /dev/null -w '%{http_code}' https://aroute.ishanparihar.com/v1/models` → 401.
7. `curl -s -o /dev/null -w '%{http_code}' https://aroute.ishanparihar.com/ui/` → 404 (the
   ingress block is live) — and **do not remove it** until §8.4 step 2 is
   decided.
8. `curl -sv https://<candidate-host>/api/health/ping 2>&1 | grep -i forwarded`
   — **O1**, from outside the box.

### 9.4 Regression guards to add when this plan is implemented

- A test that the release workflow publishes the dashboard tarball and its
  `.sha256` (mirroring the binary's `sha256sum` step, `release.yml:169-176`).
- A test that `install.sh` refuses a dashboard tarball whose checksum does not
  match, **before** touching `${DASH_DEST}`.
- A test that `install.sh` leaves the existing dashboard in place when the
  unpacked tree has no `server.js` (`install.sh:472` already asserts this).
- A test that `aroute sync` does **not** write a `limits:` block from a
  dashboard DB with no active rpm-carrying keys (the `docs/34` §5 second half).

---

## 10. Summary of the recommendation

1. **Do not publish the compiled dashboard** to the `aroute` hostname — not as
   `/dash`, not as a subdomain — until O1 is answered and every control in §6
   is green.
2. **Solve B1 properly** (§4): a release-attached, checksummed, attested
   linux-x64 tarball, verified by the installer before the atomic swap, with
   `keytar` rebuilt for the target. That makes the dashboard installable on the
   VPS **as a local tool** without any hand-copying.
3. **Reach a browser surface on the `aroute` hostname via the `/ui` panel**,
   which is already deployed, already gated (`Armed`), and already the surface
   that talks to the real proxy — with the `?token=`-in-the-access-log caveat
   stated.
4. **Converge on the panel** (docs/16 §6-B). The compiled dashboard is a
   temporary local tool. When §6-B lands, delete `dashboard/` entirely — which
   deletes the second gateway, the second credential store, the `enc:v1:`
   problem, the 1.6 GiB artifact, and the anonymous-spend surface with it.
5. **The single most important open question: does the cloudflared tunnel
   forward an `x-forwarded-*` (or `x-real-ip` / `forwarded` / `via`) header to
   the origin?** If not, the dashboard classifies every internet request as
   **loopback**, the LOCAL_ONLY route tier opens, and no ingress rule can see
   it. One `curl -sv` and one file read settle it.
