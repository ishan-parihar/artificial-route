# 11 — Web UI port plan (OmniRoute dashboard → `artificial-route`)

Read-only recon of OmniRoute's web UI, to decide whether (and how) it ports into
`artificial-route`. Source of truth is the OmniRoute checkout at
`/home/ishanp/Documents/github/my-projects/agentic-utility/proxy/OmniRoute`;
every claim below carries a path + line so it can be re-checked.

**Standing scope conflict, stated up front.** `docs/00-overview.md:3` already
declares artificial-route's premise as *"Minimal-RAM Rust port of OmniRoute proxy
core. CLI + file config only. **No web UI**"*, and `00-overview.md:14` repeats it:
*"No Next.js dashboard, no Electron, no PWA, no i18n UI (42 locales dropped)."*
`docs/05-roadmap.md:63` parks the UI in the "Out" column: *"Out: full 110-tool
MCP, xDS, HBONE, SPIFFE, UI, ONNX, cloud sync. Each needs proposal + RAM budget to
re-enter."* This document is that proposal's evidence section. It does **not**
re-open the decision — sections 1–5 are the facts a reviewer needs to reject it
cheaply, and the ship-shape section names the one approach that would be taken
*if* the decision flips.

---

## 1. Where the web UI lives

| Question | Answer | Evidence |
|---|---|---|
| Framework | **Next.js 16.3.5, App Router**, React 19.2.8 | `package.json` deps: `next: 16.3.5`, `react: 19.2.8`, `react-dom: 19.2.8` |
| Root | `src/app/` — Next App Router convention, no separate `web/` or `frontend/` dir | `src/app/` contains `(dashboard)/`, `api/`, `login/`, `landing/`, `docs/`, `status/`, … |
| UI components | Co-located, plus a shared barrel | `src/app/(dashboard)/**` (855 files, 6.84 MB), `src/shared/components/` (126 files, 1.04 MB) |
| Server API | Co-located App Router route handlers | `src/app/api/` — **721 `route.ts` handlers across 879 directories** |
| Build tool | Next's own compiler (Turbopack by default, webpack fallback) | `next.config.mjs:169` `turbopack: { root: projectRoot }`; `scripts/dev/run-next.mjs:127` forces webpack in dev |
| i18n | `next-intl` v4, cookie-based locale, **no `[locale]` route segment** | `next.config.mjs:15` `createNextIntlPlugin("./src/i18n/request.ts")`; `src/i18n/request.ts:2-3` reads `cookies()`/`headers()`; no `src/app/[locale]` dir exists |
| Charts / flow / editor | Recharts 3, `@xyflow/react` 12, `@monaco-editor/react` 4, mermaid 11 | `package.json` deps |
| State | Zustand 5 + React Context | `package.json` deps (`zustand: ^5.0.15`) |
| Styling | Tailwind 4 via `@tailwindcss/postcss` | `postcss.config.mjs`, `package.json` devDeps |

**Scale.** 122 dashboard pages under `src/app/(dashboard)/**/page.tsx` (e.g.
`dashboard/providers`, `dashboard/analytics/compression`, `dashboard/context/rtk`,
`dashboard/media-providers/[kind]/[id]`). 76% of dashboard files (650/855) carry
`"use client"`.

**Dev server / build commands** (`package.json` scripts):

```
dev    : node --max-old-space-size=8192 scripts/dev/run-next.mjs dev      # :46
build  : node scripts/build/build-next-isolated.mjs                     # :49
start  : node scripts/dev/run-next.mjs start                            # :58
build:fast : OMNIROUTE_SKIP_STANDALONE=1 …                             # :50
build:backend: OMNIROUTE_BUILD_BACKEND_ONLY=1 …                         # :52
build:release: rm -rf .build dist && npm run build && npm run build:cli # :57
```

`dev` needs **8 GB of V8 heap** (`--max-old-space-size=8192`) — direct evidence
this is not a light tool.

---

## 2. API surface the UI consumes

Enumerated by scanning `src/app/(dashboard)`, `src/components`, `src/hooks`,
`src/store`, `src/lib` for `/api/...` string literals:

- **510 distinct `/api/` endpoints**, **1137 call sites**.

### 2.1 Split against `ar-server`

`ar-server`'s entire HTTP surface is 17 routes, assembled in
`crates/ar-server/src/app.rs:540-569`:

```
/v1/chat/completions  /v1/messages  /v1/responses  /api/chat  /v1/completions   (:541-555)
/v1/embeddings  /v1/audio/transcriptions  /v1/audio/translations
/v1/images/generations  /v1/ocr                                              (:524-538)
/v1/models (GET+HEAD)  /v1/models/{*model}                                    (:560-566)
/healthz  /metrics  fallback(not_found)                                       (:567-569)
```

Only **7 of the UI's 510 endpoints** have an `ar-server` equivalent:

