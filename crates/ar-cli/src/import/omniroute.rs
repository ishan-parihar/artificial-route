//! Reads OmniRoute's provider registry TypeScript into an [`Imported`].
//!
//! The catalog is generated, never hand-written: `crates/ar-registry/src/registry.json`
//! is whatever this module extracts from
//! `../OmniRoute/open-sse/config/providers/{index.ts,shared.ts,registry/*/index.ts}`
//! plus the price table under `../OmniRoute/src/shared/constants/pricing/*`.
//!
//! # What a TypeScript reader has to survive
//!
//! These files are data with four escape hatches, and each one is a way a naive
//! regex parse silently produces a *shorter* catalog rather than an error:
//!
//! * `buildOpenAiCompatibleRegistryEntry({...})` — a constructor supplying
//!   `format: "openai"`, `executor: "default"`, `authType: "apikey"`,
//!   `authHeader: "bearer"` (shared.ts). Fifty-odd providers declare nothing but
//!   `id`, `baseUrl` and `models`; without the defaults they parse as blank.
//! * `...KIMI_CODING_SHARED` — a spread of a `const` in the same file
//!   (`kimi/coding-apikey/index.ts`) or an imported one (`kimi/coding/index.ts`).
//! * `...buildModels([...])` and `...KIMI_CODING_MODELS` — the model list arrives
//!   as an identifier, an array of ids, or an array of objects.
//! * `` `${POE_DEFAULT_BASE_URL}/chat/completions` `` — a base URL assembled from
//!   a template literal over another `const`.
//!
//! So the reader is a small expression evaluator, not a regex sweep: it resolves
//! `const` chains within a file, follows relative imports one level, and treats a
//! value it cannot resolve as *absent* rather than guessing. A provider missing
//! its base URL is reported by name so the gap is visible instead of shipping a
//! catalog entry that cannot route.
//!
//! # The parser's boundaries
//!
//! Nothing here executes TypeScript or shells out: it reads bytes and
//! interprets a data subset. A construct outside that subset loses one field
//! rather than running code, and every such loss is reported by provider name so
//! a short catalog is a visible defect instead of a silent one.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use ar_config::{Combo, Strategy};
use ar_core::Strng;
use ar_registry::free::{FreeBudgetRow, FreeBudgets, FreeRegime};
use ar_registry::meta::ProviderMeta;
use ar_registry::{AuthClass, Price, ProviderDef, wire_format};

use crate::commands::fail;
use crate::import::{Imported, render_yaml};

/// Read the OmniRoute provider tree rooted at `providers_dir`.
///
/// `providers_dir` is `OmniRoute/open-sse/config/providers`. The price table lives
/// outside it (`OmniRoute/src/shared/constants/pricing`), so it is looked up as a
/// sibling of the two parents that reach it; a missing price tree is not an
/// error, because a registry with no prices is still a registry.
pub fn scan(providers_dir: &Path) -> anyhow::Result<Imported> {
    let root = read_tree(providers_dir)?;
    let prices = pricing(providers_dir);
    let flat_rate = flat_rate_ids(providers_dir);
    let free = free_budgets(providers_dir);

    let mut defs: BTreeMap<Strng, ProviderDef> = BTreeMap::new();
    let mut metas: BTreeMap<Strng, ProviderMeta> = BTreeMap::new();
    let mut notes: Vec<String> = Vec::new();

    for (_, src) in &root.files {
        // The file's own bindings win; everything it imports resolves through
        // the tree-wide index.
        let consts = root.lookup(&Consts::of(src));
        for entry in entries(src) {
            let Some(mut def) = to_def(&entry, &consts, &root) else {
                continue;
            };
            let id = Strng::from(entry.id.as_str());
            // The short id resolves here, so the flat-rate match reads the
            // metadata rather than the definition: upstream names `cc` in the
            // same set as `claude`, and the two are one provider.
            let meta = to_meta(&entry, &consts, &root);
            def.flat_rate =
                flat_rate.contains(entry.id.as_str()) || flat_rate.contains(meta.alias.as_ref());
            metas.entry(id.clone()).or_insert(meta);
            for ((p, m), price) in &prices {
                if *p == entry.id {
                    def.prices.insert(Strng::from(m.as_str()), *price);
                }
            }
            match defs.get(&id) {
                Some(kept) if kept.base_url != def.base_url => {
                    notes.push(format!(
                        "{id} redeclared with a different base URL; kept {}",
                        kept.base_url
                    ));
                }
                Some(_) => {}
                None => {
                    if def.base_url.is_empty() {
                        notes.push(format!(
                            "{id} has no resolvable base URL; listed but unroutable"
                        ));
                    }
                    defs.insert(id, def);
                }
            }
        }
    }

    if defs.is_empty() {
        return Err(fail(
            format!("no provider entries under {}", providers_dir.display()),
            "pass --path <OmniRoute/open-sse/config/providers>; the reader looks for registry/*/index.ts",
        ));
    }
    for note in &notes {
        eprintln!("note: {note}");
    }

    let combos = combos(&defs);
    let config_yaml = render_yaml(&defs, &combos);
    Ok(Imported {
        registry: defs,
        combos,
        config_yaml,
        free_budgets: free,
        provider_meta: metas,
    })
}

/// The parsed provider tree: every `.ts` file under `providers_dir`, plus a flat
/// index of every `const` in any of them and in the two directories above.
///
/// The index exists because a spread may name a `const` declared in a sibling
/// file (`kimi-coding-apikey/index.ts` spreads `KIMI_CODING_SHARED` from
/// `../coding/index.ts`), and because a base URL may be a `const` from a file
/// outside the tree entirely (`antigravityUpstream.ts`, `museCode.ts`).
/// Resolving that per entry by re-scanning is quadratic over ~250 files;
/// indexing once makes it a lookup. The names are distinctive, so
/// first-declaration-wins is enough.
struct Tree {
    files: Vec<(PathBuf, String)>,
    consts: Consts,
}

impl Tree {
    #[cfg(test)]
    fn empty() -> Self {
        Self {
            files: Vec::new(),
            consts: Consts::default(),
        }
    }

    /// The const table to resolve against: this file's own bindings shadow the
    /// tree-wide index, so a local `const BASE = ...` still wins.
    fn lookup(&self, local: &Consts) -> Consts {
        if local.0.is_empty() {
            return self.consts.clone();
        }
        let mut merged = self.consts.0.clone();
        for (k, v) in &local.0 {
            merged.insert(k.clone(), v.clone());
        }
        Consts(merged)
    }
}

fn read_tree(dir: &Path) -> anyhow::Result<Tree> {
    let mut files = Vec::new();
    walk(dir, &mut files, 0)?;
    let up: Vec<PathBuf> = [
        dir.parent().map(Path::to_path_buf),
        dir.parent().and_then(Path::parent).map(Path::to_path_buf),
    ]
    .into_iter()
    .flatten()
    .collect();
    for dir in up {
        collect_siblings(&dir, &mut files);
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut all = BTreeMap::new();
    for (_, src) in &files {
        for (name, value) in bindings(src) {
            all.entry(name).or_insert(value);
        }
    }
    Ok(Tree {
        files,
        consts: Consts(all),
    })
}

/// Reads the `.ts` files directly in `dir`, skipping subdirectories.
fn collect_siblings(dir: &Path, out: &mut Vec<(PathBuf, String)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        if path.is_file()
            && path.extension().is_some_and(|x| x == "ts")
            && let Ok(src) = std::fs::read_to_string(&path)
        {
            out.push((path, strip_comments(&src)));
        }
    }
}

fn walk(dir: &Path, out: &mut Vec<(PathBuf, String)>, depth: usize) -> anyhow::Result<()> {
    // Bounded so a symlink loop is a clear error rather than a hang.
    if depth > 8 {
        return Ok(());
    }
    let entries = std::fs::read_dir(dir).map_err(|e| {
        fail(
            format!("cannot read {}: {e}", dir.display()),
            "pass --path <OmniRoute/open-sse/config/providers>",
        )
    })?;
    for e in entries.flatten() {
        let path = e.path();
        if path.is_dir() {
            walk(&path, out, depth + 1)?;
        } else if path.extension().is_some_and(|x| x == "ts")
            && let Ok(src) = std::fs::read_to_string(&path)
        {
            out.push((path, strip_comments(&src)));
        }
    }
    Ok(())
}

