# Web UI port — the compiled-dashboard approach (implemented)

Status: **shipped**. `aroute dashboard` runs a rebranded, fully-compiled
OmniRoute web UI as a child Node process. This doc records what was built,
what was deliberately left alone, and how to regenerate the artifact.

## Why not a source fork

The original plan was to copy `src/app` + `src/shared` + `src/lib` into
`artificial-route/dashboard/` and rebrand at the source level. Investigation
killed it:

- The dashboard's surface is ~1,700 files under `src/app/` plus a ~1,100-file
  `src/lib` service layer, with 721 `/api/*` route handlers backed by
  OmniRoute's own database layer. A source fork means maintaining all of it.
- Every dashboard page is `force-dynamic` App Router SSR — `output: "export"`
  (pure static) is not possible, so some server must exist at runtime either way.
- OmniRoute already produces exactly the artifact the release wants: a
  `next build` standalone tree (`server.js` + traced `node_modules` + built
  `.next`) containing **zero TypeScript source**. Rebranding the compiled
  output is one pipeline step instead of a maintained fork.

So the shipped shape is: **OmniRoute compiles itself, we copy + rebrand the
compiled output, aroute supervises it.** OmniRoute's repo is never edited.

## The pipeline: `dashboard/rebrand-dist.sh`

Run `dashboard/update-dist.sh` to do the whole upstream refresh in one step —
pull `../OmniRoute` (`--ff-only --autostash`), `npm ci`, `npm run build`, then
`dashboard/rebrand-dist.sh`. The last step alone is enough when only the
rebrand rules changed. `dashboard/rebrand-dist.sh`:

1. Copies standalone essentials (`server.js`, `package.json`, `public/`,
   `node_modules/`, `.build/`, `migrations/`) into `dashboard/dist/`.
2. Applies targeted PWA-manifest patches (name → "Artificial Route",
   short_name → "aroute", English description) to the pre-rendered
   `manifest.webmanifest.body` and the compiled chunks carrying manifest data.
3. Applies the general display-text rebrand — capital `OmniRoute` →
   `Artificial Route` in compiled JS/JSON/HTML/CSS, guarded so identifiers,
   URLs, package names, and header names are untouched (see below).
4. **Sponsor scrub**: `dashboard/scrub-sponsors.cjs` deletes the Kimi and
   Cheaper Inference sponsor objects from every locale payload with a real
   JSON parser, blanks the supporter badge/tooltip strings, and forces the
   two banner components + the provider partner flags to render nothing.
   A previous regex version stopped at the first `}` inside nested ICU
   plurals and corrupted all 134 locale chunks; the parser version is what
   keeps `dist/` bootable.
5. **Parse gate**: `node --check` over all ~18,400 compiled JS files before and
   after the patch. Any file the patch breaks is restored and re-patched with
   quote-anchored-only (string-safe) substitutions. This gate is what makes a
   regex rebrand of minified code trustworthy: last run, 18,415 files parsed
   before, **zero failed after**.
6. Boot smoke on an OS-picked free port: healthz 200, manifest name
   `"Artificial Route"`, login page renders the brand.

The dist is a release artifact (≈2.0 GB, dominated by the traced
`node_modules`) and is **gitignored**; the script is the reproducible source
of truth.

## What is rebranded vs. deliberately retained

Rebranded (user-visible): page titles, i18n sentences in every locale, the PWA
manifest, service-worker notification titles, SVG brand text, the OpenAPI
description, `package.json` name.

Retained on purpose (internal identifiers — renaming them in compiled JS
risks runtime breakage for zero visible gain, and each must stay consistent
with `node_modules` layout, assembled scripts, or wire formats):

| Identifier | Why it stays |
|---|---|
| `omniroute` (lowercase) | localStorage keys, env hints, collection names |
| `sk_omniroute` | the API-key prefix the server generates and shows |
| `OmniRoute/1.0` | User-Agent wire string sent upstream |
| `x-omniroute-*` headers | response-header contract clients may read |
| `@omniroute/*` | npm namespace — must match `node_modules` layout |
| `OMNIROUTE_*` env vars | read by server.js and assembled runtime scripts |
| `~/.omniroute` | the child's data-dir fallback — `aroute dashboard` overrides it with `$DATA_DIR` |
| GitHub repo URLs | link targets to the real upstream repo |

## The `aroute dashboard` verb

`crates/ar-cli/src/dashboard.rs` resolves the dist (`--path` →
`$AR_DASHBOARD_DIR` → `dashboard/dist` beside the binary), then spawns
`node peer-stamp-launcher.cjs` with:

- `PORT` — the `--port` flag (default 20149; `aroute serve` keeps 20128)
- `HOSTNAME=127.0.0.1` — loopback only; the dashboard manages credentials
- `DATA_DIR` — `$DATA_DIR` if set, else `~/.config/ar/dashboard-data`, so the
  rebranded product never writes into OmniRoute's own `~/.omniroute`

The launcher restores the compiled UI's own peer-stamp contract before handing
control to `server.js` (without it, every request — including loopback — is
classified as remote and the wizard demands the bootstrap token from the log)
and warms the `/v1/models` catalog cache after boot so the first page load does
not pay the ~3 s catalog build.

It blocks until the child exits and propagates failure. Same process group, so
an interactive Ctrl-C stops both; under systemd, `KillMode` handles it.

## Release shape

- `aroute` (Rust binary, ~15 MB) — the `/v1` proxy API
- `dashboard/dist/` (compiled web UI + its Node runtime) — served by the
  `aroute dashboard` verb, not embedded in the binary (2 GB does not fit a
  "lightweight" binary; the dist is the "node compiled binary" deliverable)

No TypeScript, no `npm` and no network access at runtime; the only runtime
prerequisite is a `node` binary on PATH.
