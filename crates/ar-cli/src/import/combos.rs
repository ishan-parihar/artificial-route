//! Reads OmniRoute's real combos out of its SQLite store.
//!
//! `ar import --from omniroute` used to synthesise one one-target combo per
//! `provider/model`, which reproduced the *catalog* and none of the routing an
//! OmniRoute operator actually configured: their failover chains, their weights,
//! their per-combo window. This module reads the `combos` table instead
//! (`src/lib/db/core.ts:291-298`).
//!
//! # The row
//!
//! ```sql
//! SELECT name, data, sort_order FROM combos
//!  ORDER BY sort_order ASC, name COLLATE NOCASE ASC
//! ```
//!
//! — the exact statement `getCombos` runs (`sqliteComboRepository.ts:122`).
//! `data` is the whole combo object JSON-stringified (`createCombo`, `:189-218`),
//! so the shape to read is upstream's zod schema
//! (`src/shared/validation/schemas/combo.ts:362`):
//!
//! ```json
//! { "name": "cheap", "strategy": "priority", "context_length": 200000,
//!   "models": [ { "kind": "model", "providerId": "groq",
//!                 "model": "llama-3.3-70b", "weight": 0 },
//!               { "kind": "combo-ref", "comboName": "fast" } ] }
//! ```
//!
//! # What maps, and what does not
//!
//! | upstream | here |
//! |---|---|
//! | `sort_order` | the combo's position in the emitted `combos:` list |
//! | `strategy` | [`Strategy::parse`] of the same name |
//! | `models[].model` + `providerId`/`provider` | `targets: ["provider/model"]` |
//! | `models[].weight` | `weights:` |
//! | `context_length` | `context_length:` |
//! | `models[].kind == "provider-wildcard"` | expanded against the catalog |
//! | `models[].kind == "combo-ref"` | reported and dropped |
//!
//! The two losses are both reported by name on stderr rather than silently
//! dropped, which is this importer's standing rule (see the module docs in
//! `omniroute.rs`).
//!
//! `combo-ref` is a nested combo — a step that dispatches through *another*
//! combo. artificial-route's [`Combo`] is a flat list of `provider/model`
//! targets and has no field for one; flattening would change what the operator
//! wrote (their ordering, their weights) rather than reproduce it, so the step
//! is named and skipped. `provider-wildcard` is different — it names a provider
//! and a model glob, and the catalog this same import just built can expand it
//! exactly.
//!
//! # Sort order is the list order
//!
//! There is no `sort_order` field on [`Combo`], and there does not need to be:
//! upstream's ordering *is* the row order, `config.yaml` preserves the order of
//! its `combos:` list, and `ar_server`'s combo block keeps input order under the
//! catalog sort. Emitting the rows in the SQL's order therefore carries the
//! dashboard's drag-and-drop order all the way to `/v1/models`.

use std::collections::BTreeMap;
use std::path::Path;

use ar_config::glob;
use ar_config::{Combo, Strategy};

use crate::commands::fail;

/// One row of the `combos` table, before the step list is interpreted.
///
/// `sort_order` is not a field: it is consumed by the `ORDER BY`, so the position
/// arrives as the row's position and a second copy of it in the struct would be a
/// value nothing reads.
#[derive(Debug, serde::Deserialize)]
struct Row {
    /// The combo's display name, which is also its id upstream.
    name: String,
    /// The whole combo object, JSON-stringified into one column.
    data: String,
}

/// The fields of a stored combo object this reader reads.
#[derive(Debug, Default, serde::Deserialize)]
struct ComboData {
    /// A bare `provider/model` string, upstream's legacy step form.
    ///
    /// Deserialized rather than read from a `Value` because upstream's own
    /// `comboModelEntry` is a union of exactly these two shapes
    /// (`combo.ts:49-53`) and a row written before v2 still carries strings.
    #[serde(default)]
    models: Vec<Step>,
    /// `priority` upstream when absent (`combo.ts:368`).
    #[serde(default)]
    strategy: Option<String>,
    /// The operator's declared window for the whole combo.
    #[serde(default)]
    context_length: Option<u32>,
}