/// `//` and `/* */` removal, quote-aware.
///
/// A block comment in this tree documents a URL that looks exactly like a base
/// URL, so a reader that does not strip comments reads prose as configuration.
fn strip_comments(src: &str) -> String {
    let b: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    let mut quote: Option<char> = None;
    while i < b.len() {
        let c = b[i];
        if let Some(q) = quote {
            out.push(c);
            if c == '\\' && i + 1 < b.len() {
                out.push(b[i + 1]);
                i += 2;
                continue;
            }
            if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match c {
            '"' | '\'' | '`' => {
                quote = Some(c);
                out.push(c);
                i += 1;
            }
            '/' if i + 1 < b.len() && b[i + 1] == '/' => {
                while i < b.len() && b[i] != '\n' {
                    i += 1;
                }
            }
            '/' if i + 1 < b.len() && b[i + 1] == '*' => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == '*' && b[i + 1] == '/') {
                    i += 1;
                }
                i = (i + 2).min(b.len());
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// The `const` bindings in one file, name → initialiser text.
#[derive(Default, Clone)]
struct Consts(BTreeMap<String, String>);

impl Consts {
    fn of(src: &str) -> Self {
        let mut out = BTreeMap::new();
        for (name, value) in bindings(src) {
            out.insert(name, value);
        }
        Self(out)
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }
}

/// Every `const NAME = <expr>` in `src`, paired with its balanced initialiser.
fn bindings(src: &str) -> Vec<(String, String)> {
    let b: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if !starts_word(&b, i, "const") {
            i += 1;
            continue;
        }
        let mut j = i + 5;
        while j < b.len() && (b[j] == ' ' || b[j] == '\n' || b[j] == '\t' || b[j] == '\r') {
            j += 1;
        }
        let name_start = j;
        while j < b.len() && (b[j].is_alphanumeric() || b[j] == '_') {
            j += 1;
        }
        if j == name_start {
            i = j;
            continue;
        }
        let name: String = b[name_start..j].iter().collect();
        // `=`, skipping a type annotation.
        while j < b.len() && b[j] != '=' && b[j] != ';' && b[j] != '\n' {
            j += 1;
        }
        if j >= b.len() || b[j] != '=' {
            i = j.max(i + 1);
            continue;
        }
        j += 1;
        while j < b.len() && b[j] == ' ' {
            j += 1;
        }
        let Some(end) = balanced_end(&b, j) else {
            i = j;
            continue;
        };
        out.push((name, b[j..end].iter().collect()));
        i = end;
    }
    out
}

/// True when `word` starts at `i` and is not part of a longer identifier.
fn starts_word(b: &[char], i: usize, word: &str) -> bool {
    let w: Vec<char> = word.chars().collect();
    if i + w.len() > b.len() || b[i..i + w.len()] != w[..] {
        return false;
    }
    let before_ok = i == 0 || !(b[i - 1].is_alphanumeric() || b[i - 1] == '_');
    let after = i + w.len();
    let after_ok = after >= b.len() || !(b[after].is_alphanumeric() || b[after] == '_');
    before_ok && after_ok
}

/// Index just past the value starting at `i`, balancing brackets and skipping
/// string bodies. `None` when the value never closes.
///
/// A scalar value (`"x"`, `42`, `SOME_CONST`) opens no bracket, so its end is
/// the next top-level `,` or closing bracket — otherwise a `}` would drive the
/// depth counter below zero and swallow the rest of the object.
fn balanced_end(b: &[char], i: usize) -> Option<usize> {
    if i >= b.len() {
        return None;
    }
    if matches!(b[i], '"' | '\'' | '`') {
        return Some(skip_string(b, i));
    }
    if !matches!(b[i], '{' | '[' | '(') {
        // A scalar, which may still contain brackets: `buildModels(["a", "b"])`.
        // Depth tracking stops the scan at the *top-level* comma, not the one
        // inside the call's argument list.
        let mut depth = 0usize;
        let mut j = i;
        while j < b.len() {
            match b[j] {
                '"' | '\'' | '`' => {
                    j = skip_string(b, j);
                    continue;
                }
                '{' | '[' | '(' => depth += 1,
                '}' | ']' | ')' => {
                    if depth == 0 {
                        return Some(j);
                    }
                    depth -= 1;
                }
                ',' | '\n' if depth == 0 => return Some(j),
                _ => {}
            }
            j += 1;
        }
        return Some(j);
    }
    let mut depth = 0usize;
    let mut j = i;
    let mut quote: Option<char> = None;
    while j < b.len() {
        let c = b[j];
        if let Some(q) = quote {
            if c == '\\' {
                j += 2;
                continue;
            }
            if c == q {
                quote = None;
            }
            j += 1;
            continue;
        }
        match c {
            '"' | '\'' | '`' => quote = Some(c),
            '{' | '[' | '(' => depth += 1,
            '}' | ']' | ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(j + 1);
                }
            }
            _ => {}
        }
        j += 1;
    }
    None
}

/// One registry entry: its id and the object literal carrying its fields.
struct Entry {
    id: String,
    object: String,
    /// `true` when the literal came from `buildOpenAiCompatibleRegistryEntry({...})`.
    defaulted: bool,
}

/// The provider entries declared in one file.
fn entries(src: &str) -> Vec<Entry> {
    let b: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if !starts_word(&b, i, "const") {
            i += 1;
            continue;
        }
        let mut j = i + 5;
        while j < b.len() && b[j].is_whitespace() {
            j += 1;
        }
        let ns = j;
        while j < b.len() && (b[j].is_alphanumeric() || b[j] == '_') {
            j += 1;
        }
        let name: String = b[ns..j].iter().collect();
        while j < b.len() && b[j] != '=' && b[j] != ';' && b[j] != '\n' {
            j += 1;
        }
        if j >= b.len() || b[j] != '=' {
            i = j.max(i + 1);
            continue;
        }
        j += 1;
        while j < b.len() && b[j] == ' ' {
            j += 1;
        }
        let Some(end) = balanced_end(&b, j) else {
            break;
        };
        let literal: String = b[j..end].iter().collect();
        let Some((object, defaulted)) = entry_object(&literal) else {
            i = end;
            continue;
        };
        let Some(id) = string_field(&object, "id") else {
            i = end;
            continue;
        };
        // A file can declare `*_SHARED` (no `id`) and `*Provider` (with one); the
        // `id` check is what separates them, so no name convention is needed.
        if !id.is_empty() && name.ends_with("Provider") {
            out.push(Entry {
                id,
                object,
                defaulted,
            });
        }
        i = end;
    }
    out
}

/// The object literal behind an entry's initialiser.
///
/// `buildOpenAiCompatibleRegistryEntry({...})` yields the inner literal and
/// `defaulted = true`; a bare `{...}` yields itself.
fn entry_object(literal: &str) -> Option<(String, bool)> {
    let t = literal.trim();
    if let Some(open) = t.find('{') {
        if t.starts_with("buildOpenAiCompatibleRegistryEntry") && open < 40 {
            let b: Vec<char> = t.chars().collect();
            let end = balanced_end(&b, open)?;
            return Some((b[open..end].iter().collect(), true));
        }
        return Some((t.to_owned(), false));
    }
    None
}

/// A string-valued field of an object literal, by name.
///
/// Returns `None` when the field holds an identifier or a call rather than a
/// literal: the caller resolves or gives up rather than storing the expression.
fn string_field(object: &str, name: &str) -> Option<String> {
    value_of(object, name).and_then(|v| as_literal(&v))
}

/// The raw initialiser text of a top-level field.
fn value_of(object: &str, name: &str) -> Option<String> {
    fields(object)
        .into_iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v)
}

/// The top-level fields of an object literal, as `(key, raw value)` pairs.
///
/// A `...NAME` spread contributes a key of `..NAME` so the caller can resolve it
/// against a `const`; anything else is reported as absent.
///
/// Walks with an explicit key/value alternation rather than trying to recognise
/// keys positionally: in TypeScript both a key and a string value are `"..."`,
/// so a scan cannot tell them apart without tracking which side of the `:` it is
/// on.
fn fields(object: &str) -> Vec<(String, String)> {
    let b: Vec<char> = object.chars().collect();
    let mut out = Vec::new();
    let mut i = skip_ws(&b, 1);
    while i < b.len() && b[i] != '}' {
        if b[i] == '.' {
            // A spread is `...NAME` with no colon: the value is the name.
            i = skip_ws(&b, i + 3);
            let start = i;
            while i < b.len() && (b[i].is_alphanumeric() || b[i] == '_') {
                i += 1;
            }
            if i == start {
                break;
            }
            out.push(("..".to_owned(), b[start..i].iter().collect()));
            i = skip_ws(&b, i);
            if i < b.len() && (b[i] == '(' || b[i] == '[') {
                // `...buildModels(["a"])` — the spread target is a call.
                if let Some(end) = balanced_end(&b, i) {
                    out.push(("..call".to_owned(), b[i..end].iter().collect()));
                    i = skip_ws(&b, end);
                }
            }
            if i < b.len() && (b[i] == ',' || b[i] == ';') {
                i += 1;
            }
            i = skip_ws(&b, i);
            continue;
        }
        let key = if matches!(b[i], '"' | '\'' | '`') {
            let (key, next) = read_quoted(&b, i);
            i = next;
            key
        } else {
            let start = i;
            while i < b.len() && b[i] != ':' && !b[i].is_whitespace() {
                i += 1;
            }
            b[start..i].iter().collect()
        };
        i = skip_ws(&b, i);
        if i >= b.len() || b[i] != ':' {
            // A shorthand property or malformed literal; step over it rather
            // than re-reading the same characters forever.
            i = skip_to_next_key(&b, i);
            continue;
        }
        i = skip_ws(&b, i + 1);
        let Some(end) = balanced_end(&b, i) else {
            break;
        };
        out.push((key, b[i..end].iter().collect()));
        i = skip_ws(&b, end);
        if i < b.len() && (b[i] == ',' || b[i] == ';') {
            i += 1;
        }
        i = skip_ws(&b, i);
    }
    out
}

fn skip_ws(b: &[char], mut i: usize) -> usize {
    while i < b.len() && b[i].is_whitespace() {
        i += 1;
    }
    i
}

/// Steps past one value so a key with no `: value` cannot stall the walk.
fn skip_to_next_key(b: &[char], i: usize) -> usize {
    let mut depth = 0usize;
    let mut j = i;
    while j < b.len() {
        match b[j] {
            '{' | '[' | '(' => depth += 1,
            '}' | ']' | ')' => {
                if depth == 0 {
                    return j;
                }
                depth -= 1;
            }
            ',' if depth == 0 => return j + 1,
            '"' | '\'' | '`' => {
                j = skip_string(b, j);
                continue;
            }
            _ => {}
        }
        j += 1;
    }
    j
}

/// The contents of the string literal at `i`, plus the index just past it.
fn read_quoted(b: &[char], i: usize) -> (String, usize) {
    let q = b[i];
    let mut j = i + 1;
    let mut out = String::new();
    while j < b.len() {
        if b[j] == '\\' && j + 1 < b.len() {
            out.push(b[j + 1]);
            j += 2;
            continue;
        }
        if b[j] == q {
            return (out, j + 1);
        }
        out.push(b[j]);
        j += 1;
    }
    (out, b.len())
}

/// The index just past the string literal starting at `i`.
fn skip_string(b: &[char], i: usize) -> usize {
    let mut j = i + 1;
    while j < b.len() {
        if b[j] == '\\' {
            j += 2;
            continue;
        }
        if b[j] == b[i] {
            return j + 1;
        }
        j += 1;
    }
    b.len()
}

/// The string a literal expression denotes, with `${CONST}` interpolation
/// resolved against `consts`. Backticks and quotes both.
fn as_literal(expr: &str) -> Option<String> {
    let t = expr.trim();
    if t.len() < 2 {
        return None;
    }
    let quote = t.chars().next()?;
    if !matches!(quote, '"' | '\'' | '`') {
        return None;
    }
    if !t.ends_with(quote) {
        return None;
    }
    Some(t[1..t.len() - 1].to_owned())
}

