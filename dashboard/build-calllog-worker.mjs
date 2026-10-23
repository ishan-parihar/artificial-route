// F15: compile OmniRoute's call-log artifact worker into a self-contained
// CommonJS bundle the installed dist can spawn.
//
// The dashboard's writer traces the worker's module location from the bundled
// writer chunk (e.F("src/lib/usage/callLogArtifactWriter.ts")) and tries, in
// order: <moduleDir>/callLogArtifactWorker.js, <cwd>/src/lib/usage/….js,
// <cwd>/src/lib/usage/….ts with tsx, then <moduleDir>/….ts with tsx. A dist
// that ships no `src/` tree and no `tsx` resolves none of them, so every
// artifact write failed with "Cannot find module …callLogArtifactWorker.js"
// and the queue silently dropped (docs/28 gap F15).
//
// The fix is to ship the FIRST candidate: a bundle placed exactly where the
// resolver looks first, so no tsx/execArgv path is ever taken. The worker's
// entire closure (callLogArtifacts → dataPaths + logEnv + size-limit markers)
// imports only node builtins plus one type-only open-sse import, which esbuild
// erases — so "external" can stay limited to node: builtins and the bundle is
// fully self-contained.
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { createRequire } from "node:module";

// Resolve the upstream checkout RELATIVE TO THIS SCRIPT, not the cwd: the
// packager is invoked by rebrand-dist.sh from an arbitrary directory, and a
// cwd-relative path silently picked up nothing (the first attempt resolved
// `artificial-route/external-repos/…`, which does not exist).
const HERE = path.dirname(fileURLToPath(import.meta.url));
const AR_ROOT = path.resolve(HERE, "..");
const REPO = path.resolve(AR_ROOT, "../external-repos/OmniRoute");
const SRC_ROOT = path.join(REPO, "src");

// Fail loudly rather than deep inside esbuild: the packager's two hard inputs
// are the upstream SOURCE (for callLogArtifactWorker.ts and its closure) and
// esbuild itself, and a missing one is otherwise reported as an opaque
// "Cannot find module" from a helper path.
const entryFile = path.join(SRC_ROOT, "lib/usage/callLogArtifactWorker.ts");
if (!fs.existsSync(entryFile)) {
  console.error(
    `[calllog-worker] FATAL: upstream worker source not found at ${entryFile}\n` +
    "  (the upstream checkout must have its src/ tree; see the Prerequisites in rebrand-dist.sh)",
  );
  process.exit(1);
}

// esbuild lives in the upstream checkout's own node_modules: this repo ships no
// dependencies of its own, and the packager must use the same toolchain the
// upstream build used rather than a globally installed one.
let esbuildEntry;
try {
  const requireUpstream = createRequire(path.join(REPO, "package.json"));
  esbuildEntry = requireUpstream.resolve("esbuild");
} catch {
  console.error(
    "[calllog-worker] FATAL: cannot resolve esbuild from the upstream checkout\n" +
    `  ${REPO}\n` +
    "  run once: (cd " + REPO + " && npm install)",
  );
  process.exit(1);
}
const { build } = await import(pathToFileURL(esbuildEntry).href);

// The same alias map the app's own tsconfig uses. Without `@/` and
// `@omniroute/…` the two logEnv/dataPaths imports cannot resolve.
const alias = {
  "@": SRC_ROOT,
  "@/lib": path.join(SRC_ROOT, "lib"),
  "@omniroute/open-sse": path.resolve(REPO, "open-sse"),
};

const out = process.argv[2];
if (!out) {
  console.error("usage: node build-calllog-worker.mjs <output.js>");
  process.exit(2);
}

await build({
  entryPoints: [path.join(SRC_ROOT, "lib/usage/callLogArtifactWorker.ts")],
  outfile: out,
  bundle: true,
  platform: "node",
  // A plain .js in a dist with no "type":"module" is CommonJS. The resolver
  // passes it to new Worker(pathToFileURL(workerFile)) with NO execArgv on the
  // .js branch, so it must not be ESM.
  format: "cjs",
  target: "node20",
  // Node builtins stay external so the worker thread resolves them natively;
  // everything else is inlined, which is what makes the dist self-contained.
  // Both spellings are listed: the worker imports `node:worker_threads`, while
  // dataPaths/logEnv import the bare `fs`/`path`/`os` names, and esbuild treats
  // the two forms as distinct specifiers.
  external: [
    "node:*",
    "node:fs", "node:path", "node:os", "node:worker_threads",
    "fs", "path", "os", "worker_threads",
  ],
  alias,
  resolveExtensions: [".ts", ".tsx", ".mjs", ".js", ".jsx", ".json", ".node"],
  // Keep names readable in stack traces; size is irrelevant at this scale.
  minify: false,
  sourcemap: false,
  logLevel: "info",
});