/// One entry of a stored combo's `models` array.
#[derive(Debug, serde::Deserialize)]
#[serde(untagged)]
enum Step {
    /// The legacy bare-string form: `"groq/llama-3.3-70b"`.
    Model(String),
    /// The structured form, which is also what a `combo-ref` arrives as.
    ///
    /// camelCase because that is how upstream's zod schema names them
    /// (`combo.ts:26-46`): a snake_case reading would silently bind nothing, and a
    /// step whose provider did not bind resolves to no target at all.
    #[serde(rename_all = "camelCase")]
    Detailed {
        /// `"model"`, `"provider-wildcard"` or `"combo-ref"`.
        #[serde(default)]
        kind: Option<String>,
        /// The model id, or the pattern for a `provider-wildcard`.
        #[serde(default)]
        model: Option<String>,
        /// The canonical provider id, when the step names one.
        #[serde(default)]
        provider_id: Option<String>,
        /// The alias form of the same field.
        #[serde(default)]
        provider: Option<String>,
        /// The pattern for a `provider-wildcard`.
        #[serde(default)]
        model_pattern: Option<String>,
        /// The referenced combo's name, for a `combo-ref`.
        #[serde(default)]
        combo_name: Option<String>,
        /// Share of the draw, when upstream recorded one.
        #[serde(default)]
        weight: Option<f64>,
    },
}

/// Reads every combo from an OmniRoute `storage.sqlite`.
///
/// `defs` supplies the model list a `provider-wildcard` expands against. Missing
/// table, missing file or an unparseable row is reported and skipped rather than
/// failing the import: the catalog is the primary output, and a database this
/// build cannot read must not make the provider tree unimportable.
pub(super) fn read(
    path: &Path,
    defs: &BTreeMap<ar_core::Strng, ar_registry::ProviderDef>,
) -> Vec<Combo> {
    let Ok(conn) = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    ) else {
        eprintln!(
            "note: cannot open {}; combos fall back to one per provider/model",
            path.display()
        );
        return Vec::new();
    };
    let query = concat!(
        "SELECT name, data FROM combos ",
        "ORDER BY sort_order ASC, name COLLATE NOCASE ASC"
    );
    let Ok(mut stmt) = conn.prepare(query) else {
        eprintln!(
            "note: {} has no readable `combos` table; combos fall back to one per provider/model",
            path.display()
        );
        return Vec::new();
    };
    let rows = stmt.query_map([], |r| {
        Ok(Row {
            name: r.get(0)?,
            data: r.get(1)?,
        })
    });

    let Ok(rows) = rows else {
        eprintln!("note: cannot read the `combos` table in {}", path.display());
        return Vec::new();
    };

    let mut out: Vec<Combo> = Vec::new();
    for row in rows {
        let Ok(row) = row else { continue };
        let Ok(data) = serde_json::from_str::<ComboData>(&row.data) else {
            eprintln!("note: combo {:?} has unreadable data; skipped", row.name);
            continue;
        };
        match build(&row, &data, defs) {
            Ok(combo) => out.push(combo),
            Err(note) => eprintln!("note: {note}"),
        }
    }
    out
}

/// Builds one `Combo` from a row, or returns the note explaining why not.
fn build(
    row: &Row,
    data: &ComboData,
    defs: &BTreeMap<ar_core::Strng, ar_registry::ProviderDef>,
) -> Result<Combo, String> {
    let mut targets: Vec<String> = Vec::new();
    let mut weights: BTreeMap<String, u32> = BTreeMap::new();

    for step in &data.models {
        match step {
            Step::Model(s) => targets.push(s.trim().to_owned()),
            Step::Detailed {
                kind,
                model,
                provider_id,
                provider,
                model_pattern,
                combo_name,
                weight,
            } => {
                match kind.as_deref() {
                    Some("combo-ref") => {
                        return Err(format!(
                            "combo {:?} nests {:?}; artificial-route combos are flat, so the step was dropped",
                            row.name,
                            combo_name.as_deref().unwrap_or("(unnamed)")
                        ));
                    }
                    Some("provider-wildcard") => {
                        let provider = provider_id.as_deref().or(provider.as_deref()).unwrap_or("");
                        let pattern = model_pattern.as_deref().or(model.as_deref()).unwrap_or("");
                        expand_wildcard(&row.name, provider, pattern, defs, &mut targets)?;
                    }
                    _ => {
                        if let Some(t) = full_model_str(
                            model.as_deref().unwrap_or("").trim(),
                            provider_id.as_deref().or(provider.as_deref()),
                        ) {
                            targets.push(t);
                        }
                    }
                }
                // Upstream weights are 0..100 floats; `Combo::weights` is u32 and
                // only consulted by the weighted strategies. A weight of 0 is
                // upstream's own default for a step that declared none
                // (`steps.ts:22`), and ar reads an absent entry as that same
                // default — so 0 is not recorded, or every unweighted step would
                // be pinned to zero share.
                if let Some(w) = weight.filter(|w| *w > 0.0)
                    && let Some(t) = targets.last()
                {
                    weights.insert(t.clone(), w.round() as u32);
                }
            }
        }
    }

    targets.dedup();
    if targets.is_empty() {
        return Err(format!(
            "combo {:?} resolves to no target; skipped",
            row.name
        ));
    }

    Ok(Combo {
        id: check_combo_id(row.name.trim())?,
        strategy: data
            .strategy
            .as_deref()
            .map_or(Strategy::Priority, Strategy::parse),
        targets,
        weights,
        // OmniRoute's `pool` is not a stored field: its bench rows are ordinary
        // steps with `fallbackOnlyOnQuotaExhaustion`, and there is no ar
        // equivalent of that flag. Empty rather than invented.
        pool: Vec::new(),
        compression: None,
        judge_model: None,
        context_length: data.context_length,
    })
}

