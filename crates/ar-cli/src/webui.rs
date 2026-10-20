//! `aroute serve`'s built-in control page: the Rust binary serves its own UI.
//!
//! docs/16 Option B. The vendored Node dashboard (docs/14) is a second
//! product with a second store and a second gateway; this module is the
//! thin skin over the primitives the proxy already owns — the same yaml
//! `aroute sync` splices, the same env file, the same `ar-keys` gate, the
//! same usage ledger — so there is nothing to bridge and nothing to sync.
//!
//! Scope discipline: read state, set provider credentials, edit combos,
//! mint client keys, set per-key limits (rpm/spend/tokens), read usage, and
//! report the resolver's live resilience state. No analytics, no agent
//! tasks, no i18n. Every write validates before the file is touched, and
//! the file is never rewritten when a body is rejected.
//!
//! Trust model: `serve` binds loopback only (its `bind_addr` call hardcodes
//! `public: false`), so on a box with no gate the page inherits the CLI's own
//! trust boundary — whoever can reach the socket can already call the proxy.
//!
//! **When a gate IS armed, the panel carries its own** (see
//! [`auth`]). The socket's boundary is not enough there: the panel mints the
//! tokens that authorize `/v1` and rewrites the config that decides where
//! `/v1` sends traffic, so a panel reachable without a credential is a
//! panel that can hand out spend. The three-armed gate state — unarmed,
//! armed, broken — is what makes the difference between those cases
//! decidable at boot rather than at request time.
//!
//! Boot-built routing means a write changes the *file*, not the running
//! tables: every save answers with `pending_restart`, and the banner shows
//! the restart command until `ConfigHandle::watch` is attached (docs/16 P2).

pub mod auth;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Json, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::{Value, json};

use ar_config::{Combo, Config, ProviderCfg};
use ar_registry::global as registry;
use ar_route::Resilience;

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
    /// The panel's own gate, resolved at boot. `Unarmed` on a box that never
    /// armed one, `Armed` when the row decrypts, `Broken` when it does not —
    /// and `Broken` is the arm that refuses writes.
    ///
    /// The **only** source of the panel's armed-badge: the badge used to be
    /// computed from a second `gate_master` call, and two calls could
    /// disagree — a badge saying "unarmed" on a panel that was refusing every
    /// request. One value, one source, no way to disagree.
    gate: Arc<auth::GateState>,
    /// The server's live resilience table, so the resilience view reports the
    /// resolver's actual state rather than a guess read from the config.
    resilience: Arc<Resilience>,
    /// The server's live provider ids, so the resilience view names the
    /// universe it reports on.
    providers: Vec<String>,
    /// The server's resolved config, for the settings view: the values the
    /// proxy actually booted with, which are not always the values the file
    /// spells (a discovered context window, a reduced model deadline).
    settings: ServerSettings,
}

/// The settings the panel shows that do NOT live in the yaml.
///
/// The yaml-backed ones (`server:`, `limits:`) are read from the **live
/// file** at request time, because the panel edits that file and a pane
/// showing the boot snapshot would show the operator the wrong thing after
/// their own edit. These four are the opposite: they are read at boot from
/// the environment, and the panel names the variable that carries each one.
#[derive(Debug, Clone, Default)]
pub struct ServerSettings {
    /// The router-wide request deadline, in seconds.
    pub request_timeout_secs: u64,
    /// Per-model deadlines, in seconds, by model or combo id.
    pub timeouts: BTreeMap<String, u64>,
    /// Whether `/v1/models` overlays the models.dev discovery catalog.
    pub model_discovery: bool,
    /// The gate's mode for a request with no usable credential.
    pub auth_mode: String,
}

impl Ctx {
    /// Assembled by `serve` from what it already resolved. `boot_text` is
    /// read here rather than passed, so the snapshot is by construction the
    /// text this process booted from.
    pub fn new(
        config: PathBuf,
        ledger: Option<Arc<Mutex<ar_tokens::Ledger>>>,
        store: Option<ar_keys::CredentialStore>,
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
            gate: Arc::new(auth::GateState::Unarmed),
            resilience: Arc::new(Resilience::new()),
            providers: Vec::new(),
            settings: ServerSettings::default(),
        }
    }

    /// Whether the gate is armed, read from the one gate state.
    ///
    /// Derived rather than stored, so the badge and the gate cannot disagree:
    /// there is no second copy of the fact to fall out of step.
    #[must_use]
    pub fn gate_armed(&self) -> bool {
        self.gate.is_armed()
    }

    /// The gate's state, as the page names it: the label and the badge class.
    #[must_use]
    pub fn gate_badge(&self) -> (&'static str, &'static str) {
        auth::gate_badge(&self.gate)
    }

    /// The reason the gate is broken, when it is.
    #[must_use]
    pub fn gate_broken_reason(&self) -> Option<&str> {
        match self.gate.as_ref() {
            auth::GateState::Broken(reason) => Some(reason),
            _ => None,
        }
    }

    /// The server's live state, attached after the server is built.
    ///
    /// The panel's read views report the *running* proxy — its resilience
    /// table, its provider list, its resolved settings — not a second
    /// derivation from the file. A file and a running proxy can disagree (the
    /// file was edited, the proxy has not restarted), and a panel that
    /// reported the file's version would show a state the proxy is not in.
    ///
    /// The combo table is deliberately absent: the panel reads combos from
    /// the config file, which is the file it edits, and threading a second
    /// copy of the combo table through the server's own shape would be a
    /// second source of truth for the same thing.
    pub fn attach_server(
        &mut self,
        resilience: Arc<Resilience>,
        providers: Vec<String>,
        settings: ServerSettings,
    ) {
        self.resilience = resilience;
        self.providers = providers;
        self.settings = settings;
    }

    /// The panel's gate state, set once the credential store has been read.
    ///
    /// Three arms, and the arm is decided by *decrypting* the gate row rather
    /// than by finding its name: a row that will not open is a broken gate,
    /// and a broken gate refuses writes.
    pub fn set_gate(&mut self, gate: auth::GateState) {
        self.gate = Arc::new(gate);
    }
}

