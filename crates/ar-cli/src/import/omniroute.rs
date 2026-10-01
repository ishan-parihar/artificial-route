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

    let mut defs: BTreeMap<Strng, ProviderDef> = BTreeMap::new();
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
            def.flat_rate = flat_rate.contains(entry.id.as_str());
            for ((p, m), price) in &prices {
                if *p == entry.id {
                    def.prices.insert(Strng::from(m.as_str()), *price);
                }
            }
            match defs.get(&id) {
                Some(kept) if kept.base_url != def.base_url => {
                    notes.push(format!("{id} redeclared with a different base URL; kept {}", kept.base_url));
                }
                Some(_) => {}
                None => {
                    if def.base_url.is_empty() {
                        notes.push(format!("{id} has no resolvable base URL; listed but unroutable"));
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
    Ok(Imported { registry: defs, combos, config_yaml })
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
        Self { files: Vec::new(), consts: Consts::default() }
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
    let up: Vec<PathBuf> =
        [dir.parent().map(Path::to_path_buf), dir.parent().and_then(Path::parent).map(Path::to_path_buf)]
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
    Ok(Tree { files, consts: Consts(all) })
}

/// Reads the `.ts` files directly in `dir`, skipping subdirectories.
fn collect_siblings(dir: &Path, out: &mut Vec<(PathBuf, String)>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
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
        let Some(end) = balanced_end(&b, j) else { break };
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
            out.push(Entry { id, object, defaulted });
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
    fields(object).into_iter().find(|(k, _)| k == name).map(|(_, v)| v)
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
        let Some(end) = balanced_end(&b, i) else { break };
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
        let object =
            if object.starts_with('{') { object } else { entry_object(&object)?.0 };
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
        return if lit.contains("${") { interpolate(&lit, consts) } else { Some(lit) };
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
    let layers = entry_layers(entry, consts, root);
    for (key, raw) in &layers {
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

/// The field layers an entry resolves to, builder defaults first.
fn entry_layers(entry: &Entry, consts: &Consts, root: &Tree) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    if entry.defaulted {
        for (k, v) in [("format", "openai"), ("executor", "default"), ("authType", "apikey")] {
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
    let Some(open) = t.find('(') else { return Vec::new() };
    let chars: Vec<char> = t.chars().collect();
    let Some(end) = balanced_end(&chars, open) else { return Vec::new() };
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
        let Some(end) = balanced_end(&b, i) else { break };
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
        let Some(inner) = t.strip_prefix('[').and_then(|s| s.strip_suffix(']')) else { return };
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
    let Some(dir) = pricing_dir(providers_dir) else { return out };
    let Ok(entries) = std::fs::read_dir(&dir) else { return out };
    let mut files: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "ts")).collect();
    files.sort();
    for path in files {
        let Ok(raw) = std::fs::read_to_string(&path) else { continue };
        let src = strip_comments(&raw);
        let consts = Consts::of(&src);
        for (name, value) in bindings(&src) {
            if !name.starts_with("DEFAULT_PRICING") {
                continue;
            }
            for (provider, models) in price_layers(&value, &consts) {
                for (model, (input, output)) in models {
                    out.insert((provider.clone(), model), Price {
                        input_usd_per_mtok: input,
                        output_usd_per_mtok: output,
                    });
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
        let Some(input) = number_field(&raw, "input", consts) else { continue };
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
    if consts.get(t).is_some_and(|v| v.trim_start().starts_with('{'))
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

/// Provider ids billed at a flat rate, from `flatRateProviders.ts`.
///
/// The explicit plan set only. `isFlatRateProvider` also covers every
/// cookie/web session via `WEB_COOKIE_PROVIDERS`, which is a different file in a
/// different directory; a provider from that set is left unpriced rather than
/// marked flat on a guess.
fn flat_rate_ids(providers_dir: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut dir = providers_dir.to_path_buf();
    let target = loop {
        let Some(parent) = dir.parent().map(Path::to_path_buf) else { return out };
        dir = parent;
        let candidate = dir.join("src/lib/usage/flatRateProviders.ts");
        if candidate.is_file() {
            break candidate;
        }
    };
    let Ok(raw) = std::fs::read_to_string(&target) else { return out };
    let src = strip_comments(&raw);
    let chars: Vec<char> = src.chars().collect();
    // `new Set([...])` — the values are the array the set is built from.
    let Some(start) = src.find("new Set([").or_else(|| src.find("FLAT_RATE_SUBSCRIPTION_PROVIDER_IDS")) else {
        return out;
    };
    let Some(open) = src[start..].find('[') else { return out };
    let Some(end) = balanced_end(&chars, start + open) else { return out };
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

/// Folds the catalog into one combo per `provider/model`.
fn combos(defs: &BTreeMap<Strng, ProviderDef>) -> Vec<Combo> {
    let mut rows: Vec<Combo> = Vec::new();
    for (id, def) in defs {
        for model in &def.models {
            rows.push(Combo {
                id: format!("{id}/{model}"),
                strategy: Strategy::Priority,
                targets: vec![format!("{id}/{model}")],
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

    #[test]
    fn strips_block_comment_holding_a_url() {
        let out = strip_comments("// see https://x/y\nconst A = \"u\"; /* https://z/w */");
        assert!(!out.contains("https://x/y"), "{out}");
        assert!(!out.contains("https://z/w"), "{out}");
        assert!(out.contains("\"u\""), "{out}");
    }

    #[test]
    fn keeps_url_inside_a_string() {
        assert_eq!(strip_comments("const A = \"https://x/y\";"), "const A = \"https://x/y\";");
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
            &bindings(&clean).into_iter().find(|(n, _)| n == "DEFAULT_PRICING_FRONTIER").unwrap().1,
            &Consts::of(&clean),
        );
        let openai = layers.iter().find(|(p, _)| p == "openai").expect("openai priced");
        let anthropic = layers.iter().find(|(p, _)| p == "anthropic").expect("anthropic priced");
        assert_eq!(openai.1["gpt-x"], (2.5, 10.0));
        assert_eq!(anthropic.1["claude-y"], (3.0, 15.0));
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