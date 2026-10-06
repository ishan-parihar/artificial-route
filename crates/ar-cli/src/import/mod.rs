//! `ar import --from omniroute|litellm`.
//!
//! Converts another tool's provider/model list into the two files this proxy
//! reads: `config.yaml` (what to route) and `registry.json` (the catalog).
//! Ported from OmniRoute's `bin/commands/registry.mjs` pattern (docs/02 `cli/`);
//! the cloud bundle and OAuth paths there are DROP, and an import that could
//! refresh a signed-in session is a different tool with a different credential
//! model.
//!
//! Two readers, one per `--from`:
//!
//! * [`omniroute`] reads OmniRoute's own provider tree. `--path` is the
//!   `config/providers` directory and the reader walks the TypeScript; without
//!   `--path` there is nothing local to read and the command says so, because
//!   models.dev carries no executor, format or flat-rate classification and
//!   importing it under `omniroute` would quietly drop them. It also reads three
//!   files *beside* that tree, each of which exists to correct a field the
//!   provider entries cannot answer for themselves: `flatRateProviders.ts` +
//!   `web-cookie.ts` (whose providers are all subscription-backed), and
//!   `freeModelCatalog.data.ts` (the free-tier allowances, emitted as the third
//!   generated file).
//! * [`discovery`] handles the models.dev-shaped provider map, which is what
//!   `ar-registry`'s live overlay parses.
//!
//! No credential is ever written. A literal LiteLLM `api_key` is discarded,
//! because the one file an import produces is the one a user pastes into a bug
//! report.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::OnceLock;

use ar_config::{Combo, Strategy};
use ar_core::Strng;
use ar_registry::discovery::{self, DiscoveryError, LiveCatalog};
use ar_registry::free::FreeBudgets;
use ar_registry::lifecycle::ModelLifecycle;
use ar_registry::meta::ProviderMeta;
use ar_registry::{AuthClass, ProviderDef, WireFormat, global};
use serde::Deserialize;

mod combos;
pub mod omniroute;

use crate::commands::{block_on_value, fail};

/// How long a live catalog is served without re-fetching.
const TTL_SECS: u64 = 3600;

/// Which upstream config shape to convert.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ImportFrom {
    /// An OmniRoute provider tree. `--path` is required and points at
    /// `OmniRoute/open-sse/config/providers`; this is what regenerates
    /// `crates/ar-registry/src/registry.json`.
    Omniroute,
    /// A LiteLLM `config.yaml` with a `model_list:`.
    Litellm,
}

/// The two files an import produces.
#[derive(Debug, Clone)]
pub struct Imported {
    /// Provider definitions keyed by id: the `registry.json` body. Parses back
    /// into `ProviderDef`, which is what makes it a valid rebuild input rather
    /// than a file that only looks like one.
    pub registry: BTreeMap<Strng, ProviderDef>,
    /// One combo per upstream alias, each targeting every provider serving it.
    /// An alias two providers answer is a failover chain — the point of
    /// importing someone else's list.
    pub combos: Vec<Combo>,
    /// The `config.yaml` body, credentials left as `$VAR` references.
    pub config_yaml: String,
    /// The free-model budget table, empty for a source that carries none.
    ///
    /// A third generated file rather than a field on [`Imported::registry`]:
    /// the table is keyed `(provider, model)` — one row per model, not one per
    /// provider — so folding it in would either duplicate every provider key or
    /// make the catalog a document no `BTreeMap<Strng, ProviderDef>` loader can
    /// parse.
    pub free_budgets: FreeBudgets,
    /// What each provider declares about itself: short id, auth header, context
    /// window, alternate protocols, anonymous key.
    ///
    /// A third generated file rather than fields on `ProviderDef`, which is built
    /// field-by-field in four places outside this crate's write scope. Keyed by
    /// provider id like the registry, so both documents keep a single shape.
    pub provider_meta: BTreeMap<Strng, ProviderMeta>,
    /// The vendor lifecycle snapshot, empty for a source that carries none.
    ///
    /// The fourth generated file, for the same reason `free_budgets` is the
    /// third: the table is keyed by model id alone, so folding it into
    /// `registry.json` would make that document a two-shape file no
    /// `BTreeMap<Strng, ProviderDef>` loader can parse.
    pub lifecycle: ModelLifecycle,
}