| UI endpoint | `ar-server` equivalent | ar-server site |
|---|---|---|
| `/api/v1/chat/completions` | `/v1/chat/completions` | `app.rs:541` |
| `/api/v1/models` | `/v1/models` | `app.rs:560` |
| `/api/v1/embeddings` | `/v1/embeddings` | `app.rs:525` |
| `/api/v1/audio/transcriptions` | `/v1/audio/transcriptions` | `app.rs:527` |
| `/api/v1/images/generations` | `/v1/images/generations` | `app.rs:534` |
| `/api/v1/ocr` | `/v1/ocr` | `app.rs:538` |
| `/api/chat` | `/api/chat` | `app.rs:547` |

**The 7 are all playback, not control.** The UI calls them to *test* a model from
the playground; the dashboard's actual job — showing which providers, keys,
combos, quotas, caches, and settings exist — has **zero** overlap. Examples with
their UI call counts:

```
  99  /api/settings                       35  /api/providers
  23  /api/provider-models                18  /api/settings/compression
  14  /api/cli-tools/backups              13  /api/provider-nodes
  11  /api/combos                         11  /api/keys
   9  /api/settings/proxy                  8  /api/cli-tools/keys
   8  /api/monitoring/health               6  /api/a2a/status
   5  /api/usage/provider-limits           5  /api/resilience
   5  /api/db-backups                      5  /api/chaos/config
```

Families by distinct-endpoint count: `providers` 68, `settings` 64, `v1` 43,
`tools` 24, `cli-tools` 19, `usage` 16, `services` 15, `context` 14, `quota` 13,
`combos` 11, `gamification` 10, `memory` 10.

### 2.2 Response-shape mismatch on the one endpoint that matters

Even `/v1/models` is not wire-compatible:

- OmniRoute `/api/models/catalog` (`src/app/api/models/catalog/route.ts:11-33`)
  returns models **regrouped by provider** — `{ provider, active, models: [] }`
  built from `body.data[].owned_by`.
- `ar-server` `/v1/models` (`crates/ar-server/src/routes.rs:2595-2613`) returns
  the flat OpenAI `{"object":"list","data":[card_json(...)]}`.

So even the 7 shared paths need a shim, not a rename.

### 2.3 Auth the UI requires

The UI is behind a server-side authz pipeline with no `ar-server` counterpart:

- `src/server/authz/pipeline.ts:1-40` — Next middleware: dashboard session JWT
  (`verifyDashboardSessionToken`, `DASHBOARD_SESSION_COOKIE`,
  `getDashboardJwtSecret`), CSRF (`validateDashboardCsrfToken`), route classes
  (`classifyRoute`), peer-IP locality stamping, body-size limits, CORS.
- UI-side auth calls: `/api/auth/csrf`, `/api/auth/login`, `/api/auth/logout`,
  `/api/auth/status`, `/api/auth/oidc/…`, `/api/settings/require-login`
  (login page: `src/app/login/page.tsx:36`).
- A CSRF token must accompany every browser mutation — a cross-origin dashboard
  that talks to `/v1/*` on the same origin still needs this if it mutates.

`ar-server` has `axum-middleware` CORS (`app.rs:583`, `app.rs:587-660`) and
nothing else — no session, no CSRF, no login.

### 2.4 Other runtime requirements

- **WebSocket**: 2 dashboard files open a live WS to the server's Live WS server.
  `ar-server` has no WS route.
- **i18n**: 67 locale files in `src/i18n/messages/` totalling **73.11 MB**
  (`en.json` alone is 778 KB). `00-overview.md:14` already says these are dropped.
- **Electron shell + PWA**: `electron/` dir, `src/app/manifest.ts`,
  `src/app/offline/`. Not portable.

---

## 3. How OmniRoute serves the built UI

There is **no separate Node static-file server**. Next.js *is* the app server, and
it serves its own output.

1. `next.config.mjs:16` — `const distDir = process.env.NEXT_DIST_DIR || ".build/next";`
3. `scripts/dev/run-standalone.mjs:31` — `const entry = existsSync("server-ws.mjs") ? "server-ws.mjs" : "server.js";`
   then `spawnWithForwardedSignals(process.execPath, [entry], …)`. **The
   `scripts/build/backendOnlyPages.mjs:108-110` — standalone is on unless
   `OMNIROUTE_SKIP_STANDALONE=1` or a contributor profile.
3. `scripts/build/assembleStandalone.mjs:10-22` documents the bundle layout —
   Next's `.next/standalone` → outDir, then `.next/static`, `public/`, plus
   native modules (`wreq-js`, `better-sqlite3`, `@swc/helpers`, `pino-*`, `split2`)
   and `src/lib/db/migrations` copied in beside them.
4. `scripts/dev/run-standalone.mjs:33` — `const entry = existsSync("server-ws.mjs") ? "server-ws.mjs" : "server.js";`
   then `spawnWithForwardedSignals(process.env.execPath, [entry], …)`. **The
   production entry point is Next's own `server.js`.**

**Consequence for a port.** OmniRoute's UI is *not* a static asset drop — it is
served by a Node runtime that also executes the API. `output: "export"` (full
static export) is **not configured** (`next.config.mjs` has no `output: "export"`)
and would not be free: 10 dashboard files do server-side data fetching
(`cookies()`/`headers()`/`await fetch` in non-`"use client"` modules, e.g.
`dashboard/settings/components/proxyRegistryData.ts`,
`dashboard/providers/components/onboarding/providerOnboardingApi.ts`), 8 pages
import `getTranslations` from `next-intl/server` (e.g.
`dashboard/chaos/page.tsx:4`, `dashboard/compression/page.tsx:2`), and
`src/i18n/request.ts` resolves locale from `cookies()`/`headers()`. Each is a
static-export blocker that would need removing.