/// Resolves an expression to a string, following `const` chains and template
/// interpolation. `None` when any part is unresolvable.
fn resolve_str(expr: &str, consts: &Consts) -> Option<String> {
    let t = expr.trim();
    if let Some(mut lit) = as_literal(t) {
        if lit.contains("${") {
            lit = interpolate(&lit, consts)?;
        }
        return Some(lit);
    }
    // `NAME` resolves to a literal or another const; `NAME.member` reads a
    // field out of an object const (`baseUrl: cursorProvider.baseUrl`).
    if t.contains('.') {
        let (owner, field) = t.rsplit_once('.')?;
        let object = consts.get(owner)?.trim().to_owned();
        let object = if object.starts_with('{') {
            object
        } else {
            entry_object(&object)?.0
        };
        return resolve_str(&value_of(&object, field)?, consts);
    }
    if !t.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return None;
    }
    consts.get(t).and_then(|v| resolve_str(v, consts))
}

/// Resolves a bare `NAME` reference to its literal value.
fn resolve_name(name: &str, consts: &Consts) -> Option<String> {
    let value = consts.get(name)?;
    let t = value.trim();
    if let Some(lit) = as_literal(t) {
        return if lit.contains("${") {
            interpolate(&lit, consts)
        } else {
            Some(lit)
        };
    }
    resolve_name(t, consts)
}

/// Substitutes every `${NAME}` in a template body from `consts`.
fn interpolate(lit: &str, consts: &Consts) -> Option<String> {
    let mut out = String::with_capacity(lit.len());
    let mut rest = lit;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after.find('}')?;
        let name = &after[..end];
        // Nested interpolation is not used in this tree; refusing beats guessing.
        if name.contains("${") {
            return None;
        }
        // `${NAME}` names a const. A numeric const (`MLX_GEMMA_PORT`) is not a
        // string, so an unresolvable name falls back to its literal text
        // rather than dropping the whole template.
        if name.contains("${") {
            return None;
        }
        out.push_str(&resolve_name(name, consts).unwrap_or_else(|| name.to_owned()));
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Some(out)
}

/// A numeric field read from an already-extracted *value*, not a whole object.
///
/// [`number_field`] takes the object and looks the field up itself; the callers
/// here already hold the value (`entry_layers` yielded the `(key, raw)` pair), so
/// passing the value again would search a number for a named field and find
/// nothing. `_` separators are stripped because TypeScript writes `1_048_576`.
fn number_value(raw: &str, consts: &Consts) -> Option<f64> {
    let t = raw.trim().replace('_', "");
    t.parse::<f64>().ok().or_else(|| {
        // `128_000` strips to a literal, but a value may still name a const.
        consts
            .get(&t)
            .and_then(|v| v.trim().replace('_', "").parse::<f64>().ok())
    })
}

/// Builds a [`ProviderDef`] from one entry, or `None` when the id is unreadable.
fn to_def(entry: &Entry, consts: &Consts, root: &Tree) -> Option<ProviderDef> {
    let mut format = None;
    let mut executor = None;
    let mut auth = None;
    let mut base_url = None;
    let mut models: Vec<Strng> = Vec::new();

    // The builder's defaults, then the object's own fields, then any spread.
    // `resolve_field` walks the const chain, so `...KIMI_CODING_SHARED` in one
    // file and an imported `...SHARED` in another behave the same way.
    for (key, raw) in &entry_layers(entry, consts, root) {
        match key.as_str() {
            "format" => format = resolve_str(raw, consts),
            "executor" => executor = resolve_str(raw, consts),
            "authType" => auth = resolve_str(raw, consts),
            "baseUrl" => base_url = resolve_str(raw, consts),
            "baseUrls" => base_url = first_of_array(raw, consts),
            "models" => models = model_ids(raw, consts, root),
            _ => {}
        }
    }

    Some(ProviderDef {
        base_url: base_url.unwrap_or_default(),
        wire_format: wire_format(&format.unwrap_or_else(|| "openai".to_owned())),
        auth: AuthClass::ApiKey,
        env_hint: env_hint(&entry.id),
        models,
        prices: BTreeMap::new(),
        executor: Strng::from(executor.unwrap_or_else(|| "default".to_owned()).as_str()),
        auth_kind: Strng::from(auth.unwrap_or_else(|| "apikey".to_owned()).as_str()),
        flat_rate: false,
        // `RegistryEntry.headers` is read into the layered entry above; the
        // generated catalog carries them so a custom node and an imported one
        // dispatch through the same merge.
        headers: BTreeMap::new(),
    })
}

/// Reads an entry's metadata: its short id, its auth header, its window, the
/// other protocols it speaks, and the anonymous key it publishes.
///
/// Separate from [`to_def`] because a `ProviderDef` is built field-by-field in
/// four places outside this file, so a field it does not need today is a
/// workspace-wide struct change. Both read the same resolved layers, so a
/// spread-carried value (`kimi-coding`'s `x-api-key`, which arrives through
/// `KIMI_CODING_SHARED`) lands here exactly as it would have landed there.
///
/// Recorded rather than interpreted: this module describes what a provider
/// declares, and `ar-exec`/`ar-route` keep every verdict.
fn to_meta(entry: &Entry, consts: &Consts, root: &Tree) -> ProviderMeta {
    let mut meta = ProviderMeta::default();
    let mut context_length = 0f64;
    let mut max_input_tokens = 0f64;
    for (key, raw) in &entry_layers(entry, consts, root) {
        match key.as_str() {
            // A spread can carry `authHeader` for a whole provider family, so
            // this reads the resolved layers rather than the entry's own object.
            "alias" => meta.alias = Strng::from(resolve_str(raw, consts).unwrap_or_default()),
            "authHeader" => {
                // Only what the entry declared. The default belongs to the
                // reader (`MetaCatalog::auth_header`), not to the file: writing
                // `bearer` on the ~200 entries that never mention it would make
                // the generated table restate a default 200 times and hide the
                // ~30 that declare something else.
                meta.auth_header = Strng::from(resolve_str(raw, consts).unwrap_or_default());
            }
            "responsesBaseUrl" => {
                meta.responses_base_url = Strng::from(resolve_str(raw, consts).unwrap_or_default());
            }
            "anonymousApiKey" => {
                meta.anonymous_api_key = Strng::from(resolve_str(raw, consts).unwrap_or_default());
            }
            "defaultContextLength" => {
                context_length = context_length.max(number_value(raw, consts).unwrap_or(0.0));
            }
            "alternateFormats" => {
                meta.alternate_formats = alternate_formats_in(raw, consts, root);
            }
            "models" => {
                // A per-model `contextLength` / `maxInputTokens` is a property of
                // the model, not the provider, so the provider-wide figure is the
                // largest one any model declares: the ceiling a candidate context
                // window is filtered against, never a per-model claim.
                context_length =
                    context_length.max(model_number(raw, consts, root, "contextLength"));
                max_input_tokens =
                    max_input_tokens.max(model_number(raw, consts, root, "maxInputTokens"));
            }
            _ => {}
        }
    }
    meta.context_length = context_length as u32;
    meta.max_input_tokens = max_input_tokens as u32;
    meta
}

/// The largest value any of a provider's models declares for `field`.
///
/// The models arrive in three shapes (see [`model_ids`]), so this walks the same
/// expression and reads one numeric field per model object. A number the reader
/// cannot resolve contributes `0.0`, which is the "catalog names none" state.
fn model_number(raw: &str, consts: &Consts, root: &Tree, field: &str) -> f64 {
    let mut out = 0.0f64;
    for (id, value) in model_fields(raw, consts, root, field) {
        if id.is_some() {
            out = out.max(value);
        }
    }
    out
}

/// Every `(model id, field value)` pair in a `models` expression.
///
/// The id is `None` for a shape that declares no `id` (a bare string id has one,
/// but a `const` holding a plain id array does not reach here), and the caller
/// only trusts a pair whose id resolved — a `contextLength` on a provider-wide
/// object is not a model's window.
fn model_fields(
    raw: &str,
    consts: &Consts,
    root: &Tree,
    field: &str,
) -> Vec<(Option<String>, f64)> {
    let mut out: Vec<(Option<String>, f64)> = Vec::new();
    collect_model_fields(raw, consts, root, field, &mut out, 0);
    out
}

fn collect_model_fields(
    raw: &str,
    consts: &Consts,
    root: &Tree,
    field: &str,
    out: &mut Vec<(Option<String>, f64)>,
    depth: usize,
) {
    if depth > 4 {
        return;
    }
    let t = raw.trim();
    let body: Vec<String> = if t.starts_with('[') {
        let Some(inner) = t.strip_prefix('[').and_then(|s| s.strip_suffix(']')) else {
            return;
        };
        array_elements(inner)
    } else if t.starts_with("buildModels(") {
        match call_fields(t) {
            v if !v.is_empty() => v.into_iter().map(|(_, raw)| raw).collect(),
            _ => {
                let inner = t.trim_start_matches("buildModels(").trim_end_matches(')');
                array_elements(inner.trim_start_matches('[').trim_end_matches(']'))
            }
        }
    } else if let Some(name) = t.strip_prefix("...") {
        let name = name.trim();
        if let Some(v) = consts.get(name).or_else(|| root.consts.get(name)) {
            collect_model_fields(v, consts, root, field, out, depth + 1);
        }
        return;
    } else if let Some(v) = consts.get(t).or_else(|| root.consts.get(t)) {
        collect_model_fields(v, consts, root, field, out, depth + 1);
        return;
    } else {
        return;
    };
    for value in body {
        if value.starts_with("..") {
            continue;
        }
        if value.trim_start().starts_with('{') {
            let f = fields(&value);
            let id = f
                .iter()
                .find(|(k, _)| k == "id")
                .and_then(|(_, v)| resolve_str(v, consts));
            let n = f
                .iter()
                .find(|(k, _)| k == field)
                .and_then(|(_, v)| number_value(v, consts));
            if let Some(n) = n {
                out.push((id, n));
            }
        }
    }
}

/// The `format` label of every entry in an `alternateFormats` array.
///
/// Each alternate carries its own base URL, auth header and extra headers, so
/// only the protocol names are recorded here: the catalog describes which
/// protocols a provider accepts, and the connection still chooses one and
/// supplies the rest. The primary [`ar_registry::WireFormat`] is deliberately not
/// repeated — it is already [`ProviderDef::wire_format`].
fn alternate_formats_in(raw: &str, consts: &Consts, root: &Tree) -> Vec<Strng> {
    let t = raw.trim();
    let body = if t.starts_with('[') {
        t.strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
            .map(str::to_owned)
    } else {
        // A `const` holding the array may be wrapped (`Object.freeze([…])`).
        consts
            .get(t)
            .or_else(|| root.consts.get(t))
            .map(unwrap_array)
    };
    let Some(body) = body else { return Vec::new() };
    let mut out: Vec<Strng> = Vec::new();
    for element in array_elements(&body) {
        if !element.trim_start().starts_with('{') {
            continue;
        }
        if let Some(format) = string_field(&element, "format")
            && !out.contains(&Strng::from(format.as_str()))
        {
            out.push(Strng::from(format.as_str()));
        }
    }
    out.sort_by(|a, b| a.as_ref().cmp(b.as_ref()));
    out
}

