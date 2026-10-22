#!/usr/bin/env node
// vendor-base-path-patcher.cjs — vendor OmniRoute's runtime base-path patcher
// into a dashboard dist.
//
// Upstream's `scripts/docker/patch-standalone-base-path.mjs` rewrites a
// root-built Next.js standalone bundle so it serves under a subpath, and
// `scripts/docker/ensure-docker-base-path.mjs` wraps it with the
// `BUILD_OMNIROUTE_BASE_PATH` sentinel contract (compare runtime env against
// what the image was built for; refuse a mismatch; write the sentinel after a
// patch). The vendored pipeline DROPS `scripts/docker/` from the dist copy
// (rebrand-dist.sh copies server.js/package.json/public/node_modules/.build/
// migrations only), so the patcher must be vendored in explicitly (docs/41
// §4: "the patcher must be copied into the tarball").
//
// The patcher imports one thing, `normalizeBasePath` from
// `../build/normalizeBasePath.mjs` — a relative path that will not resolve
// once the file sits in a dist root. Rather than shipping a second file and a
// directory layout to hold it, the normalizer (9 lines, pure) is INLINED over
// the import. The vendored file is then self-contained: `node
// patch-base-path.mjs` works from the dist root with no other files from the
// upstream repo.
//
// Usage: node vendor-base-path-patcher.cjs <omni-root> <dest-file>
"use strict";

const fs = require("node:fs");
const path = require("node:path");

const [omniRoot, destFile] = process.argv.slice(2);
if (!omniRoot || !destFile) {
  console.error("usage: node vendor-base-path-patcher.cjs <omni-root> <dest-file>");
  process.exit(1);
}

const src = path.join(omniRoot, "scripts", "docker", "patch-standalone-base-path.mjs");
if (!fs.existsSync(src)) {
  console.error(`[vendor-patcher] no patcher at ${src} — is the OmniRoute checkout current?`);
  process.exit(1);
}

// The normalizer, inlined verbatim from scripts/build/normalizeBasePath.mjs
// so the vendored patcher has no relative imports. If upstream's copy ever
// drifts from this one, the assertion below fails the vendoring loudly
// instead of shipping a stale inlining.
const NORMALIZER_SRC = path.join(omniRoot, "scripts", "build", "normalizeBasePath.mjs");
const normalizerRaw = fs.readFileSync(NORMALIZER_SRC, "utf8");
const normalizeMatch = /export function normalizeBasePath\(value\) \{[\s\S]*?\n\}/.exec(normalizerRaw);
if (!normalizeMatch) {
  console.error("[vendor-patcher] cannot find `export function normalizeBasePath` in the upstream normalizer — the inlining needs a review");
  process.exit(1);
}

const inlinedNormalizer = normalizeMatch[0].replace(
  "export function normalizeBasePath",
  "function normalizeBasePath",
);

const patcherRaw = fs.readFileSync(src, "utf8");
const importLine = /import \{ normalizeBasePath \} from "[^"]+";\n/;
if (!importLine.test(patcherRaw)) {
  console.error("[vendor-patcher] the upstream patcher no longer imports normalizeBasePath — the inlining needs a review");
  process.exit(1);
}

const banner = `// patch-base-path.mjs — vendored from OmniRoute's
// scripts/docker/patch-standalone-base-path.mjs, with the
// \`BUILD_OMNIROUTE_BASE_PATH\` sentinel contract of
// scripts/docker/ensure-docker-base-path.mjs and normalizeBasePath inlined.
// One-way by design: run ONCE at install (root -> the target subpath); a
// reverse patch is the installer's .old swap, not this file.
`;

const out = banner + patcherRaw.replace(importLine, `${inlinedNormalizer}\n`);
fs.mkdirSync(path.dirname(destFile), { recursive: true });
fs.writeFileSync(destFile, out);
console.log(`[vendor-patcher] wrote ${destFile} (${out.length} bytes)`);