/// The `/ui` routes on top of the proxy's router. `app()` has already
/// applied its own state, so the merge is a plain stateless join.
pub fn mount(proxy: Router, ctx: Ctx) -> Router {
    let gate = Arc::clone(&ctx.gate);
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
        .route("/ui/api/resilience", get(resilience))
        .route("/ui/api/health", get(health))
        .route("/ui/api/budgets", get(budgets))
        .route("/ui/api/settings", get(settings).post(save_settings))
        .route("/ui/api/combos", post(save_combos))
        .route("/ui/api/provider-key", post(provider_key))
        .route("/ui/api/provider-custom", post(provider_custom))
        .route("/ui/api/provider-remove", post(provider_remove))
        .route("/ui/api/keys/mint", post(mint_key))
        .route("/ui/api/limits", post(set_limits))
        .with_state(Arc::new(ctx));
    // One guard for the whole panel, and it is NOT the proxy's `loopback_only`.
    //
    // `loopback_only` refuses a request whose `Origin` names a non-loopback
    // authority. That is the right defence for a loopback service and the
    // wrong one for a panel published through a reverse proxy: the browser
    // attaches `Origin` to every same-origin `POST` and to a CSS-driven font
    // fetch, so the check refused exactly the legitimate operator while
    // admitting every script on the internet (which sends no `Origin` at
    // all). That is the F2 finding, and it is why the panel was half-dead on
    // its own public URL.
    //
    // So the panel's guard keeps the half of the rule that is a *rebinding*
    // defence — the `Host` header, which a rebinding page cannot choose — and
    // replaces the `Origin` half with the thing that is actually an access
    // control: **the credential**. A cross-origin caller that presents a
    // valid token is the operator's own browser; a cross-origin caller that
    // does not is refused by the gate.
    //
    // The static assets carry no data, so they get the `Host` check and
    // nothing else — refusing a font because a browser attached an `Origin`
    // to it protects nothing and costs the panel its typography.
    ui.layer(axum::middleware::from_fn(move |req: axum::extract::Request, next: axum::middleware::Next| {
        let gate = Arc::clone(&gate);
        async move { ui_guard(&gate, req, next).await }
    }))
        .merge(proxy)
}

/// The panel's own guard: the `Host` rebinding check, the credential on the
/// API surface, and an `Origin` rule that only bites when there is no
/// credential to check.
///
/// Four rules, and the order they fire in:
///
/// 1. **`Host` must be loopback.** The rebinding defence. A page that
///    rebinds its own name to `127.0.0.1` cannot choose this header, so this
///    is what stops a drive-by page from reaching the panel at all.
/// 2. **An API request with a valid credential is admitted**, whatever its
///    `Origin`. The credential is the access control, and it is in the
///    request body of the caller's choosing — a cross-origin page cannot
///    attach a token it cannot read.
/// 3. **An API request with no valid credential is refused** with a 401.
/// 4. **An API request on an UNARMED panel is admitted only when its
///    `Origin` is absent or loopback.** This is the loopback install's only
///    CSRF defence and it must not be traded away: the panel mints spendable
///    tokens, and on a box with no gate the *socket* is the trust boundary,
///    so a cross-site `POST` from the operator's own browser is exactly the
///    attack the `Origin` header identifies.
///
/// Rule 4 is why the `Origin` check is not simply deleted. It used to be
/// unconditional, and that was the F2 finding: a browser attaches `Origin`
/// to every same-origin `POST` and to a CSS-driven font fetch, so the check
/// refused the legitimate operator on the public URL while admitting every
/// non-browser client (which sends no `Origin` at all). Now it fires only
/// where there is no credential to check instead.
async fn ui_guard(
    gate: &auth::GateState,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let host_ok = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .is_some_and(loopback_authority);
    if !host_ok {
        return (
            StatusCode::FORBIDDEN,
            [(header::CONTENT_TYPE, "application/json")],
            r#"{"error":{"type":"forbidden","code":"non_loopback_request","message":"this panel answers loopback clients only; the Host header names another host"}}"#,
        )
            .into_response();
    }
    if req.uri().path().starts_with("/ui/api/") {
        // Two rules, and which one applies is decided by the gate's ARM, not
        // by the outcome of the credential check. `auth::check` short-circuits
        // to `Ok` for an unarmed panel (there is no token to ask for there),
        // so a rule keyed on the check's *outcome* would never run on exactly
        // the install it exists to protect.
        if gate.requires_token() {
            // Armed: the credential is the control. A valid token stands in
            // for the CSRF check — a cross-origin page cannot attach a token
            // it cannot read — so no `Origin` rule applies here.
            if let Err(reason) = auth::check(req.headers(), gate) {
                return unauthorized(reason);
            }
        } else if !origin_is_absent_or_loopback(req.headers()) {
            // Unarmed: the socket's loopback bind is the trust boundary, and
            // the `Origin` header is the only thing that says "this came from
            // another page's browser". The panel mints spendable tokens, so
            // this is the install's only CSRF defence and it must not be
            // traded away.
            return (
                StatusCode::FORBIDDEN,
                [(header::CONTENT_TYPE, "application/json")],
                r#"{"error":{"type":"forbidden","code":"cross_site_request","message":"this panel refuses a cross-site request while its gate is unarmed"}}"#,
            )
                .into_response();
        }
    }
    next.run(req).await
}

/// Whether a request's `Origin` is absent or names a loopback authority.
///
/// An absent `Origin` is a non-browser client — `curl`, a script, the CLI —
/// and the loopback bind is its trust boundary. A present one must name
/// loopback, which is what a browser on the operator's own machine sends.
fn origin_is_absent_or_loopback(headers: &axum::http::HeaderMap) -> bool {
    headers.get(header::ORIGIN).is_none_or(|v| {
        v.to_str()
            .ok()
            .and_then(|o| o.split_once("://"))
            .is_some_and(|(_, rest)| {
                loopback_authority(rest.split('/').next().unwrap_or_default())
            })
    })
}

/// `true` when a `host[:port]` authority names the loopback interface.
///
/// The same rule `ar_server::app::loopback_only` applies, reimplemented here
/// because that one is private and the panel's rule differs: the panel checks
/// `Host` and not `Origin`.
fn loopback_authority(authority: &str) -> bool {
    let host = if let Some(bracketed) = authority.strip_prefix('[') {
        match bracketed.split_once(']') {
            Some((inner, _)) => inner,
            None => return false,
        }
    } else {
        match authority.rsplit_once(':') {
            Some((h, port)) if !h.contains(':') && port.bytes().all(|b| b.is_ascii_digit()) => h,
            _ => authority,
        }
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// The 401 the panel's gate answers with.
///
/// The same envelope shape `/v1` uses, so one error renderer covers both
/// surfaces, and `no-store` so a refusal the browser may keep does not also
/// keep a successful response.
fn unauthorized(reason: String) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        json!({
            "error": {
                "type": "invalid_request_error",
                "code": "credential_rejected",
                "message": reason,
            }
        })
        .to_string(),
    )
        .into_response()
}

