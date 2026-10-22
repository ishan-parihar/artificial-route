# Dashboard surface audit — 2026-10-22

Status: **audit + fixes, 2026-10-22.** Owner: this doc, for the *evidence and the
findings*; the state of every open item lives in `docs/28-gap-ledger.md`
(rows **F11**, **F12**, **F13**) and the register rows **DASH-05**…**DASH-07**
in `TODO.md`. Companions: `docs/41-dashboard-public-deployment-plan.md`
(the publish plan this audit gates), `docs/37-dashboard-deployability-plan.md`
(prior deployability analysis), `docs/16-dashboard-unity-plan.md` (the
two-surfaces decision this audit does not reopen).

The ask: audit the web dashboard's surface area — how it is created, whether
we hold its source, whether it is wired into the binary effectively, why it
showed *"Server is experiencing issues. Some features may be unavailable."*,
whether the WebSocket connections work, whether the database/config
integration drifts, a red-team pass over redundancy/over-engineering/
fragmentation/gaps, and runtime RSS with optimization levers.

**A second pass, later the same evening, re-tested every claim this audit had
made from inference rather than measurement and corrected three of them.**
The corrections are marked **[CORRECTED]** inline and are the reason this
document supersedes the first report given to the operator in chat.

---

## 1. How the dashboard is created, and do we have its source?

**We do not hold dashboard source in this repo.** `dashboard/dist` is a
**vendored compiled artifact** of the sibling checkout
`../external-repos/OmniRoute`:

* `dashboard/update-dist.sh` pulls upstream, builds the Next.js standalone
  output in its own tree, then calls the rebrand pipeline;
* `dashboard/rebrand-dist.sh` copies `server.js`, the traced `node_modules`,
  the `.build/` output and the launch family, then rebrands user-visible
  strings. The rebrand is bounded away from auth paths (headers, env names,
  the `auth_token` cookie, scopes, the `sk-` mint) — verified by the
  2026-10-21 auth parity report;
* the current dist was **rebuilt 2026-10-22** from OmniRoute v3.8.53
  (`27c0b4903e`) with `OMNIROUTE_BASE_PATH=/dash` **baked at build time**
  (sentinel `BUILD_OMNIROUTE_BASE_PATH` present); both GHSA fingerprints
  verified gone (docs/41 §5, control 14);
* it is **1.6 GiB** (`.build/` 1.3 G, `node_modules` 306 M after the ONNX
  prune), gitignored, and ships as two tarball shapes via
  `dashboard/pack-dist.sh` (docs/41 §4).

The operator's error was reported at 19:59:53 — the same second the docs/41
rebuild was overwriting the chunk files (measured: chunk mtime
19:59:53.699). The banner, however, was **not** a transient of the rebuild;
it is a real defect that survived the rebuild (§4).

## 2. Wiring into the aroute binary

`aroute dashboard` (`crates/ar-cli/src/dashboard.rs`) is **only a
supervisor**: it resolves the dist (`--path` → `$AR_DASHBOARD_DIR` → beside
the binary → `~/.config/ar/dashboard`), picks the entry
(`server-ws.mjs` → `peer-stamp-launcher.cjs` → `server.js`), spawns `node`
as a **sibling process** (not a reverse-proxied route), and blocks. The
`/v1` proxy and the UI each own their socket and neither can fail the other.

