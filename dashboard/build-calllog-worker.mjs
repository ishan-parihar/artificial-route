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

// Paths are resolved RELATIVE TO THIS SCRIPT (rebrand-dist.sh may run from
// an arbitrary cwd), and the upstream checkout is overridable: an explicit
// argument wins, because rebrand-dist.sh passes its own $OMNI_ROOT (which the
// operator can redirect, and which is the same tree the dist is copied from).
// Deriving a different path here would compile the worker from a different
// checkout than the dist it lands in, so the default is only a fallback.
//
// Usage: node build-calllog-worker.mjs [upstreamRoot] <output.js>
//
// The two-argument form (used by rebrand-dist.sh) is told apart from the
// one-argument stand-alone form by the suffix of the second-to-last argument:
// an OUTPUT ends in .js, an UPSTREAM ROOT never does. Deciding by position
// alone silently mis-parsed one of the two forms, so the suffix decides.
const HERE = path.dirname(fileURLToPath(import.meta.url));
const AR_ROOT = path.resolve(HERE, "..");
const argv = process.argv.slice(2);
const isOutputPath = (p) => /\.(?:cjs|mjs|js)$/i.test(p);
const upstreamArg = argv.length >= 2 && !isOutputPath(argv[argv.length - 2]) ? argv[argv.length - 2] : null;
const out = argv[argv.length - 1];
if (!out) {
  console.error("usage: node build-calllog-worker.mjs [upstreamRoot] <output.js>");
  process.exit(2);
}
const REPO = path.resolve(
  // A relative upstream root follows shell convention (resolve from the cwd);
  // an omitted one falls back to the sibling of the script's parent directory.
  upstreamArg ?? path.join(AR_ROOT, "../external-repos/OmniRoute"),
);
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

// The output directory does not exist in a freshly copied dist (src/ is this
// fix's own addition), so it is created rather than letting the write fail.
fs.mkdirSync(path.dirname(path.resolve(out)), { recursive: true });

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
  // No absolute build path ends up in the comment headers. See the post-pass
  // below for why that is a normalisation step rather than an esbuild option.
  logLevel: "warning",
});

// esbuild has no option to suppress the per-module provenance banners it emits
// inside each __commonJS wrapper (`// <path-to-source>.ts`). Those paths are
// computed RELATIVE TO THE OUTFILE, so the banner text — and therefore the
// output hash — changes with where (and from which cwd) the bundle is written:
// the repo build lands in $DST/src/lib/usage while an ad-hoc build lands in a
// scratch dir, and the two never matched. They also ship the build machine's
// absolute path inside a file that runs in a user's home directory. So the
// banner lines are dropped and the result asserted clean and stable.
//
// Only FULL-LINE comments are matched, so a path that happened to appear inside
// a string literal or a trailing comment is left alone.
function stripModulePathHeaders(file) {
  const lines = fs.readFileSync(file, "utf8").split("\n");
  const kept = lines.filter((line) => !/^\/\/ \S*\.(ts|tsx|js|jsx|mjs|cjs)(#\S+)?$/.test(line));
  const removed = lines.length - kept.length;
  if (removed > 0) {
    fs.writeFileSync(file, kept.join("\n"));
  }
  return removed;
}

const removed = stripModulePathHeaders(out);
if (removed > 0) {
  process.stdout.write(`[calllog-worker] normalised ${removed} esbuild path banner(s)\n`);
}

// A leaked absolute path is a real defect (it ships the builder's home layout),
// so it fails the build rather than being noticed later by a reader. The check is
// deliberately narrow: the bundle legitimately carries esbuild's `__toESM`
// explanatory comments, so "no // at all" would false-positive on a correct
// bundle. What must not appear is a whole-line banner carrying a path.
if (fs.readFileSync(out, "utf8").match(/^\/\/ \S*\//m)) {
  console.error("[calllog-worker] FATAL: the bundle still carries an absolute-path banner");
  process.exit(1);
}