---

## 4. Ship shape for artificial-route

Requirement: releases containing **only the compiled Rust binary + compiled
static frontend**, no external runtime deps.

**What a static frontend would need at build time** (fine — happens on the release
machine, never on the user's): Node ≥ the repo's `.node-version`, and the Next
16.3.5 toolchain (`next build` via `scripts/build/build-next-isolated.mjs`),
`@tailwindcss/postcss`, `postcss.config.mjs`, `next-intl`'s plugin. Note the build
is *heavy*: `dev` asks for 8 GB of V8 heap, and the release script
(`package.json:57`) does `rm -rf .build dist && npm run build && npm run build:cli`.

**At runtime** a pure static bundle needs: nothing but the Rust binary. `ar-server`
currently serves **no files at all** — no `ServeDir`, no `include_str!`, no
`rust-embed`, no `include_dir` anywhere in `crates/ar-server` (only
`tower_http::{limit,timeout}` layers, `app.rs:33-34`). Adding static serving
means either enabling `tower-http`'s `fs` feature (workspace `Cargo.toml:24`
currently pins `features = ["limit", "timeout"]`) or embedding.

**The blocker for either option.** Even with perfect static serving, the frontend
would land on a server exposing **17 routes**, and **503 of the 510 endpoints the
UI calls would 404**. A binary that embeds a dashboard whose every panel errors
is strictly worse than a binary with no dashboard. The port is gated on building
a control-plane API, not on the shipping mechanism — which is exactly what
`docs/05-roadmap.md:63` is pricing as "Out".

---

## 5. Size

No build output exists to measure: `node_modules/` is **absent**, and neither
`.build/` nor `.next/` exists in the checkout. A real `next build` here would need
a full `npm ci` first, so **the figures below are source-side estimates, labelled
as estimates.**

Source-side measurement (exact):

| Path | Bytes | Files |
|---|---|---|
| `src/app/(dashboard)/` | 6.84 MB | 855 |
| `src/i18n/messages/` (67 locales) | **73.11 MB** | 67 |
| `src/lib/` | 7.30 MB | 1125 |
| `public/` | 1.45 MB | 153 |
| `src/shared/components/` | 1.04 MB | 126 |
| `src/app/api/` | 4.9 MB (`du`, block-rounded) | 721 `route.ts` |

Derived estimates (`[INFERENCE]`):

- `public/` (1.45 MB) transfers near-verbatim into the served output.
- The 67-locale `messages/` tree is **not** browser payload — `next-intl` ships
  the active locale per request — but it is **73 MB of source** the build must
  read, and with the i18n UI already dropped (`00-overview.md:14`) it is dead
  weight for this port.
- A `.build/next/static` chunk tree for a 122-page Next 16 app of this source size
  is conventionally **15–40 MB** gzipped-ish before compression. `[INFERENCE]`
  from source size, not measured.

Embedded into the binary (`rust-embed`/`include_str!`), the whole output becomes
**part of the executable** — the `ar` binary grows by the full asset size and each
rebuild of the UI forces a full Rust relink. Shipped beside the binary, it stays
a separate artifact but the release is no longer "one file".

---

## Recommended ship shape

**Recommendation: do not port the UI. Keep `docs/00-overview.md:3` as it stands.**
If a UI is later approved, the concrete shape is:

> **`rust-embed` the `.build/next/static` output into a new `ar-ui` crate, served
> by a `tower-http` `ServeDir` on `include_dir!` (in-memory) mounted at `/`, with
> the existing `routes::not_found` fallback demoted to `/api`-only.** It is the
> only shape that keeps the release a single binary with no runtime dependency
> and no Node process.

Tradeoffs, 3 bullets:

1. **Binary bloat + relink coupling.** Embedded assets land in the executable, so
   every UI change forces a full Rust rebuild and pushes a multi-MB binary past
   the `ar` size a CLI-first tool advertises. Beside-binary (`dist/` next to the
   binary) avoids the relink but forfeits single-file release, which is the
   stated requirement.
2. **The build is a CI-only, heavyweight step.** Node ≥ `.node-version` +
   `next build` (dev asks for 8 GB V8 heap) must run on the release machine, and
   the 73 MB / 67-locale `src/i18n/messages/` tree should be stripped first —
   it is dead weight given `00-overview.md:14` drops i18n UI anyway.
3. **The real cost is the missing API, not the asset.** Serving the UI needs
   `ar-server` to grow from 17 routes to the ~50+ the *core* panels require
   (`/api/providers`, `/api/keys`, `/api/combos`, `/api/usage/*`, `/api/settings`,
   plus a session/CSRF authz pipeline and a WS route). That is a control plane
   with an opinionated data model, not a static-file mount — price it against
   `05-roadmap.md:63` before touching the shipping mechanism.