/// One `provider/model` pair, plus the alias a client would ask for.
struct Row {
    /// A LiteLLM `model_name`, or `provider/model`.
    alias: String,
    /// Provider half of the upstream model id.
    provider: String,
    /// Model half of the upstream model id.
    model: String,
    /// `api_base`, when the source named one.
    base_url: Option<String>,
    /// Variable from an `os.environ/NAME` key, when present.
    env: Option<String>,
}

/// The part of a LiteLLM config this converter reads. Every `Params` field is
/// optional because real configs omit `api_base` for hosted providers.
#[derive(Debug, Deserialize)]
struct LiteLlm {
    #[serde(default)]
    model_list: Vec<Entry>,
}

/// One `model_list` row.
#[derive(Debug, Deserialize)]
struct Entry {
    model_name: String,
    #[serde(default)]
    litellm_params: Params,
}

/// The `litellm_params` block.
#[derive(Debug, Default, Deserialize)]
struct Params {
    model: Option<String>,
    api_base: Option<String>,
    api_key: Option<String>,
}

/// Converts `body`, written in `from`'s shape, into the two files this proxy
/// reads. Failures name the offending entry: a silently short model list is the
/// one failure mode a user cannot notice.
///
/// The OmniRoute shape is a directory of TypeScript rather than one document, so
/// it goes through [`omniroute::scan`] instead; this function keeps the
/// models.dev/LiteLLM document shapes.
pub fn convert(from: ImportFrom, body: &str) -> anyhow::Result<Imported> {
    let rows = match from {
        ImportFrom::Omniroute => from_omniroute(body)?,
        ImportFrom::Litellm => from_litellm(body)?,
    };
    assemble(rows)
}

/// Reads an OmniRoute / models.dev provider map.
fn from_omniroute(body: &str) -> anyhow::Result<Vec<Row>> {
    let catalog = discovery::parse_catalog(body).map_err(|e| {
        fail(
            e,
            "expected a models.dev-shaped map: {\"<providerId>\": {\"api\": …, \"env\": […], \"models\": {…}}}",
        )
    })?;
    Ok(catalog
        .into_iter()
        .flat_map(|(id, d)| {
            let (provider, base_url, env) =
                (id.to_string(), d.base_url.clone(), d.env_hint.clone());
            d.models.into_iter().map(move |(m, _meta)| Row {
                alias: format!("{provider}/{m}"),
                provider: provider.clone(),
                model: m.to_string(),
                base_url: base_url.clone(),
                env: env.clone(),
            })
        })
        .collect())
}

/// Reads a LiteLLM `model_list` into rows.
fn from_litellm(body: &str) -> anyhow::Result<Vec<Row>> {
    let cfg: LiteLlm = serde_yaml::from_str(body).map_err(|e| {
        fail(
            e,
            "expected a LiteLLM config: `model_list:` of `{model_name, litellm_params: {model, api_base, api_key}}`",
        )
    })?;
    cfg.model_list
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let raw = e.litellm_params.model.as_deref().ok_or_else(|| {
                fail(
                    format!(
                        "model_list[{i}] ({:?}) has no litellm_params.model",
                        e.model_name
                    ),
                    "give the row a `litellm_params.model: <provider>/<model>`",
                )
            })?;
            // LiteLLM treats a bare model name as OpenAI's, so `gpt-4o` there
            // means `openai/gpt-4o`. Carrying the default over keeps the two
            // lists equivalent rather than rejecting a config LiteLLM accepts.
            let (provider, model) = raw.split_once('/').unwrap_or(("openai", raw));
            Ok(Row {
                alias: e.model_name.clone(),
                provider: provider.to_owned(),
                model: model.to_owned(),
                base_url: e.litellm_params.api_base.clone(),
                // A literal key is discarded on purpose: see the module docs.
                env: e.litellm_params.api_key.as_deref().and_then(env_ref),
            })
        })
        .collect()
}

/// Reads the variable out of a LiteLLM `os.environ/NAME` reference. Anything
/// else yields `None`, so the provider falls back to its own env hint.
fn env_ref(key: &str) -> Option<String> {
    key.strip_prefix("os.environ/").map(str::to_owned)
}