The launch env is constructed, not inherited: `REQUIRE_API_KEY=true`,
`AUTH_COOKIE_SECURE=true`, `OMNIROUTE_MEMORY_MB=1024` by default, parent
overrides win, secrets are never invented, `DATA_DIR` defaults to
`~/.config/ar/dashboard-data` (never OmniRoute's `~/.omniroute`).
`install.sh`'s unit runs the verb (`ExecStart=$TARGET dashboard --port …`),
so the installed shape inherits all of it. The wiring is correct and the
coupling is deliberately loose.

## 3. The "Server is experiencing issues" banner — root-caused, fixed, verified

**Reproduced** in headless Chromium against the live preview (`:20149`):
15 distinct root-path 404s (`/api/health/ping`, `/api/auth/csrf`,
`/api/settings`, `/api/providers`, `/api/models`, `/api/usage/call-logs`,
`/api/v1/ws`, …), and the amber banner rendered. *(The count is 15 —
**[CORRECTED]** from the first report's 14, on a re-read of the probe's
`failed-urls` set.)*

**Root cause chain** (each step measured in the compiled bundle):

1. The banner polls `fetch("/api/health/ping")` every 10 s and shows
   `common.maintenanceServerIssues` after **2** consecutive failures
   (chunk `0_s9q1l56lbg_.js`).
2. Client fetches are supposed to gain the base path at runtime:
   `BasePathNetworkProvider` → `installBasePathFetch` (chunk
   `37glxh7v131db.js`) patches `globalThis.fetch` and `EventSource` to
   prefix `/dash`, resolving it from
   `process.env.NEXT_PUBLIC_OMNIROUTE_BASE_PATH || process.env.OMNIROUTE_BASE_PATH`.
3. In the browser, `process` is the Turbopack **shim** (module `935451`,
   chunk `26g4tn5b226u5.js`) whose factory ships **`env={}`**. A
   `/dash`-**baked** build populates the manifests, `server.js` and some
   literals, but **never the shim** — upstream's own `patchProcessEnvShim`
   exists for exactly this splice, but `patchStandaloneBasePath` is one-way
   root→subpath, and a dist built for `/dash` never passes through it.
4. So the resolver sees `""`, the prefix is never applied, every root-relative
   fetch 404s, and the banner fires. Verified at the page level:
   `fetch("/api/health/ping")` → 404 at `http://127.0.0.1:20149/api/health/ping`
   (root, not `/dash/api/...`).

**Fix, landed in both places:** (1) a standing pipeline step —
`rebrand-dist.sh` §2.5 — splices the sentinel's base path into every client
chunk's shim `env={}` (guarded, counted, parse-gated downstream, dies loudly
if the shim is absent); (2) the shipped dist's chunk was patched in place
with the same splice.

**Verified end-to-end:** the identical probe after the patch shows
`fetch("/api/health/ping")` → **200** at `/dash/api/health/ping`, the banner
is **absent**, and the page works (models, providers, keys, usage all load).

**Residual, honestly scoped.** The only surviving root-path 404s are the two
SSR-rendered `<link rel="icon" href="/favicon.svg">` / `icon-512.png` head
tags (server-rendered, outside the fetch wrapper's reach — a cosmetic favicon
miss). **[CORRECTED]:** the first report claimed early `fetch("/api/token-health")`
calls made before `installBasePathFetch` runs also survive — **withdrawn**;
the re-probe's failed-URL set contained no `/api/*` entries. The fetch
wrapper covers every client API call. Ledger **F11**, register **DASH-05**.

## 4. WebSockets — the honest verdict (ledger F13, register DASH-07)

**[CORRECTED] — this section replaces the first report's "works/broken"
split, which mixed inference with measurement.** All three surfaces were
live-tested 2026-10-22 against the rebuilt dist with a real `ws` client,
root and `/dash` variants:

| Surface | root variant | `/dash` variant | verdict |
|---|---|---|---|
| Responses-WS `/v1/responses` | proxy matched (`Upgrade: ftp` → its own **426**), then the handshake dies at the auth bridge | falls through to Next's upgrade listener → socket hang-up, no response | **broken at root, absent at /dash** |
| Responses-WS internal bridge `/api/internal/codex-responses-ws` | POST → **404** | POST → **403** (route exists, denies without the bridge secret) | the proxy asks at the wrong place — `callInternal` POSTs `INTERNAL_ROUTE` root-relative (`responses-ws-proxy.mjs:29,411`) |
| LiveWS `/live-ws` via :20149 | **101 open** | timeout | server relay works; `LIVE_WS_PATH` match is basePath-blind |
| LiveWS direct `:20132` | **101 open** | n/a (the client default dials `ws://<hostname>:20132/live-ws`, chunk `1u-n6dhbqfml9.js`) | works same-host; **unreachable behind the tunnel** (the port is never forwarded) |
| Traffic-inspector `/api/tools/traffic-inspector/ws` | **404** (client hardcodes root, chunk `4273mqo-xu5m0.js`) | GET → **426** (handler exists) but a real upgrade **hangs with zero bytes** | **dead in standalone mode regardless of base path** — the handler writes 101 on `e.socket`, which an App Router request never exposes |