// ── assets ──────────────────────────────────────────────────────────────────

/// One page, three files, embedded: a UI that must also work on an offline
/// VPS cannot fetch fonts or bundles at runtime.
///
/// The response carries the panel's two framing headers:
///
/// * `Referrer-Policy: no-referrer` — the page can be reached with a
///   `?token=` on the URL, and the Referer of the next navigation would
///   carry it. The page strips the token from the address bar, but the
///   header is the guarantee rather than the habit.
/// * `X-Frame-Options: DENY` — the panel is not a thing to embed in a
///   frame, and a framed panel is a clickjacking surface over the write
///   buttons.
async fn page() -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::X_FRAME_OPTIONS, "DENY"),
        ],
        include_str!("webui/page/index.html"),
    )
        .into_response()
}

/// The trailing-slash form serves the page directly rather than redirecting.
///
/// A redirect would drop the query string, and the query string is how the
/// panel's `?token=` first-paint flow arrives. It would also put the token
/// in this server's access log and in every proxy log in front of it — the
/// page strips the token from the address bar on arrival, but a redirect
/// happens first. Serving the page is one fewer hop and one fewer copy of
/// the secret.
async fn page_slash() -> Response {
    page().await
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

/// The refusal every write handler makes when the gate is broken.
///
/// AR-3: a store whose gate row will not decrypt leaves the operator
/// believing the gate is armed while it is not — the boot log says "serving
/// ungated". Reads may still degrade (a stale credential should not black out
/// the operator's dashboard), but a **write** in that state is how the panel
/// mints a token nobody can revoke and rewrites the routing nobody can
/// audit. So the write path refuses, and it names the cause.
///
/// This is one function rather than a check in each handler because "every
/// write refuses" is a property of the whole class, and a check per handler
/// is a check one handler can forget.
fn refuse_if_broken(ctx: &Ctx) -> Option<Response> {
    let reason = ctx.gate.write_refusal()?;
    Some(conflict(
        format!("the credential store's gate row cannot be read: {reason}"),
        "the panel cannot tell which credentials it is managing; fix $AR_MASTER_KEY or re-arm with `aroute keys arm-gate`",
    ))
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
        "gate_armed": ctx.gate_armed(),
        "gate_badge": ctx.gate_badge().0,
        "gate_broken": ctx.gate_broken_reason(),
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
            // The per-combo figure: what each name the CLIENT asked for cost.
            // Grouped by the recorded combo, so two combos sharing a target
            // are still told apart — a join on the serving target would
            // charge the same spend to both of them.
            "combos": a.combos.iter().map(|c| json!({
                "name": c.name, "requests": c.requests, "tokens": c.tokens, "usd": c.cost_usd_micros as f64 / 1_000_000.0,
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

/// `GET /ui/api/resilience` — the resolver's live resilience state.
///
/// The three tables the router owns, read once: the key cooldowns, the
/// `provider:model` lockouts and the per-provider breakers. This is the
/// thing the failover rail *implies* — a rail shows the order the targets
/// would be tried in, and this shows which of them is currently out of
/// rotation and for how long.
///
/// Read from the server's own table rather than derived from the config or
/// the ledger: the table is the only place that knows a provider was
/// skipped. The ledger knows what was served; it does not know what was
/// skipped.
async fn resilience(State(ctx): State<Arc<Ctx>>) -> Response {
    let ids: Vec<&str> = ctx.providers.iter().map(String::as_str).collect();
    let snapshot = ctx.resilience.snapshot(&ids);
    let rows: Vec<Value> = snapshot
        .iter()
        .map(|p| {
            json!({
                "provider": p.provider,
                "breaker": match p.breaker {
                    ar_route::BreakerState::Closed => "closed",
                    ar_route::BreakerState::Open => "open",
                    ar_route::BreakerState::HalfOpen => "half_open",
                },
                "breaker_failures": p.breaker_failures,
                "breaker_threshold": p.breaker_threshold,
                "breaker_reset_secs": p.breaker_reset_secs,
                "retired": p.retired,
                "retired_models": p.retired_models,
                "coolings": p.coolings.iter().map(|c| json!({
                    "model": c.model,
                    "remaining_secs": c.remaining.as_secs(),
                    "failures": c.failures,
                })).collect::<Vec<_>>(),
                "locks": p.locks.iter().map(|c| json!({
                    "model": c.model,
                    "remaining_secs": c.remaining.as_secs(),
                    "failures": c.failures,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    Json(json!({
        "providers": rows,
        "open_breakers": snapshot.iter().filter(|p| p.breaker != ar_route::BreakerState::Closed).count(),
        "cooling": snapshot.iter().map(|p| p.coolings.len() + p.locks.len()).sum::<usize>(),
    }))
    .into_response()
}

/// `GET /ui/api/health` — per-provider credential health and expiry.
///
/// Two facts per provider, and they answer different questions:
///
/// * **health** — whether the credential is present, and whether the store can
///   read it. A provider whose key is missing is a provider that will 401
///   upstream; the panel shows that before the traffic does.
/// * **expiry** — the OAuth session's expiry, when the provider has one. A
///   session that expires in an hour is a routing decision the operator has
///   to make today, not after the outage.
///
/// A provider with no session has no expiry, and says so rather than showing
/// a dash that reads as "expired".
async fn health(State(ctx): State<Arc<Ctx>>) -> Response {
    let (doc, cfg) = match load_cfg(&ctx) {
        Ok(v) => v,
        Err(resp) => return *resp,
    };
    let env_set = env_vars_set(&ctx.env);
    let keys = raw_keys(&doc);

    // The store's own view of each credential. A row that will not decrypt is
    // a *broken* credential, not a missing one, and the two need different
    // advice — so the store is asked, and its refusal is the answer.
    let store_state = ctx.store.as_ref().map(|store| {
        let guard = store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut states: BTreeMap<String, String> = BTreeMap::new();
        for name in keys.keys() {
            states.insert(
                name.clone(),
                match guard.get_text(name) {
                    Ok(Some(_)) => "ok".to_owned(),
                    Ok(None) => "missing".to_owned(),
                    Err(e) => format!("unreadable: {e}"),
                },
            );
        }
        states
    });

    let providers: Vec<Value> = cfg
        .providers
        .iter()
        .map(|row| {
            let refs: Vec<Value> = row
                .key_refs()
                .iter()
                .map(|name| {
                    let var = keys
                        .get(*name)
                        .map(|v| v.trim_start_matches('$').to_owned())
                        .unwrap_or_default();
                    let in_env = !var.is_empty() && env_set.contains(&var);
                    let store = store_state
                        .as_ref()
                        .and_then(|s| s.get(*name))
                        .cloned()
                        .unwrap_or_else(|| "not_in_store".to_owned());
                    json!({
                        "ref": name,
                        "var": var,
                        "in_env": in_env,
                        "store": store,
                    })
                })
                .collect();
            json!({ "id": row.id, "credentials": refs })
        })
        .collect();

    // OAuth sessions: the store's own rows, with their terminal status and
    // whatever expiry the session carries.
    let sessions: Vec<Value> = ctx
        .store
        .as_ref()
        .map(|store| {
            let guard = store
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut out = Vec::new();
            for provider in &cfg.providers {
                match guard.get_session(&provider.id) {
                    Ok(Some(session)) => out.push(json!({
                        "provider": provider.id,
                        "kind": session.kind().as_str(),
                        "terminal_status": session.terminal_status(),
                        "terminal_reason": session.terminal_reason(),
                    })),
                    Ok(None) => {}
                    Err(e) => out.push(json!({
                        "provider": provider.id,
                        "kind": "unknown",
                        "error": e.to_string(),
                    })),
                }
            }
            out
        })
        .unwrap_or_default();

    Json(json!({
        "providers": providers,
        "sessions": sessions,
        "store_open": ctx.store.is_some(),
    }))
    .into_response()
}

/// `GET /ui/api/settings` — the resolved settings, as the proxy booted them.
/// `GET /ui/api/budgets` — what each key has spent against its ceiling, and
/// what each provider's quota window says.
///
/// Two different questions, and they answer different ones:
///
/// * **The budget half** compares the ledger's cumulative spend for a key
///   against the `limits:` row that key carries. The ceiling is
///   **cumulative**, not a daily window — `ar_config::Limit`'s own
///   semantics — so the figure shown is "spent of ceiling", and a key with no
///   row inherits `limits.default`.
/// * **The quota half** is the provider's own allowance: the freshest
///   `quota_snapshots` row the credential store holds, with the percentage
///   left and when the window resets.
///
/// The quota half is empty on a store that has never recorded a snapshot,
/// which is the honest answer: nobody asked the provider, so nobody knows.
async fn budgets(State(ctx): State<Arc<Ctx>>) -> Response {
    let (_, cfg) = match load_cfg(&ctx) {
        Ok(v) => v,
        Err(resp) => return *resp,
    };

    // The spend half: the ledger's CUMULATIVE totals per key.
    //
    // `Ledger::spend(key_id)` is the exact query — a `SUM` over the key's
    // whole history. `report(n)` is NOT: it returns the n most RECENT rows,
    // so summing a truncated slice would show a key with 50 000 requests as
    // having spent only its newest 1 000, and the operator would read a
    // ceiling check against the wrong number.
    let mut spend: BTreeMap<String, (f64, u64)> = BTreeMap::new();
    if let Some(ledger) = ctx.ledger.clone() {
        let guard = ledger
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for key in cfg.limits.keys.keys() {
            if let Ok(s) = guard.spend(key) {
                spend.insert(
                    key.clone(),
                    (s.usd.micros as f64 / 1_000_000.0, s.tokens),
                );
            }
        }
    }

    let rows: Vec<Value> = cfg
        .limits
        .keys
        .iter()
        .map(|(key, limit)| {
            let (usd, tokens) = spend.get(key).copied().unwrap_or((0.0, 0));
            budget_json(key, limit, usd, tokens)
        })
        .collect();
    // The default row governs every key without one of its own, so it is
    // shown on its own line rather than folded into the per-key rows.
    let default_row = budget_json("(default)", &cfg.limits.default, 0.0, 0);

    // The quota half: the freshest snapshot per provider, in the "default"
    // window — the same window the router reads (`WINDOW_KEY` in
    // `ar-server`'s config). A store that recorded a snapshot under a
    // different window key is not shown here: the panel reports the window
    // the router actually consults, and inventing a second reader would be
    // a second answer to "what is the provider's allowance".
    let quotas: Vec<Value> = ctx
        .store
        .as_ref()
        .map(|store| {
            let guard = store
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut out = Vec::new();
            for provider in &cfg.providers {
                let Ok(Some(snapshot)) = guard.latest_quota_snapshot(&provider.id, "default") else {
                    continue;
                };
                out.push(json!({
                    "provider": provider.id,
                    "remaining_pct": snapshot.remaining_percentage,
                    "exhausted": snapshot.exhausted,
                    "next_reset_at": snapshot.next_reset_at,
                    "recorded_at": snapshot.recorded_at,
                    "stale": snapshot.is_stale(now_epoch()),
                }));
            }
            out
        })
        .unwrap_or_default();

    Json(json!({
        "keys": rows,
        "default": default_row,
        "quotas": quotas,
        "open": ctx.ledger.is_some(),
    }))
    .into_response()
}

/// One budget row: what a key has spent against what it may.
fn budget_json(key: &str, limit: &ar_config::Limit, usd: f64, tokens: u64) -> Value {
    let pct = |spent: u64, cap: Option<u64>| {
        cap.filter(|c| *c > 0)
            .map(|c| (spent as f64 / c as f64 * 100.0).min(100.0))
    };
    json!({
        "key": key,
        "usd": usd,
        "tokens": tokens,
        "usd_cap": limit.usd_micros,
        "token_cap": limit.tokens,
        "rpm": limit.rpm,
        "model": limit.model,
        "usd_pct": pct((usd * 1_000_000.0) as u64, limit.usd_micros),
        "token_pct": pct(tokens, limit.tokens),
    })
}

/// Unix seconds. The store's snapshots and the ledger's rows are both in
/// seconds, and the panel's "as of" is a clock reading beside them.
fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `GET /ui/api/settings` — the settings, as the FILE now reads them.
///
/// The yaml-backed values come from the **live file**, not from the boot
/// snapshot: the panel edits that file, and a pane that showed the values
/// the proxy booted with while `pending_restart` said "restart needed" would
/// show the operator the wrong thing after their own edit.
///
/// The env-backed values are the opposite — they are read at boot and live
/// in the environment — so they are marked as such in the payload, and the
/// pane names the variable that carries each one.
async fn settings(State(ctx): State<Arc<Ctx>>) -> Response {
    let (_, cfg) = match load_cfg(&ctx) {
        Ok(v) => v,
        Err(resp) => return *resp,
    };
    let s = &ctx.settings;
    Json(json!({
        // Live, from the file the panel edits.
        "host": cfg.server.host,
        "port": cfg.server.port,
        "limits": {
            "default": limit_json(&cfg.limits.default),
            "keys": cfg.limits.keys.iter().map(|(k, v)| (k.clone(), limit_json(v))).collect::<BTreeMap<_, _>>(),
        },
        // Read at boot, from the environment.
        "request_timeout_secs": s.request_timeout_secs,
        "timeouts": s.timeouts,
        "model_discovery": s.model_discovery,
        "auth_mode": s.auth_mode,
        "pending_restart": std::fs::read_to_string(&ctx.config).unwrap_or_default() != ctx.boot_text,
    }))
    .into_response()
}

/// One ceiling row, in the shape the page edits it in.
fn limit_json(limit: &ar_config::Limit) -> Value {
    json!({
        "rpm": limit.rpm,
        "usd_micros": limit.usd_micros,
        "tokens": limit.tokens,
        "refuse_unpriced": limit.refuse_unpriced,
        "model": limit.model,
    })
}

/// `POST /ui/api/settings` — writes the yaml-backed settings.
///
/// **Only the yaml-backed ones are writable.** `server:` and `limits:` live in
/// the config file; the per-model deadlines, the discovery overlay and the
/// auth mode live in the **environment** (`AR_STREAM_TIMEOUT_SECS`,
/// `AR_MODEL_DISCOVERY`, `AR_AUTH_MODE`) because that is where the systemd
/// unit puts them and where they have always been read from. A settings pane
/// that wrote those to the yaml would be editing a file nothing reads — the
/// worst kind of editor, because it reports success and changes nothing.
///
/// The env-backed values are therefore shown, with the variable that carries
/// them named, and they are **not** editable here. An operator changes them
/// in the environment, which is the same place `aroute doctor` reads them
/// from.
async fn save_settings(
    State(ctx): State<Arc<Ctx>>,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    if let Some(refusal) = refuse_if_broken(&ctx) {
        return refusal;
    }
    let body = match body {
        Ok(Json(v)) => v,
        Err(r) => return body_error(r),
    };
    let (doc, mut cfg) = match load_cfg(&ctx) {
        Ok(v) => v,
        Err(resp) => return *resp,
    };
    let mut next = doc.clone();

    // `server:` — the listen address. Two fields, and a bad value is refused
    // naming the field rather than written and left to fail at boot.
    if let Some(server) = body.get("server") {
        let host = server
            .get("host")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or(cfg.server.host.as_str());
        if host.is_empty() {
            return bad_request(
                "server.host is empty".into(),
                "the listen address must be a host or an IP, e.g. 127.0.0.1",
            );
        }
        let port = server
            .get("port")
            .and_then(Value::as_u64)
            .unwrap_or(cfg.server.port as u64);
        if port == 0 || port > u16::MAX as u64 {
            return bad_request(
                format!("server.port {port} is not a port"),
                "the port must be between 1 and 65535",
            );
        }
        let section = format!(
            "server:\n  host: {}\n  port: {}\n",
            serde_json::Value::from(host),
            port
        );
        next = commands::splice_section(&next, "server", &section);
        cfg.server.host = host.to_owned();
        cfg.server.port = port as u16;
    }

    // `limits:` — the default row only. The per-key rows have their own
    // endpoint (`/ui/api/limits`), because a per-key row is edited *with its
    // key*, and a settings form that carried the whole map would be a second
    // editor for the same data.
    if let Some(default) = body.get("limits").and_then(|l| l.get("default")) {
        let row = match limit_from(default, &cfg.limits.default) {
            Ok(row) => row,
            Err((message, help)) => return bad_request(message, &help),
        };
        cfg.limits.default = row;
        next = commands::splice_section(
            &next,
            "limits",
            &crate::import::limits::render(&cfg.limits),
        );
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
    // The write is only reported as done once the file says so. A handler
    // that answers `ok` and hopes the write landed is how an operator ends up
    // trusting a config that was never changed.
    let reread = std::fs::read_to_string(&ctx.config).unwrap_or_default();
    if reread != next {
        return bad_request(
            "the file was written but does not read back as written".into(),
            "check for another process rewriting the config; nothing else was changed",
        );
    }
    Json(json!({ "ok": true, "pending_restart": true })).into_response()
}

/// One ceiling row from a JSON body.
///
/// The arms are the same four `ar_config::Limit` carries, and the scope is
/// checked against the config's own combos so a typo cannot arm a ceiling
/// that never applies.
fn limit_from(
    body: &Value,
    current: &ar_config::Limit,
) -> Result<ar_config::Limit, (String, String)> {
    let mut row = current.clone();
    if let Some(v) = body.get("rpm") {
        row.rpm = match v {
            Value::Null => None,
            Value::Number(n) => Some(n.as_u64().ok_or((
                "rpm must be a whole number".to_owned(),
                "the rpm arm is a requests-per-minute ceiling".to_owned(),
            ))? as u32),
            _ => {
                return Err((
                    "rpm must be a number or null".to_owned(),
                    "the rpm arm is a requests-per-minute ceiling".to_owned(),
                ));
            }
        };
    }
    if let Some(v) = body.get("usd_micros") {
        row.usd_micros = match v {
            Value::Null => None,
            Value::Number(n) => Some(n.as_u64().ok_or((
                "usd_micros must be a whole number".to_owned(),
                "the spend arm is in micro-dollars; 1000000 is one dollar".to_owned(),
            ))?),
            _ => {
                return Err((
                    "usd_micros must be a number or null".to_owned(),
                    "the spend arm is in micro-dollars; 1000000 is one dollar".to_owned(),
                ));
            }
        };
    }
    if let Some(v) = body.get("tokens") {
        row.tokens = match v {
            Value::Null => None,
            Value::Number(n) => Some(n.as_u64().ok_or((
                "tokens must be a whole number".to_owned(),
                "the token arm is a cumulative ceiling".to_owned(),
            ))?),
            _ => {
                return Err((
                    "tokens must be a number or null".to_owned(),
                    "the token arm is a cumulative ceiling".to_owned(),
                ));
            }
        };
    }
    if let Some(v) = body.get("refuse_unpriced") {
        row.refuse_unpriced = match v {
            Value::Bool(b) => *b,
            Value::Null => false,
            _ => {
                return Err((
                    "refuse_unpriced must be a boolean".to_owned(),
                    "true fails closed on a model with no pricing row".to_owned(),
                ));
            }
        };
    }
    Ok(row)
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
    if let Some(refusal) = refuse_if_broken(&ctx) {
        return refusal;
    }
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
    if let Some(refusal) = refuse_if_broken(&ctx) {
        return refusal;
    }
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

/// The panel's default token lifetime, in days.
///
/// Shorter than the CLI's 30 on purpose. The CLI's default is for a token an
/// operator hands to a harness they configured; the panel's default is for a
/// token minted from a browser, and a shorter life is the one a leaked
/// credential cannot outlive. The panel can also **revoke** what it mints
/// (`aroute keys revoke`), but a revocation is only useful for a token that
/// is still alive when someone remembers to use it.
const PANEL_MINT_TTL_DAYS: u32 = 7;

/// The longest lifetime the panel will mint.
///
/// A year is generous for a credential; a decade is a credential that has
/// stopped being one.
const MAX_MINT_TTL_DAYS: u32 = 365;

/// Mints a client token through the same gate the proxy verifies against.
async fn mint_key(
    State(ctx): State<Arc<Ctx>>,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    if let Some(refusal) = refuse_if_broken(&ctx) {
        return refusal;
    }
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
    // The token's lifetime, validated BEFORE the store is consulted: a bad
    // input is a bad input whether or not there is a store, and validating
    // the body first means the error names the field rather than the
    // environment.
    //
    // The panel's default is **7 days**, not the CLI's 30: a panel hands the
    // token to whoever reaches it, and a shorter life is the one a leaked
    // credential cannot outlive. `0` mints a token that never expires, and the
    // operator has to ask for that explicitly.
    let ttl_days = match body.get("ttl_days") {
        None | Some(Value::Null) => PANEL_MINT_TTL_DAYS,
        Some(Value::Number(n)) => match n.as_u64() {
            Some(v) if v <= u64::from(MAX_MINT_TTL_DAYS) => {
                u32::try_from(v).unwrap_or(MAX_MINT_TTL_DAYS)
            }
            Some(_) => {
                return bad_request(
                    format!("ttl_days is too large; the maximum is {MAX_MINT_TTL_DAYS}"),
                    "0 mints a token that never expires",
                );
            }
            None => {
                return bad_request(
                    "ttl_days must be a whole number of days".into(),
                    "0 mints a token that never expires",
                );
            }
        },
        Some(_) => {
            return bad_request(
                "ttl_days must be a number".into(),
                "0 mints a token that never expires",
            );
        }
    };
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
    match commands::mint_from_with_ttl(&master, &key_id, ttl_days) {
        Ok(issued) => Json(json!({
            "ok": true,
            "key_id": key_id,
            "access": issued.access,
            "refresh": issued.refresh,
            "expires_at": issued.expires_at,
            "ttl_days": ttl_days,
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
    if let Some(refusal) = refuse_if_broken(&ctx) {
        return refusal;
    }
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
    if let Some(refusal) = refuse_if_broken(&ctx) {
        return refusal;
    }
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
    if let Some(refusal) = refuse_if_broken(&ctx) {
        return refusal;
    }
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
        ui_with_gate(tag, ledger, auth::GateState::Unarmed)
    }

    /// The same, with an explicit gate state.
    fn ui_with_gate(
        tag: &str,
        ledger: Option<ar_tokens::Ledger>,
        gate: auth::GateState,
    ) -> (Router, PathBuf) {
        let dir = std::env::temp_dir().join(format!("ar-webui-http-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let config = dir.join("config.yaml");
        std::fs::write(&config, CONFIG).expect("config");
        let ledger = ledger.map(|l| Arc::new(Mutex::new(l)));
        let mut ctx = Ctx::new(config.clone(), ledger, None);
        ctx.set_gate(gate);
        let app = mount(Router::new(), ctx);
        (app, config)
    }

    /// A gate state that is armed, plus a token it admits.
    fn armed_gate() -> (auth::GateState, String) {
        armed_gate_with_list(None)
    }

    /// The same, with a revocation list attached.
    ///
    /// The list is the **same handle** the server's `/v1` gate would hold, so
    /// a token revoked through it is dead on both surfaces — which is the
    /// "both or neither" property the whole design rests on.
    fn armed_gate_with_list(
        revocations: Option<std::sync::Arc<ar_keys::Revocations>>,
    ) -> (auth::GateState, String) {
        let master = ar_keys::MasterKey::new(
            ar_keys::Secret::from_slice(&[3u8; 32]).expect("32 bytes"),
            ar_keys::KeyMeta::generate(),
        )
        .expect("master");
        let tokens = ar_keys::Tokens::new(&master);
        let token = tokens
            .issue(ar_keys::Issue {
                key_id: "panel",
                scopes: ar_keys::ScopeSet::of([ar_keys::Scope::ExecuteCompletions]),
                device_id: None,
                ttl: None,
            })
            .expect("token")
            .access;
        (auth::GateState::Armed(tokens, revocations), token)
    }

    async fn call(app: &Router, method: Method, uri: &str, body: &str) -> (StatusCode, Value) {
        call_with_token(app, method, uri, body, None).await
    }

    /// The same request with an optional bearer token.
    async fn call_with_token(
        app: &Router,
        method: Method,
        uri: &str,
        body: &str,
        token: Option<&str>,
    ) -> (StatusCode, Value) {
        call_full(app, method, uri, body, token, None).await
    }

    /// The same request with an optional `Origin`, for the public-URL case.
    async fn call_full(
        app: &Router,
        method: Method,
        uri: &str,
        body: &str,
        token: Option<&str>,
        origin: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut req = Request::builder()
            .header("host", "127.0.0.1:20128")
            .method(method)
            .uri(uri)
            .header("content-type", "application/json");
        if let Some(token) = token {
            req = req.header("authorization", format!("Bearer {token}"));
        }
        if let Some(origin) = origin {
            req = req.header("origin", origin);
        }
        let resp = app
            .clone()
            .oneshot(req.body(Body::from(body.to_owned())).expect("request"))
            .await
            .expect("router answers");
        let status = resp.status();
        let bytes = resp.into_body().collect().await.expect("body").to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[tokio::test]
    async fn the_trailing_slash_form_should_serve_the_page_not_redirect() {
        // A redirect would drop the `?token=` the panel's first-paint flow
        // arrives on, and would put the token in this server's access log.
        let (app, _) = ui("slash", None);
        let req = Request::builder()
            .header("host", "127.0.0.1:20128")
            .uri("/ui/?token=secret")
            .body(Body::empty())
            .expect("request");
        let resp = app.clone().oneshot(req).await.expect("router answers");
        assert_eq!(resp.status(), StatusCode::OK, "the page is served, not redirected");
        assert_eq!(
            resp.headers().get("location"),
            None,
            "no redirect: the query must survive to the page"
        );
    }

    #[tokio::test]
    async fn the_page_should_carry_the_referrer_and_framing_headers() {
        // The page can be reached with a token on the URL, so the Referer of
        // the next navigation must not carry it.
        let (app, _) = ui("headers", None);
        let req = Request::builder()
            .header("host", "127.0.0.1:20128")
            .uri("/ui")
            .body(Body::empty())
            .expect("request");
        let resp = app.clone().oneshot(req).await.expect("router answers");
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get("referrer-policy").and_then(|v| v.to_str().ok()),
            Some("no-referrer")
        );
        assert_eq!(
            resp.headers().get("x-frame-options").and_then(|v| v.to_str().ok()),
            Some("DENY")
        );
    }

    #[tokio::test]
    async fn an_armed_panel_should_refuse_an_api_call_without_a_token() {
        let (gate, _) = armed_gate();
        let (app, _) = ui_with_gate("armed", None, gate);
        let (status, body) = call(&app, Method::GET, "/ui/api/state", "").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["code"], "credential_rejected");
    }

    #[tokio::test]
    async fn an_armed_panel_should_admit_an_api_call_with_a_valid_token() {
        let (gate, token) = armed_gate();
        let (app, _) = ui_with_gate("armed-ok", None, gate);
        let (status, _) = call_with_token(
            &app,
            Method::GET,
            "/ui/api/state",
            "",
            Some(&token),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn an_armed_panel_should_serve_the_page_without_a_token() {
        // The page carries no data and no secret; a login wall in front of it
        // would make the panel's own error handling unreadable.
        let (gate, _) = armed_gate();
        let (app, _) = ui_with_gate("armed-page", None, gate);
        let req = Request::builder()
            .header("host", "127.0.0.1:20128")
            .uri("/ui")
            .body(Body::empty())
            .expect("request");
        let resp = app.clone().oneshot(req).await.expect("router answers");
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn a_broken_gate_should_refuse_a_write_and_admit_a_read() {
        let (app, config) = ui_with_gate(
            "broken",
            None,
            auth::GateState::Broken("authentication tag mismatch".to_owned()),
        );
        let (read_status, _) = call(&app, Method::GET, "/ui/api/state", "").await;
        assert_eq!(read_status, StatusCode::OK, "a read still degrades");
        let (write_status, body) = call(
            &app,
            Method::POST,
            "/ui/api/limits",
            r#"{"key":"ci","rpm":5}"#,
        )
        .await;
        assert_eq!(write_status, StatusCode::CONFLICT);
        assert!(
            body["error"].as_str().unwrap_or_default().contains("cannot be read"),
            "the refusal names the cause: {body}"
        );
        assert_eq!(
            std::fs::read_to_string(&config).expect("config"),
            CONFIG,
            "a refused write leaves the file alone"
        );
    }

    #[tokio::test]
    async fn an_out_of_range_lifetime_should_be_refused() {
        // The mint's lifetime is bounded. A year is generous for a
        // credential; a decade is a credential that has stopped being one.
        let (app, _) = ui("mint-ttl-range", None);
        let (status, body) = call(
            &app,
            Method::POST,
            "/ui/api/keys/mint",
            r#"{"key_id":"forever","ttl_days":36500}"#,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(
            body["error"].as_str().unwrap_or_default().contains("too large"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn a_malformed_lifetime_should_be_refused() {
        let (app, _) = ui("mint-ttl-bad", None);
        let (status, body) = call(
            &app,
            Method::POST,
            "/ui/api/keys/mint",
            r#"{"key_id":"x","ttl_days":"forever"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    }

    #[tokio::test]
    async fn an_unarmed_panel_should_refuse_a_cross_site_write() {
        // The loopback install's ONLY CSRF defence. The panel mints spendable
        // tokens, and on a box with no gate the socket is the trust boundary —
        // so a drive-by page that POSTs /ui/api/keys/mint from the operator's
        // own browser is exactly the attack this refuses.
        //
        // The refusal is a 403 (cross-site), not a 401 (needs a token): the
        // panel has no token to ask for on an unarmed install, and the page
        // has to be able to tell the two apart.
        let (app, _) = ui("csrf", None);
        let (status, body) = call_full(
            &app,
            Method::POST,
            "/ui/api/keys/mint",
            r#"{"key_id":"attacker"}"#,
            None,
            Some("https://evil.example.com"),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["error"]["code"], "cross_site_request");
    }

    #[tokio::test]
    async fn an_unarmed_panel_should_admit_a_loopback_origin_write() {
        // The same request from the operator's own browser: a loopback
        // `Origin` is the operator, not an attacker.
        let (app, _) = ui("csrf-loopback", None);
        let (status, _) = call_full(
            &app,
            Method::POST,
            "/ui/api/keys/mint",
            r#"{"key_id":"operator"}"#,
            None,
            Some("http://127.0.0.1:20128"),
        )
        .await;
        // The mint itself refuses (no store in the fixture), but the guard let
        // the request through — a 409 from the handler, not a 403 from the
        // guard.
        assert_ne!(status, StatusCode::FORBIDDEN, "the guard admitted it");
    }

    #[tokio::test]
    async fn an_unarmed_panel_should_admit_a_request_with_no_origin() {
        // A non-browser client sends no `Origin`, and the loopback bind is its
        // trust boundary.
        let (app, _) = ui("csrf-no-origin", None);
        let (status, _) = call(&app, Method::GET, "/ui/api/state", "").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn a_public_origin_with_a_valid_token_should_be_admitted() {
        // The F2 finding, measured: a browser on the public URL attaches
        // `Origin` to every same-origin POST and to a CSS-driven font fetch.
        // The old guard refused those on the Origin header while admitting
        // every curl, so the panel was half-dead on its own public URL and
        // fully open to a script. The credential is now the control, so a
        // valid token is admitted whatever the Origin says.
        let (gate, token) = armed_gate();
        let (app, _) = ui_with_gate("public-origin", None, gate);
        let (status, _) = call_full(
            &app,
            Method::GET,
            "/ui/api/state",
            "",
            Some(&token),
            Some("https://aroute.example.com"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn a_revoked_token_should_be_refused_by_the_panel() {
        // The "both or neither" property, from the panel's side.
        //
        // The panel and `/v1` share ONE revocation handle. A token revoked
        // through `aroute keys revoke` writes a `jti` into the table in the
        // credential store, and both gates consult that table. If only one of
        // them did, a token revoked through the panel would still work on
        // `/v1` — and two surfaces that disagree about the same token is
        // worse than either being consistently incomplete.
        let dir = std::env::temp_dir().join(format!("ar-webui-revoke-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let store_path = dir.join("credentials.db");
        let list = Arc::new(
            ar_keys::Revocations::open(&store_path, 64).expect("the revocation list opens"),
        );

        let (gate, token) = armed_gate_with_list(Some(Arc::clone(&list)));
        let (app, _) = ui_with_gate("revoked-token", None, gate);

        // Before the revoke: the token opens the panel.
        let (status, _) = call_full(
            &app,
            Method::GET,
            "/ui/api/state",
            "",
            Some(&token),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "precondition: the token opens the panel");

        // Revoke it through the same path `aroute keys revoke` uses: the
        // token's `jti`, with the token's own `exp`.
        let master = ar_keys::MasterKey::new(
            ar_keys::Secret::from_slice(&[3u8; 32]).expect("32 bytes"),
            ar_keys::KeyMeta::generate(),
        )
        .expect("master");
        let tokens = ar_keys::Tokens::new(&master);
        let verified = tokens.introspect(&token).expect("the token verifies");
        list.revoke(&verified.session_id, verified.expires_at)
            .expect("the revoke lands");

        // After the revoke: it does not.
        let (status, body) = call_full(
            &app,
            Method::GET,
            "/ui/api/state",
            "",
            Some(&token),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
        assert_eq!(body["error"]["code"], "credential_rejected");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_public_origin_without_a_token_should_be_refused() {
        // The credential, not the Origin, is what decides.
        let (gate, _) = armed_gate();
        let (app, _) = ui_with_gate("public-origin-no-token", None, gate);
        let (status, _) = call_full(
            &app,
            Method::GET,
            "/ui/api/state",
            "",
            None,
            Some("https://aroute.example.com"),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_rebinding_host_should_be_refused_whatever_the_token() {
        // The half of the rule that IS a rebinding defence: the `Host`
        // header, which a drive-by page cannot choose.
        let (gate, token) = armed_gate();
        let (app, _) = ui_with_gate("rebind", None, gate);
        let req = Request::builder()
            .header("host", "evil.example.com")
            .header("authorization", format!("Bearer {token}"))
            .uri("/ui/api/state")
            .body(Body::empty())
            .expect("request");
        let resp = app.clone().oneshot(req).await.expect("router answers");
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn the_resilience_view_should_report_every_provider() {
        let (app, _) = ui("resilience", None);
        let (status, body) = call(&app, Method::GET, "/ui/api/resilience", "").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["providers"].is_array(), "{body}");
    }

    #[tokio::test]
    async fn the_settings_view_should_report_the_yaml_backed_settings() {
        let (app, _) = ui("settings", None);
        let (status, body) = call(&app, Method::GET, "/ui/api/settings", "").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["limits"].is_object(), "{body}");
    }

    #[tokio::test]
    async fn the_settings_view_should_read_the_file_and_not_the_boot_snapshot() {
        // The panel EDITS the yaml, so the settings view must show the yaml
        // as it reads NOW. A view that showed the boot snapshot would show
        // the operator the old value after their own edit, while
        // `pending_restart` said "restart needed" - two truths on one screen.
        let (app, config) = ui("settings-live", None);
        // The file is edited underneath the running panel, the way an editor
        // or a `git pull` would: a `server:` block appears with a host the
        // panel never booted with.
        let text = std::fs::read_to_string(&config).expect("read");
        let edited = format!("{text}server:\n  host: 0.0.0.0\n  port: 20128\n");
        std::fs::write(&config, &edited).expect("write");

        let (status, body) = call(&app, Method::GET, "/ui/api/settings", "").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["host"], "0.0.0.0",
            "the settings view must show the file, not the boot snapshot: {body}"
        );
    }

    #[tokio::test]
    async fn a_settings_write_should_refuse_a_port_that_is_not_a_port() {
        let (app, config) = ui("settings-bad-port", None);
        let (status, body) = call(
            &app,
            Method::POST,
            "/ui/api/settings",
            r#"{"server":{"host":"127.0.0.1","port":0}}"#,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body["error"].as_str().unwrap_or_default().contains("port"),
            "the refusal names the field: {body}"
        );
        assert_eq!(
            std::fs::read_to_string(&config).expect("config"),
            CONFIG,
            "a refused write leaves the file alone"
        );
    }

    #[tokio::test]
    async fn the_log_should_answer_failures_inside_the_date_range() {
        let ledger = ar_tokens::Ledger::open_in_memory().expect("ledger");
        ledger
            .record_failure("ci", "-", "c", 502, "every provider is down", 1_000_000)
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
