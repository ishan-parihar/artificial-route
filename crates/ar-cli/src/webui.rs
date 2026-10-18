//! `aroute serve`'s built-in control page: the Rust binary serves its own UI.
//!
//! docs/16 Option B. The vendored Node dashboard (docs/14) is a second
//! product with a second store and a second gateway; this module is the
//! thin skin over the primitives the proxy already owns — the same yaml
//! `aroute sync` splices, the same env file, the same `ar-keys` gate, the
//! same usage ledger — so there is nothing to bridge and nothing to sync.
//!
//! Scope discipline: read state, set provider credentials, edit combos,
//! mint client keys, set per-key limits (rpm/spend/tokens), read usage. No analytics, no agent
//! tasks, no i18n. Every write validates before the file is touched, and
//! the file is never rewritten when a body is rejected.
//!
//! Trust model: `serve` binds loopback only (its `bind_addr` call hardcodes
//! `public: false`), so the page inherits the CLI's own trust boundary —
//! whoever can reach the socket can already call the proxy. No second auth
//! layer, for the same reason P0 has none.
//!
//! Boot-built routing means a write changes the *file*, not the running
//! tables: every save answers with `pending_restart`, and the banner shows
//! the restart command until `ConfigHandle::watch` is attached (docs/16 P2).

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Json, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use serde_json::{Value, json};

use ar_config::{Combo, Config, ProviderCfg};
use ar_registry::global as registry;

use crate::commands;

/// The strategies a UI save may name. `Strategy::parse` is total — unknown
/// spellings load as `Deferred` and serve 501 per combo — so the write path
/// keeps the list honest instead of letting a typo half-route.
///
/// shortcut: hand-copied from `ar_config::Strategy::parse`; keep both in sync
/// when a strategy is added there.
const STRATEGIES: &[&str] = &[
    "priority",
    "round-robin",
    "cost-optimized",
    "lkgp",
    "weighted",
    "fill-first",
    "p2c",
    "least-used",
    "random",
    "strict-random",
    "headroom",
    "reset-window",
    "reset-aware",
    "quota-weighted",
    "quota-share",
    "quota-share-fair",
    "expiry-first",
    "context-relay",
    "context-optimized",
    "cache-optimized",
    "fusion",
    "pipeline",
];

/// Everything a handler needs, frozen at serve boot.
pub struct Ctx {
    /// The yaml file the proxy loaded; writes splice into it.
    config: PathBuf,
    /// The env file the systemd unit loads (`EnvironmentFile`), same
    /// convention as `aroute sync`: `ar.env` beside the config.
    env: PathBuf,
    /// The config text as it was at boot; a differing current text is what
    /// `pending_restart` reports without new state anywhere.
    boot_text: String,
    /// The usage ledger the response path records into, shared with the
    /// server so the page and `/v1` describe one file.
    ledger: Option<Arc<Mutex<ar_tokens::Ledger>>>,
    /// The encrypted credential store behind the gate. rusqlite connections
    /// are `Send` but not `Sync`, so the one connection rides a mutex.
    store: Option<Mutex<ar_keys::CredentialStore>>,
    /// Whether the gate is armed at boot; the page refuses mints when off,
    /// the same refusal `aroute keys mint` makes.
    gate_armed: bool,
}

impl Ctx {
    /// Assembled by `serve` from what it already resolved. `boot_text` is
    /// read here rather than passed, so the snapshot is by construction the
    /// text this process booted from.
    pub fn new(
        config: PathBuf,
        ledger: Option<Arc<Mutex<ar_tokens::Ledger>>>,
        store: Option<ar_keys::CredentialStore>,
        gate_armed: bool,
    ) -> Self {
        let env = config
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("ar.env");
        let boot_text = std::fs::read_to_string(&config).unwrap_or_default();
        Self {
            config,
            env,
            boot_text,
            ledger,
            store: store.map(Mutex::new),
            gate_armed,
        }
    }
}

/// The `/ui` routes on top of the proxy's router. `app()` has already
/// applied its own state, so the merge is a plain stateless join.
pub fn mount(proxy: Router, ctx: Ctx) -> Router {
    let ui = Router::new()
        .route("/ui", get(page))
        // Trailing-slash entries redirect instead of falling through to the
        // proxy's 404 — the page is the one thing at this prefix that has no
        // deterministic fallback.
        .route("/ui/", get(page_slash))
        .route("/ui/app.css", get(css))
        .route("/ui/app.js", get(js))
        .route("/ui/fonts/inter-var-latin.woff2", get(font_inter))
        .route("/ui/fonts/sora-var-latin.woff2", get(font_sora))
        .route("/ui/api/state", get(state))
        .route("/ui/api/usage", get(usage))
        .route("/ui/api/log", get(request_log))
        .route("/ui/api/models", get(models))
        .route("/ui/api/combos", post(save_combos))
        .route("/ui/api/provider-key", post(provider_key))
        .route("/ui/api/provider-custom", post(provider_custom))
        .route("/ui/api/provider-remove", post(provider_remove))
        .route("/ui/api/keys/mint", post(mint_key))
        .route("/ui/api/limits", post(set_limits))
        .with_state(Arc::new(ctx));
    // `app()` guards the proxy routes; this guards `/ui`, which is merged in
    // after `app()` ran. The mint, provider-key and limits routes are the ones a
    // rebinding page would reach, so they are the reason this layer exists.
    ui.layer(axum::middleware::from_fn(ar_server::app::loopback_only))
        .merge(proxy)
}

// ── assets ──────────────────────────────────────────────────────────────────

/// One page, three files, embedded: a UI that must also work on an offline
/// VPS cannot fetch fonts or bundles at runtime.
async fn page() -> Html<&'static str> {
    Html(include_str!("webui/page/index.html"))
}

async fn page_slash() -> impl IntoResponse {
    Redirect::to("/ui")
}

async fn css() -> Response {
    asset(
        include_str!("webui/page/app.css"),
        "text/css; charset=utf-8",
    )
}

async fn js() -> Response {
    asset(
        include_str!("webui/page/app.js"),
        "text/javascript; charset=utf-8",
    )
}

/// The page's two faces, embedded at compile time: Sora for display, Inter
/// for body, both latin-subset variable fonts (~73 kB total). Offline VPSes
/// see the same typography a dev box does; the binaries already ship an
/// offline UI, and 73 kB keeps that claim true.
async fn font_inter() -> Response {
    font(include_bytes!("webui/fonts/inter-var-latin.woff2"))
}

async fn font_sora() -> Response {
    font(include_bytes!("webui/fonts/sora-var-latin.woff2"))
}

fn font(body: &'static [u8]) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "font/woff2"),
            (header::CACHE_CONTROL, "public, max-age=86400, immutable"),
        ],
        body,
    )
        .into_response()
}