/// The field layers an entry resolves to, builder defaults first.
fn entry_layers(entry: &Entry, consts: &Consts, root: &Tree) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    if entry.defaulted {
        for (k, v) in [
            ("format", "openai"),
            ("executor", "default"),
            ("authType", "apikey"),
        ] {
            out.push((k.to_owned(), format!("\"{v}\"")));
        }
    }
    out.extend(fields(&entry.object));
    // A spread's object replaces the same-named defaults rather than appending,
    // which `fields` order already gives us; resolve it and merge over.
    let mut resolved: Vec<(String, String)> = Vec::new();
    for (key, raw) in out {
        if key == "..call" {
            for (k, v) in call_fields(&raw) {
                upsert(&mut resolved, k, v);
            }
            continue;
        }
        if key == ".." {
            for (k, v) in spread_fields(&raw, consts, root) {
                upsert(&mut resolved, k, v);
            }
            continue;
        }
        upsert(&mut resolved, key, raw);
    }
    resolved
}

/// The object literal inside a call expression, e.g. `buildModels(["a"])`.
fn call_fields(call: &str) -> Vec<(String, String)> {
    let t = call.trim();
    let Some(open) = t.find('(') else {
        return Vec::new();
    };
    let chars: Vec<char> = t.chars().collect();
    let Some(end) = balanced_end(&chars, open) else {
        return Vec::new();
    };
    let inner: String = chars[open + 1..end.saturating_sub(1)].iter().collect();
    if inner.trim_start().starts_with('{') {
        fields(&inner)
    } else {
        Vec::new()
    }
}

fn upsert(list: &mut Vec<(String, String)>, key: String, value: String) {
    if let Some(slot) = list.iter_mut().find(|(k, _)| *k == key) {
        slot.1 = value;
    } else {
        list.push((key, value));
    }
}

/// The fields of a spread target, following one level of relative import.
fn spread_fields(name: &str, consts: &Consts, root: &Tree) -> Vec<(String, String)> {
    if let Some(v) = consts.get(name).or_else(|| root.consts.get(name)) {
        if v.trim_start().starts_with('{') {
            return fields(v);
        }
        if let Some((obj, _)) = entry_object(v) {
            return fields(&obj);
        }
    }
    if let Some(v) = root.consts.get(name)
        && v.trim_start().starts_with('{')
    {
        return fields(v);
    }
    Vec::new()
}

/// The first element of an array expression, resolving `NAME`, `[...]`, and a
/// `NAME` that itself holds an array (`Object.freeze([...])`).
fn first_of_array(raw: &str, consts: &Consts) -> Option<String> {
    let t = raw.trim();
    let body = match t.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        Some(inner) => inner.to_owned(),
        None => {
            // An identifier naming an array (`baseUrls: [...ANTIGRAVITY_RUNTIME_BASE_URLS]`
            // resolves to a `NAME` whose value is `Object.freeze([…])`, which is
            // neither a string literal nor a bracket, so the const is read
            // directly rather than through `resolve_str`.
            let resolved = match resolve_str(t, consts).or_else(|| resolve_name(t, consts)) {
                Some(v) => v,
                None => consts.get(t)?.to_owned(),
            };
            return first_of_array(&unwrap_array(&resolved), consts);
        }
    };
    // An element may itself resolve to an array (`[...NAME]`), in which case the
    // first element is inside that.
    array_elements(&body).into_iter().find_map(|e| {
        let named = consts.get(e.trim()).map(str::to_owned);
        let resolved = resolve_str(&e, consts)
            .or_else(|| resolve_name(&e, consts))
            .or(named)
            .unwrap_or(e);
        if resolved.trim_start().starts_with('[') || resolved.contains("Object.freeze") {
            first_of_array(&unwrap_array(&resolved), consts)
        } else {
            Some(resolved)
        }
    })
}

/// Strips a wrapper that only wraps an array literal, e.g. `Object.freeze([...])`.
fn unwrap_array(value: &str) -> String {
    let v = value.trim();
    if v.contains("Object.freeze(")
        && let Some(open) = v.find('[')
    {
        let chars: Vec<char> = v.chars().collect();
        if let Some(end) = balanced_end(&chars, open) {
            return chars[open..end].iter().collect();
        }
    }
    v.to_owned()
}

/// The comma-separated elements of an array body.
///
/// `fields()` cannot do this: an array element has no `key: value` shape, and a
/// scalar element under it would be read as a bare key.
fn array_elements(body: &str) -> Vec<String> {
    let b: Vec<char> = body.chars().collect();
    let mut out = Vec::new();
    let mut i = skip_ws(&b, 0);
    while i < b.len() {
        if matches!(b[i], ',' | ']') {
            i += 1;
            i = skip_ws(&b, i);
            continue;
        }
        if b[i] == '.' {
            // A spread element: `[...NAME]`.
            i = skip_ws(&b, i + 3);
        }
        let Some(end) = balanced_end(&b, i) else {
            break;
        };
        let element = b[i..end].iter().collect::<String>();
        if element != "..." && !element.is_empty() {
            out.push(element);
        }
        i = skip_ws(&b, end);
    }
    out
}

/// The model ids in a `models` expression.
///
/// Three shapes occur: an array of `{id}` objects, `buildModels(["a","b"])`, and a
/// `const` holding either.
fn model_ids(raw: &str, consts: &Consts, root: &Tree) -> Vec<Strng> {
    let mut out: Vec<Strng> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    collect_models(raw, consts, root, &mut out, &mut seen, 0);
    out
}

fn collect_models(
    raw: &str,
    consts: &Consts,
    root: &Tree,
    out: &mut Vec<Strng>,
    seen: &mut BTreeSet<String>,
    depth: usize,
) {
    if depth > 4 {
        return;
    }
    let t = raw.trim();
    let body: Vec<String> = if t.starts_with('[') {
        let Some(inner) = t.strip_prefix('[').and_then(|s| s.strip_suffix(']')) else {
            return;
        };
        array_elements(inner)
    } else if t.starts_with("buildModels(") {
        match call_fields(t) {
            v if !v.is_empty() => v.into_iter().map(|(_, raw)| raw).collect(),
            _ => {
                let inner = t.trim_start_matches("buildModels(").trim_end_matches(')');
                array_elements(inner.trim_start_matches('[').trim_end_matches(']'))
            }
        }
    } else if let Some(name) = t.strip_prefix("...") {
        let name = name.trim();
        if let Some(v) = consts.get(name).or_else(|| root.consts.get(name)) {
            collect_models(v, consts, root, out, seen, depth + 1);
        }
        return;
    } else if let Some(v) = consts.get(t).or_else(|| root.consts.get(t)) {
        collect_models(v, consts, root, out, seen, depth + 1);
        return;
    } else {
        return;
    };

    for value in body {
        if value.starts_with("..") {
            continue;
        }
        if value.trim_start().starts_with('{') {
            for (key, raw) in fields(&value) {
                if key == "id"
                    && let Some(id) = resolve_str(&raw, consts)
                    && seen.insert(id.clone())
                {
                    out.push(Strng::from(id.as_str()));
                }
            }
            continue;
        }
        // A bare string element: `buildModels(["a", "b"])`.
        if let Some(id) = resolve_str(&value, consts)
            && seen.insert(id.clone())
        {
            out.push(Strng::from(id.as_str()));
        }
    }
}

/// The conventional env var for a provider id.
///
/// A convention, not a fact, and the same one `ar import --from litellm` uses: the
/// generated config is a starting point the user edits, and a wrong guess is a
/// one-line fix rather than a secret this process has to hold.
fn env_hint(id: &str) -> String {
    format!("AR_KEY_{}", id.to_uppercase().replace(['-', '.', '/'], "_"))
}

/// `(provider, model)` → price, read from `src/shared/constants/pricing`.
///
/// The files are a pure-data barrel of object literals plus `const` references,
/// which is exactly what the layer resolver above handles. Anything built by a
/// call (`devin.ts`'s `priced(variantIds(...))`) is out of reach and leaves those
/// models unpriced — a missing price row is the honest state, and pretending
/// otherwise would corrupt every budget decision downstream.
fn pricing(providers_dir: &Path) -> BTreeMap<(String, String), Price> {
    let mut out = BTreeMap::new();
    let Some(dir) = pricing_dir(providers_dir) else {
        return out;
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return out;
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "ts"))
        .collect();
    files.sort();
    for path in files {
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let src = strip_comments(&raw);
        let consts = Consts::of(&src);
        for (name, value) in bindings(&src) {
            if !name.starts_with("DEFAULT_PRICING") {
                continue;
            }
            for (provider, models) in price_layers(&value, &consts) {
                for (model, (input, output)) in models {
                    out.insert(
                        (provider.clone(), model),
                        Price {
                            input_usd_per_mtok: input,
                            output_usd_per_mtok: output,
                        },
                    );
                }
            }
        }
    }
    out
}

/// `providers/../../src/shared/constants/pricing`, if it exists.
fn pricing_dir(providers_dir: &Path) -> Option<PathBuf> {
    let mut p = providers_dir.to_path_buf();
    for _ in 0..4 {
        p = p.parent()?.to_path_buf();
        let candidate = p.join("src/shared/constants/pricing");
        if candidate.is_dir() {
            return Some(candidate);
        }
    }
    None
}

/// One provider's `model` → `(input, output)` rows.
type ProviderPrices = (String, BTreeMap<String, (f64, f64)>);

/// `provider` → `model` → `(input, output)`, resolving the barrel's spreads.
fn price_layers(value: &str, consts: &Consts) -> Vec<ProviderPrices> {
    let mut out: BTreeMap<String, BTreeMap<String, (f64, f64)>> = BTreeMap::new();
    for (provider, raw) in price_fields(value, consts) {
        // `or_default` rather than `find`: the first row for a provider has
        // nothing to find yet, and dropping it would lose every provider the
        // barrel names exactly once.
        let entry = out.entry(provider).or_default();
        for (model, (i, o)) in price_rows(&raw, consts) {
            entry.insert(model, (i, o));
        }
    }
    out.into_iter().collect()
}