**Net: on the `/dash` publish, none of the three WS surfaces works from a
browser.** On a root-mounted dist, LiveWS works same-host and responses-WS
would work with credentials once its bridge path is right.

**Fixes, cheapest first (ours, env-only):** set
`NEXT_PUBLIC_LIVE_WS_PUBLIC_URL=wss://aroute.ishanparihar.com/dash/live-ws` —
`deriveLiveWsPath()` (`server-ws.mjs:158`) derives the server-side match from
that env and the client reads the same env, so **one variable revives LiveWS
through the tunnel**. Responses-WS needs the four `/dash` spellings in
`RESPONSES_WS_PUBLIC_PATHS` (`responses-ws-proxy.mjs:27-31`) and a basePath
prefix on `INTERNAL_ROUTE` — upstream-shaped but small. Traffic-inspector
needs an upstream-shaped rewrite (its upgrade can never work in the App
Router; it must be hosted by the custom server like LiveWS). Until they land,
treat the three as root-mount-only features and say so in the publish notes;
docs/41 §7's control set should gain the row.

## 5. Database/config integration — the drift map

Two stores, deliberately: the proxy (config.yaml + `ar.env` + the usage
ledger the request path bills against) and the dashboard (its own SQLite:
`storage.sqlite`, own `api_keys` table, own `sk-` keys).

* `aroute sync` (`commands.rs:2073-2210`) carries exactly **four things,
  one-way**: `combos`, `custom_providers`, provider credentials → `ar.env`
  (skipping `enc:v1:` rows), and **one** `api_keys` column —
  `max_requests_per_minute` → `limits.keys.<n>.rpm`. That is 1 of 20+
  `api_keys` columns.
* A dashboard `sk-` key is a **401** against the proxy (aroute verifies
  HS256 JWTs) — the two auth models are deliberately non-interoperable
  (docs/41 §9; control 12 pins the boundary).
* The dashboard's usage/cost views **never see proxy traffic** — its
  `usage_history` records only what flows through its own gateway.
* The sync carry is implicit: it created the surprise `limits:` block of the
  2026-10-20 incident from a dashboard table nobody thinks of as a budget
  (ledger D12; `docs/35` owns the removal).

**Verdict: the drift is bounded and deliberate, not accidental — but it is
real.** The dashboard is a second product with a second store; it can never
reflect the proxy's truth. The `/ui` panel is the zero-drift surface (same
YAML, same `ar_tokens::Ledger` mutex the request path uses). **Red-team
note:** two dashboard data stores exist on this machine alone
(`/tmp/ar-dashboard-data` — the preview's `DATA_DIR` — and
`~/.config/ar/dashboard-data`); the preview store holds 0 API keys. Any
long-lived preview should point `DATA_DIR` at the canonical store or be
treated as throwaway, or the operator will configure one store and read
another.

## 6. Red-team: the bind bug, redundancy, over-engineering, fragmentation

**The live bind defect (ledger F12, register DASH-06).**
`build/runtime-env.mjs:177` — `HOSTNAME: env.OMNIROUTE_HOSTNAME ||
"0.0.0.0"` — is an **unconditional overwrite**: the `dev/run-standalone.mjs`
wrapper drops the caller's explicit `HOSTNAME`. Proven live: the transient
preview unit set `HOSTNAME=127.0.0.1`, the wrapper parent (513258) still
carried it, and the spawned server child (513267) came up `HOSTNAME=0.0.0.0`
— listener `0.0.0.0:20149`, with `REQUIRE_API_KEY` **unset** on that process
(upstream default `false` → its gateway tier admits anonymous completions;
spend is blocked only because no credentials are configured). Exposure is
bounded to the LAN (the tunnel routes neither `:20149` nor `/dash`), but the
posture is wrong.