fn asset(body: &'static str, content_type: &'static str) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response()
}

// ── errors ───────────────────────────────────────────────────────────────────

/// The envelope every refusal uses, so the page renders one error shape.
fn bad_request(error: String, help: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": error, "help": help })),
    )
        .into_response()
}

fn conflict(error: String, help: &str) -> Response {
    (
        StatusCode::CONFLICT,
        Json(json!({ "error": error, "help": help })),
    )
        .into_response()
}

/// A section the page could not serialize. Nothing has been written yet, so the
/// config stays as it was and the refusal says so.
fn render_failed(error: anyhow::Error) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({
            "error": format!("cannot render the config section: {error:#}"),
            "help": "nothing was written",
        })),
    )
        .into_response()
}

/// A body that did not parse is a 400 with the rejection's own words; the
/// page never sees a bare axum rejection.
fn body_error(rejection: JsonRejection) -> Response {
    bad_request(
        rejection.body_text(),
        "send JSON with the shape the page posts",
    )
}

// ── reads ────────────────────────────────────────────────────────────────────

/// The raw `keys:` mapping as written (name -> `$VAR`), parsed from the
/// document rather than `Config`, which resolves the same section into
/// secrets — the page shows variable names, never values.
fn raw_keys(doc: &str) -> BTreeMap<String, String> {
    serde_yaml::from_str::<serde_yaml::Value>(doc)
        .ok()
        .and_then(|root| root.get("keys").cloned())
        .and_then(|keys| serde_yaml::from_value(keys).ok())
        .unwrap_or_default()
}

/// The raw `keys:` mapping, with one binding upserted.
fn with_key_binding(doc: &str, name: &str, var: &str) -> anyhow::Result<String> {
    let mut keys = raw_keys(doc);
    keys.insert(name.to_owned(), format!("${var}"));
    let section = crate::import::render_keys_section(&keys)?;
    Ok(commands::splice_section(doc, "keys", &section))
}

/// The env resolver `Config::parse` wants: a present variable, or the parse
/// failure naming the one that is missing — the same contract `serve`'s boot
/// holds, so the page cannot show a config the proxy would refuse to load.
fn resolve_env(id: &str) -> Result<Option<String>, String> {
    Ok(std::env::var(id).ok())
}

/// Everything the page renders, from the live files rather than boot state:
/// an edit made in another terminal shows up on the next poll.
async fn state(State(ctx): State<Arc<Ctx>>) -> Response {
    let doc = match std::fs::read_to_string(&ctx.config) {
        Ok(doc) => doc,
        Err(e) => {
            return bad_request(
                format!("cannot read {}: {e}", ctx.config.display()),
                "the config file disappeared; restore it and reload",
            );
        }
    };
    let cfg = match Config::parse(&doc, resolve_env) {
        Ok(cfg) => cfg,
        Err(e) => {
            return bad_request(
                format!("the config did not parse: {e}"),
                "fix the file by hand; the page refuses to render a config it cannot load",
            );
        }
    };
    let env_set = env_vars_set(&ctx.env);
    let keys = raw_keys(&doc);

    let configured: Vec<Value> = cfg
        .providers
        .iter()
        .map(|row| provider_json(&keys, &env_set, row.id.as_str(), row.key_refs(), None))
        .chain(cfg.custom_providers.iter().map(|c| {
            let mut row = provider_json(
                &keys,
                &env_set,
                c.id.as_str(),
                vec![c.key_ref.as_str()],
                None,
            );
            row["custom"] = Value::Bool(true);
            row["base_url"] = json!(c.base_url);
            // serde gives the catalog spelling (kebab-case), the one the page
            // also sends back on an edit.
            row["protocol"] = json!(c.protocol);
            row["endpoint"] = match &c.endpoint {
                Some(e) => json!(e),
                None => Value::Null,
            };
            row
        }))
        .collect();

    let combos = cfg
        .combos
        .iter()
        .map(|c| serde_json::to_value(c).unwrap_or_else(|_| json!({})))
        .collect::<Vec<_>>();

    let in_use = cfg
        .combos
        .iter()
        .map(|c| c.strategy.as_str().to_owned())
        .collect::<Vec<_>>();

    Json(json!({
        "version": crate::version::VERSION,
        "bin": crate::version::BIN,
        "config_path": ctx.config.display().to_string(),
        "env_path": ctx.env.display().to_string(),
        "pending_restart": doc != ctx.boot_text,
        "gate_armed": ctx.gate_armed,
        "providers": configured,
        "combos": combos,
        "limits": serde_json::to_value(&cfg.limits).unwrap_or_else(|_| json!({})),
        "strategies": STRATEGIES,
        "strategies_in_use": in_use,
    }))
    .into_response()
}

/// One provider row for the page: which credential names it dispatches
/// under, which env var each resolves to, and whether that var is set.
fn provider_json(
    keys: &BTreeMap<String, String>,
    env_set: &BTreeSet<String>,
    id: &str,
    key_refs: Vec<&str>,
    base_url: Option<&str>,
) -> Value {
    let vars: Vec<Value> = key_refs
        .iter()
        .map(|name| {
            let var = keys
                .get(*name)
                .map(|v| v.trim_start_matches('$').to_owned());
            json!({
                "ref": name,
                "var": var.clone().unwrap_or_default(),
                "set": var.as_deref().is_some_and(|v| env_set.contains(v)),
            })
        })
        .collect();
    json!({
        "id": id,
        "base_url": base_url,
        "key_refs": vars,
        "custom": base_url.is_some(),
    })
}

/// The `AR_KEY_*` names present in the env file, ignoring values: the page
/// shows presence, never the credential itself.
fn env_vars_set(path: &std::path::Path) -> BTreeSet<String> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    text.lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, _)| k.trim().to_owned())
        .collect()
}

