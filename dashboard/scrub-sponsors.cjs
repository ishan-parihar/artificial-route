#!/usr/bin/env node
// scrub-sponsors.cjs — remove sponsor/partner branding from the vendored
// dashboard build WITHOUT breaking the embedded i18n JSON.
//
// Why this exists: the locale bundles ship as `JSON.parse('...')` string
// literals inside compiled chunks. A naive `s/"key":\{[^}]*\}//` regex stops
// at the first `}` — which lives *inside* a nested value (ICU plurals like
// `{days, plural, ...}`, `{initial}`, trend sub-objects) — and leaves the
// rest of the object dangling. That corrupted all 134 locale chunks
// (`Expected ',' or '}' after property value in JSON ...`), so every page
// using next-intl 500s. This script decodes each payload with a real parser,
// mutates the object, and re-encodes it.
//
// Scope (visible sponsor branding only — provider/model integrations such as
// the kimi/moonshot providers, Kimi combo presets and setup docs stay):
//   messages: delete kimiSponsorBanner + cheaperInferenceSponsorBanner
//     objects; blank the supporter badge/tooltip leaves, the partner-link
//     note, the supporter-offers page copy and the referral subtitle.
//   code: force the two sponsor banner components to render null, and force
//     the ProviderCard / ProviderPageHeader partner flags false (the message
//     fallbacks are hardcoded English sponsor copy, so blanking strings alone
//     is not enough).
//
// Usage: node scrub-sponsors.cjs <dist-root>
// Exits nonzero with the offending file list if anything fails to re-parse.

const fs = require("fs");
const path = require("path");

const DST = process.argv[2];
if (!DST) {
  console.error("usage: scrub-sponsors.cjs <dist-root>");
  process.exit(1);
}

// --- message transform ------------------------------------------------------

const DELETE_KEYS = new Set([
  "kimiSponsorBanner",
  "cheaperInferenceSponsorBanner",
]);

const BLANK_PATHS = [
  ["providers", "kimiOfficialSupporterBadge"],
  ["providers", "kimiOfficialSupporterTooltip"],
  ["providers", "cheaperInferenceSupporterBadge"],
  ["providers", "cheaperInferenceSupporterTooltip"],
  ["providers", "kimiPartnerLinkNote"],
  ["radarPage", "freeCreditsSubtitle"],
  ["radarIntelPage", "supporterBadge"],
];

function blankAllLeaves(obj) {
  let n = 0;
  (function walk(o) {
    if (Array.isArray(o)) return o.forEach(walk);
    if (o && typeof o === "object") {
      for (const k of Object.keys(o)) {
        if (typeof o[k] === "string") {
          if (o[k] !== "") {
            o[k] = "";
            n++;
          }
        } else walk(o[k]);
      }
    }
  })(obj);
  return n;
}

function scrubMessages(obj) {
  let touched = 0;
  for (const k of DELETE_KEYS) {
    if (obj && typeof obj === "object" && obj[k] !== undefined) {
      delete obj[k];
      touched++;
    }
  }
  for (const segs of BLANK_PATHS) {
    let o = obj;
    for (let i = 0; i < segs.length - 1; i++) {
      o = o && typeof o === "object" ? o[segs[i]] : undefined;
    }
    const last = segs[segs.length - 1];
    if (o && typeof o[last] === "string" && o[last] !== "") {
      o[last] = "";
      touched++;
    }
  }
  // Supporter-offers page: its whole purpose is partner benefits — empty it.
  if (obj && typeof obj.radarOffersPage === "object" && obj.radarOffersPage) {
    touched += blankAllLeaves(obj.radarOffersPage);
  }
  return touched;
}

const MARKERS = [
  "kimiSponsorBanner",
  "cheaperInferenceSponsorBanner",
  "kimiOfficialSupporter",
  "cheaperInferenceSupporter",
  "kimiPartnerLinkNote",
  "radarOffersPage",
];

function hasMarker(s) {
  return MARKERS.some((m) => s.includes(m));
}

// Scan a JS string literal starting at index i (s[i] is ' or "). Returns
// { literal, end } with end = index just past the closing quote.
function scanLiteral(s, i) {
  const q = s[i];
  let j = i + 1;
  while (j < s.length) {
    const c = s[j];
    if (c === "\\") {
      j += 2;
      continue;
    }
    if (c === q) return { literal: s.slice(i, j + 1), end: j + 1 };
    j++;
  }
  throw new Error("unterminated string literal");
}

function scrubChunkFile(file) {
  let code = fs.readFileSync(file, "utf8");
  if (!hasMarker(code)) return { file, changed: false };
  let out = "";
  let i = 0;
  let changed = false;
  let payloads = 0;
  for (;;) {
    const at = code.indexOf("JSON.parse(", i);
    if (at === -1) {
      out += code.slice(i);
      break;
    }
    let q = at + "JSON.parse(".length;
    while (q < code.length && /\s/.test(code[q])) q++;
    if (code[q] !== "'" && code[q] !== '"') {
      out += code.slice(i, q);
      i = q;
      continue;
    }
    const { literal, end } = scanLiteral(code, q);
    let decoded;
    try {
      decoded = new Function(`return ${literal};`)();
    } catch {
      out += code.slice(i, end);
      i = end;
      continue;
    }
    let obj;
    try {
      obj = JSON.parse(decoded);
    } catch {
      out += code.slice(i, end);
      i = end;
      continue;
    }
    if (!obj || typeof obj !== "object" || !hasMarker(decoded)) {
      out += code.slice(i, end);
      i = end;
      continue;
    }
    payloads++;
    scrubMessages(obj);
    // Re-encode as a double-quoted literal (JSON string syntax is valid JS).
    const fresh = JSON.stringify(JSON.stringify(obj));
    // Self-verify before accepting.
    JSON.parse(JSON.parse(fresh));
    out += code.slice(i, at) + "JSON.parse(" + fresh;
    i = end;
    changed = true;
    // skip the original closing paren handling: `end` already consumed the
    // literal; the existing `)` that follows stays in place via code.slice.
  }
  if (!changed) return { file, changed: false };
  // Whole-file verify: every JSON.parse payload must still decode.
  verifyChunk(out);
  fs.writeFileSync(file, out);
  return { file, changed: true, payloads };
}