/// The `provider: {…}` layers of a pricing object, spreads resolved.
fn price_fields(value: &str, consts: &Consts) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (key, raw) in fields(value) {
        if key == ".." {
            if let Some(inner) = consts.get(&raw) {
                out.extend(fields(inner));
            }
            continue;
        }
        out.push((key, raw));
    }
    out
}

/// The `{model: {input, output}}` rows inside one provider's object.
fn price_rows(value: &str, consts: &Consts) -> Vec<(String, (f64, f64))> {
    let mut out = Vec::new();
    for (model, raw) in fields(value) {
        if model == ".." {
            if let Some(inner) = consts.get(&raw) {
                out.extend(price_rows(inner, consts));
            }
            continue;
        }
        let Some(input) = number_field(&raw, "input", consts) else {
            continue;
        };
        // `output` is optional upstream (`?? 0` in transform.ts); a row with only
        // an input price is a real row, priced at zero output.
        let output = number_field(&raw, "output", consts).unwrap_or(0.0);
        out.push((model, (input, output)));
    }
    out
}

/// A numeric field, resolving an identifier reference to a number in the same
/// file or to another object literal's field.
fn number_field(object: &str, name: &str, consts: &Consts) -> Option<f64> {
    let t = object.trim();
    // `object` may itself be a bare `NAME` alias for another price object, so
    // the alias resolves first: `fields()` on `"TIER"` would skip its first
    // character looking for an opening brace and find nothing. The name is
    // carried through, or an aliased row would price `output` at `input`.
    if consts
        .get(t)
        .is_some_and(|v| v.trim_start().starts_with('{'))
        && let Some(v) = number_of(t, name, consts)
    {
        return Some(v);
    }
    number_of(&value_of(object, name)?, name, consts)
}

/// Resolves a raw expression to one numeric field, following `NAME` aliases.
fn number_of(raw: &str, name: &str, consts: &Consts) -> Option<f64> {
    let t = raw.trim();
    if let Ok(v) = t.parse::<f64>() {
        return Some(v);
    }
    let v = consts.get(t)?;
    if v.trim_start().starts_with('{') {
        return number_of(&value_of(v, name)?, name, consts);
    }
    v.trim().parse::<f64>().ok()
}

/// Provider names billed at a flat rate, from `flatRateProviders.ts` **and**
/// `web-cookie.ts`.
///
/// Two sets, one flag, because upstream's `isFlatRateProvider` unions them and
/// splitting them would import a lie: the explicit plan list is the
/// *subscription and coding-plan* providers, while `WEB_COOKIE_PROVIDERS` is
/// the cookie/web sessions, every one of which is backed by a consumer
/// subscription too. The deliberate import rule is that a web-session provider
/// is flat-rate on the strength of being one — not because a hand-maintained
/// list happens to name it — which is exactly what
/// `hasOwnProperty(WEB_COOKIE_PROVIDERS, id)` says upstream.
///
/// The set mixes ids and aliases: `cc` is in the upstream list and is `claude`'s
/// short id, not a provider of its own. [`scan`] therefore matches an entry on
/// its id *or* its alias, and the alias lands in [`ProviderDef::alias`] — one
/// entry per real provider, never a duplicate invented for the alias.
fn flat_rate_ids(providers_dir: &Path) -> BTreeSet<String> {
    let mut out = subscription_ids(providers_dir);
    out.extend(web_cookie_ids(providers_dir));
    out
}

/// The explicit subscription / coding-plan set in `flatRateProviders.ts`.
fn subscription_ids(providers_dir: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let Some(target) = find_up(providers_dir, "src/lib/usage/flatRateProviders.ts") else {
        return out;
    };
    let Ok(raw) = std::fs::read_to_string(&target) else {
        return out;
    };
    let src = strip_comments(&raw);
    let chars: Vec<char> = src.chars().collect();
    // `new Set([...])` — the values are the array the set is built from.
    let Some(start) = src
        .find("new Set([")
        .or_else(|| src.find("FLAT_RATE_SUBSCRIPTION_PROVIDER_IDS"))
    else {
        return out;
    };
    let Some(open) = src[start..].find('[') else {
        return out;
    };
    let Some(end) = balanced_end(&chars, start + open) else {
        return out;
    };
    let body: String = chars[start + open..end].iter().collect();
    // The array holds bare strings, not `key: value` pairs, so it needs the
    // array walk rather than the object one.
    let inner = body.trim_start_matches('[').trim_end_matches(']');
    for element in array_elements(inner) {
        if let Some(id) = as_literal(element.trim()) {
            out.insert(id);
        }
    }
    out
}

/// Ids of `WEB_COOKIE_PROVIDERS`, read as the top-level keys of that object
/// literal.
///
/// A provider from this set is flat-rate whether or not the explicit plan list
/// names it: every entry in it is a browser-cookie session backed by a
/// consumer subscription, which is the same economics as a coding plan.
fn web_cookie_ids(providers_dir: &Path) -> BTreeSet<String> {
    let Some(path) = find_up(
        providers_dir,
        "src/shared/constants/providers/web-cookie.ts",
    ) else {
        return BTreeSet::new();
    };
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return BTreeSet::new();
    };
    let src = strip_comments(&raw);
    let consts = Consts::of(&src);
    let Some((_, value)) = bindings(&src)
        .into_iter()
        .find(|(n, _)| n == "WEB_COOKIE_PROVIDERS")
    else {
        return BTreeSet::new();
    };
    // The value is a bare object literal keyed by provider id, so `fields()`
    // reads it directly. A `const` alias to another object resolves one level.
    let object = if value.trim_start().starts_with('{') {
        value
    } else {
        let Some(inner) = consts.get(value.trim()) else {
            return BTreeSet::new();
        };
        inner.to_owned()
    };
    fields(&object)
        .into_iter()
        .map(|(k, _)| k)
        .filter(|k| !k.starts_with(".."))
        .collect()
}

/// Walks up from `dir` until `rel` exists, bounded so a `/` root cannot spin.
///
/// Six levels covers the OmniRoute layout: `providers` → `config` → `open-sse`
/// → the repo root, which is where `src/…` lives.
fn find_up(dir: &Path, rel: &str) -> Option<PathBuf> {
    let mut cur = dir.to_path_buf();
    for _ in 0..6 {
        let candidate = cur.join(rel);
        if candidate.is_file() {
            return Some(candidate);
        }
        cur = cur.parent()?.to_path_buf();
    }
    None
}

/// The free-model budget table, read from `config/freeModelCatalog.data.ts`.
///
/// An empty table when the file is absent: a registry with no free-tier rows is
/// still a registry, and failing the whole import over an optional table would
/// make the catalog unregenerable from a partial checkout. The importer reports
/// the shortfall on stderr rather than shipping a silently empty table.
///
/// The rows are sorted by `(provider, model, regime)` so two imports of the same
/// tree produce byte-identical JSON — the property that makes a regenerated
/// `freeBudgets.json` reviewable in a diff.
fn free_budgets(providers_dir: &Path) -> FreeBudgets {
    let Some(config_dir) = providers_dir.parent() else {
        return FreeBudgets::default();
    };
    // The data file is a sibling of the `config` directory the provider tree
    // lives under, so it is reached by name from the tree's parent. No upward
    // search here, unlike the flat-rate files: this one is fixed to that layout,
    // and a search would silently read a *different* checkout's table.
    let data = config_dir.join("freeModelCatalog.data.ts");
    let traits_file = config_dir.join("freeModelCatalog.ts");
    let Ok(raw) = std::fs::read_to_string(&data) else {
        eprintln!(
            "note: no freeModelCatalog.data.ts under {}; the free-tier table will be empty",
            config_dir.display()
        );
        return FreeBudgets::default();
    };
    let src = strip_comments(&raw);
    let chars: Vec<char> = src.chars().collect();
    let curated_at = bindings(&src)
        .into_iter()
        .find(|(n, _)| n == "FREE_CATALOG_CURATED_AT")
        .and_then(|(_, v)| as_literal(v.trim()))
        .unwrap_or_default();
    let empty = FreeBudgets {
        curated_at: Strng::from(curated_at.clone()),
        rows: Vec::new(),
    };
    let Some(open) = src
        .find("FREE_MODEL_BUDGETS: FreeModelBudget[] = [")
        .map(|i| i + "FREE_MODEL_BUDGETS: FreeModelBudget[] = ".len())
    else {
        eprintln!(
            "note: {} declares no FREE_MODEL_BUDGETS array; the free-tier table will be empty",
            data.display()
        );
        return empty;
    };
    let Some(end) = balanced_end(&chars, open) else {
        eprintln!(
            "note: the FREE_MODEL_BUDGETS array in {} never closes; the free-tier table will be empty",
            data.display()
        );
        return empty;
    };
    let body: String = chars[open..end].iter().collect();
    // The array's own brackets are stripped so the walk below sees every row at
    // depth 0; left in, they push each `{` to depth 1 and the walk finds none.
    let inner = body.trim_start_matches('[').trim_end_matches(']');

    let mut rows: Vec<FreeBudgetRow> = Vec::new();
    // One row per top-level `{…}` in the array body. A naive `{` split also
    // matches nested model objects, so the walk tracks depth and the row is read
    // from the balanced literal rather than from the text up to the next brace.
    for object in top_level_objects(inner) {
        let (Some(provider), Some(model)) = (
            string_field(&object, "provider"),
            string_field(&object, "modelId"),
        ) else {
            continue;
        };
        let Some(free_type) = string_field(&object, "freeType") else {
            continue;
        };
        let Some(regime) = FreeRegime::from_label(&free_type) else {
            eprintln!(
                "note: freeModelCatalog.data.ts regime {free_type:?} is unclassified; the row is skipped"
            );
            continue;
        };
        rows.push(FreeBudgetRow {
            provider: Strng::from(provider.as_str()),
            model: Strng::from(model.as_str()),
            monthly_tokens: literal_number(&object, "monthlyTokens") as u64,
            credit_tokens: literal_number(&object, "creditTokens") as u64,
            regime,
            pool: string_field(&object, "poolKey")
                .filter(|p| !p.is_empty())
                .map(Strng::from),
            tos_avoid: string_field(&object, "tos").as_deref() == Some("avoid"),
            gated: value_of(&object, "eligibilityGate").is_some(),
        });
    }
    rows.sort_by(|a, b| {
        (&*a.provider, &*a.model, a.regime.as_str()).cmp(&(
            &*b.provider,
            &*b.model,
            b.regime.as_str(),
        ))
    });
    // The 7-regime taxonomy lives in a *different* file (`freeModelCatalog.ts`),
    // and a `freeType` it does not classify above is named on stderr rather than
    // defaulted. This check is what makes that visible when the data file gains
    // a regime before the taxonomy does.
    if !traits_file.is_file() {
        eprintln!(
            "note: no freeModelCatalog.ts beside {}; regime classification came from the importer alone",
            data.display()
        );
    }
    FreeBudgets {
        curated_at: Strng::from(curated_at),
        rows,
    }
}
/// A number in a bare object literal, `0.0` when the field is absent.
///
/// The `_` separator is stripped because TypeScript writes these as digit
/// groups — upstream has `500_000_000`, which `parse::<f64>` rejects. Without it
/// the table parses and every figure reads zero, which is worse than a parse
/// failure because the row count still matches.
///
/// The free-tier data file is flat data with no `const` indirection, so unlike
/// [`number_field`] there is nothing to resolve.
fn literal_number(object: &str, name: &str) -> f64 {
    value_of(object, name)
        .and_then(|raw| raw.trim().replace('_', "").parse::<f64>().ok())
        .unwrap_or(0.0)
}