/// Recent usage from the ledger the response path writes to: the same rows
/// `aroute cost-report` prints.
async fn usage(State(ctx): State<Arc<Ctx>>, Query(q): Query<UsageQuery>) -> Response {
    let Some(ledger) = ctx.ledger.clone() else {
        return Json(json!({ "rows": [], "totals": { "usd": 0.0, "tokens": 0 }, "open": false, "analytics": { "days": [], "models": [], "providers": [] } })).into_response();
    };
    // Poison recovery rather than `expect`: the ledger is read-only here, and
    // a writer that panicked elsewhere must not take the page down with it.
    let guard = ledger
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let report = match guard.report(50) {
        Ok(r) => r,
        Err(e) => {
            return Json(
                json!({ "rows": [], "totals": { "usd": 0.0, "tokens": 0 }, "open": true, "error": e.to_string() }),
            )
            .into_response()
        }
    };
    // Analytics is best-effort beside the report: a full chart grid failing
    // with the request rows would blank the bill table on a broken query.
    let analytics = q
        .days
        .map_or_else(|| guard.analytics(10), |d| guard.analytics_range(d.clamp(1, 365), 10))
        .map(|a| json!({
            "days": a.days.iter().map(|d| json!({
                "name": d.name, "requests": d.requests, "tokens": d.tokens,
                "usd": d.cost_usd_micros as f64 / 1_000_000.0,
            })).collect::<Vec<_>>(),
            "models": a.models.iter().map(|m| json!({
                "name": m.name, "requests": m.requests, "tokens": m.tokens, "usd": m.cost_usd_micros as f64 / 1_000_000.0,
            })).collect::<Vec<_>>(),
            "providers": a.providers.iter().map(|p| json!({
                "name": p.name, "requests": p.requests, "tokens": p.tokens, "usd": p.cost_usd_micros as f64 / 1_000_000.0,
            })).collect::<Vec<_>>(),
        }))
        .map_err(|e| e.to_string())
        .unwrap_or_else(|e| json!({ "error": e }));
    let usd = |u: ar_tokens::Usd| u.micros as f64 / 1_000_000.0;
    let rows: Vec<Value> = report
        .rows
        .iter()
        .map(|r| {
            json!({
                "key": r.key_id, "provider": r.provider, "model": r.model,
                "tokens": r.total_tokens, "usd": usd(r.cost_usd), "priced": r.priced,
            })
        })
        .collect();
    Json(json!({
        "rows": rows,
        "totals": { "usd": usd(report.totals.usd), "tokens": report.totals.tokens },
        "open": true,
        "analytics": analytics,
    }))
    .into_response()
}

/// `?days=` on the usage endpoint: absent keeps the bill-style all-time ranks.
#[derive(serde::Deserialize)]
struct UsageQuery {
    days: Option<u32>,
}

/// The request-log filter bar's query string.
#[derive(serde::Deserialize)]
struct LogQuery {
    days: Option<u32>,
    model: Option<String>,
    provider: Option<String>,
    /// `error` shows failed requests only; anything else shows all.
    status: Option<String>,
    /// Rows to skip from the newest end: the page after the last one shown.
    offset: Option<usize>,
}

/// Most log rows one response carries; the page shows a window, not a dump.
const LOG_LIMIT: usize = 200;