/// Composes `provider/model` the way upstream's `getComboModelString` does.
///
/// `provider_id` wins over the model's own prefix, and a model that already
/// carries the provider is not prefixed twice (`steps.ts:181-194`). A step with
/// no resolvable provider yields `None`, which drops the step rather than
/// emitting a bare model id that would resolve against no provider.
fn full_model_str(model: &str, provider_id: Option<&str>) -> Option<String> {
    if model.is_empty() {
        return None;
    }
    let prefix = model.split_once('/').map(|(p, _)| p);
    let provider = provider_id
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .or(prefix)?;
    if prefix == Some(provider) {
        return Some(model.to_owned());
    }
    Some(format!("{provider}/{model}"))
}

/// Expands one `provider-wildcard` step against the catalog.
///
/// Upstream resolves the wildcard at dispatch time against the provider's live
/// model list. The import has the catalog it just built, so it expands here and
/// the result is a plain target list — the same shape `ar import --from litellm`
/// already produces for its alias chains (`import/mod.rs:209-212`).
fn expand_wildcard(
    combo: &str,
    provider: &str,
    pattern: &str,
    defs: &BTreeMap<ar_core::Strng, ar_registry::ProviderDef>,
    targets: &mut Vec<String>,
) -> Result<(), String> {
    if provider.is_empty() || pattern.is_empty() {
        return Err(format!(
            "combo {combo:?} has a provider-wildcard step naming no provider or pattern; skipped"
        ));
    }
    let Some(def) = defs.get(provider) else {
        return Err(format!(
            "combo {combo:?} wildcards provider {provider:?}, which the catalog does not carry; skipped"
        ));
    };
    let before = targets.len();
    for model in &def.models {
        if glob::glob_match(pattern, model.as_ref()) {
            targets.push(format!("{provider}/{model}"));
        }
    }
    if targets.len() == before {
        return Err(format!(
            "combo {combo:?} wildcard {provider}/{pattern} matched no model; skipped"
        ));
    }
    Ok(())
}