**[CORRECTED] — the first report overstated the scope.** It claimed "the
`aroute dashboard` verb is equally defeated." That is false: the verb's entry
chain is `server-ws.mjs`, which requires `server.js` **in-process** and
imports only its guard modules — it never imports `runtime-env.mjs`, so the
rewrite never runs and the verb's `HOSTNAME=127.0.0.1` survives to the
listener. The defect vector is the **`dev/run-standalone.mjs` wrapper only**
(preview units; the pipeline's own boot smoke). **Fixed on our side as
defence in depth:** the verb now pins `OMNIROUTE_HOSTNAME=127.0.0.1`
alongside `HOSTNAME` (the input the wrapper honours), and the smoke boot
sets it too — so both sanctioned paths are covered even though only one was
broken. The upstream-shaped remainder (honour an existing `HOSTNAME` the way
`PORT` already is) is filed in F12.

**Redundancy.** Three peer-stamp implementations exist (upstream's
`peer-stamp.mjs`, the vendored launcher ×2) — acceptable, the launcher is now
fallback-only. Five base-path mechanisms now exist (build bake, manifests,
the runtime patcher, the fetch wrapper, the new §2.5 shim splice) — fragile
by accumulation, documented in F11; the §2.5 step is the one that carries
the rebrand pipeline, the others are upstream's.

**Over-engineering.** A 1.6 GiB, 885-route, 122-page dist serves ~5
daily-use features. `docs/16` §6-B (an aroute-native thin panel) remains the
right long-term answer, and the rebuild treadmill (update-dist, rebrand,
ONNX prune, fingerprint gates, this audit) is the recurring cost of not
doing it. This audit adds two more treadmill items (F11's shim step, F12's
env pin) — evidence for §6-B, not against.

**Fragmentation.** The `/ui` panel (3,605 lines Rust + 1,752 lines JS) and
the dashboard cover overlapping operator surfaces with different stores and
auth models; `aroute sync` is the lossy one-way bridge. This is the decided
two-panels posture (DASH-01) and stays decided.

## 7. Runtime RSS and optimization

Measured 2026-10-22 from `/proc` (VmRSS / VmHWM / VmSwap):

| Process | RSS | Peak (VmHWM) | Swap |
|---|---|---|---|
| aroute proxy (pid 3070309) | 138 MB under load (idle budget 21.7 MB per docs/00) | 194 MB | 29 MB |
| dashboard node (pid 513267) | 379–433 MB | **653 MB** | 224 MB |

The dashboard costs **~20× the proxy's idle budget**. Levers, ranked:

1. **`OMNIROUTE_MEMORY_MB` was never set on the preview** — the wrapper
   defaulted `--max-old-space-size=4096` on a 47 GiB host. Set 512–1024 to
   cap V8's heap growth (the verb already defaults to 1024).
2. **`MemoryMax`/`MemoryHigh` are `infinity` on the preview unit** — the
   docs/41 guard (1536M/1280M) was never applied to it. Apply it to any
   long-lived unit.
3. The ONNX prune (already landed) removed the largest single offender.
4. V8 `--max-old-space-size` plus lazy compilation trim boot RSS further;
   marginal after (1).
5. The proxy itself is healthy: 138 MB under load is within budget, and the
   jemalloc pin (`narenas:2, dirty_decay_ms:1000, muzzy_decay_ms:1000`,
   compiled in via `.cargo/config.toml`) is doing its job — the idle figure
   (21.7 MB) is the one to watch, and it is unchanged.

## 8. What the operator should do next (ranked)

1. **Restart the preview with a correct env** — `REQUIRE_API_KEY=true`,
   `OMNIROUTE_HOSTNAME=127.0.0.1`, `OMNIROUTE_MEMORY_MB=1024`, and
   `DATA_DIR` pointed at the canonical store (or accept it as throwaway).
2. Before the docs/41 publish: set
   `NEXT_PUBLIC_LIVE_WS_PUBLIC_URL=wss://aroute.ishanparihar.com/dash/live-ws`
   (one variable revives LiveWS through the tunnel), and accept that
   responses-WS and the traffic inspector are root-mount-only until the
   upstream-shaped fixes land — then add the §7 control row.
3. Keep the two-panels question closed: this audit's findings (the shim step,
   the env pin, the WS gaps) are all *cost-of-vendoring* evidence for
   `docs/16` §6-B, and none of them reopens DASH-01.

**Gates at the time of writing:** `cargo clippy --all-targets
--all-features --locked -- -D warnings` clean; workspace tests green;
`bash -n` clean; the patched chunk `node --check` clean; genome lint 0
errors. Fixes landed in `634a3c2` and the corrections in the commit that
carries this document.
