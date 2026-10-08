//! Reads OmniRoute's real combos out of its SQLite store.
//!
//! `aroute import --from omniroute` used to synthesise one one-target combo per
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
//! | `models[].weight` | `weights:` and the per-step record |
//! | `models[].prompt`/`tags`/`allowedConnectionIds` | preserved on the matching `ComboStep` |
//! | `models[].fallbackOnlyOnQuotaExhaustion` | copied to the matching `ComboStep` and the combo's `pool:` |
//! | `context_length` | `context_length:` |
//! | `models[].kind == "provider-wildcard"` | expanded against the catalog |
//! | `models[].kind == "combo-ref"` | expanded inline, with cycle detection |
//!
//! Nested `combo-ref` steps are flattened, because an ar combo is a flat target
//! list. The referenced combo's own order, weights, prompts, tags, connection
//! allow-lists, and quota-exhaustion-only markers are preserved on the parent's
//! steps; the parent keeps its own strategy. `provider-wildcard` is different —
//! it names a provider and a model glob, and the catalog this same import just
//! built can expand it exactly.
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
    /// Boxed so the two variants stay near each other in size; the structured
    /// arm carries a dozen strings and would otherwise make every `Step`
    /// roughly 300 bytes on the stack.
    Detailed(Box<DetailedStep>),
}

/// The structured form of one combo step.
///
/// camelCase because that is how upstream's zod schema names them
/// (`combo.ts:26-46`): a snake_case reading would silently bind nothing, and a
/// step whose provider did not bind resolves to no target at all.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct DetailedStep {
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
    /// Per-step system instruction for pipeline stages.
    #[serde(default)]
    prompt: Option<String>,
    /// Freeform labels on the step.
    #[serde(default)]
    tags: Option<Vec<String>>,
    /// Optional connection ids this step may run on.
    #[serde(rename = "allowedConnectionIds", default)]
    allowed_connection_ids: Option<Vec<String>>,
    /// True when upstream marks this step as quota-exhaustion-only.
    #[serde(rename = "fallbackOnlyOnQuotaExhaustion", default)]
    fallback_only_on_quota_exhaustion: Option<bool>,
    /// Step label.
    #[serde(default)]
    label: Option<String>,
    /// Step id.
    #[serde(default)]
    id: Option<String>,
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

    // Parse every row first so a `combo-ref` can be resolved against the full
    // combo set rather than dropping its parent.
    let mut parsed: Vec<(Row, ComboData)> = Vec::new();
    for row in rows {
        let Ok(row) = row else { continue };
        let Ok(data) = serde_json::from_str::<ComboData>(&row.data) else {
            eprintln!("note: combo {:?} has unreadable data; skipped", row.name);
            continue;
        };
        parsed.push((row, data));
    }

    let combos_by_name: BTreeMap<String, usize> = parsed
        .iter()
        .enumerate()
        .map(|(i, (row, _))| (row.name.trim().to_owned(), i))
        .collect();

    let graph = ComboGraph {
        defs,
        parsed: &parsed,
        by_name: &combos_by_name,
    };
    let mut out: Vec<Combo> = Vec::new();
    for (row, data) in parsed.iter() {
        match build(row, data, &graph) {
            Ok(combo) => out.push(combo),
            Err(note) => eprintln!("note: {note}"),
        }
    }
    out
}

/// The immutable tables a combo expansion needs, grouped so the recursive
/// walker stays inside clippy’s argument budget.
struct ComboGraph<'a> {
    defs: &'a BTreeMap<ar_core::Strng, ar_registry::ProviderDef>,
    parsed: &'a [(Row, ComboData)],
    by_name: &'a BTreeMap<String, usize>,
}