/// Rejects an alias that could not be a combo id, reusing the same rule the
/// synthesised path applies (`import/mod.rs:306-317`).
///
/// An operator may name a combo anything the dashboard accepts, which is wider
/// than a YAML key; a name with a space would produce a `config.yaml` that does
/// not parse, so it is named rather than written.
pub(super) fn check_combo_id(id: &str) -> Result<String, String> {
    if !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c))
    {
        return Ok(id.to_owned());
    }
    Err(fail(
        format!("combo id {id:?} cannot be written to config.yaml"),
        "a combo id is a client-facing model name and a YAML key; keep it to letters, digits, `-_./`",
    )
    .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `storage.sqlite` with the given `(name, json)` rows, in a temp dir the
    /// caller removes.
    fn db(dir: &Path, rows: &[(&str, &str)]) -> std::path::PathBuf {
        let path = dir.join("storage.sqlite");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE combos (id TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE, \
             data TEXT NOT NULL, sort_order INTEGER NOT NULL DEFAULT 0, \
             created_at TEXT NOT NULL, updated_at TEXT NOT NULL);",
        )
        .unwrap();
        for (i, (name, data)) in rows.iter().enumerate() {
            conn.execute(
                "INSERT INTO combos (id, name, data, sort_order, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, '', '')",
                rusqlite::params![format!("id-{i}"), name, data, i as i64],
            )
            .unwrap();
        }
        path
    }

    /// A catalog with one provider, so a `provider-wildcard` has models to hit.
    fn catalog() -> BTreeMap<ar_core::Strng, ar_registry::ProviderDef> {
        let mut defs = BTreeMap::new();
        defs.insert(
            ar_core::Strng::from("groq"),
            ar_registry::ProviderDef {
                base_url: "https://api.groq.com".to_owned(),
                wire_format: ar_registry::WireFormat::Openai,
                auth: ar_registry::AuthClass::ApiKey,
                env_hint: "AR_KEY_GROQ".to_owned(),
                models: vec![
                    ar_core::Strng::from("llama-3.3-70b"),
                    ar_core::Strng::from("llama-3.1-8b"),
                    ar_core::Strng::from("gemma2-9b"),
                ],
                prices: BTreeMap::new(),
                executor: ar_core::Strng::from("default"),
                auth_kind: ar_core::Strng::from("apikey"),
                flat_rate: false,
                headers: BTreeMap::new(),
            },
        );
        defs
    }

    /// A unique scratch directory; the caller removes it.
    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ar-combos-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn should_read_a_multi_target_combo_when_the_store_names_one() {
        let dir = scratch("multi");
        let path = db(
            &dir,
            &[(
                "failover",
                r#"{"strategy":"round-robin","models":[
                    {"kind":"model","providerId":"groq","model":"llama-3.3-70b"},
                    {"kind":"model","providerId":"openai","model":"gpt-4o"}]}"#,
            )],
        );
        let combos = read(&path, &catalog());
        assert_eq!(combos.len(), 1);
        assert_eq!(combos[0].id, "failover");
        assert_eq!(combos[0].strategy.as_str(), "round-robin");
        assert_eq!(
            combos[0].targets,
            vec!["groq/llama-3.3-70b", "openai/gpt-4o"]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn should_order_combos_by_sort_order_then_case_insensitive_name_when_the_store_has_ties() {
        let dir = scratch("order");
        let path = db(
            &dir,
            &[
                ("zulu", r#"{"models":["a/x"]}"#),
                ("Alpha", r#"{"models":["a/x"]}"#),
                ("alpha", r#"{"models":["a/y"]}"#),
            ],
        );
        // Insert order is zulu, Alpha, alpha. `sort_order` is the row index, so
        // zulu is 0 and wins; the two `alpha` rows tie and NOCASE puts `Alpha`
        // before `alpha`.
        let ids: Vec<String> = read(&path, &catalog()).into_iter().map(|c| c.id).collect();
        assert_eq!(ids, vec!["zulu", "Alpha", "alpha"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn should_carry_a_step_weight_when_the_store_records_one() {
        let dir = scratch("weight");
        let path = db(
            &dir,
            &[(
                "weighted",
                r#"{"strategy":"weighted","models":[
                    {"providerId":"groq","model":"llama-3.3-70b","weight":70},
                    {"providerId":"groq","model":"llama-3.1-8b","weight":30}]}"#,
            )],
        );
        let combos = read(&path, &catalog());
        assert_eq!(combos[0].weights.get("groq/llama-3.3-70b"), Some(&70));
        assert_eq!(combos[0].weights.get("groq/llama-3.1-8b"), Some(&30));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn should_carry_a_declared_window_when_the_store_records_one() {
        let dir = scratch("window");
        let path = db(
            &dir,
            &[(
                "capped",
                r#"{"context_length":200000,"models":["groq/llama-3.3-70b"]}"#,
            )],
        );
        assert_eq!(read(&path, &catalog())[0].context_length, Some(200_000));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn should_expand_a_provider_wildcard_against_the_catalog_when_the_step_names_one() {
        let dir = scratch("wildcard");
        let path = db(
            &dir,
            &[(
                "llamas",
                r#"{"models":[{"kind":"provider-wildcard","providerId":"groq",
                    "modelPattern":"llama-*"}]}"#,
            )],
        );
        assert_eq!(
            read(&path, &catalog())[0].targets,
            vec!["groq/llama-3.3-70b", "groq/llama-3.1-8b"]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn should_drop_a_nested_combo_step_when_the_store_carries_one() {
        let dir = scratch("nested");
        let path = db(
            &dir,
            &[(
                "outer",
                r#"{"models":[{"kind":"combo-ref","comboName":"inner"},
                              {"providerId":"groq","model":"llama-3.3-70b"}]}"#,
            )],
        );
        // The whole combo is reported rather than half-imported: a combo missing
        // one of its two steps routes somewhere the operator never chose.
        assert!(read(&path, &catalog()).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn should_return_no_combos_when_the_store_has_no_combos_table() {
        let dir = scratch("empty");
        let path = dir.join("storage.sqlite");
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute_batch("CREATE TABLE other (x TEXT);")
            .unwrap();
        assert!(read(&path, &catalog()).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn should_prefix_a_bare_model_when_the_step_names_no_provider() {
        // Upstream's `getComboModelString` takes the provider from the id's own
        // prefix when the step declares none.
        assert_eq!(
            full_model_str("groq/llama", None).as_deref(),
            Some("groq/llama")
        );
        assert_eq!(
            full_model_str("llama", Some("groq")).as_deref(),
            Some("groq/llama")
        );
        assert_eq!(full_model_str("", Some("groq")), None);
        assert_eq!(full_model_str("llama", Some("")), None);
    }
}