/// Every depth-0 object literal in an array body, each as its own string.
///
/// Depth is tracked across brackets so a nested `{ … }` inside a row — a
/// per-model `evidence` block, say — cannot split the row in two.
fn top_level_objects(body: &str) -> Vec<String> {
    let b: Vec<char> = body.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    let mut depth = 0usize;
    while i < b.len() {
        match b[i] {
            '"' | '\'' | '`' => {
                i = skip_string(&b, i);
                continue;
            }
            '[' | '(' => depth += 1,
            ']' | ')' => depth = depth.saturating_sub(1),
            '{' if depth == 0 => {
                let Some(end) = balanced_end(&b, i) else {
                    break;
                };
                out.push(b[i..end].iter().collect());
                i = end;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    out
}

/// Folds the catalog into one combo per `provider/model`.
fn combos(defs: &BTreeMap<Strng, ProviderDef>) -> Vec<Combo> {
    let mut rows: Vec<Combo> = Vec::new();
    for (id, def) in defs {
        for model in &def.models {
            rows.push(Combo {
                id: format!("{id}/{model}"),
                strategy: Strategy::Priority,
                targets: vec![format!("{id}/{model}")],
                weights: BTreeMap::new(),
                // One target per combo is what this importer makes: it folds a
                // provider catalog, not OmniRoute's combo files, so it has no
                // candidate pool to read and must not fabricate one.
                pool: Vec::new(),
                compression: None,
            });
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway directory tree for a reader that takes a path.
    ///
    /// `scan` is the only importer entry point that reads the filesystem, so its
    /// tests need one. `TempTree` rather than a fixture directory in the repo:
    /// these tests are about *reading* a tree, and a checked-in tree would be a
    /// second copy of OmniRoute's shape to keep in sync with the first.
    mod tempdir {
        use std::path::PathBuf;

        /// An owned directory removed when the value drops.
        pub struct TempTree {
            root: PathBuf,
        }

        impl TempTree {
            /// Creates a uniquely-named tree under the system temp dir.
            pub fn new() -> Self {
                // ponytail: the process id plus a counter is enough
                // uniqueness for a test fixture, and avoids a `tempfile`
                // dependency for one helper.
                use std::sync::atomic::{AtomicU32, Ordering};
                static N: AtomicU32 = AtomicU32::new(0);
                let n = N.fetch_add(1, Ordering::Relaxed);
                let root =
                    std::env::temp_dir().join(format!("ar-import-test-{}-{n}", std::process::id()));
                std::fs::create_dir_all(&root).expect("temp dir");
                Self { root }
            }

            /// Writes `body` at `rel`, creating parent directories.
            pub fn write(&self, rel: &str, body: &str) {
                let path = self.root.join(rel);
                std::fs::create_dir_all(path.parent().expect("rel has a parent"))
                    .expect("parent dir");
                std::fs::write(&path, body).expect("fixture write");
            }

            /// The `config/providers` directory [`crate::scan`] expects.
            pub fn providers_dir(&self) -> PathBuf {
                self.root.join("open-sse/config/providers")
            }
        }

        impl Drop for TempTree {
            fn drop(&mut self) {
                // Best-effort: a leftover temp tree is noise, not a test failure,
                // and a `remove_dir_all` that races another test's temp dir would
                // be worse than the noise.
                let _ = std::fs::remove_dir_all(&self.root);
            }
        }
    }

    #[test]
    fn strips_block_comment_holding_a_url() {
        let out = strip_comments("// see https://x/y\nconst A = \"u\"; /* https://z/w */");
        assert!(!out.contains("https://x/y"), "{out}");
        assert!(!out.contains("https://z/w"), "{out}");
        assert!(out.contains("\"u\""), "{out}");
    }

    #[test]
    fn keeps_url_inside_a_string() {
        assert_eq!(
            strip_comments("const A = \"https://x/y\";"),
            "const A = \"https://x/y\";"
        );
    }

    #[test]
    fn applies_builder_defaults_when_object_omits_them() {
        let src = r#"
            export const tinyProvider: RegistryEntry = buildOpenAiCompatibleRegistryEntry({
              id: "tiny",
              baseUrl: "https://tiny/v1/chat/completions",
              models: [{ id: "m1", name: "M1" }],
            });
        "#;
        let consts = Consts::of(&strip_comments(src));
        let e = &entries(&strip_comments(src))[0];
        let def = to_def(e, &consts, &Tree::empty()).unwrap();
        assert_eq!(def.executor.as_ref(), "default");
        assert_eq!(def.auth_kind.as_ref(), "apikey");
        assert_eq!(def.wire_format, ar_registry::WireFormat::Openai);
        assert_eq!(def.models, vec![Strng::from("m1")]);
    }

    #[test]
    fn resolves_spread_of_a_shared_const() {
        let src = r#"
            const SHARED = { format: "claude", executor: "default", baseUrl: "https://k/v1/messages", authType: "oauth", models: [{ id: "k3", name: "K3" }] };
            export const kimi_codingProvider: RegistryEntry = {
              id: "kimi-coding",
              ...SHARED,
              authType: "oauth",
            };
        "#;
        let clean = strip_comments(src);
        let consts = Consts::of(&clean);
        let def = to_def(&entries(&clean)[0], &consts, &Tree::empty()).unwrap();
        assert_eq!(def.wire_format, ar_registry::WireFormat::Anthropic);
        assert_eq!(def.base_url, "https://k/v1/messages");
        assert_eq!(def.models, vec![Strng::from("k3")]);
    }

    #[test]
    fn object_field_overrides_the_spread() {
        let src = r#"
            const SHARED = { format: "claude", baseUrl: "https://a/v1", authType: "oauth" };
            export const pProvider: RegistryEntry = { id: "p", ...SHARED, baseUrl: "https://b/v1" };
        "#;
        let clean = strip_comments(src);
        let def = to_def(&entries(&clean)[0], &Consts::of(&clean), &Tree::empty()).unwrap();
        assert_eq!(def.base_url, "https://b/v1");
    }

    #[test]
    fn resolves_template_literal_base_url() {
        let src = r#"
            const POE_DEFAULT_BASE_URL = "https://api.poe.com/v1";
            export const POE_CHAT_COMPLETIONS_URL = `${POE_DEFAULT_BASE_URL}/chat/completions`;
            export const poeProvider: RegistryEntry = { id: "poe", baseUrl: POE_CHAT_COMPLETIONS_URL, models: [] };
        "#;
        let clean = strip_comments(src);
        let def = to_def(&entries(&clean)[0], &Consts::of(&clean), &Tree::empty()).unwrap();
        assert_eq!(def.base_url, "https://api.poe.com/v1/chat/completions");
    }

    #[test]
    fn reads_models_from_build_models_call() {
        let src = r#"
            export const pProvider: RegistryEntry = { id: "p", baseUrl: "https://x", models: buildModels(["a", "b"]) };
        "#;
        let clean = strip_comments(src);
        let def = to_def(&entries(&clean)[0], &Consts::of(&clean), &Tree::empty()).unwrap();
        assert_eq!(def.models, vec![Strng::from("a"), Strng::from("b")]);
    }

    #[test]
    fn reads_base_urls_array_first_element() {
        let src = r#"
            export const pProvider: RegistryEntry = { id: "p", baseUrls: ["https://one", "https://two"], models: [] };
        "#;
        let clean = strip_comments(src);
        let def = to_def(&entries(&clean)[0], &Consts::of(&clean), &Tree::empty()).unwrap();
        assert_eq!(def.base_url, "https://one");
    }

    #[test]
    fn reads_base_urls_through_a_frozen_array_const() {
        // `antigravity` and `agy`: `baseUrls: [...ANTIGRAVITY_RUNTIME_BASE_URLS]`
        // where the const is `Object.freeze([...])`, neither a string nor a
        // bracket at the reference site.
        let src = r#"
            const URLS = Object.freeze([
              "https://one",
              "https://two",
            ]);
            export const pProvider: RegistryEntry = { id: "p", baseUrls: [...URLS], models: [] };
        "#;
        let clean = strip_comments(src);
        let def = to_def(&entries(&clean)[0], &Consts::of(&clean), &Tree::empty()).unwrap();
        assert_eq!(def.base_url, "https://one");
    }

    #[test]
    fn prices_from_a_spread_barrel() {
        let src = r#"
            const TIER = { input: 2.5, output: 10.0 };
            const FAMILY = { openai: { "gpt-x": TIER } };
            export const DEFAULT_PRICING_FRONTIER = { ...FAMILY, anthropic: { "claude-y": { input: 3, output: 15 } } };
        "#;
        let clean = strip_comments(src);
        let layers = price_layers(
            &bindings(&clean)
                .into_iter()
                .find(|(n, _)| n == "DEFAULT_PRICING_FRONTIER")
                .unwrap()
                .1,
            &Consts::of(&clean),
        );
        let openai = layers
            .iter()
            .find(|(p, _)| p == "openai")
            .expect("openai priced");
        let anthropic = layers
            .iter()
            .find(|(p, _)| p == "anthropic")
            .expect("anthropic priced");
        assert_eq!(openai.1["gpt-x"], (2.5, 10.0));
        assert_eq!(anthropic.1["claude-y"], (3.0, 15.0));
    }

    // ── flat rate: the two upstream sets, and the alias case ──────────────

    /// A tree with both flat-rate sources plus a provider entry.
    fn flat_rate_tree(files: &[(&str, &str)]) -> tempdir::TempTree {
        let t = tempdir::TempTree::new();
        for (rel, body) in files {
            t.write(rel, body);
        }
        t
    }
    const FLAT_RATE_TS: &str = r#"
        const IDS: ReadonlySet<string> = new Set([
          "claude", // Claude Code plan
          "cc",     // alias id
          // `byteplus` is deliberately EXCLUDED upstream (a metered inference
          // host, billed per token), so the fixture names it only in a comment.
        ]);
    "#;

    const WEB_COOKIE_TS: &str = r#"
        export const WEB_COOKIE_PROVIDERS = {
          "grok-web": { id: "grok-web", name: "Grok Web" },
          "gemini-web": { id: "gemini-web", name: "Gemini Web" },
        };
    "#;

    #[test]
    fn marks_the_web_cookie_set_flat_rate_beyond_the_explicit_plan_list() {
        // The deliberate import rule: every WEB_COOKIE_PROVIDERS entry is
        // subscription-backed, so it is flat-rate whether or not the plan list
        // names it. `grok-web` is in neither explicit list and must still land.
        let t = flat_rate_tree(&[
            ("src/lib/usage/flatRateProviders.ts", FLAT_RATE_TS),
            (
                "src/shared/constants/providers/web-cookie.ts",
                WEB_COOKIE_TS,
            ),
            (
                "open-sse/config/providers/registry/grok-web/index.ts",
                "export const grok_webProvider: RegistryEntry = { id: \"grok-web\", baseUrl: \"https://grok.com\" };",
            ),
            (
                "open-sse/config/providers/registry/gemini-web/index.ts",
                "export const gemini_webProvider: RegistryEntry = { id: \"gemini-web\", baseUrl: \"https://gemini.google.com\" };",
            ),
            (
                "open-sse/config/providers/registry/byteplus/index.ts",
                "export const byteplusProvider: RegistryEntry = { id: \"byteplus\", baseUrl: \"https://byteplus\" };",
            ),
        ]);
        let flat = flat_rate_ids(&t.providers_dir());
        assert!(
            flat.contains("grok-web"),
            "a web-cookie session is subscription-backed: {flat:?}"
        );
        assert!(flat.contains("gemini-web"), "{flat:?}");
        // The excluded metered provider stays out: the import does not re-decide
        // the upstream exclusions, it unions the two sets.
        assert!(!flat.contains("byteplus"), "{flat:?}");
    }

    #[test]
    fn carries_a_flat_rate_alias_through_the_entry_alias_field() {
        // `cc` names no provider; it is `claude`'s short id. The entry stays one
        // entry, and the alias is what makes the flat-rate name resolvable.
        let t = flat_rate_tree(&[
            ("src/lib/usage/flatRateProviders.ts", FLAT_RATE_TS),
            (
                "open-sse/config/providers/registry/claude/index.ts",
                "export const claudeProvider: RegistryEntry = { id: \"claude\", alias: \"cc\", baseUrl: \"https://api.anthropic.com\" };",
            ),
        ]);
        let out = scan(&t.providers_dir()).expect("the tree holds one entry");
        // The alias is metadata, read from the metadata table; the flat-rate flag
        // it resolved is on the definition. Neither half is the answer alone,
        // which is the point of keeping them in two files.
        let claude = out
            .registry
            .get("claude")
            .expect("claude is in the catalog");
        assert_eq!(
            out.provider_meta["claude"].alias.as_ref(),
            "cc",
            "the alias is what resolved the flat-rate name"
        );
        assert!(
            claude.flat_rate,
            "`cc` in the plan set marks the provider it aliases"
        );
        assert!(
            !out.registry.contains_key("cc"),
            "the alias must not become a second entry"
        );
    }

    #[test]
    fn reads_the_web_cookie_set_when_the_plan_list_is_absent() {
        // A checkout without `flatRateProviders.ts` still has the web set, and a
        // missing file must not silently drop the web-session economics.
        let t = flat_rate_tree(&[(
            "src/shared/constants/providers/web-cookie.ts",
            WEB_COOKIE_TS,
        )]);
        assert_eq!(
            flat_rate_ids(&t.providers_dir()),
            BTreeSet::from(["gemini-web".to_owned(), "grok-web".to_owned()])
        );
    }

    // ── free-model budget table ────────────────────────────────────────────

    /// The shape of `freeModelCatalog.data.ts`, reduced to what the reader needs:
    /// a curation date, a row literal, and one nested object per row.
    const FREE_DATA_TS: &str = r#"
        export const FREE_CATALOG_CURATED_AT = "2026-09-12";
        export const FREE_MODEL_BUDGETS: FreeModelBudget[] = [
          { provider: "mistral", modelId: "m1", monthlyTokens: 500_000_000, creditTokens: 0, freeType: "recurring-daily", poolKey: "mistral-free", tos: "caution" },
          { provider: "mistral", modelId: "m2", monthlyTokens: 1_000_000_000, creditTokens: 0, freeType: "recurring-daily", poolKey: "mistral-free", tos: "caution" },
          { provider: "kiro", modelId: "claude", monthlyTokens: 25_000, creditTokens: 0, freeType: "keyless", poolKey: null, tos: "avoid" },
          { provider: "bytez", modelId: "m", monthlyTokens: 0, creditTokens: 1_000_000, freeType: "recurring-credit", poolKey: "bytez", tos: "ok" },
          { provider: "signup", modelId: "m", monthlyTokens: 0, creditTokens: 400_000_000, freeType: "one-time-initial", poolKey: "signup", tos: "caution" },
          { provider: "modelscope", modelId: "q", monthlyTokens: 6_000_000, creditTokens: 0, freeType: "recurring-daily", poolKey: "modelscope-free", tos: "caution", eligibilityGate: "regional-identity" },
          { provider: "siliconflow", modelId: "deepseek", monthlyTokens: 0, creditTokens: 0, freeType: "recurring-uncapped", poolKey: "siliconflow", tos: "caution" },
          { provider: "old", modelId: "m", monthlyTokens: 9_000_000, creditTokens: 0, freeType: "discontinued", poolKey: "old", tos: "ok" },
          { provider: "brandnew", modelId: "m", monthlyTokens: 1, creditTokens: 0, freeType: "pay-as-you-go", poolKey: "x", tos: "ok" },
        ];
    "#;

    /// A tree holding the fixture files, laid out the way OmniRoute is.
    fn free_tree() -> tempdir::TempTree {
        let t = tempdir::TempTree::new();
        // `config/providers` is the tree `scan` is pointed at, and the free-tier
        // files are its parent's siblings — the real OmniRoute layout.
        t.write(
            "open-sse/config/providers/registry/openai/index.ts",
            "export const openaiProvider: RegistryEntry = { id: \"openai\", baseUrl: \"https://api.openai.com/v1\" };",
        );
        t.write("open-sse/config/freeModelCatalog.data.ts", FREE_DATA_TS);
        t.write("open-sse/config/freeModelCatalog.ts", "export const X = 1;");
        t
    }

    #[test]
    fn reads_every_free_tier_row_upstream_declares() {
        // Nine literals, one of which names a regime this build has not
        // classified — that row is skipped by name, so the count is 8 not 9.
        let t = free_tree();
        let free = free_budgets(&t.providers_dir());
        let models: Vec<&str> = free.iter().map(|r| r.model.as_ref()).collect();
        assert_eq!(free.rows.len(), 8, "{models:?}");
        assert_eq!(free.curated_at.as_ref(), "2026-09-12");
    }

    #[test]
    fn reads_a_pool_key_and_a_null_one() {
        // `poolKey: null` is the "this model is independent" spelling and has to
        // stay distinguishable from a missing field, or the row would be pooled
        // against nothing.
        let free = free_budgets(&free_tree().providers_dir());
        let m1 = free.row("mistral", "m1").expect("m1 is a row");
        assert_eq!(m1.pool.as_deref(), Some("mistral-free"));
        assert_eq!(
            free.row("kiro", "claude").expect("kiro is a row").pool,
            None
        );
    }

    #[test]
    fn carries_the_to_avoid_flag_onto_the_row() {
        let free = free_budgets(&free_tree().providers_dir());
        assert!(free.row("kiro", "claude").expect("kiro is a row").tos_avoid);
        assert!(!free.row("mistral", "m1").expect("m1 is a row").tos_avoid);
    }

    #[test]
    fn carries_a_regional_identity_gate_onto_the_row() {
        let free = free_budgets(&free_tree().providers_dir());
        assert!(free.row("modelscope", "q").expect("gated row").gated);
        assert!(!free.row("mistral", "m1").expect("m1 is a row").gated);
    }

    #[test]
    fn keeps_credit_and_uncapped_regimes_distinct_from_a_monthly_quota() {
        let free = free_budgets(&free_tree().providers_dir());
        let totals = free.totals(Default::default());
        // Four regimes, four different figures: a totals() that summed every
        // numeric field into the headline would report a different number here.
        assert_eq!(totals.recurring_credit_tokens, 1_000_000, "bytez refills");
        assert_eq!(
            totals.one_time_credit_tokens, 400_000_000,
            "the signup credit is first-month only"
        );
        assert_eq!(
            totals.steady_monthly_tokens, 1_000_025_000,
            "the mistral pool at its max, plus kiro's 25K"
        );
        assert_eq!(totals.uncapped_providers, vec![Strng::from("siliconflow")]);
    }

    #[test]
    fn drops_a_discontinued_row_from_every_figure() {
        // A retired tier is catalogued, so its row must exist, and it grants
        // nothing, so it must reach no total.
        let free = free_budgets(&free_tree().providers_dir());
        let row = free.row("old", "m").expect("the retired row is catalogued");
        assert_eq!(row.monthly_tokens, 9_000_000);
        assert!(!row.regime.grants_free_access());
        let totals = free.totals(Default::default());
        assert_eq!(
            totals.steady_monthly_tokens, 1_000_025_000,
            "the 9M retired row is in no figure"
        );
        assert!(!free.usable("old", "m"));
    }

    #[test]
    fn excludes_a_to_avoid_row_from_usable_headroom() {
        // The routing-facing figure: the mistral pool survives, kiro's 25K does
        // not, because its terms forbid proxy use.
        let free = free_budgets(&free_tree().providers_dir());
        assert_eq!(free.usable_monthly_tokens(), 1_000_000_000);
        assert!(!free.usable("kiro", "claude"));
        assert!(free.usable("mistral", "m1"));
    }

    #[test]
    fn marks_a_regional_identity_gate_apart_from_the_headline() {
        // The gated row is real and recurring, so it is reported beside the
        // headline rather than inside it: a reader who cannot pass a
        // region-bound identity check has no access to it.
        let free = free_budgets(&free_tree().providers_dir());
        let totals = free.totals(Default::default());
        assert_eq!(totals.gated_recurring_tokens, 6_000_000);
        assert_eq!(
            totals.steady_monthly_tokens, 1_000_025_000,
            "the 6M gated row is excluded"
        );
    }

    #[test]
    fn sorts_free_rows_so_a_regenerated_table_is_byte_stable() {
        let rows = free_budgets(&free_tree().providers_dir()).rows;
        let mut sorted = rows.clone();
        sorted.sort_by(|a, b| {
            (&*a.provider, &*a.model, a.regime.as_str()).cmp(&(
                &*b.provider,
                &*b.model,
                b.regime.as_str(),
            ))
        });
        assert_eq!(rows, sorted, "the importer emits sorted rows");
    }

    #[test]
    fn reports_an_empty_free_table_when_the_source_is_absent() {
        // A partial checkout still imports a catalog; the missing table is named
        // on stderr rather than failing the whole conversion.
        let t = tempdir::TempTree::new();
        assert!(free_budgets(&t.providers_dir()).is_empty());
    }

    #[test]
    fn carries_the_free_table_alongside_a_scanned_registry() {
        // The end-to-end shape: one `scan` produces both generated files, so a
        // caller never has to know which upstream file fed which.
        let out = scan(&free_tree().providers_dir()).expect("the fixture tree has one entry");
        assert!(out.registry.contains_key("openai"));
        assert_eq!(out.free_budgets.rows.len(), 8);
        assert!(!out.config_yaml.is_empty());
    }

    #[test]
    fn counts_a_shared_pool_once_across_the_parsed_rows() {
        // The pool-dedup assertion, on rows this importer actually parsed rather
        // than on a hand-built fixture: mistral publishes 500M and 1B for one
        // allowance, so the table's figure is 1B, not 1.5B.
        let free = free_budgets(&free_tree().providers_dir());
        let row_sum: u64 = free
            .iter()
            .filter(|r| r.regime == FreeRegime::RecurringDaily)
            .map(|r| r.monthly_tokens)
            .sum();
        assert!(
            row_sum > free.pool_monthly_tokens("mistral-free"),
            "row sum {row_sum} vs pool"
        );
        assert_eq!(free.pool_monthly_tokens("mistral-free"), 1_000_000_000);
    }

    // ── provider metadata ─────────────────────────────────────────────────

    #[test]
    fn reads_the_auth_header_out_of_a_shared_spread() {
        // The case that matters: `kimi-coding` declares no `authHeader` of its
        // own, it arrives through `KIMI_CODING_SHARED`, and it is the provider
        // that needs `x-api-key` rather than a bearer token.
        let src = r#"
            const SHARED = { format: "claude", authHeader: "x-api-key", baseUrl: "https://k/v1" };
            export const kimi_codingProvider: RegistryEntry = { id: "kimi-coding", ...SHARED };
        "#;
        let clean = strip_comments(src);
        let meta = to_meta(&entries(&clean)[0], &Consts::of(&clean), &Tree::empty());
        assert_eq!(meta.auth_header.as_ref(), "x-api-key");
    }

    #[test]
    fn records_no_auth_header_when_none_is_declared() {
        // The file records declarations, not the reader's default: an empty
        // field is "the entry never mentioned one", which is a different fact
        // from "the entry said bearer". `MetaCatalog::auth_header` supplies the
        // default and has its own assertion for it.
        let src = r#"
            export const pProvider: RegistryEntry = { id: "p", baseUrl: "https://x", models: [] };
        "#;
        let clean = strip_comments(src);
        let meta = to_meta(&entries(&clean)[0], &Consts::of(&clean), &Tree::empty());
        assert!(
            meta.auth_header.is_empty(),
            "the reader supplies the default"
        );
    }

    #[test]
    fn reads_the_responses_base_url_override() {
        let src = r#"
            export const xaiProvider: RegistryEntry = {
              id: "xai", baseUrl: "https://api.x.ai/v1/chat/completions",
              responsesBaseUrl: "https://api.x.ai/v1/responses", models: [],
            };
        "#;
        let clean = strip_comments(src);
        let meta = to_meta(&entries(&clean)[0], &Consts::of(&clean), &Tree::empty());
        assert_eq!(
            meta.responses_base_url.as_ref(),
            "https://api.x.ai/v1/responses"
        );
    }

    #[test]
    fn records_every_declared_alternate_protocol() {
        // Three alternates, sorted so the generated JSON is byte-stable, and the
        // primary `format` is not repeated among them.
        let src = r#"
            export const hcnsecProvider: RegistryEntry = {
              id: "hcnsec", format: "openai", baseUrl: "https://x",
              alternateFormats: [
                { format: "gemini", baseUrl: "https://g" },
                { format: "claude", baseUrl: "https://c" },
                { format: "openai-responses", baseUrl: "https://r" },
              ],
              models: [],
            };
        "#;
        let clean = strip_comments(src);
        let meta = to_meta(&entries(&clean)[0], &Consts::of(&clean), &Tree::empty());
        let got: Vec<&str> = meta.alternate_formats.iter().map(|f| f.as_ref()).collect();
        assert_eq!(got, vec!["claude", "gemini", "openai-responses"]);
        let again = strip_comments(src);
        let def = to_def(&entries(&again)[0], &Consts::of(&again), &Tree::empty()).unwrap();
        assert_eq!(
            def.wire_format,
            ar_registry::WireFormat::Openai,
            "the primary format stays on the definition, not among the alternates"
        );
    }

    #[test]
    fn reads_the_anonymous_api_key_literal() {
        let src = r#"
            export const aihordeProvider: RegistryEntry = buildOpenAiCompatibleRegistryEntry({
              id: "aihorde", baseUrl: "https://oai.aihorde.net", anonymousApiKey: "0000000000",
              models: [{ id: "m", name: "M" }],
            });
        "#;
        let clean = strip_comments(src);
        let meta = to_meta(&entries(&clean)[0], &Consts::of(&clean), &Tree::empty());
        assert_eq!(meta.anonymous_api_key.as_ref(), "0000000000");
    }

    #[test]
    fn takes_the_largest_context_window_any_model_declares() {
        // A provider-wide ceiling, not a per-model claim: `openai` spans 128K to
        // 1M, and the candidate filter needs the largest, never the first.
        let src = r#"
            export const pProvider: RegistryEntry = { id: "p", baseUrl: "https://x", models: [
              { id: "small", name: "S", contextLength: 128000 },
              { id: "big", name: "B", contextLength: 1000000 },
            ] };
        "#;
        let clean = strip_comments(src);
        let meta = to_meta(&entries(&clean)[0], &Consts::of(&clean), &Tree::empty());
        assert_eq!(meta.context_length, 1_000_000);
        assert_eq!(
            meta.max_input_tokens, 0,
            "no model declares an input ceiling"
        );
    }

    #[test]
    fn reads_an_explicit_max_input_budget() {
        // Distinct from the window: the input ceiling can be smaller because the
        // backend reserves part of the window for output.
        let src = r#"
            export const pProvider: RegistryEntry = { id: "p", baseUrl: "https://x", models: [
              { id: "m", name: "M", contextLength: 1048576, maxInputTokens: 272000 },
            ] };
        "#;
        let clean = strip_comments(src);
        let meta = to_meta(&entries(&clean)[0], &Consts::of(&clean), &Tree::empty());
        assert_eq!(meta.max_input_tokens, 272_000);
    }

    #[test]
    fn reads_a_provider_wide_default_context_length() {
        let src = r#"
            export const pProvider: RegistryEntry = { id: "p", baseUrl: "https://x", defaultContextLength: 200000, models: [] };
        "#;
        let clean = strip_comments(src);
        let meta = to_meta(&entries(&clean)[0], &Consts::of(&clean), &Tree::empty());
        assert_eq!(meta.context_length, 200_000);
    }

    #[test]
    fn leaves_metadata_empty_for_an_entry_that_declares_none() {
        let src = r#"
            export const pProvider: RegistryEntry = { id: "p", baseUrl: "https://x", models: [] };
        "#;
        let clean = strip_comments(src);
        let meta = to_meta(&entries(&clean)[0], &Consts::of(&clean), &Tree::empty());
        assert_eq!(meta.context_length, 0);
        assert_eq!(meta.max_input_tokens, 0);
        assert!(meta.responses_base_url.as_ref().is_empty());
        assert!(meta.alternate_formats.is_empty());
        assert!(meta.anonymous_api_key.as_ref().is_empty());
        assert!(meta.alias.as_ref().is_empty());
    }

    #[test]
    fn keeps_nested_objects_out_of_a_free_row_split() {
        // A row with a nested literal must stay one row: the depth walk is what
        // stops the split.
        // String values and quoted keys, as TypeScript writes them: `fields()` steps
        // over a key it cannot parse and `string_field` reads literals only, so a
        // numeric fixture would test neither.
        let objects = top_level_objects(r#"{ "a": "1", "nested": { "b": "2" } }, { "c": "3" }"#);
        assert_eq!(objects.len(), 2, "{objects:?}");
        assert_eq!(string_field(&objects[0], "a").as_deref(), Some("1"));
        // The nested object is part of row 1, never its own entry.
        assert_eq!(string_field(&objects[0], "nested"), None);
        assert_eq!(string_field(&objects[1], "c").as_deref(), Some("3"));
    }

    #[test]
    fn prices_row_without_output_is_priced_at_zero_output() {
        let src = r#"export const DEFAULT_PRICING_X = { p: { m: { input: 1.0 } } };"#;
        let rows = price_rows(
            &fields("{\"p\":{\"m\":{\"input\":1.0}}}")[0].1,
            &Consts::default(),
        );
        assert_eq!(rows, vec![("m".to_owned(), (1.0, 0.0))], "{rows:?}");
        let _ = src;
    }
}
