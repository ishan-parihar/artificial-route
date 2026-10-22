# 41 — Publish the dashboard on the hostname, behind OmniRoute's own auth

Status: **plan, 2026-10-21.** Owner: this doc, for the *plan and its decisions*.
No code was changed to write it; nothing was built, tested or linted. Every
load-bearing claim carries its evidence; `[INFERENCE]` marks what is deduced.
Companions: [`docs/37-dashboard-deployability-plan.md`](37-dashboard-deployability-plan.md)
(the prior deployability analysis — §5 supersedes its "do not publish"
conclusion), [`docs/16-dashboard-unity-plan.md`](16-dashboard-unity-plan.md)
(§6-A rejected, §6-B "the real integration" — §9 checks against it),
[`docs/32-per-model-token-budgets.md`](32-per-model-token-budgets.md) and
[`docs/40-project-closing-outline.md`](40-project-closing-outline.md) (the
closure sequence these phases slot into).

The operator's decision, recorded here so it is not re-litigated: **the dashboard
must be publicly reachable on the `aroute` hostname, behind the same auth OmniRoute
itself uses.** Not behind an SSH tunnel, not on loopback. That ruling answers the
"which surface" half of ask **Q18**; the remaining half is §7's open items.

## Contents

- [§0 The decision, and the three facts that make it cheap](#0-the-decision-and-the-three-facts-that-make-it-cheap)
- [§1 Serving topology: a path on the aroute hostname — /dash](#1-serving-topology-a-path-on-the-aroute-hostname--dash)
- [§2 The auth configuration, exactly](#2-the-auth-configuration-exactly)
- [§3 The launch, as a service](#3-the-launch-as-a-service)
- [§4 Delivery, concretely](#4-delivery-concretely)
- [§5 The rebuild decision and the security delta](#5-the-rebuild-decision-and-the-security-delta)
- [§6 The parity gap register](#6-the-parity-gap-register)
- [§7 The security control set](#7-the-security-control-set)
- [§8 The phase sequence](#8-the-phase-sequence)
- [§9 What stays, and the cost of two UIs](#9-what-stays-and-the-cost-of-two-uis)
- [§10 The open items](#10-the-open-items)

---

## §0 — The decision, and the three facts that make it cheap

The plan's shape is set by three MEASURED facts, not by preference.

**(a) The auth posture needs NO rebuild — env at launch is sufficient.** The
compiled dist's own resolver states its precedence verbatim, in the docstring baked
into the bundle: *"DB feature flag override > `process.env.REQUIRE_API_KEY` >
default"* (`dashboard/dist/.build/next/server/chunks/_01g6ld-._.js`, module
908995). It reads `process.env.REQUIRE_API_KEY` at request time and **fails
closed** — on any resolution error it logs and returns `true` ("defaulting to
required"), identical to upstream
`external-repos/OmniRoute/src/shared/utils/featureFlags.ts:64-74`. So
`REQUIRE_API_KEY=true` in the child environment closes the gateway tier on the
SHIPPED artifact, no build. `JWT_SECRET`, `API_KEY_SECRET`, `INITIAL_PASSWORD`,
`AUTH_COOKIE_SECURE` and `OMNIROUTE_PUBLIC_BASE_URL` are likewise read at
instrumentation/request time by the compiled chunks (the instrumentation chunk
`src_instrumentation-node_ts_1c--sr1._.js` runs `enforceWebRuntimeEnv` at boot;
`apiKey.ts`'s compiled twin reads `API_KEY_SECRET` at call time).

**(b) The locality question is CLOSED — the open item F10 had guarded is settled.**
`dashboard/peer-stamp-launcher.cjs:62-75`'s `hasProxyHopHeader` accepts any
`x-forwarded-*`, `x-real-ip`, `forwarded` or `via`. **Cloudflare's own edge always
sends `X-Forwarded-For`** — proven by inspecting the tunnel binary itself:
`strings /usr/bin/cloudflared` contains the literals `X-Forwarded-For`,
`x-forwarded-for`, and the format string `X-Forwarded-For%s %s %s %s %s`
(measured 2026-10-21 on racknerd, binary at `/usr/bin/cloudflared`, 30 MB,
`30818424` bytes). The dashboard's TCP peer is the on-host tunnel's loopback socket,
`isTrustedProxyPeer("127.0.0.1")` is true (`peer-stamp-launcher.cjs:51-57`),
`hasProxyHopHeader` is true, so `viaProxy="1"` → upstream
`src/server/authz/peerStamp.ts:98` (`if (viaProxy && locality !== "remote") return
"remote"`) → **an internet request classifies REMOTE, never loopback**. The
`LOCAL_ONLY` route tier (`src/server/authz/routeGuard.ts:4,34`) therefore stays
closed to the internet. Residual risk, stated plainly: if the tunnel is ever
reconfigured to strip `x-forwarded-*`, BOTH implementations fail open — a
deployment condition, not a rebrand defect. Add that to the control set.

**(c) The SECURITY posture DOES need a rebuild — the shipped artifact carries two
known-vulnerable paths.** This is why the 1108-commit distance matters, and it is
proven by fingerprint, not inferred from a count:

- **GHSA-66vh-35g3-78qv is unpatched.** Upstream fix `709a00a2ca` deleted a block
  that returned `true` on a Redis cache hit without consulting SQLite ("a retired
  credential stayed valid for the 1h TTL"). The deleted block is PRESENT in the
  compiled dist — chunk `_01h8oiv._.js` contains, verbatim: the `auth:api_key:` key
  read, the `isBanned/isActive/revokedAt/expiresAt` checks, and then **`return!0`**
  — before the SQLite read that follows (`let n=(0,s.getDbInstance)()`). The
  vulnerability is live in the artifact we would publish.
- **GHSA-3fxv-j4h8-9mgg is unpatched.** Upstream fix `faed233501` ADDED
  `src/shared/components/oauthCallbackState.ts` exporting
  `isCallbackStateAcceptable()` — an automatic OAuth relay callback must echo the
  active attempt's state. A grep for `oauthCallbackState` /
  `isCallbackStateAcceptable` across every chunk in
  `dashboard/dist/.build/next/server/chunks/*.js` returns **zero matches**. The
  file is absent.

**So the plan has two independent parts: an env-and-patch step that publishes
safely on the `aroute` hostname at `/dash`,
and a rebuild step that publishes CURRENTLY-safe.** §8 sequences them so the
operator's goal is met in hours, not weeks, and the security delta lands as its own
release.

**One correction to the earlier analysis** (`docs/37`), recorded so it is not
repeated: upstream's shipped default for `REQUIRE_API_KEY` is **also `"false"`**
(`external-repos/OmniRoute/src/shared/constants/featureFlagDefinitions.ts:22`,
`.env.example:448`). Upstream is safe by **loopback deployment**, not by flag
default. The rebrand did NOT open the gateway — the auth parity report proves it:
every string the rebrand rewrites is bounded away from the `x-omniroute-*` headers,
the `OMNIROUTE_*` env names, the `auth_token` cookie, the `manage`/`admin`/
`mcp:connect` scopes and the `sk-` mint/CRC. The gap is that **the vendored
pipeline never SETS the env**, not that it changed behaviour. Publish is therefore
an *enablement* task, not a repair task.

## §1 — Serving topology: a path on the `aroute` hostname — `/dash`

**Recommendation, AMENDED 2026-10-21 after the operator's ruling: the dashboard is
integrated INTO `aroute.ishanparihar.com` at the path `/dash`.** The first draft
of this plan recommended a separate hostname; the operator overruled it, and the
overrule is cheaper than the first draft believed — because **upstream ships a
runtime base-path patcher** that the first draft missed:

> `external-repos/OmniRoute/scripts/docker/ensure-docker-base-path.mjs` +
> `patch-standalone-base-path.mjs`: at container start they compare
> `OMNIROUTE_BASE_PATH` against a `BUILD_OMNIROUTE_BASE_PATH` sentinel, and when
> they differ **they rewrite the standalone bundle in place** — the five JSON
> manifests (`routes-manifest.json`, `prerender-manifest.json`,
> `required-server-files.json`, `images-manifest.json`,
> `app-path-routes-manifest.json`) and every embedded `basePath`/`assetPrefix`/
> `NEXT_PUBLIC_OMNIROUTE_BASE_PATH` literal (the regex at
> `patch-standalone-base-path.mjs:56`).

**PROVEN on our own dist, 2026-10-21** (a scratch copy, not the tree): running
`patchStandaloneBasePath({ appRoot, fromBasePath: '', toBasePath: '/dash' })` over
`dashboard/dist` reported `{"changed":true,"patchedManifests":4,
"patchedTextFiles":171}` and `server.js` went from `"basePath":""` to
`"basePath":"/dash"`, as did `routes-manifest.json` and
`required-server-files.json`. **The patch is one-way by design** — reverting
`/dash` → root throws (`runtime OMNIROUTE_BASE_PATH ((root)) does not match the
image build (/dash)`), so the patch runs once at install, on a fresh copy, and
rollback is the installer's `.old` swap, not a reverse patch.

So the honest constraint table, corrected:

| | **Path mount `/dash` (chosen)** | Separate hostname (rejected by the operator) | Cloudflare Access |
|---|---|---|---|
| Rebuild needed | **No** — the runtime patcher converts the shipped root-built bundle (PROVEN, above). Baking it at build (`OMNIROUTE_BASE_PATH=/dash`) is the *cleaner* variant and happens anyway with the §5 rebuild | No | No |
| One origin with `/v1` | **yes — managed by ingress ORDER** (below) | no | no |
| DNS work | **none** — the hostname already resolves | one record | one record + an Access app |
| Blast radius | a mis-ordered ingress rule could reach the dashboard's gateway — mitigated by `REQUIRE_API_KEY=true` (its gateway 401s) and by control 5's ordering check | isolated | isolated |
| The operator's URL | `https://aroute.ishanparihar.com/dash` — what was asked for | a second name to remember | a second name + an IdP login |

**Why the first draft said no, and why that was wrong.** The draft read only
`next.config.mjs:111-123` (basePath is a build-time constant) and concluded a path
mount needs a full rebuild. That is true of the *build*, but the standalone bundle
is **patchable at start** by upstream's own script — the Docker flow has always
done this (`scripts/check-permissions.sh:14-16` runs it), and the first draft
missed it because the vendored dist does not ship the script (confirmed:
`dashboard/dist/docker/` is absent). The lesson is recorded here: **the vendored
pipeline dropped `scripts/docker/` from the copy** — `rebrand-dist.sh:46-51` copies
`server.js`, `package.json`, `public/`, `node_modules/`, `.build/`, `migrations/`
only. Shipping the patcher alongside the dist is part of this plan (§4).

**Cloudflare Access** stays §8 Phase 2 as an optional hardening layer — an IdP
wall in front of `/dash`, independent of the dashboard's own auth.

**The exact change.** Add this block to `~/.cloudflared/hermes-config.yml`,
BEFORE the existing `aroute.ishanparihar.com` passthrough rule (ingress matches
in order; the more specific `/dash` rule must come first) and before the
catch-all:

```yaml
  - hostname: aroute.ishanparihar.com
    path: ^/dash(/.*)?$
    service: http://127.0.0.1:20149
    originRequest:
      httpHostHeader: 127.0.0.1:20149
```

**The operator's step** (the tunnel is a root system unit; the admin API cannot do
this — re-measured 2026-10-21 by inserting a probe rule via
`PUT http://127.0.0.1:20241/config`, which returns 200 and applies NOTHING):

```bash
sudo systemctl restart cloudflared     # reads ingress at startup
cloudflared tunnel --config ~/.cloudflared/hermes-config.yml ingress validate
cloudflared tunnel --config ~/.cloudflared/hermes-config.yml ingress rule https://aroute.ishanparihar.com/dash
```

**What the rule must NOT do, and why ordering carries the safety.** The dashboard
under a basePath only claims `/dash/*` — its own `/v1` rewrites become
`/dash/v1/*`, so the dashboard's gateway is NOT reachable at the origin's top
level while the bake is in place. The tunnel's rule sends ONLY `/dash(/.*)?` to
:20149; everything else (including the whole `/v1` family) falls through to the
existing :20128 rule untouched. The one failure mode is a rule written WITHOUT the
`path:` selector — that would forward the entire origin to the second gateway.
Control 5 in §7 catches exactly that: `/dash/*` → the dashboard (2xx/3xx), and
`/v1/models` at the top level → still the aroute gate's **401**, never the
dashboard's answer. The existing `aroute.ishanparihar.com` rules (the `/ui` 404
and the :20128 passthrough) are
untouched by this change.

## §2 — The auth configuration, exactly

Every variable with its source and the consequence of getting it wrong. Sources
are upstream, and all are read at launch or request time by the compiled bundle
(§0a).

| Var | Value / how | If it is wrong | Source |
|---|---|---|---|
| `REQUIRE_API_KEY` | `true` | the gateway tier admits **anonymous completions** — an unledgered spend surface over your provider keys, outside aroute's gate and ledger. This is the single most important line in the table. | `featureFlagDefinitions.ts:17-26`; compiled precedence `_01g6ld-._.js` |
| `INITIAL_PASSWORD` | a generated 32-char secret, `chmod 600` | `CHANGEME` is rejected at login and loudly warned (`managementPassword.ts:9` `INSECURE_DEFAULT_PASSWORDS`); unset → the onboarding wizard, which for a non-loopback peer demands the one-shot BOOTSTRAP token from the log | `.env.selfhost.example`; `runtimeEnv.ts:62` |
| `JWT_SECRET` | generated, ≥32 chars, never in the dist | login **500s** at boot (`login/route.ts:65-80`); a weak value is on the block list in `secretsValidator` (`change-me`, `omniroute-default-secret-change-me`, …) | `login/route.ts:65-80`; `runtimeEnv.ts:59` |
| `API_KEY_SECRET` | generated, ≥16 chars | key mint/validation breaks; `validateSecrets` marks it `required:!0`, `minLength:16` (compiled `[root-of-the-server]__16g15nu._.js`) | `secretsValidator.ts`; `runtimeEnv.ts:60` |
| `AUTH_COOKIE_SECURE` | `true` | without it the cookie's `secure` flag depends on `x-forwarded-proto: https` arriving — true through the tunnel, but by inference not configuration | `login/route.ts:228-239`; `pipeline.ts:115-119` |
| `OMNIROUTE_PUBLIC_BASE_URL` | `https://aroute.ishanparihar.com/dash` — the **origin**, and `OMNIROUTE_BASE_PATH=/dash` carries the path. Upstream's `normalizeOrigin` keeps the path when the configured value has one, and the client mirror `NEXT_PUBLIC_OMNIROUTE_BASE_PATH` (patched in by §1's patcher) handles the browser-side prefixes | the CSRF origin check fails or falls through; redirects and OAuth callbacks go to the wrong host | `pipeline.ts:201-205`; `publicOrigin.ts:19-22`; `next.config.mjs:111-123` |
| `DATA_DIR` | `~/.config/ar/dashboard-data` (the Rust side already computes it) | a second, empty store on a different path — the dashboard boots "fresh" and loses `api_keys`/history | `crates/ar-cli/src/dashboard.rs:25` |
| `HOSTNAME` | `127.0.0.1` | binding every interface exposes an **unauthenticated** :20149 to the LAN/internet regardless of the tunnel | `dashboard.rs:111` (already set) |
| `OMNIROUTE_BASE_PATH` | **`/dash`** — set it in the launch env so `ensure-docker-base-path` patches the root-built bundle at start (§1). It must MATCH what the bundle carries or the patcher throws; with the §5 rebuilt dist it is baked and the patcher is a no-op | a mismatch throws at start (`ensure-docker-base-path.mjs` main); an unset value against a `/dash`-built bundle also throws — the error names the fix | `ensure-docker-base-path.mjs`; `patch-standalone-base-path.mjs:56` |
| `OMNIROUTE_MEMORY_MB` | **1024** — see §3's arithmetic, NOT upstream's 2048 | too low → V8 OOM → the dashboard dies → the hostname 502s while aroute keeps serving; too high on this box → the dashboard's heap competes with the proxy | `.env.selfhost.example` |
| `OMNIROUTE_PEER_STAMP_TOKEN` | **pin it** in the env file (the launcher otherwise mints a per-process random, mints a fresh one on every restart, and every restart then re-opens the first-password window) | not pinned → each restart looks like a fresh install to the peer-stamp contract | `peer-stamp-launcher.cjs:30-33` |
| `OMNIROUTE_TRUST_PROXY` | `true` (upstream default "none") | with "none" the launcher's own relay helpers are inert; see B3 in §6 | upstream `peer-stamp.mjs:396-412` |

**`INITIAL_PASSWORD` vs the bootstrap-token flow — decision: pre-seed
`INITIAL_PASSWORD`.** Reasons: (1) the bootstrap token is a **one-shot secret
printed to the process log** — visible to anyone who can read the unit's journal
on a box where the operator is not the only account, and it is minted *per process*,
so every restart re-opens the window; (2) upstream's own self-host guide lists
`INITIAL_PASSWORD` as the pre-seed path and the wizard as the alternative for
people who skipped it; (3) pre-seeding makes the very first public request require
the session cookie rather than a POST to `/api/settings/require-login`. The
trade-off, stated: the password lives in the env file, which must be `chmod 600`
and never copied off the VPS (the same discipline as the gate master keys,
project memory #114).

**A second decision — keep `aroute dashboard` out of the publish path.** The Rust
verb exists and is correct, but it spawns Node as a child of the CLI process. For a
public always-on service the dashboard needs its own unit (§3), not a foreground
command. The verb stays for local use.

## §3 — The launch, as a service

**A `systemd --user` unit, not a system unit** — `nerd` owns `~/.config/ar/`, the
dist, the store and the env; a system unit would need root to read them. The other
units in `~/.config/systemd/user/` (`aroute`, `agentgateway`, `agentmemory-engine`)
set the precedent.

**The resource arithmetic, measured on racknerd 2026-10-21.** `43 GB` disk with
**`6.9 GB` free (84 % used)**; **`2.4 GiB` RAM total, `844 MiB` available**,
`3.0 GiB` swap of which `954 MiB` is used; **2 vCPU**. The dist is 1.6 GiB
(`.build/next/static` 86 M, `node_modules` 284 M, the bulk `.build/next/server`
~1.1 G). So: extracting the tarball consumes **~23 % of remaining disk**; delete
the tarball afterwards (keep the checksum); ship the dist to
`~/.local/share/aroute-dashboard/` (a service data dir, not the config tree).
`OMNIROUTE_MEMORY_MB=1024` — roughly the available RAM, safely under the swap
pressure that a 2048 MB ceiling would cause on a box where the proxy is resident.
A too-small ceiling degrades the CONTROL PLANE (long dashboard renders, the
model-catalog cache), not the inference path, which flows through aroute.

**The unit's memory guard** — because the dashboard and the proxy share a box, the
dashboard must not be allowed to OOM-kill the proxy:

```ini
[Unit]
Description=aroute dashboard (OmniRoute-derived, published)
After=network-online.target

[Service]
Type=simple
EnvironmentFile=%h/.config/ar/dashboard.env
WorkingDirectory=%h/.local/share/aroute-dashboard
ExecStart=/usr/bin/node peer-stamp-launcher.cjs
Restart=on-failure
RestartSec=5s
MemoryMax=1536M
MemoryHigh=1280M

[Install]
WantedBy=default.target
```

`MemoryMax=1536M` is a hard kill at 1.5× the heap ceiling; `MemoryHigh` starts
reclaiming earlier. Both are below what would starve the proxy (which needs far
less). Boot survival comes from `WantedBy=default.target` plus `systemctl --user
enable`.

**The command: `node peer-stamp-launcher.cjs`, not `node server.js`.** The bare
Next standalone `server.js` does NOT stamp the peer, and without the stamp
**every** request — including a genuine 127.0.0.1 browser — fails closed to
"remote" (`peer-stamp-launcher.cjs:9-12`). The launcher is what makes the first
password settable at all. (See §5 for the rebuild's launcher decision, which
supersedes this one.)

**No locally built Rust binary is involved.** The project's rule — never scp a
locally built *Rust* binary, because a glibc build fails on the host (memory #226)
— does not apply: the dashboard is a **Node artifact**, interpreted by the same
Node v22.22.2 already on the host. It is shipped as a tarball with a checksum
(§4), never as an ad-hoc scp of 1.6 GiB.

## §4 — Delivery, concretely

The installer is already ready for this: `install.sh:150-160` selects a
`--dashboard-src` that accepts a `*.tar.gz`/`*.tgz` (else a directory containing
`server.js`), and `install_dashboard` at `:437-479` checks Node ≥ 18, extracts into
`${DASH_DEST}.new`, swaps `.new` → (`.old`) → `DASH_DEST` atomically, and writes the
unit. **Measured: Node v22.22.2 on racknerd satisfies that check.**

The measured sizes set the recipe: `tar.gz` of the dist **without `node_modules`**
is **280 MB** (measured 2026-10-22 on the rebuilt dist; 1.6 GiB raw after the
ONNX prune). Two tarball shapes ship, both proven 2026-10-22:

* **`--with-deps` (335 MB) — the recommended VPS artifact.** It carries the
  exact pruned tree the pipeline booted and gate-checked, so the 2-vCPU host
  runs NO `npm ci` at all. This matters because a host-side `npm ci` re-pulls
  the ONNX stack the pipeline deliberately prunes (the lockfile lists it),
  costing disk AND minutes of 2-vCPU postinstall work the .npmrc
  `onnxruntime-node-install=skip` flag only partially avoids.
* **no-deps (280 MB)** — requires `npm ci --omit=dev --legacy-peer-deps` on the
  host. `--legacy-peer-deps` is MANDATORY (upstream's own .npmrc sets it; the
  tree has a real marked@18 vs marked-terminal@<16 peer conflict), and the
  dist's `package.json` must carry NO `scripts` block (upstream's standalone
  package.json retains the dev `postinstall` → it dies on the missing
  `scripts/` dir; the pipeline strips it). The pipeline ships
  `package-lock.json` + `.npmrc` in the dist for exactly this path.
  Proven 2026-10-22: extract → npm ci (670 pkgs, exit 0) → boot 2s →
  /dash/healthz 200, manifest + login branded.

Both are produced by `dashboard/pack-dist.sh [--with-deps]`, checksummed
`.sha256` files beside them.

```bash
# on the build machine — pack-dist.sh does the packing, member list, and
# checksum (its member list carries the whole launch family: server-ws.mjs's
# guard imports, dev/run-standalone.mjs, build/, package-lock.json, .npmrc,
# the BUILD_OMNIROUTE_BASE_PATH sentinel — and the patcher rides along).
dashboard/pack-dist.sh --with-deps     # → aroute-dashboard-vX-deps.tar.gz (335 MB)
dashboard/pack-dist.sh                 # → aroute-dashboard-vX.tar.gz (280 MB, npm ci on host)

# on the VPS — the sanctioned install path, not an scp of 1.6 GiB
curl -fLO https://github.com/ishan-parihar/artificial-route/releases/download/vX/aroute-dashboard-vX-deps.tar.gz
curl -fLO .../aroute-dashboard-vX-deps.tar.gz.sha256 && sha256sum -c aroute-dashboard-vX-deps.tar.gz.sha256
~/install.sh --with-dashboard --dashboard-src ~/aroute-dashboard-vX-deps.tar.gz
```

**The `OMNI_ROOT` build defect, and its fix.** `rebrand-dist.sh:30` defaults to
`$AR_ROOT/../OmniRoute` — which does not exist; the upstream checkout is at
`…/proxy/external-repos/OmniRoute`. `update-dist.sh:12` repeats the same wrong
default, so the whole pipeline dies at its first step. One line each:

```bash
# dashboard/rebrand-dist.sh:30
OMNI_ROOT="${OMNI_ROOT:-$AR_ROOT/../external-repos/OmniRoute}"
# dashboard/update-dist.sh:12  (and export it before pulling, so the two cannot diverge)
OMNI_ROOT="${OMNI_ROOT:-$AR_ROOT/../external-repos/OmniRoute}"
```

The chain is otherwise correct in shape: `update-dist.sh` pulls upstream, builds it
in its own tree (`npm ci && npm run build`), then calls `rebrand-dist.sh` with no
arguments, which re-derives the same default.

## §5 — The rebuild decision and the security delta

**Recommendation: REBUILD from `../external-repos/OmniRoute`.** The reasoning, in
order:

1. **Two advisory fingerprints are live in the shipped artifact** (§0c). Publishing
   the current dist publishes a known-vulnerable key-auth path and a known-vulnerable
   OAuth relay.
2. The dist is **1108 commits / 12 days / one minor version** behind (built
   2026-10-07 21:59, `BUILD_ID=TBztz6DZojIekK5_cwrJM`, declares v3.8.52; upstream
   HEAD `27c0b4903e` v3.8.53 2026-10-19). That gap includes a materially different
   provider catalog, ~20 routing/quota/billing correctness fixes, and `next` 16.3.7
   → 16.3.8. None of it touches the three auth surfaces — which is *why* the auth
   posture matches despite the gap — but none of it is nothing.
3. The rebuild also **removes three parity gaps for free**: upstream's
   `peer-stamp.mjs` accepts `cf-connecting-ip` **and** Cloudflare's edge CIDRs
   (`:170-186`, 15 IPv4 + 7 IPv6 ranges, snapshot 2026-08-25) and honours
   `OMNIROUTE_TRUSTED_PROXIES` (`:262-268`), and ships `relayForwardingHeaders` /
   `OMNIROUTE_TRUST_PROXY` (`:396-412`) — the three things the vendored
   `peer-stamp-launcher.cjs` drops (A5, B2, B3 in §6).
4. It costs one `npm install` + one Next production build **on the build machine**,
   where node **v24.19.0** satisfies upstream's `engines`
   (`>=22.22.2 <23 || >=24.0.0 <27`) and the build is `node
   scripts/build/build-next-isolated.mjs`. **It must NOT be built on the VPS** —
   2 vCPU, `844 MiB` available, no `node_modules`; a Next production build there is
   not viable.

**The build recipe.** `OMNIROUTE_BASE_PATH=/dash` is **baked** (§1's chosen topology — the path mount is the operator's ruling, and baking beats patching for a rebuilt artifact); `next.config.mjs:187` adds `output:"standalone"` automatically when `shouldBuildStandalone(process.env)`. Auth env is **not** set at build time — it is read at run time (§0a), so nothing auth-related must be baked in. Until that rebuild ships, the runtime patcher (§1, PROVEN) converts the current dist.

**Launcher decision on rebuild: use UPSTREAM's `scripts/dev/peer-stamp.mjs`, and
retire `dashboard/peer-stamp-launcher.cjs` from the pipeline.** Our launcher exists
only because the standalone tree lost the stamping; upstream's script has it, plus
the Cloudflare handling we lack. Keep our file in the tree (it still serves the
v3.8.52 fallback path if the rebuild is deferred), but do not use it in a rebuilt
dist.

**The honest statement of the trade-off:** the env-and-patch step is hours and needs no
rebuild; the rebuild is a day and closes two GHSAs plus 1108 commits. So §8
publishes first and rebuilds second — and the rebuild is **required**, not
optional, before this is called done.

## §6 — The parity gap register

Severity: **AUTH** (changes who gets in), **FUNC** (changes what works),
**COSMETIC**.

| id | Gap | Upstream | Dist / vendored | Sev | Closes by |
|---|---|---|---|---|---|
| A1 | `REQUIRE_API_KEY` never set by the pipeline | ships `false` with loopback guidance; the operator sets `true` for non-loopback (`.env.example:448`, `:137-140`) | `defaultValue:"false"` in `_01g6ld-._.js` — **identical**, never overridden | **AUTH** | §2 env, Phase 1 |
| A2 | `JWT_SECRET`/`API_KEY_SECRET` never operator-set | operator sets them | auto-generated + persisted by compiled instrumentation `src_instrumentation-node_ts_1c--sr1._.js` | **AUTH** | §2 env, Phase 1 |
| A3 | `AUTH_COOKIE_SECURE` never set | `login/route.ts:228-239` sets it from the env | unset; secure in practice only via `x-forwarded-proto` | **AUTH** | §2 env, Phase 1 |
| A4 | `enforceWebRuntimeEnv` live but its `API_KEY_SECRET` requirement masked | `instrumentation-node.ts:583` validator | same validator present, auto-generation hides a missing secret | AUTH (low) | §2 env + §5 rebuild |
| A5 | `cf-connecting-ip` + Cloudflare CIDRs dropped from the locality check | `peer-stamp.mjs:340-347` OR-clause; `isCloudflareIP` `:170-186` | `peer-stamp-launcher.cjs:62-114` accepts only `x-forwarded-*`/`x-real-ip`/`forwarded`/`via` | AUTH (**bounded** — see §0b: not a bypass in this topology) | §5 rebuild (or patch) |
| B1 | `basePath` baked at build time | runtime-patchable: `ensure-docker-base-path.mjs` rewrites the bundle at start | `""` frozen in `server.js:12` / `required-server-files.json`; **the patcher is PROVEN to convert it** (§1) but is not shipped in the dist | FUNC | §1 path mount + §4 ship the patcher |
| B2 | Cloudflare CIDRs missing from `isTrustedProxyPeer` | `peer-stamp.mjs:170-186, 277-283` | only loopback + RFC-1918/CGNAT (`peer-stamp-launcher.cjs:40-49`) | FUNC | §5 rebuild |
| B3 | `relayForwardingHeaders` / `OMNIROUTE_TRUST_PROXY` dropped | `peer-stamp.mjs:396-412` | absent | FUNC | §5 rebuild |
| B4 | `OMNI_ROOT` default points at a non-existent path | `$AR_ROOT/../external-repos/OmniRoute` | `rebrand-dist.sh:30`, `update-dist.sh:12` → `$AR_ROOT/../OmniRoute` | FUNC (build dies) | §4 one-line fix |
| C1 | dist is **1108 commits** and **two GHSAs** behind | HEAD `27c0b4903e` v3.8.53 | built 2026-10-07, v3.8.52, **GHSA-66vh and GHSA-3fxv fingerprints PRESENT** (§0c) | **AUTH (security)** | **§5 rebuild — required** |
| C2 | Provider catalog a version behind | v3.8.53 table, incl. new providers + a removal | v3.8.52 table | FUNC | §5 rebuild |
| C3 | `sk_omniroute` → `sk_artificial` placeholder swap | UI placeholder | mint/validation unaffected (`patch-artificial.sh:8-11`) | COSMETIC | none |
| C4 | Rebrand's prose rewrites | — | `manifest_text`, `homebrew_wording`, `openapi_*` — **all bounded away from auth paths** | COSMETIC | none |

**The finding that matters most in this table: the rebrand itself changed NOTHING
in the auth path.** The auth parity report verified each rewrite against the
compiled chunks — every string it rewrites is bounded away from the
`x-omniroute-*` headers, the `OMNIROUTE_*` env names, the `auth_token` cookie, the
`manage`/`admin`/`mcp:connect` scopes and the `sk-` mint/CRC. What needs fixing is
what the pipeline never **sets**, not what it changed.

## §7 — The security control set

What must be TRUE before the dashboard is public, each with the command that
proves it. Gate the publish on all of these.

| # | Condition | Proof command |
|---|---|---|
| 1 | The gateway tier refuses anonymous | `curl -s -o /dev/null -w "%{http_code}\n" https://aroute.ishanparihar.com/dash/api/v1/models` → **401** |
| 2 | The dashboard requires login | `curl -s -o /dev/null -w "%{http_code}\n" -L https://aroute.ishanparihar.com/dash/` → the login page, not the dashboard |
| 3 | `LOCAL_ONLY` closed to the internet | `curl -s -o /dev/null -w "%{http_code}\n" https://aroute.ishanparihar.com/dash/api/discovery/` → **403 LOCAL_ONLY** (never 200) |
| 4 | The bootstrap window is closed | a password exists: the unit log shows **no** `[BOOTSTRAP] Fresh install detected` line |
| 5 | The ingress sends ONLY `/dash/*` to :20149, and `/v1` still reaches the proxy | `curl -s -o /dev/null -w "%{http_code}\n" https://aroute.ishanparihar.com/v1/models` → **401** (the ARROUTE gate — if this ever answers with the dashboard's shape, the rule is wrong); and `curl .../dash/` → 2xx/3xx |
| 6 | aroute's own `/v1` is unaffected | `curl -s -o /dev/null -w "%{http_code}\n" https://aroute.ishanparihar.com/v1/models` → **401** (the ar gate), unchanged from before |
| 7 | The tunnel forwards `x-forwarded-for` | the method in §0b — `strings /usr/bin/cloudflared \| grep -c X-Forwarded-For` ≥ 1; plus (after a temp echo rule with the operator's restart) `curl https://<echo-host>/ \| grep -i x-forwarded-for` non-empty |
| 8 | The cookie is `secure` | `curl -sI https://aroute.ishanparihar.com/dash/ \| grep -i set-cookie` → contains `Secure` and `SameSite` |
| 9 | The store is not world-readable | `stat -c %a ~/.config/ar/dashboard-data` → `700` or `750`; `storage.sqlite` not `-rw-r--r--` |
| 10 | The dashboard binds loopback | `ss -ltnp \| grep 20149` → `127.0.0.1:20149`, never `0.0.0.0` |
| 11 | Memory guarded | `systemctl --user show aroute-dashboard -p MemoryMax` → `1536M`; and after 24 h, the proxy's RSS unchanged |
| 12 | An `sk-` key is useless against the aroute proxy | a dashboard-minted key against `https://aroute.ishanparihar.com/v1/chat/completions` → **401** (aroute verifies HS256 JWTs) — proved once, to define the two-store boundary |
| 13 | The tarball matches its checksum | `sha256sum -c SHA256SUMS` after download, before extraction |
| 14 | (after a rebuild) both GHSAs gone | the §0c fingerprints return ZERO matches in the rebuilt bundle |

## §8 — The phase sequence

In dependency order. **Phase 1 first: the auth posture and the loopback bind are in
force BEFORE anything is exposed.**

| Phase | Change | Verification | Rollback | Closes |
|---|---|---|---|---|
| **0 — code, no exposure** | fix `OMNI_ROOT` (one line ×2); wire the §2 env into `crates/ar-cli/src/dashboard.rs:106-114` so the verb *and* the unit both inherit them; write the unit and `dashboard.env` | the verb boots on loopback with the env; anonymous `POST /ui/api/keys/mint` still 401; `aroute serve` unaffected | revert the commit | F9 (partly), DASH-02 |
| **1 — publish on `/dash`, env + patch** | run the base-path patcher once on the installed copy (§1, PROVEN: `{patchedManifests:4, patchedTextFiles:171}`); add the §1 ingress rule (with `path:`, BEFORE the passthrough); **operator: `sudo systemctl restart cloudflared`**; create the password (`INITIAL_PASSWORD`); start + enable the unit | the **whole §7 control set** — all 13 rows that apply pre-rebuild, with rows 1–5 and 8 pointed at `/dash` | remove the ingress rule + restart; stop the unit. Nothing in the serving path changes | **F10, DASH-01, DASH-03, Q18** |
| **2 — Cloudflare Access (optional, operator)** | an Access application in front of `dash.*` | an unauthenticated browser redirect is blocked by Access **before** the dashboard | delete the Access app | hardens F10 |
| **3 — rebuild, REQUIRED** | build upstream (node v24.19.0, empty `OMNIROUTE_BASE_PATH`), rebrand, use upstream's `peer-stamp.mjs`, ship as the §4 tarball, installer-install | control set row 14: both GHSA fingerprints gone; `BUILD_ID` new; control set rows 1–13 re-run green | keep the v3.8.52 dist and the `.old` swap the installer left | **C1, C2, A5, B2, B3, DASH-04** |
| **4 — the panel's own publish decision** | the operator rules whether `/ui` is republished (delete the 404 rule) or stays SSH-only | if published: the same control-set discipline with the ar-keys gate in front, not OmniRoute's | re-add the 404 rule + restart | ledger A2/F1 follow-up, `Q7` |

The **operator-only** steps are: the `sudo systemctl restart cloudflared` (phase 1),
the first password, the optional Access app, and the phase-4 ruling. Everything else
is code, which is why phase 0 is a single commit.

## §9 — What stays, and the cost of two UIs

**The Rust `/ui` panel keeps its role as the operator's private, live-state
surface.** It is the only surface that edits the config the proxy actually serves
(`~/.config/ar/config.yaml`), and its ledger is the one the proxy bills against —
the panel reads the same `ar_tokens::Ledger` mutex the request path does, so its
usage and per-model-spend views are *correct by construction*. The dashboard cannot
have that: it reads its own store. Its `ar-check_quota`-equivalent, its live
resilience view over the router's own breaker/cooldown tables, its three-state gate
badge, and its `pending_restart` drift marker have no dashboard equivalent.

**The `aroute sync` bridge stays, with its boundary stated.** It carries exactly
four things (`crates/ar-cli/src/commands.rs:2073-2215`): `combos`,
`custom_providers`, provider credentials → `ar.env` (skipping `enc:v1:` rows), and
**one** `api_keys` column — `max_requests_per_minute` → `limits.keys.<n>.rpm`.
It carries **1 of 20+ `api_keys` columns**. A dashboard `sk-` key is an `sk-` string
in its own SQLite; aroute verifies HS256 JWTs, so an `sk-` string is a **401**
against the proxy. The dashboard's `usage_history` never sees proxy traffic. Sync is
one-way. Closing this gap — making the two auth models interoperable — is a
separate piece of work and is **not** in scope here; control-set row 12 pins the
boundary so it is not forgotten.

**`docs/16` §6-B stands.** An aroute-native thin dashboard that *replaces* the
vendored 1.6 GiB app remains the right long-term answer for the ~5 features the
operator actually uses daily (usage, health, resilience, combos, playground).
Publishing OmniRoute's dashboard now buys the other 45 sections and 1108 upstream
commits of free maintenance — which is exactly why Phase 3 is required rather than
optional: the cost of two UIs is a vendored Node app to track, and the rebuild is
what keeps that cost from becoming a security debt.

**What the operator ends up with** after phases 1 and 3: a public, authenticated,
122-page control plane at `https://aroute.ishanparihar.com/dash` running a current
build; the
private, live-state `/ui` panel behind an SSH forward; and the aroute inference
path untouched. Phase 4 then decides whether the panel moves public too.

## §10 — The open items

1. **RESOLVED: does the tunnel forward an `x-forwarded-*` header?** — Yes, proven
   by `strings /usr/bin/cloudflared` containing `X-Forwarded-For` and the format
   string `X-Forwarded-For%s %s %s %s %s` (§0b). Ledger **F10**'s "UNMEASURED"
   note should now read measured. The residual — a tunnel reconfigured to strip
   the header — is a deployment condition, in the control set (row 7).
2. The operator's first login password and `sudo systemctl restart cloudflared` —
   human steps in Phase 1.
3. Whether Phase 2 (Cloudflare Access) is wanted. Recommendation: yes, once Phase 1
   is green.
4. The two-store auth boundary (§9) — a future piece of work, not this plan's.
5. After a rebuild, re-derive the counts (the 1108/12-day gap) from the new
   `BUILD_ID` and the then-current upstream HEAD; do not inherit this document's
   numbers.