function verifyChunk(code) {
  let i = 0;
  for (;;) {
    const at = code.indexOf("JSON.parse(", i);
    if (at === -1) return;
    let q = at + "JSON.parse(".length;
    while (q < code.length && /\s/.test(code[q])) q++;
    if (code[q] !== "'" && code[q] !== '"') {
      i = q;
      continue;
    }
    const { literal, end } = scanLiteral(code, q);
    try {
      JSON.parse(new Function(`return ${literal};`)());
    } catch {
      // Non-JSON payload (or undecodable literal) — left untouched above,
      // so skip it here too.
    }
    i = end;
  }
}

// --- code gates ---------------------------------------------------------------

const BANNERS = ["kimiSponsorBanner", "cheaperInferenceSponsorBanner"];

function scrubCodeGates(file) {
  let code = fs.readFileSync(file, "utf8");
  let changed = false;
  for (const name of BANNERS) {
    // Force the banner component's render expression falsy:
    //   ... useTranslations("name") ... return(0, jsx...)  →  return!1&&(0, ...
    const re = new RegExp(
      `useTranslations\\)\\((["'])${name}\\1\\)([\\s\\S]{0,600}?)return\\s*\\(\\s*0\\s*,`,
      "s"
    );
    if (re.test(code)) {
      // Keep the useTranslations(...) call intact; only poison the render
      // expression. Dropping the call would leave `(0,d.` dangling and break
      // the chunk's syntax.
      code = code.replace(re, (_m, q, mid) => `useTranslations)(${q}${name}${q})${mid}return!1&&(0,`);
      changed = true;
    }
  }
  // ProviderCard / ProviderPageHeader partner flags → always false.
  // Compiled form: (0,alias.isKimiPartnerProviderId)(...) — replace the callee
  // with a false thunk so the call itself yields false.
  for (const fn of ["isKimiPartnerProviderId", "isCheaperInferenceProviderId"]) {
    const re = new RegExp(`\\(0,[A-Za-z_$][\\w$]*\\.${fn}\\)`, "g");
    if (re.test(code)) {
      code = code.replace(re, "(()=>!1)");
      changed = true;
    }
  }
  if (!changed) return { file, changed: false };
  fs.writeFileSync(file, out_check(code, file));
  return { file, changed: true };
}

function out_check(code, file) {
  // Syntax check that tolerates ESM (node --check equivalent happens in the
  // rebrand gate; here just ensure we didn't truncate the file).
  if (code.length < 100) throw new Error(`refusing to write suspiciously short ${file}`);
  return code;
}

// --- walk ---------------------------------------------------------------------

function walkJsFiles(root, out) {
  for (const e of fs.readdirSync(root, { withFileTypes: true })) {
    if (e.name === "node_modules") continue;
    const p = path.join(root, e.name);
    if (e.isDirectory()) walkJsFiles(p, out);
    else if (
      e.isFile() &&
      (p.endsWith(".js") || p.endsWith(".cjs") || p.endsWith(".mjs")) &&
      !p.endsWith(".map")
    )
      out.push(p);
  }
}

const roots = [path.join(DST, ".build"), path.join(DST, "public")].filter((d) =>
  fs.existsSync(d)
);
const files = [];
for (const r of roots) walkJsFiles(r, files);

let msgChanged = 0;
let gateChanged = 0;
const errors = [];
for (const f of files) {
  try {
    const r1 = scrubChunkFile(f);
    if (r1.changed) msgChanged++;
    const r2 = scrubCodeGates(f);
    if (r2.changed) gateChanged++;
  } catch (e) {
    errors.push(`${f}: ${e.message}`);
  }
}

// Plain .json files carrying sponsor keys (defense in depth; manifests etc.).
function walkJson(root) {
  let out = [];
  for (const e of fs.readdirSync(root, { withFileTypes: true })) {
    if (e.name === "node_modules") continue;
    const p = path.join(root, e.name);
    if (e.isDirectory()) out = out.concat(walkJson(p));
    else if (
      e.isFile() &&
      p.endsWith(".json") &&
      !p.endsWith(".map") &&
      !p.endsWith(".nft.json")
    )
      out.push(p);
  }
  return out;
}
let jsonChanged = 0;
for (const r of roots) {
  for (const f of walkJson(r)) {
    try {
      const raw = fs.readFileSync(f, "utf8");
      if (!hasMarker(raw)) continue;
      const obj = JSON.parse(raw);
      if (scrubMessages(obj) > 0) {
        fs.writeFileSync(f, JSON.stringify(obj));
        jsonChanged++;
      }
    } catch (e) {
      errors.push(`${f}: ${e.message}`);
    }
  }
}

console.log(
  `[scrub] files=${files.length} messagePayloadsPatched=${msgChanged} codeGatesPatched=${gateChanged} jsonFilesPatched=${jsonChanged}`
);
if (errors.length) {
  console.error("[scrub] ERRORS:\n" + errors.join("\n"));
  process.exit(1);
}