/// `GET /ui/api/log` — answered and failed requests, newest first, inside a
/// date range, optionally narrowed by model, provider or failure-only.
async fn request_log(State(ctx): State<Arc<Ctx>>, Query(q): Query<LogQuery>) -> Response {
    let Some(ledger) = ctx.ledger.clone() else {
        return Json(json!({ "rows": [], "open": false })).into_response();
    };
    // Empty strings are the filter bar's "any": treat them as absent.
    let pick = |v: &Option<String>| v.as_deref().filter(|s| !s.is_empty()).map(str::to_owned);
    let (model, provider) = (pick(&q.model), pick(&q.provider));
    let offset = q.offset.unwrap_or(0);
    let filter = ar_tokens::LogFilter {
        days: q.days.unwrap_or(30).clamp(1, 365),
        model: model.as_deref(),
        provider: provider.as_deref(),
        errors_only: q.status.as_deref() == Some("error"),
        limit: LOG_LIMIT,
        offset,
    };
    let guard = ledger
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match guard.log(&filter) {
        Ok(rows) => Json(json!({
            "open": true,
            "limit": LOG_LIMIT,
            // A short page is the last one; a full page may have older rows behind it.
            "next": (rows.len() == LOG_LIMIT).then_some(offset + LOG_LIMIT),
            "rows": rows.iter().map(|r| json!({
                "at": r.created_at, "key": r.key_id, "provider": r.provider, "model": r.model,
                "tokens": r.tokens, "usd": r.cost_usd.micros as f64 / 1_000_000.0,
                "priced": r.priced, "status": r.status, "error": r.error,
            })).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => Json(json!({ "open": true, "rows": [], "error": e.to_string() })).into_response(),
    }
}

// ── writes ──────────────────────────────────────────────────────────────────

/// Reads the config file and its parsed form. `Err` carries the refusal the
/// page shows — `Response` instead of a second error type, because every
/// caller of this lives inside an HTTP handler. One shape for every writer:
/// parse first, refuse without touching the file on failure.
fn load_cfg(ctx: &Ctx) -> Result<(String, Config), Box<Response>> {
    let doc = match std::fs::read_to_string(&ctx.config) {
        Ok(doc) => doc,
        Err(e) => {
            return Err(Box::new(bad_request(
                format!("cannot read {}: {e}", ctx.config.display()),
                "restore the config and retry",
            )));
        }
    };
    let cfg = match Config::parse(&doc, resolve_env) {
        Ok(cfg) => cfg,
        Err(e) => {
            return Err(Box::new(bad_request(
                format!("the config did not parse: {e}"),
                "fix the file by hand and retry",
            )));
        }
    };
    Ok((doc, cfg))
}

/// The registry catalog for the browser pane: every compiled-in provider with
/// its models and the prices the pricing table knows, read-only. `configured`
/// says this config already routes to the id, which is what turns the browser
/// into the provider-add form's autocomplete.
async fn models(State(ctx): State<Arc<Ctx>>) -> Response {
    let (_, cfg) = match load_cfg(&ctx) {
        Ok(v) => v,
        Err(resp) => return *resp,
    };
    let providers: Vec<Value> = registry()
        .iter()
        .map(|(id, def)| {
            let models = def
                .models
                .iter()
                .map(|m| {
                    let price = def.prices.get(m);
                    json!({
                        "id": m,
                        "in": price.map(|p| p.input_usd_per_mtok),
                        "out": price.map(|p| p.output_usd_per_mtok)
                    })
                })
                .collect::<Vec<_>>();
            json!({
                "id": id,
                "wire": def.wire_format.as_str(),
                "auth": &def.auth,
                "configured": cfg.providers.iter().any(|row| row.id == **id),
                "base_url": &def.base_url,
                "models": models
            })
        })
        .collect();
    Json(json!({ "providers": providers })).into_response()
}

/// Replaces the whole `combos:` section. The page round-trips the full
/// `Combo` objects the state endpoint handed it, so a save cannot lose a
/// field the editor does not show.
async fn save_combos(
    State(ctx): State<Arc<Ctx>>,
    body: Result<Json<Vec<Combo>>, JsonRejection>,
) -> Response {
    let combos = match body {
        Ok(Json(v)) => v,
        Err(r) => return body_error(r),
    };
    let (doc, cfg) = match load_cfg(&ctx) {
        Ok(v) => v,
        Err(resp) => return *resp,
    };
    if let Err(msg) = check_combos(&combos, &cfg) {
        return bad_request(msg, "fix the fields the error names; nothing was written");
    }
    let section = match crate::import::render_combos_section(&combos) {
        Ok(section) => section,
        Err(e) => return render_failed(e),
    };
    let next = commands::splice_section(&doc, "combos", &section);
    if next == doc {
        return Json(json!({ "ok": true, "pending_restart": false, "note": "identical" }))
            .into_response();
    }
    if let Err(e) = commands::write_file(&ctx.config, &next) {
        return bad_request(
            format!("cannot write {}: {e}", ctx.config.display()),
            "check permissions on the config directory",
        );
    }
    Json(json!({ "ok": true, "pending_restart": true })).into_response()
}

/// A combo list is valid when every id is a legal model name, every target
/// names a provider the registry or `custom_providers:` knows, and every
/// strategy is one this build routes rather than defers. Strategies already
/// in the file are exempt: a save must round-trip a config this page did
/// not write.
fn check_combos(combos: &[Combo], current: &Config) -> Result<(), String> {
    let custom: Vec<&str> = current
        .custom_providers
        .iter()
        .map(|c| c.id.as_str())
        .collect();
    let known_provider = |id: &str| registry().get(id).is_some() || custom.contains(&id);
    let mut ids = BTreeSet::new();
    let allowed: BTreeSet<&str> = STRATEGIES
        .iter()
        .copied()
        .chain(current.combos.iter().map(|c| c.strategy.as_str()))
        .collect();
    for c in combos {
        let id_legal = !c.id.is_empty()
            && c.id.len() <= 64
            && c.id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
        if !id_legal {
            return Err(format!(
                "combo id {:?} is not a legal model name (1-64 of a-z 0-9 . _ -)",
                c.id
            ));
        }
        if !ids.insert(c.id.clone()) {
            return Err(format!("combo id {:?} appears twice", c.id));
        }
        if c.targets.is_empty() {
            return Err(format!("combo {:?} has no targets", c.id));
        }
        for t in &c.targets {
            match t.split_once('/') {
                Some((p, m)) if !p.is_empty() && !m.is_empty() => {
                    if !known_provider(p) {
                        return Err(format!("combo {:?} targets unknown provider {p:?}", c.id));
                    }
                }
                _ if known_provider(t) => {}
                _ => {
                    return Err(format!(
                        "combo {:?} has a malformed target {t:?} (expected provider/model)",
                        c.id
                    ));
                }
            }
        }
        if !allowed.contains(c.strategy.as_str()) {
            return Err(format!(
                "combo {:?} names strategy {:?}, which this build does not route",
                c.id,
                c.strategy.as_str()
            ));
        }
        for (target, w) in &c.weights {
            if !c.targets.contains(target) {
                return Err(format!(
                    "combo {:?} weights target {target:?} it does not list",
                    c.id
                ));
            }
            if *w == 0 {
                return Err(format!(
                    "combo {:?} weights target {target:?} at zero",
                    c.id
                ));
            }
        }
        if !c.steps.is_empty() && c.steps.len() != c.targets.len() {
            return Err(format!(
                "combo {:?} carries {} steps for {} targets",
                c.id,
                c.steps.len(),
                c.targets.len()
            ));
        }
    }
    Ok(())
}

/// Sets one provider's credential in the env file. A provider the config
/// already lists keeps the variable its own `keys:` binding names — a
/// hand-wired `$AR_KEY_MINE` is written, never re-pointed. A registry
/// provider the config does not list is added whole: row, `keys:` ref and
/// env value land together, so the save cannot leave it half-configured.
async fn provider_key(
    State(ctx): State<Arc<Ctx>>,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    let body = match body {
        Ok(Json(v)) => v,
        Err(r) => return body_error(r),
    };
    let Some(id) = body["id"].as_str().map(|s: &str| s.to_owned()) else {
        return bad_request(
            "missing provider id".into(),
            "send { \"id\": \"...\", \"key\": \"...\" }",
        );
    };
    let Some(key) = body["key"].as_str() else {
        return bad_request(
            "missing provider key".into(),
            "send { \"id\": \"...\", \"key\": \"...\" }",
        );
    };
    if key.is_empty() {
        return bad_request(
            "the key is empty".into(),
            "paste the credential the provider issued",
        );
    }
    if registry().get(&id).is_none() {
        return bad_request(
            format!("unknown provider {id:?}"),
            "the id must name a registry provider (see `aroute providers`)",
        );
    }

    let (doc, cfg) = match load_cfg(&ctx) {
        Ok(v) => v,
        Err(resp) => return *resp,
    };
    let keys = raw_keys(&doc);

    let (next, var) = match cfg.providers.iter().find(|row| row.id == id) {
        Some(row) => {
            // Configured: the binding names the variable; write that one and
            // never re-point a hand-wired `$AR_KEY_*`.
            match row
                .key_refs()
                .first()
                .and_then(|name| keys.get(*name))
                .map(|v| v.trim_start_matches('$').to_owned())
            {
                Some(var) if var.starts_with("AR_KEY_") => (doc.clone(), var),
                _ => {
                    return bad_request(
                        format!("provider {id:?} has no AR_KEY_* binding for its credential"),
                        "give its `key:` entry an `$AR_KEY_*` value in config.yaml, or remove the provider row and re-add it here",
                    );
                }
            }
        }
        None => {
            // Not configured: row, `keys:` ref and env value land together,
            // matching what `aroute import` generates (entry name = id).
            let var = format!("AR_KEY_{}", crate::import::env_name(&id));
            let mut rows = cfg.providers.clone();
            rows.push(ProviderCfg {
                id: id.clone(),
                key: id.clone(),
                keys: None,
            });
            let section = match crate::import::render_providers_section(&rows) {
                Ok(section) => section,
                Err(e) => return render_failed(e),
            };
            let next = commands::splice_section(&doc, "providers", &section);
            match with_key_binding(&next, &id, var.as_str()) {
                Ok(next) => (next, var),
                Err(e) => return render_failed(e),
            }
        }
    };

    if let Err(e) = set_env_var(&ctx.env, &var, key) {
        return bad_request(
            format!("cannot write {}: {e}", ctx.env.display()),
            "check permissions on the env file",
        );
    }
    if next != doc
        && let Err(e) = commands::write_file(&ctx.config, &next)
    {
        return bad_request(
            format!("cannot write {}: {e}", ctx.config.display()),
            "the env value was set; fix the config write and retry",
        );
    }
    Json(json!({ "ok": true, "var": var, "pending_restart": true })).into_response()
}

/// Upserts one `NAME=value` in the env file, 0600, preserving every other
/// line: the same merge `sync_env_file` performs, minus the dashboard DB.
fn set_env_var(path: &std::path::Path, name: &str, value: &str) -> std::io::Result<()> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let entry = format!("{name}={value}");
    let mut lines: Vec<String> = text.lines().map(|s: &str| s.to_owned()).collect();
    match lines
        .iter()
        .position(|l| l.starts_with(&format!("{name}=")))
    {
        Some(i) => lines[i] = entry,
        None => lines.push(entry),
    }
    commands::write_private(path, &(lines.join("\n") + "\n"))
}

/// Mints a client token through the same gate the proxy verifies against.
async fn mint_key(
    State(ctx): State<Arc<Ctx>>,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    let body = match body {
        Ok(Json(v)) => v,
        Err(r) => return body_error(r),
    };
    let Some(key_id) = body["key_id"].as_str().map(|s: &str| s.to_owned()) else {
        return bad_request(
            "missing key_id".into(),
            "send { \"key_id\": \"ci-runner\" }",
        );
    };
    let legal = !key_id.is_empty()
        && key_id.len() <= 64
        && key_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if !legal {
        return bad_request(
            format!("key id {key_id:?} is not legal (1-64 of a-z 0-9 . _ -)"),
            "pick a name the ledger can key rows by",
        );
    }
    let Some(store) = ctx.store.as_ref() else {
        return conflict(
            "no credential store".into(),
            "the store lives beside the config; check `aroute doctor`",
        );
    };
    let guard = store
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(master) = commands::gate_master(&guard).ok().flatten() else {
        return conflict(
            "the gate is not armed".into(),
            "run `aroute keys arm-gate` first; minting without a master would issue tokens nothing verifies",
        );
    };
    match commands::mint_from(&master, &key_id) {
        Ok(issued) => Json(json!({
            "ok": true,
            "key_id": key_id,
            "access": issued.access,
            "refresh": issued.refresh,
            "expires_at": issued.expires_at,
        }))
        .into_response(),
        Err(e) => bad_request(
            format!("minting failed: {e}"),
            "this is an ar-keys failure; `aroute doctor` names the cause",
        ),
    }
}

/// One ceiling arm off the request body: a number sets it, an explicit
/// `null` clears it, and an *absent* field returns `Err("keep")` — the
/// handler's signal to leave whatever the row already had.
fn arm_of(body: &Value, field: &str, max: u64) -> Result<Option<u64>, String> {
    let Some(v) = body.get(field) else {
        return Err("keep".to_owned());
    };
    match v {
        Value::Null => Ok(None),
        Value::Number(n) => match n.as_u64() {
            Some(v) if v <= max => Ok(Some(v)),
            Some(_) => Err(format!("{field} is too large")),
            None => Err(format!("{field} must be a whole number")),
        },
        _ => Err(format!("{field} must be a number or null")),
    }
}

/// Sets one client key's ceiling row: any subset of the four arms `Limit`
/// carries, plus its `model` scope. Arms the body omits keep their current value; an explicit `null`
/// clears the arm. A row that ends up empty is removed, which reads back as
/// "no ceiling" — the state every config written before `limits:` existed has.
async fn set_limits(
    State(ctx): State<Arc<Ctx>>,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    let body = match body {
        Ok(Json(v)) => v,
        Err(r) => return body_error(r),
    };
    let Some(key) = body["key"].as_str().map(|s: &str| s.to_owned()) else {
        return bad_request(
            "missing key".into(),
            "send { \"key\": \"ci-runner\", \"rpm\": 600 }",
        );
    };
    if key.len() > 64 {
        return bad_request("key name too long".into(), "keep it under 64 characters");
    }
    let mut rpm: Option<u32> = None;
    let mut usd_micros: Option<u64> = None;
    let mut tokens: Option<u64> = None;
    let mut refuse_unpriced = false;
    let present = ["rpm", "usd_micros", "tokens", "refuse_unpriced"].map(|f| body.get(f).is_some());
    if present[0] {
        match arm_of(&body, "rpm", 100_000) {
            Ok(v) => rpm = v.map(|v| v as u32),
            Err(e) if e == "keep" => {}
            Err(e) => return bad_request(e, "rpm is a requests-per-minute ceiling"),
        }
        if rpm == Some(0) {
            return bad_request(
                "rpm must be 1-100000, or null to clear".into(),
                "a zero ceiling denies every request",
            );
        }
    }
    if present[1] {
        match arm_of(&body, "usd_micros", 1_000_000_000) {
            Ok(v) => usd_micros = v,
            Err(e) if e == "keep" => {}
            Err(e) => {
                return bad_request(
                    e,
                    "usd_micros is a spend ceiling in micro-dollars (1000000 = $1)",
                );
            }
        }
    }
    if present[2] {
        match arm_of(&body, "tokens", 1_000_000_000_000) {
            Ok(v) => tokens = v,
            Err(e) if e == "keep" => {}
            Err(e) => return bad_request(e, "tokens is a cumulative token ceiling"),
        }
    }
    if present[3] {
        refuse_unpriced = match &body["refuse_unpriced"] {
            Value::Bool(b) => *b,
            Value::Null => false,
            _ => {
                return bad_request(
                    "refuse_unpriced must be a boolean".into(),
                    "true fails closed on a model with no pricing row",
                );
            }
        };
    }
    // Scope: omitted keeps, null or "" clears, a string sets. Checked against
    // the config below, so a typo cannot arm a ceiling that never applies.
    let scope_present = body.get("model").is_some();
    let raw_scope = match &body["model"] {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        _ => {
            return bad_request(
                "model must be a string or null".into(),
                "name a combo id, or null to scope the row to every model",
            );
        }
    };
    let (doc, mut cfg) = match load_cfg(&ctx) {
        Ok(v) => v,
        Err(resp) => return *resp,
    };
    // Checked against the config, so a typo cannot arm a ceiling that never applies.
    let scope = match commands::limit_scope(&raw_scope, &cfg.combos) {
        Ok(scope) => scope,
        Err((message, help)) => return bad_request(message, help),
    };
    let row = cfg.limits.keys.entry(key).or_default();
    if scope_present {
        row.model = scope;
    }
    if present[0] {
        row.rpm = rpm;
    }
    if present[1] {
        row.usd_micros = usd_micros;
    }
    if present[2] {
        row.tokens = tokens;
    }
    if present[3] {
        row.refuse_unpriced = refuse_unpriced;
    }
    if row.is_empty() {
        cfg.limits.keys.retain(|_, r| !r.is_empty());
    }
    let next =
        commands::splice_section(&doc, "limits", &crate::import::limits::render(&cfg.limits));
    if next == doc {
        return Json(json!({ "ok": true, "pending_restart": false, "note": "identical" }))
            .into_response();
    }
    if let Err(e) = commands::write_file(&ctx.config, &next) {
        return bad_request(
            format!("cannot write {}: {e}", ctx.config.display()),
            "check permissions on the config directory",
        );
    }
    Json(json!({ "ok": true, "pending_restart": true })).into_response()
}

// ── writes: custom providers ───────────────────────────────────────────────

/// Registers or edits one custom provider. Rows live under `custom_providers:`
/// (model prefix `{id}` → base_url) and their credentials ride the same `ar.env`
/// + `keys:` indirection every other provider does.
///
/// Body fields: `id`, `protocol` (`openai-compatible` | `anthropic-compatible`),
/// `base_url`, optional `endpoint` (a relative path overriding the protocol's
/// default one), `key` — required on create, optional on edit, where an empty
/// key means “rewrite nothing” (the binding keeps its variable).
async fn provider_custom(
    State(ctx): State<Arc<Ctx>>,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    let body = match body {
        Ok(Json(v)) => v,
        Err(r) => return body_error(r),
    };
    let trim = |v: Option<&str>| {
        v.map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    const SHAPE: &str = "send { \"id\", \"protocol\", \"base_url\" [, \"endpoint\", \"key\"] }";
    let Some(id) = trim(body["id"].as_str()) else {
        return bad_request("missing provider id".into(), SHAPE);
    };
    let legal = id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if !legal {
        return bad_request(
            format!("provider id {id:?} is not legal (1-64 of a-z 0-9 . _ -)"),
            "the id becomes a model prefix clients route through",
        );
    }
    if registry().get(&id).is_some() {
        return bad_request(
            format!("{id:?} is a registry catalog id"),
            "custom providers take a fresh name; set a catalog provider's key with \"add provider\" instead",
        );
    }
    let protocol = match body["protocol"].as_str().unwrap_or_default() {
        "openai-compatible" => ar_registry::Protocol::OpenaiCompatible,
        "anthropic-compatible" => ar_registry::Protocol::AnthropicCompatible,
        other => {
            return bad_request(
                format!("unknown protocol {other:?}"),
                "protocol is openai-compatible or anthropic-compatible",
            );
        }
    };
    let Some(base_url) = trim(body["base_url"].as_str())
        .filter(|s| s.starts_with("http://") || s.starts_with("https://"))
    else {
        return bad_request(
            "missing or bad base_url".into(),
            "the endpoint the {id}/ prefix joins, usually https://host/v1",
        );
    };
    let endpoint = trim(body["endpoint"].as_str());
    if endpoint
        .as_ref()
        .is_some_and(|e| e.starts_with('/') || e.contains("://"))
    {
        return bad_request(
            format!("endpoint {endpoint:?} is not a relative path"),
            "the endpoint overrides the protocol's default path joined to base_url, e.g. chat/completions2",
        );
    }
    let key = body["key"].as_str().unwrap_or_default().trim().to_owned();

    let (doc, cfg) = match load_cfg(&ctx) {
        Ok(v) => v,
        Err(resp) => return *resp,
    };
    let existed = cfg.custom_providers.iter().any(|c| c.id == id);
    if key.is_empty() && !existed {
        return bad_request(
            "a new custom provider needs its key".into(),
            "send the credential the gateway issued alongside the row",
        );
    }
    // Edit keeps the binding's variable: a hand-wired `$AR_KEY_MINE` never
    // re-points to the canonical name just because a credential was pasted.
    let canonical = format!("AR_KEY_{}", crate::import::env_name(&id));
    let bound = raw_keys(&doc)
        .get(&id)
        .and_then(|v| v.strip_prefix('$'))
        .unwrap_or(&canonical)
        .to_owned();
    if key.is_empty() && !bound.starts_with("AR_KEY_") {
        return bad_request(
            format!("{id:?} binds to ${bound}, not an AR_KEY_* variable"),
            "re-point the keys entry by hand, or send the current key again",
        );
    }
    let var: &str = if existed && bound.starts_with("AR_KEY_") && bound != canonical {
        &bound
    } else {
        &canonical
    };
    let rows = upsert_custom_rows(
        cfg.custom_providers,
        ar_registry::CustomProvider {
            id: id.clone(),
            protocol,
            base_url,
            endpoint,
            key_ref: id.clone(),
            headers: BTreeMap::new(),
        },
    );
    let section = match crate::import::render_custom_section(&rows) {
        Ok(section) => section,
        Err(e) => return render_failed(e),
    };
    let mut next = commands::splice_section(&doc, "custom_providers", &section);
    if !key.is_empty() {
        // Env first: the config write below is the one that publishes it.
        if let Err(e) = set_env_var(&ctx.env, var, &key) {
            return bad_request(
                format!("cannot write {}: {e}", ctx.env.display()),
                "check permissions on the env file",
            );
        }
        next = match with_key_binding(&next, &id, var) {
            Ok(next) => next,
            Err(e) => return render_failed(e),
        };
    }
    if next == doc {
        return Json(json!({ "ok": true, "pending_restart": false, "note": "identical" }))
            .into_response();
    }
    if let Err(e) = commands::write_file(&ctx.config, &next) {
        return bad_request(
            format!("cannot write {}: {e}", ctx.config.display()),
            "check permissions on the config directory",
        );
    }
    Json(json!({ "ok": true, "pending_restart": true })).into_response()
}

/// The next `custom_providers:` row list: `custom` upserted by id, sorted by
/// id — the order the import renderers emit.
fn upsert_custom_rows(
    mut rows: Vec<ar_registry::CustomProvider>,
    custom: ar_registry::CustomProvider,
) -> Vec<ar_registry::CustomProvider> {
    rows.retain(|c| c.id != custom.id);
    rows.push(custom);
    rows.sort_by(|a, b| a.id.cmp(&b.id));
    rows
}

/// Removes everything one provider wrote to config.yaml — its row (registry
/// or custom) plus its `keys:` entries, once no surviving row shares them.
/// Returns `None` when the config names no row for the id: the handler turns
/// that into a refusal rather than a lying success.
///
/// The `ar.env` line is never touched: an unset `$AR_KEY_*` is harmless, and
/// the same variable may still back a row that stays.
fn splice_remove_provider(doc: &str, cfg: &Config, id: &str) -> anyhow::Result<Option<String>> {
    if !cfg.providers.iter().any(|p| p.id == id) && !cfg.custom_providers.iter().any(|c| c.id == id)
    {
        return Ok(None);
    }
    let providers = crate::import::render_providers_section(
        &cfg.providers
            .iter()
            .filter(|p| p.id != id)
            .cloned()
            .collect::<Vec<_>>(),
    )?;
    let next = commands::splice_section(doc, "providers", &providers);
    let kept: BTreeSet<String> = cfg
        .providers
        .iter()
        .filter(|p| p.id != id)
        .flat_map(|p| p.key_refs().into_iter().map(str::to_owned))
        .chain(
            cfg.custom_providers
                .iter()
                .filter(|c| c.id != id)
                .map(|c| c.key_ref.clone()),
        )
        .collect();
    let customs = crate::import::render_custom_section(
        &cfg.custom_providers
            .iter()
            .filter(|c| c.id != id)
            .cloned()
            .collect::<Vec<_>>(),
    )?;
    let next = commands::splice_section(&next, "custom_providers", &customs);
    // Keys entries the removed rows alone held: every entry the removed rows
    // name, minus the entries the surviving rows name. Shared entries survive
    // by construction, so this is exactly a “no dangling reference” edit.
    let orphaned: BTreeSet<String> = cfg
        .providers
        .iter()
        .filter(|p| p.id == id)
        .flat_map(|p| p.key_refs().into_iter().map(str::to_owned))
        .chain(
            cfg.custom_providers
                .iter()
                .filter(|c| c.id == id)
                .map(|c| c.key_ref.clone()),
        )
        .filter(|name| !kept.contains(name))
        .collect();
    if orphaned.is_empty() {
        return Ok(Some(next));
    }
    let keys = raw_keys(doc)
        .into_iter()
        .filter(|(name, _)| !orphaned.contains(name))
        .collect();
    let section = crate::import::render_keys_section(&keys)?;
    Ok(Some(commands::splice_section(&next, "keys", &section)))
}

/// Removes one provider: refuses while any combo still references it, refuses
/// when the id is configured nowhere, otherwise drops the row and its
/// orphaned `keys:` entries in one file write that keeps the rest byte-true.
///
/// Body: { "id": "..." }. The `ar.env` line is never touched (see
/// [`splice_remove_provider`]).
async fn provider_remove(
    State(ctx): State<Arc<Ctx>>,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    let body = match body {
        Ok(Json(v)) => v,
        Err(r) => return body_error(r),
    };
    let Some(id) = body["id"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
    else {
        return bad_request("missing provider id".into(), "send { \"id\": \"...\" }");
    };
    let (doc, cfg) = match load_cfg(&ctx) {
        Ok(v) => v,
        Err(resp) => return *resp,
    };
    let referencing = |c: &Combo| {
        c.targets
            .iter()
            .chain(c.pool.iter())
            .chain(c.steps.iter().map(|s| &s.target))
            .chain(c.judge_model.iter())
            .any(|t| commands::target_provider(t) == id.as_str())
    };
    let users: Vec<&str> = cfg
        .combos
        .iter()
        .filter(|c| referencing(c))
        .map(|c| c.id.as_str())
        .collect();
    if !users.is_empty() {
        return conflict(
            format!("{id:?} is still referenced by: {}", users.join(", ")),
            "delete or repoint those combos first; a dead provider fails loudly, a dangling one fails late",
        );
    }
    let next = match splice_remove_provider(&doc, &cfg, &id) {
        Ok(next) => next,
        Err(e) => return render_failed(e),
    };
    let Some(next) = next else {
        return conflict(
            format!("{id:?} is not configured in this file"),
            "the providers pane lists what is configured",
        );
    };
    if let Err(e) = commands::write_file(&ctx.config, &next) {
        return bad_request(
            format!("cannot write {}: {e}", ctx.config.display()),
            "check permissions on the config directory",
        );
    }
    Json(json!({ "ok": true, "pending_restart": true })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arm_of_reads_null_as_clear_and_an_absent_field_as_keep() {
        let body: Value =
            serde_json::from_str(r#"{"rpm": 600, "tokens": null}"#).expect("fixture parses");
        assert_eq!(arm_of(&body, "rpm", 100_000), Ok(Some(600)));
        assert_eq!(
            arm_of(&body, "tokens", 1_000_000_000_000),
            Ok(None),
            "an explicit null clears"
        );
        assert_eq!(
            arm_of(&body, "usd_micros", 1_000_000),
            Err("keep".to_owned()),
            "an absent field keeps the row's value"
        );
        let over: Value = serde_json::from_str(r#"{"rpm": 999999}"#).expect("fixture parses");
        assert!(
            arm_of(&over, "rpm", 100_000).is_err(),
            "a ceiling past max is refused"
        );
    }

    #[test]
    fn upsert_custom_rows_replaces_by_id_and_sorts() {
        let row = |id: &str| ar_registry::CustomProvider {
            id: id.to_owned(),
            protocol: ar_registry::Protocol::OpenaiCompatible,
            base_url: format!("https://{id}.example/v1"),
            endpoint: None,
            key_ref: id.to_owned(),
            headers: BTreeMap::new(),
        };
        let ids = |rows: Vec<ar_registry::CustomProvider>| -> Vec<String> {
            rows.into_iter().map(|r| r.id).collect()
        };
        assert_eq!(
            ids(upsert_custom_rows(vec![row("b"), row("c")], row("a"))),
            ["a", "b", "c"]
        );
        assert_eq!(
            ids(upsert_custom_rows(
                vec![row("a"), row("b"), row("c")],
                row("b")
            )),
            ["a", "b", "c"],
            "an existing id is replaced, not duplicated"
        );
    }

    #[test]
    fn splice_remove_provider_drops_owned_bindings_and_keeps_shared_ones() {
        let doc = concat!(
            "keys:\n",
            "  shared: $AR_KEY_SHARED\n",
            "  dkey: $AR_KEY_D\n",
            "providers:\n",
            "  - id: pa\n",
            "    key: shared\n",
            "  - id: pc\n",
            "    key: shared\n",
            "custom_providers:\n",
            "  - id: pd\n",
            "    protocol: openai-compatible\n",
            "    base_url: https://d.example/v1\n",
            "    key_ref: dkey\n",
        );
        let cfg = Config::parse(doc, resolve_env).expect("fixture must parse");
        assert!(
            splice_remove_provider(doc, &cfg, "zzz")
                .expect("renders")
                .is_none(),
            "an unknown id is a refusal, not an edit"
        );

        // pd alone holds `dkey`, so the row and its binding go together.
        let next = splice_remove_provider(doc, &cfg, "pd")
            .expect("renders")
            .expect("pd is configured");
        let keys = raw_keys(&next);
        assert!(!keys.contains_key("dkey"), "{keys:?}");
        assert!(keys.contains_key("shared"), "{keys:?}");
        let cfg2 = Config::parse(&next, resolve_env).expect("result must still parse");
        assert!(cfg2.custom_providers.is_empty());

        // pa shares `shared` with pc, so the binding — and therefore the
        // keys section — is untouched.
        let next2 = splice_remove_provider(&next, &cfg2, "pa")
            .expect("renders")
            .expect("pa is configured");
        assert_eq!(
            raw_keys(&next2),
            keys,
            "a shared binding survives a row's removal"
        );
    }

    #[test]
    fn set_env_var_upserts_one_line_and_preserves_the_rest() {
        let dir = std::env::temp_dir().join(format!("ar-webui-env-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("ar.env");
        std::fs::write(&path, "OTHER=1\nAR_KEY_OPENAI=old\n").ok();
        set_env_var(&path, "AR_KEY_OPENAI", "new").ok();
        set_env_var(&path, "AR_KEY_NEW", "fresh").ok();
        let text = std::fs::read_to_string(&path).ok().unwrap_or_default();
        assert_eq!(text, "OTHER=1\nAR_KEY_OPENAI=new\nAR_KEY_NEW=fresh\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn check_combos_rejects_a_target_the_catalog_does_not_know() {
        let yaml = "keys:\n  k: v\ncombos:\n  - id: c\n    strategy: priority\n    targets: [openai/gpt-5.4]\n";
        let cfg = Config::parse(yaml, resolve_env).ok();
        let Some(cfg) = cfg else {
            panic!("fixture config must parse")
        };
        let mut combo = cfg.combos[0].clone();
        combo.id = "bad".into();
        combo.targets = vec!["nobody/model".into()];
        assert!(check_combos(&[combo], &cfg).is_err());
    }

    #[test]
    fn check_combos_accepts_a_round_trip_of_the_fixture() {
        let yaml = "keys:\n  k: v\ncombos:\n  - id: c\n    strategy: priority\n    targets: [openai/gpt-5.4]\n";
        let cfg = Config::parse(yaml, resolve_env).ok();
        let Some(cfg) = cfg else {
            panic!("fixture config must parse")
        };
        assert!(check_combos(&cfg.combos, &cfg).is_ok());
    }
}

/// The `/ui` endpoints driven through the router, the way `serve` mounts them,
/// with no socket: a request in, a status and JSON body out.
#[cfg(test)]
mod http_tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Method, Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    const CONFIG: &str = "keys:\n  local: dummy\ncombos:\n  - id: c\n    strategy: priority\n    targets:\n      - openai/gpt-5.4\n";

    /// A `/ui` router over a fresh config file, and that file's path.
    fn ui(tag: &str, ledger: Option<ar_tokens::Ledger>) -> (Router, PathBuf) {
        let dir = std::env::temp_dir().join(format!("ar-webui-http-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let config = dir.join("config.yaml");
        std::fs::write(&config, CONFIG).expect("config");
        let ledger = ledger.map(|l| Arc::new(Mutex::new(l)));
        let app = mount(Router::new(), Ctx::new(config.clone(), ledger, None, false));
        (app, config)
    }

    async fn call(app: &Router, method: Method, uri: &str, body: &str) -> (StatusCode, Value) {
        let req = Request::builder()
            .header("host", "127.0.0.1:20128")
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body.to_owned()))
            .expect("request");
        let resp = app.clone().oneshot(req).await.expect("router answers");
        let status = resp.status();
        let bytes = resp.into_body().collect().await.expect("body").to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[tokio::test]
    async fn the_log_should_answer_failures_inside_the_date_range() {
        let ledger = ar_tokens::Ledger::open_in_memory().expect("ledger");
        ledger
            .record_failure("ci", "c", 502, "every provider is down", 1_000_000)
            .expect("row");
        let (app, _) = ui("log", Some(ledger));
        let (status, body) = call(&app, Method::GET, "/ui/api/log?days=30", "").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["rows"].as_array().map(Vec::len), Some(1));
        assert_eq!(body["rows"][0]["status"], 502);
        let (_, errors) = call(&app, Method::GET, "/ui/api/log?status=error", "").await;
        assert_eq!(
            errors["rows"].as_array().map(Vec::len),
            Some(1),
            "the failed-only view keeps the failure"
        );
        let (_, other) = call(&app, Method::GET, "/ui/api/log?model=nope", "").await;
        assert_eq!(
            other["rows"].as_array().map(Vec::len),
            Some(0),
            "a model filter narrows the rows"
        );
    }

    #[tokio::test]
    async fn the_log_should_say_it_is_closed_without_a_ledger() {
        let (app, _) = ui("log-closed", None);
        let (status, body) = call(&app, Method::GET, "/ui/api/log", "").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["open"], false);
        assert_eq!(body["rows"].as_array().map(Vec::len), Some(0));
    }

    #[tokio::test]
    async fn usage_should_answer_for_a_day_count_outside_the_range() {
        let ledger = ar_tokens::Ledger::open_in_memory().expect("ledger");
        let (app, _) = ui("usage-clamp", Some(ledger));
        for days in ["0", "99999"] {
            let (status, _) =
                call(&app, Method::GET, &format!("/ui/api/usage?days={days}"), "").await;
            assert_eq!(status, StatusCode::OK, "days={days} must clamp, not fail");
        }
    }

    #[tokio::test]
    async fn limits_should_refuse_a_scope_that_names_no_combo_and_write_nothing() {
        let (app, config) = ui("limits-unknown", None);
        let (status, body) = call(
            &app,
            Method::POST,
            "/ui/api/limits",
            r#"{"key":"ci","rpm":5,"model":"nope"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body["error"]
                .as_str()
                .unwrap_or_default()
                .contains("no combo named")
        );
        assert_eq!(
            std::fs::read_to_string(&config).expect("config"),
            CONFIG,
            "a refusal leaves the file alone"
        );
    }

    #[tokio::test]
    async fn limits_should_keep_a_scope_when_omitted_and_clear_it_on_null() {
        let (app, config) = ui("limits-scope", None);
        let (status, _) = call(
            &app,
            Method::POST,
            "/ui/api/limits",
            r#"{"key":"ci","rpm":5,"model":"c"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            std::fs::read_to_string(&config)
                .expect("config")
                .contains("model: \"c\"")
        );

        let (status, _) = call(
            &app,
            Method::POST,
            "/ui/api/limits",
            r#"{"key":"ci","rpm":7}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let kept = std::fs::read_to_string(&config).expect("config");
        assert!(
            kept.contains("model: \"c\"") && kept.contains("rpm: 7"),
            "an omitted scope is kept: {kept}"
        );

        let (status, _) = call(
            &app,
            Method::POST,
            "/ui/api/limits",
            r#"{"key":"ci","model":null}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            !std::fs::read_to_string(&config)
                .expect("config")
                .contains("model:"),
            "null clears the scope"
        );
    }

    #[tokio::test]
    async fn limits_should_refuse_a_scope_that_is_not_a_string() {
        let (app, _) = ui("limits-type", None);
        let (status, _) = call(
            &app,
            Method::POST,
            "/ui/api/limits",
            r#"{"key":"ci","rpm":5,"model":5}"#,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
}