/// Folds rows into a registry and one combo per alias.
fn assemble(rows: Vec<Row>) -> anyhow::Result<Imported> {
    let mut registry: BTreeMap<Strng, ProviderDef> = BTreeMap::new();
    let mut targets: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    for row in rows {
        // `global()` is `&'static`, so `known` is copyable into the closure
        // below without borrowing anything that outlives the loop body.
        let known = global().get(&row.provider);
        let base_url = row.base_url.clone().or_else(|| known.map(|d| d.base_url.clone())).ok_or_else(|| {
            fail(
                format!("no base URL for provider {:?}", row.provider),
                "the source config has no api_base and the compiled-in registry has no entry; add one by hand",
            )
        })?;
        let def = registry
            .entry(Strng::from(row.provider.as_str()))
            .or_insert_with(|| ProviderDef {
                base_url,
                wire_format: known.map_or(WireFormat::Openai, |d| d.wire_format),
                auth: AuthClass::ApiKey,
                env_hint: row
                    .env
                    .clone()
                    .or_else(|| known.map(|d| d.env_hint.clone()))
                    .unwrap_or_else(|| default_env(&row.provider)),
                models: Vec::new(),
                // A LiteLLM row carries no executor, auth class, price or flat-rate
                // flag; the compiled-in catalog's values are the only ones there are.
                prices: known.map(|d| d.prices.clone()).unwrap_or_default(),
                executor: Strng::from(known.map_or("default", |d| d.executor.as_ref())),
                auth_kind: Strng::from(known.map_or("apikey", |d| d.auth_kind.as_ref())),
                flat_rate: known.is_some_and(|d| d.flat_rate),
                // A LiteLLM row has no header block; the compiled-in entry's is the
                // only one there is. The provider *metadata* is not copied at
                // all: it lives in `ar-registry::meta`, keyed by id, so a
                // LiteLLM row reads it there or not at all.
                headers: known.map(|d| d.headers.clone()).unwrap_or_default(),
            });
        def.models.push(Strng::from(row.model.as_str()));
        targets
            .entry(row.alias.clone())
            .or_default()
            .insert(format!("{}/{}", row.provider, row.model));
    }

    for def in registry.values_mut() {
        // One alias per pair is the norm in a LiteLLM config, so the same model
        // arrives repeatedly; `models` is a set in effect.
        def.models.sort_unstable();
        def.models.dedup();
    }

    let combos = targets
        .into_iter()
        .map(|(alias, set)| {
            Ok(Combo {
                id: check_alias(&alias)?,
                strategy: Strategy::Priority,
                targets: set.into_iter().collect(),
                weights: BTreeMap::new(),
                // The LiteLLM export has no candidate-pool concept, so an import never
                // invents one: a bench that is not in the source is not in the file,
                // and an operator adds `pool:` by hand.
                pool: Vec::new(),
                compression: None,
                judge_model: None,
                // A LiteLLM export declares no per-combo window, so none is
                // invented: the reduction over this combo's targets resolves one
                // at serve time.
                context_length: None,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let config_yaml = render_yaml(&registry, &combos);
    Ok(Imported {
        registry,
        combos,
        config_yaml,
        free_budgets: FreeBudgets::default(),
        provider_meta: BTreeMap::new(),
        lifecycle: ModelLifecycle::default(),
    })
}

/// The env var an imported provider falls back to when nothing named one.
///
/// A convention, not a fact: the file is a starting point the user edits, and a
/// wrong guess is a one-line fix at the top of `config.yaml` rather than a
/// secret this process has to hold.
fn default_env(provider: &str) -> String {
    format!(
        "AR_KEY_{}",
        provider.to_uppercase().replace(['-', '.', '/'], "_")
    )
}

/// Rejects an alias that would not survive a round trip through the YAML.
fn check_alias(alias: &str) -> anyhow::Result<String> {
    if alias
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c))
    {
        return Ok(alias.to_owned());
    }
    Err(fail(
        format!("alias {alias:?} cannot be a combo id"),
        "a combo id is a client-facing model name and a YAML key; keep it to letters, digits, `-_./`",
    ))
}

/// Renders `config.yaml` from the same providers and combos the registry rows
/// come from, so the two files cannot disagree.
///
/// Hand-assembled rather than `serde_yaml::to_string(&Config)`: `Config::keys`
/// is a `HashMap`, so a serialised `keys:` block would reorder between runs and
/// every import would show as a diff in a file users hand-edit. The round-trip
/// check in the tests is what keeps hand-assembly honest, and it is one
/// assertion rather than a rule spread across a rendering function.
pub(crate) fn render_yaml(registry: &BTreeMap<Strng, ProviderDef>, combos: &[Combo]) -> String {
    let mut out: Vec<String> = vec![
        "# Generated by `ar import`. Credentials stay in the environment.".to_owned(),
        "keys:".to_owned(),
    ];
    out.extend(
        registry
            .iter()
            .map(|(id, d)| format!("  {id}: ${}", d.env_hint)),
    );
    out.push("providers:".to_owned());
    // One key per provider, named after it: sharing one credential across two
    // providers is uncommon and is a two-line edit, whereas guessing a shared
    // name would collide with a key the user already has.
    out.extend(
        registry
            .keys()
            .map(|id| format!("  - id: {id}\n    key: {id}")),
    );
    out.push("combos:".to_owned());
    for c in combos {
        out.push(format!(
            "  - id: {}\n    strategy: {}\n    targets:",
            c.id,
            c.strategy.as_str()
        ));
        // A weighted target is written in the map form, which is the only
        // `targets:` entry shape that carries a share (`ar-config`'s
        // `TargetEntry`). Emitting a plain list and dropping the weights would
        // turn an operator's weighted combo into a first-wins chain.
        for t in &c.targets {
            match c.weights.get(t) {
                Some(w) => out.push(format!("      - target: {t}\n        weight: {w}")),
                None => out.push(format!("      - {t}")),
            }
        }
        // Carried only when the source declared one: an invented window would be
        // a claim about the operator's chain that nothing measured.
        if let Some(ctx) = c.context_length {
            out.push(format!("    context_length: {ctx}"));
        }
    }
    let mut yaml = out.join("\n");
    yaml.push('\n');
    yaml
}

/// The `ar import` TOON rows: one per imported combo, and where it landed.
pub fn rows(imported: &Imported, out_dir: &str) -> Vec<Vec<String>> {
    imported
        .combos
        .iter()
        .map(|c| {
            let providers: BTreeSet<&str> = c
                .targets
                .iter()
                .map(|t| crate::commands::target_provider(t))
                .collect();
            vec![
                c.id.clone(),
                providers.into_iter().collect::<Vec<_>>().join("+"),
                "active".to_owned(),
                out_dir.to_owned(),
            ]
        })
        .collect()
}

/// Serialises the catalog compactly.
///
/// Not pretty-printed: the output is `include_str!`-ed into every binary linking
/// `ar-registry`, so indentation is `.rodata` in every one of them.
/// `python3 -m json.tool` prettifies it for reading.
pub fn to_catalog_json(registry: &BTreeMap<Strng, ProviderDef>) -> anyhow::Result<String> {
    serde_json::to_string(registry)
        .map_err(|e| fail(e, "the converted registry could not be serialised"))
}

/// The third generated file, beside `registry.json` and `config.yaml`.
///
/// Compact for the same reason as [`to_catalog_json`]: `ar-registry`
/// `include_str!`-s it, so whitespace is `.rodata`.
///
/// The free-tier table is keyed `(provider, model)`: one row per model, not one
/// per provider, so folding it into `registry.json` would make that file a
/// two-shape document no `ProviderDef` loader can parse. A `ProviderDef` names
/// a base URL, a wire format and a price; nothing in that shape holds an
/// allowance.
///
/// # Errors
///
/// [`serde_json::Error`] when the table cannot be serialised. Unreachable for
/// a struct of owned strings and integers, and kept so a future field names
/// itself rather than panicking.
pub fn to_free_budgets_json(free: &FreeBudgets) -> anyhow::Result<String> {
    serde_json::to_string(free).map_err(|e| fail(e, "the free-tier table could not be serialised"))
}

/// Serialises the per-provider metadata table: flat, one document shape, one
/// parse path, exactly like [`to_catalog_json`].
///
/// # Errors
///
/// [`serde_json::Error`] when the table cannot be serialised — unreachable for a
/// struct of owned strings and integers, and kept so a future field names
/// itself rather than panicking.
pub fn to_provider_meta_json(meta: &BTreeMap<Strng, ProviderMeta>) -> anyhow::Result<String> {
    serde_json::to_string(meta)
        .map_err(|e| fail(e, "the provider metadata could not be serialised"))
}

/// Serialises the vendor lifecycle snapshot: flat, one document shape, one parse
/// path, exactly like [`to_catalog_json`].
///
/// # Errors
///
/// [`serde_json::Error`] when the snapshot cannot be serialised — unreachable for
/// a struct of owned strings and a set, and kept so a future field names itself
/// rather than panicking.
pub fn to_lifecycle_json(lifecycle: &ModelLifecycle) -> anyhow::Result<String> {
    serde_json::to_string(lifecycle)
        .map_err(|e| fail(e, "the lifecycle snapshot could not be serialised"))
}

/// The upstream document: `path` if given, else the live catalog.
pub fn upstream(path: Option<&Path>) -> anyhow::Result<String> {
    path.map_or_else(fetch_live, |p| {
        std::fs::read_to_string(p).map_err(|e| {
            fail(
                format!("cannot read {}: {e}", p.display()),
                "pass --path <FILE>, or omit it to fetch models.dev live",
            )
        })
    })
}

/// The upstream tree for `--from omniroute`: a directory, not a document.
pub fn upstream_tree(path: Option<&Path>) -> anyhow::Result<&Path> {
    path.ok_or_else(|| {
        fail(
            "--from omniroute needs --path <OmniRoute/open-sse/config/providers>",
            "the catalog is generated from OmniRoute's own provider tree, which is TypeScript rather than one document; point --path at its `config/providers` directory",
        )
    })
}

/// Fetches models.dev, or replays the cache when it is fresh or upstream is
/// down. The cache is process-wide so two imports in one process do not refetch.
fn fetch_live() -> anyhow::Result<String> {
    static CACHE: OnceLock<LiveCatalog> = OnceLock::new();
    let c = CACHE.get_or_init(LiveCatalog::default);
    if !c.is_stale(discovery::unix_now(), TTL_SECS) {
        return c.body().ok_or_else(no_catalog);
    }
    match c.refresh(discovery::unix_now(), || {
        block_on_value(fetch_models_dev()).map_err(|e| DiscoveryError::Fetch(e.to_string()))
    }) {
        Ok(_) => {}
        // Offline-first: a stale catalog still converts. Only a cache that was
        // never populated is a hard error.
        Err(e) if c.providers() > 0 => {
            let n = c.providers();
            eprintln!("warning: {e}; replaying the cached catalog ({n} providers)");
        }
        Err(e) => {
            return Err(fail(
                e,
                "no catalog has ever been fetched here; pass --path <FILE> to import a saved one",
            ));
        }
    }
    c.body().ok_or_else(no_catalog)
}

/// The error for a cache that holds nothing to replay.
fn no_catalog() -> anyhow::Error {
    fail(
        "no cached catalog",
        "pass --path <FILE> to import a saved one",
    )
}

/// One `GET` of the models.dev catalog. The runtime's own start failure folds
/// into [`DiscoveryError::Fetch`] too: everything this seam can fail at *is* the
/// fetch, and one variant carrying the real message beats a second nobody can
/// act on.
async fn fetch_models_dev() -> anyhow::Result<String> {
    let wrap = |e: reqwest::Error| {
        fail(
            e,
            "models.dev is unreachable; pass --path <FILE> to import a saved catalog",
        )
    };
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(wrap)?;
    client
        .get(discovery::MODELS_DEV_URL)
        .send()
        .await
        .map_err(wrap)?
        .error_for_status()
        .map_err(wrap)?
        .text()
        .await
        .map_err(wrap)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two providers answering one alias (a failover chain), one standalone, and
    /// one entry whose `api_key` is a literal that must not be written.
    const LITELLM: &str = r#"
model_list:
  - model_name: fast
    litellm_params:
      model: openai/gpt-4o-mini
      api_key: os.environ/OPENAI_API_KEY
  - model_name: fast
    litellm_params:
      model: groq/llama-3.3-70b
      api_base: https://api.groq.com/openai/v1
  - model_name: smart
    litellm_params:
      model: anthropic/claude-sonnet-4
      api_key: sk-ant-not-a-file-secret
"#;

    #[test]
    fn imports_when_litellm_given() {
        let out = convert(ImportFrom::Litellm, LITELLM).unwrap();

        assert_eq!(out.registry.len(), 3, "openai, groq, anthropic");
        assert_eq!(
            out.registry["openai"].models,
            vec![Strng::from("gpt-4o-mini")],
            "one row per pair, deduped"
        );
        // An alias two providers answer is one combo with a chain, not two rows.
        let shared = out.combos.iter().find(|c| c.id == "fast").unwrap();
        assert_eq!(
            shared.targets,
            vec!["groq/llama-3.3-70b", "openai/gpt-4o-mini"]
        );

        // `keys:` is sorted by id, so assert the mapping rather than its
        // position: two imports of the same config must produce the same file.
        assert!(
            out.config_yaml.contains("\n  openai: $OPENAI_API_KEY\n"),
            "{}",
            out.config_yaml
        );
        assert!(out.config_yaml.contains("- id: openai\n    key: openai"));
        assert!(
            !out.config_yaml.contains("sk-ant-"),
            "a literal secret is never written"
        );
        // Nothing named a variable for groq, so the convention applies.
        assert_eq!(out.registry["groq"].env_hint, "AR_KEY_GROQ");

        // Both files have to survive the loaders that will read them:
        // `config.yaml` through `ar-config`, `registry.json` through the
        // `include_str!` in `ar-registry`. A file that does not round-trip is
        // not a conversion, it is a broken next boot.
        let cfg =
            ar_config::Config::parse(&out.config_yaml, |n| Ok(Some(format!("<{n}>")))).unwrap();
        assert_eq!(cfg.combos, out.combos);
        let json = serde_json::to_string_pretty(&out.registry).unwrap();
        let round: BTreeMap<Strng, ProviderDef> = serde_json::from_str(&json).unwrap();
        assert_eq!(round, out.registry);
    }

    #[test]
    fn imports_when_omniroute_map_given() {
        let out = convert(
            ImportFrom::Omniroute,
            r#"{"anthropic":{"api":"https://api.anthropic.com/v1","env":["ANTHROPIC_API_KEY"],"models":{"claude-sonnet-4":{}}}}"#,
        )
        .unwrap();
        assert_eq!(out.combos[0].id, "anthropic/claude-sonnet-4");
        assert_eq!(out.registry["anthropic"].wire_format, WireFormat::Anthropic);
    }

    #[test]
    fn carries_no_free_tier_rows_when_the_source_carries_none() {
        // A models.dev-shaped document has no free-tier table, and the field
        // exists on every `Imported` — so it must read as "none" rather than
        // leaving a caller to guess whether it was consulted.
        let out = convert(
            ImportFrom::Omniroute,
            r#"{"openai":{"api":"https://api.openai.com/v1","env":["OPENAI_API_KEY"],"models":{"gpt-4o":{}}}}"#,
        )
        .unwrap();
        assert!(out.free_budgets.is_empty());
    }

    #[test]
    fn serialises_the_free_tier_table_to_a_reloadable_document() {
        // The generated file is `include_str!`-ed by `ar-registry`, so a shape
        // that does not round-trip is a broken next build rather than a cosmetic
        // diff.
        let mut free = FreeBudgets {
            curated_at: Strng::from("2026-09-12"),
            ..FreeBudgets::default()
        };
        free.rows.push(ar_registry::free::FreeBudgetRow {
            provider: Strng::from("mistral"),
            model: Strng::from("m1"),
            monthly_tokens: 1_000_000_000,
            credit_tokens: 0,
            regime: ar_registry::free::FreeRegime::RecurringDaily,
            pool: Some(Strng::from("mistral-free")),
            tos_avoid: false,
            gated: false,
        });
        let json = to_free_budgets_json(&free).unwrap();
        let back: FreeBudgets = serde_json::from_str(&json).unwrap();
        assert_eq!(back, free);
    }

    #[test]
    fn names_the_failure_rather_than_dropping_the_row() {
        let litellm: Vec<(&str, &str)> = vec![
            ("not yaml at all: [", "expected a LiteLLM config"),
            ("model_list:\n  - model_name: m\n", "model_list[0]"),
            (
                "model_list:\n  - model_name: m\n    litellm_params:\n      model: brandnew/m1\n",
                "no base URL for provider",
            ),
        ];
        for (body, needle) in litellm {
            let e = convert(ImportFrom::Litellm, body).unwrap_err().to_string();
            assert!(e.contains(needle), "wanted {needle:?} in: {e}");
            assert!(e.contains("help:"), "every failure carries a fix: {e}");
        }
        let e = convert(ImportFrom::Omniroute, "not json")
            .unwrap_err()
            .to_string();
        assert!(e.contains("models.dev-shaped map"), "{e}");
    }
}