/// Builds one `Combo` from a row, or returns the note explaining why not.
///
/// Nested `combo-ref` steps are expanded recursively; cycle paths are named and
/// rejected rather than looping.
fn build(row: &Row, data: &ComboData, graph: &ComboGraph<'_>) -> Result<Combo, String> {
    let mut targets = Vec::new();
    let mut steps: Vec<ar_config::ComboStep> = Vec::new();
    let mut weights: BTreeMap<String, u32> = BTreeMap::new();
    let path = vec![row.name.trim().to_owned()];
    expand_data(
        row.name.as_str(),
        data,
        graph,
        &path,
        &mut targets,
        &mut steps,
        &mut weights,
    )?;

    let mut pool = Vec::new();
    for step in &steps {
        if step.fallback_only_on_quota_exhaustion && !pool.contains(&step.target) {
            pool.push(step.target.clone());
        }
    }

    // Deduplicate adjacent identical *steps*, not just identical targets: a
    // pipeline can repeat one provider/model with different prompts, and those
    // stages must survive the port.
    let mut deduped_targets = Vec::new();
    let mut deduped_steps = Vec::new();
    for (target, step) in targets.into_iter().zip(steps) {
        if deduped_targets.last() == Some(&target) && deduped_steps.last() == Some(&step) {
            continue;
        }
        deduped_targets.push(target);
        deduped_steps.push(step);
    }
    targets = deduped_targets;
    steps = deduped_steps;
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
        pool,
        steps,
        compression: None,
        judge_model: None,
        context_length: data.context_length,
    })
}

fn expand_data(
    row_name: &str,
    data: &ComboData,
    graph: &ComboGraph<'_>,
    path: &[String],
    targets: &mut Vec<String>,
    steps: &mut Vec<ar_config::ComboStep>,
    weights: &mut BTreeMap<String, u32>,
) -> Result<(), String> {
    for step in &data.models {
        match step {
            Step::Model(s) => {
                let target = s.trim().to_owned();
                targets.push(target.clone());
                steps.push(ar_config::ComboStep {
                    target,
                    weight: None,
                    prompt: None,
                    tags: vec![],
                    allowed_connection_ids: vec![],
                    fallback_only_on_quota_exhaustion: false,
                    label: None,
                    id: None,
                });
            }
            Step::Detailed(boxed) => {
                let DetailedStep {
                    kind,
                    model,
                    provider_id,
                    provider,
                    model_pattern,
                    combo_name,
                    weight,
                    prompt,
                    tags,
                    allowed_connection_ids,
                    fallback_only_on_quota_exhaustion,
                    label,
                    id,
                } = &**boxed;
                match kind.as_deref() {
                    Some("combo-ref") => {
                        let name = combo_name.as_deref().unwrap_or("(unnamed)");
                        if path.iter().any(|p| p == name) {
                            return Err(format!(
                                "combo {:?} nests a cycle through {:?}; step {:?} was dropped",
                                row_name,
                                path.join(" -> "),
                                name
                            ));
                        }
                        let Some(&idx) = graph.by_name.get(name) else {
                            return Err(format!(
                                "combo {:?} nests {:?}, which is not in the same combos table; step was dropped",
                                row_name, name
                            ));
                        };
                        let (ref_row, ref_data) = &graph.parsed[idx];
                        let mut sub_targets = Vec::new();
                        let mut sub_steps = Vec::new();
                        let mut sub_weights = BTreeMap::new();
                        let mut nested_path = path.to_vec();
                        nested_path.push(name.to_owned());
                        expand_data(
                            ref_row.name.as_str(),
                            ref_data,
                            graph,
                            &nested_path,
                            &mut sub_targets,
                            &mut sub_steps,
                            &mut sub_weights,
                        )?;
                        // The wrapped combo's own order wins, preserving its strategy order over
                        // whichever parent step referenced it.
                        for t in sub_targets {
                            targets.push(t);
                        }
                        for s in sub_steps {
                            steps.push(s);
                        }
                        for (t, w) in sub_weights {
                            weights.insert(t, w);
                        }
                    }
                    Some("provider-wildcard") => {
                        let provider = provider_id.as_deref().or(provider.as_deref()).unwrap_or("");
                        let pattern = model_pattern.as_deref().or(model.as_deref()).unwrap_or("");
                        let before = targets.len();
                        expand_wildcard(row_name, provider, pattern, graph.defs, targets)?;
                        for t in targets.iter().skip(before) {
                            steps.push(ar_config::ComboStep {
                                target: t.clone(),
                                weight: None,
                                prompt: prompt.clone(),
                                tags: tags.clone().unwrap_or_default(),
                                allowed_connection_ids: allowed_connection_ids
                                    .clone()
                                    .unwrap_or_default(),
                                fallback_only_on_quota_exhaustion:
                                    fallback_only_on_quota_exhaustion.unwrap_or(false),
                                label: label.clone(),
                                id: id.clone(),
                            });
                        }
                    }
                    _ => {
                        if let Some(t) = full_model_str(
                            model.as_deref().unwrap_or("").trim(),
                            provider_id.as_deref().or(provider.as_deref()),
                        ) {
                            targets.push(t.clone());
                            steps.push(ar_config::ComboStep {
                                target: t.clone(),
                                weight: weight.as_ref().and_then(|w| {
                                    if w.is_finite() && *w > 0.0 {
                                        Some(*w)
                                    } else {
                                        None
                                    }
                                }),
                                prompt: prompt.clone(),
                                tags: tags.clone().unwrap_or_default(),
                                allowed_connection_ids: allowed_connection_ids
                                    .clone()
                                    .unwrap_or_default(),
                                fallback_only_on_quota_exhaustion:
                                    fallback_only_on_quota_exhaustion.unwrap_or(false),
                                label: label.clone(),
                                id: id.clone(),
                            });
                            if let Some(w) = weight.as_ref().filter(|w| w.is_finite() && **w > 0.0)
                            {
                                weights.insert(t, w.round().clamp(1.0, u32::MAX as f64) as u32);
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// Composes `provider/model` the way upstream's `getComboModelString` does.
///
/// A model that already contains a `/` is returned unchanged — upstream's
/// `toFullModelString` does the same — and a bare model is prefixed with the
/// step's provider, canonicalised through the alias table so a short id like
/// `cc` still routes through the provider that owns it.
fn full_model_str(model: &str, provider_id: Option<&str>) -> Option<String> {
    let model = model.trim();
    if model.is_empty() {
        return None;
    }
    if model.contains('/') {
        return Some(model.to_owned());
    }
    let provider = provider_id
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(canonical_provider)?;
    Some(format!("{provider}/{model}"))
}

/// The canonical provider id for a step's provider field.
fn canonical_provider(id: &str) -> &str {
    ar_registry::meta::global().id_for_alias(id).unwrap_or(id)
}

/// Expands one `provider-wildcard` step against the catalog.
///
/// Upstream resolves the wildcard at dispatch time against the provider's live
/// model list. The import has the catalog it just built, so it expands here and
/// the result is a plain target list — the same shape `aroute import --from litellm`
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
    let provider = canonical_provider(provider);
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
    fn should_clamp_a_sub_one_weight_to_one_rather_than_zero() {
        // A positive upstream weight means "this step has a share". `round()`
        // sends 0.4 to 0, and `weight.max(1)` downstream would promote a
        // *saturated* 0 back to 1 anyway — indistinguishable, until the
        // saturation case, where the cast would silently invent the share.
        let dir = scratch("weight-clamp");
        let path = db(
            &dir,
            &[(
                "weighted",
                r#"{"strategy":"weighted","models":[
                    {"providerId":"groq","model":"llama-3.3-70b","weight":0.4},
                    {"providerId":"groq","model":"llama-3.1-8b","weight":1e30},
                    {"providerId":"groq","model":"llama-3.2-90b","weight":0}]}"#,
            )],
        );
        let combos = read(&path, &catalog());
        assert_eq!(combos[0].weights.get("groq/llama-3.3-70b"), Some(&1));
        assert_eq!(combos[0].weights.get("groq/llama-3.1-8b"), Some(&u32::MAX));
        assert_eq!(combos[0].weights.get("groq/llama-3.2-90b"), None);
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
    fn should_expand_a_nested_combo_ref_into_the_parent_chain() {
        let dir = scratch("nested");
        let path = db(
            &dir,
            &[
                (
                    "inner",
                    r#"{"models":[{"providerId":"groq","model":"llama-3.3-70b"},
                                    {"providerId":"groq","model":"llama-3.1-8b"}]}"#,
                ),
                (
                    "outer",
                    r#"{"models":[{"kind":"combo-ref","comboName":"inner"},
                                {"providerId":"groq","model":"gemma2-9b"}]}"#,
                ),
            ],
        );
        let combos = read(&path, &catalog());
        assert_eq!(
            combos.len(),
            2,
            "the referenced combo still exists on its own"
        );
        let outer = combos
            .iter()
            .find(|c| c.id == "outer")
            .expect("outer is imported");
        assert_eq!(
            outer.targets,
            vec!["groq/llama-3.3-70b", "groq/llama-3.1-8b", "groq/gemma2-9b"]
        );
        assert_eq!(outer.steps.len(), 3);
        assert_eq!(
            outer
                .steps
                .iter()
                .map(|s| s.target.as_str())
                .collect::<Vec<_>>(),
            outer.targets
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn should_preserve_step_metadata_when_the_store_carries_it() {
        let dir = scratch("metadata");
        let path = db(
            &dir,
            &[(
                "rich",
                r#"{"models":[{
                    "providerId":"groq","model":"llama-3.3-70b","weight":70,
                    "prompt":"Keep the answer short","tags":["fast","primary"],
                    "allowedConnectionIds":["conn-a","conn-b"],
                    "fallbackOnlyOnQuotaExhaustion":false},
                   {"providerId":"groq","model":"llama-3.1-8b","weight":30,
                    "prompt":"Answer as JSON","tags":["fallback"],
                    "allowedConnectionIds":["conn-c"],
                    "fallbackOnlyOnQuotaExhaustion":true}]}"#,
            )],
        );
        let combos = read(&path, &catalog());
        assert_eq!(combos[0].steps.len(), 2);
        assert_eq!(
            combos[0].steps[0].prompt.as_deref(),
            Some("Keep the answer short")
        );
        assert_eq!(combos[0].steps[0].tags, vec!["fast", "primary"]);
        assert_eq!(
            combos[0].steps[0].allowed_connection_ids,
            vec!["conn-a", "conn-b"]
        );
        assert!(!combos[0].steps[0].fallback_only_on_quota_exhaustion);
        assert_eq!(combos[0].steps[1].prompt.as_deref(), Some("Answer as JSON"));
        assert!(combos[0].steps[1].fallback_only_on_quota_exhaustion);
        assert_eq!(combos[0].pool, vec!["groq/llama-3.1-8b"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn should_keep_repeated_targets_that_carry_different_step_metadata() {
        let dir = scratch("repeat");
        let path = db(
            &dir,
            &[(
                "pipeline",
                r#"{"strategy":"pipeline","models":[
                    {"providerId":"groq","model":"llama-3.3-70b","prompt":"Draft"},
                    {"providerId":"groq","model":"llama-3.3-70b","prompt":"Polish"}]}"#,
            )],
        );
        let combos = read(&path, &catalog());
        assert_eq!(combos[0].targets.len(), 2);
        assert_eq!(combos[0].steps.len(), 2);
        assert_eq!(combos[0].steps[0].prompt.as_deref(), Some("Draft"));
        assert_eq!(combos[0].steps[1].prompt.as_deref(), Some("Polish"));
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
        // Upstream's `getComboModelString` leaves a model that already carries a
        // provider untouched, and prefixes a bare one with the step's provider.
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

    #[test]
    fn should_expand_a_provider_alias_when_prefixing_a_bare_model() {
        // `cc` is `claude`'s short id in the generated metadata table, and a step
        // that names it must still produce a target the registry can route.
        assert_eq!(
            full_model_str("opus", Some("cc")).as_deref(),
            Some("claude/opus")
        );
    }
}
