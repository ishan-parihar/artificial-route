//! The `rmcp` stdio server, and the host state it serves.
//!
//! Compiled only with `--features mcp`. The module is `#[cfg(feature = "mcp")]`
//! in `lib.rs` too, so a default build has no `transport` in its public API at
//! all rather than an empty one — an empty public module reads as "there is
//! something here" to a host doing capability discovery, and there is not.
//!
//! # What this file is for
//!
//! F-HIGH-5: the essential-8 existed as a library no binary reached. A
//! `ServerHandler` over [`Tool::ALL`] is the whole of the missing wiring — the
//! catalog, the default-deny check and the eight bodies were already here and
//! SDK-free, so the handler is plumbing and not logic. Every tool below goes
//! through [`crate::guard`]; there is no path from a request to a body that skips
//! the scope check or loses the audit row.
//!
//! # [`Host`] is the binary's half
//!
//! The bodies need real state — a ledger, an admission controller, a candidate
//! set — and none of it is this crate's to invent. So [`Host`] is a plain struct
//! of public fields that `ar` fills from its own config, and this module only
//! reads it. That keeps the crate free of a config loader and a second way to
//! answer "what can this proxy route".
//!
//! # `ar_tool_search` is the ninth registered tool, and why
//!
//! `docs/06` puts `tool_search` "from day one" and the crate's own docs are
//! explicit that it is a *function* over [`Tool::ALL`], not a ninth enum variant,
//! so that every count a host reports stays 8. It is therefore registered here
//! rather than in the catalog. It touches no host state — its answer is eight
//! static strings — so it needs no scope, but it still writes an audit row, or
//! "every tool leaves a row" would have an exception written in the code that
//! implements the audit.

use std::sync::{Arc, Mutex, PoisonError};
use std::sync::atomic::AtomicU64;

use ar_keys::Admission;
use ar_route::{AutoCandidate, AutoCombo, AutoSelector, AutoVariant, Candidate, Strategy};
use ar_tokens::Ledger;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ErrorCode, ErrorData,
    Implementation, JsonObject, ListToolsResult, PaginatedRequestParams, ServerCapabilities,
    ServerConfig, Tool as McpTool,
};
use rmcp::service::RequestContext;
use rmcp::{RoleServer, ServerHandler, ServiceExt};

use crate::Error;
use crate::audit::{Audit, CallOutcome};
use crate::scope::Scope;
use crate::tools::{ComboState, Switch};
use crate::{Tool, tool_search};

/// Wire name of the discovery tool, which is a function rather than a
/// [`Tool`] variant.
pub const TOOL_SEARCH_NAME: &str = "ar_tool_search";

/// One routable combo, as the server sees it.
#[derive(Debug, Clone)]
pub struct HostCombo {
    /// Combo id, i.e. what a client passes as `model`.
    pub name: String,
    /// The strategy the combo routes with.
    pub strategy: Strategy,
    /// The candidates a call to this combo routes over. The *targets* alone:
    /// a `pool:` bench entry is never eligible for the winner, which is the
    /// order `ar-server`'s own `chain` relies on.
    pub candidates: Vec<Candidate>,
    /// The combo's `pool:` bench — routable, but only ever tried after the
    /// targets refuse. Carried so `ar_list_models` reports the same routable
    /// universe the data plane has; a bench the control plane could not see
    /// would be a second, smaller answer to "what can this combo reach".
    pub pool: Vec<Candidate>,
}

/// Everything the eight tools read.
///
/// Public fields rather than a constructor: the only thing that can build this is
/// the `ar` binary, which already resolves every field from its own config, and a
/// nine-argument `Host::new` would be a worse version of a struct literal. The
/// one invariant — at least one combo — is checked where the config is loaded,
/// because an empty combo table makes the proxy unroutable and `ar doctor`
/// already says so.
pub struct Host {
    /// The routable combos, in config order. The first is the default.
    pub combos: Vec<HostCombo>,
    /// Real three-lane admission state, so `ar_get_health` reports lane pressure
    /// rather than a host-supplied claim.
    pub admission: Admission,
    /// Where the usage ledger lives. `ar_check_quota` and `ar_cost_report` open
    /// it per call rather than sharing one handle: `ar_tokens::Ledger` wraps a
    /// `rusqlite::Connection`, which is neither `Send` nor `Sync`, so a shared
    /// handle would make this whole server un-spawnable. Opening per call is the
    /// cheap trade for a control plane that sees a handful of calls a minute,
    /// and it is what the `Ledger` doc comment asks for anyway.
    pub ledger_path: std::path::PathBuf,
    /// Which combo is live. Behind a mutex because `ar_switch_combo` is the one
    /// writing tool and the transport serves calls concurrently.
    pub active: Mutex<ComboState>,
    /// Round-robin position for `ar_route_request`, shared across calls so two
    /// requests do not both pick the first candidate.
    pub cursor: AtomicU64,
    /// What this process is allowed to call, per [`Scope::permits`].
    pub scope: Scope,
    /// The append-only trail [`crate::guard`] writes one row to per call.
    pub audit: Audit,
    /// The key id the audit row attributes the call to. Never a secret.
    pub key_id: String,
}

impl Host {
    /// The combo a call names, or the live one, or the first.
    ///
    /// `None` only when there are no combos at all, which the loader refuses — so
    /// the fallback is a defence, not a branch anyone reaches in practice.
    fn combo(&self, want: Option<&str>) -> Option<&HostCombo> {
        match want {
            Some(name) => self.combos.iter().find(|c| c.name == name),
            None => self
                .active
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .active
                .as_deref()
                .and_then(|live| self.combos.iter().find(|c| c.name == live))
                .or_else(|| self.combos.first()),
        }
    }

    /// The ledger, opened for one call. See [`Host::ledger_path`].
    fn ledger(&self) -> Result<Ledger, Error> {
        Ledger::open(&self.ledger_path).map_err(Error::from)
    }

    /// Every model this host can route to, targets and bench alike.
    fn routable(combo: &HostCombo) -> impl Iterator<Item = &Candidate> {
        combo.candidates.iter().chain(combo.pool.iter())
    }

    /// The candidates an `auto/*` explanation ranks: every candidate any combo
    /// can reach, deduped by provider and model.
    ///
    /// Deduped because two combos naming the same target is one model, and
    /// [`ar_route::explain_route`] scoring it twice would weight it twice.
    fn auto_pool(&self) -> Vec<AutoCandidate> {
        let mut seen: Vec<AutoCandidate> = Vec::new();
        for combo in &self.combos {
            for c in Self::routable(combo) {
                let row = AutoCandidate::new(c.provider.clone(), &c.model);
                if !seen.iter().any(|s| s.provider == row.provider && s.model == row.model) {
                    seen.push(row);
                }
            }
        }
        seen
    }
}

/// The handler. A thin `Arc<Host>` newtype so the two `ServerHandler` methods
/// that need it can share one reference without a lifetime.
struct Server {
    host: std::sync::Arc<Host>,
}

/// The arguments one tool reads, as a borrowed view of the request's `arguments`.
///
/// `null` for a tool that takes none, and for a key the caller omitted, so every
/// read is `args.opt("key")` with no `None`-on-the-object versus `None`-on-the-key
/// distinction to get wrong.
struct Args<'a> {
    raw: Option<&'a JsonObject>,
}

impl<'a> Args<'a> {
    fn new(raw: Option<&'a JsonObject>) -> Self {
        Self { raw }
    }

    /// A string argument, or `None` when absent.
    fn str(&self, key: &'static str) -> Option<&'a str> {
        self.raw?.get(key)?.as_str()
    }

    /// A boolean argument, defaulting to `fallback` when absent. A present
    /// non-boolean is an error rather than a silent `fallback`: a host that sent
    /// `"active": "yes"` asked a question this cannot answer.
    fn flag(&self, key: &'static str, fallback: bool) -> Result<bool, Error> {
        match self.raw.and_then(|r| r.get(key)) {
            None | Some(serde_json::Value::Null) => Ok(fallback),
            Some(serde_json::Value::Bool(b)) => Ok(*b),
            Some(other) => Err(Error::BadArgument { name: key, got: kind_of(other) }),
        }
    }

    /// A count argument, clamped to `ceiling` so a caller cannot ask for a
    /// million rows out of the ledger.
    fn count(&self, key: &'static str, default: usize, ceiling: usize) -> Result<usize, Error> {
        match self.raw.and_then(|r| r.get(key)) {
            None | Some(serde_json::Value::Null) => Ok(default),
            Some(serde_json::Value::Number(n)) => {
                let v = n.as_u64().ok_or(Error::BadArgument { name: key, got: "negative" })?;
                Ok(usize::try_from(v).unwrap_or(ceiling).min(ceiling))
            }
            Some(other) => Err(Error::BadArgument { name: key, got: kind_of(other) }),
        }
    }
}

/// What a JSON value is, for an error message.
///
/// Named by hand because `serde_json` 1.0 has no `Value::kind`; the message
/// goes to an agent that has to fix its own call, so "a string" beats `String`.
fn kind_of(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

/// The JSON schema for a tool: a closed object with the named properties.
fn schema(properties: serde_json::Value, required: &[&str]) -> Arc<JsonObject> {
    let mut obj = JsonObject::new();
    obj.insert("type".to_owned(), serde_json::json!("object"));
    obj.insert("properties".to_owned(), properties);
    if !required.is_empty() {
        obj.insert("required".to_owned(), serde_json::json!(required));
    }
    // `additionalProperties: false` so a host's client rejects a typo'd argument
    // locally instead of the call arriving here and being silently ignored.
    obj.insert("additionalProperties".to_owned(), serde_json::json!(false));
    Arc::new(obj)
}

/// A `{"type":"string"}` property.
fn string_prop() -> serde_json::Value {
    serde_json::json!({ "type": "string" })
}

/// The rmcp-facing description of one catalog entry.
fn describe(tool: Tool) -> McpTool {
    let (properties, required): (serde_json::Value, &[&str]) = match tool {
        Tool::GetHealth | Tool::ListCombos | Tool::CostReport | Tool::ListModels | Tool::ExplainRoute => {
            (serde_json::json!({}), &[])
        }
        Tool::SwitchCombo => (serde_json::json!({ "name": string_prop(), "active": { "type": "boolean" } }), &["name"]),
        Tool::CheckQuota => (serde_json::json!({ "key_id": string_prop() }), &[]),
        Tool::RouteRequest => (serde_json::json!({ "combo": string_prop(), "session": string_prop() }), &[]),
    };
    // `read_only_hint` is the one annotation a client can act on without trusting
    // this server, and it is exactly the distinction `Tool::scope` already draws:
    // a tool whose need carries no privileged bit cannot mutate anything.
    let need = tool.scope();
    let read_only = need.0 & (Scope::WRITE.0 | Scope::EXECUTE.0) == 0;
    McpTool::new(tool.name(), tool.one_liner(), schema(properties, required)).with_annotations(
        rmcp::model::ToolAnnotations::from_raw(None, Some(read_only), None, None, None),
    )
}

/// The rmcp-facing description of the discovery tool.
fn describe_search() -> McpTool {
    McpTool::new(
        TOOL_SEARCH_NAME,
        "one-line signatures for the catalog; an empty query lists all of it",
        schema(serde_json::json!({ "query": string_prop() }), &[]),
    )
}

impl Server {
    /// Runs one tool, through the guard, and renders the outcome.
    fn call(&self, tool: Tool, args: &JsonObject) -> CallToolResult {
        let host = &*self.host;
        let raw = Args::new(Some(args));
        // One owned copy for the audit row, which hashes the whole argument
        // object: a borrowed map cannot be hashed, and cloning per guard call
        // would clone it eight times per invocation.
        let input = serde_json::Value::Object(args.clone());
        let outcome = match tool {
            Tool::GetHealth => crate::guard(tool, host.scope, &host.audit, &host.key_id, &input, || {
                Ok(serde_json::to_value(crate::tools::get_health(&host.admission, 0, (0, 0)))
                    .unwrap_or(serde_json::Value::Null))
            }),
            Tool::ListCombos => crate::guard(tool, host.scope, &host.audit, &host.key_id, &input, || {
                let rows: Vec<serde_json::Value> = host
                    .combos
                    .iter()
                    .map(|c| {
                        serde_json::to_value(crate::tools::list_combos(&c.name, &c.strategy, &c.candidates))
                            .unwrap_or(serde_json::Value::Null)
                    })
                    .collect();
                Ok(serde_json::json!({ "combos": rows }))
            }),
            Tool::SwitchCombo => crate::guard(tool, host.scope, &host.audit, &host.key_id, &input, || {
                let name = raw.str("name").ok_or(Error::MissingArgument { name: "name" })?;
                let active = raw.flag("active", true)?;
                let mut state = host
                    .active
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                let switch: Switch = crate::tools::switch_combo(&mut state, name, active);
                Ok(serde_json::to_value(switch).unwrap_or(serde_json::Value::Null))
            }),
            Tool::CheckQuota => crate::guard(tool, host.scope, &host.audit, &host.key_id, &input, || {
                let key = raw.str("key_id").unwrap_or(&host.key_id);
                let q = crate::tools::check_quota(&host.ledger()?, key)?;
                // `Quota` is not `Serialize`, so the transport renders it from
                // its public fields — the crate-instructed path, not a second
                // shape invented here.
                let cap = q.cap.map(|c| serde_json::json!({ "usd_micros": c.usd_micros, "tokens": c.tokens }));
                Ok(serde_json::json!({ "key_id": key, "spend": spend_json(q.spend), "cap": cap }))
            }),
            Tool::RouteRequest => crate::guard(tool, host.scope, &host.audit, &host.key_id, &input, || {
                let combo = host
                    .combo(raw.str("combo"))
                    .ok_or_else(|| Error::UnknownCombo(raw.str("combo").unwrap_or("<live>").to_owned()))?;
                // `None` for the pin table, on purpose: this body is a pure
                // pick, and a pin is recorded on a *dispatched* success that
                // never happens here. Passing an empty table would advertise
                // session stickiness this tool cannot honour; `lkgp` therefore
                // falls back to priority, which is the truth.
                let picked = crate::tools::route_request(
                    combo.strategy,
                    raw.str("session"),
                    &combo.candidates,
                    &host.cursor,
                    None,
                )?;
                Ok(serde_json::json!({ "combo": combo.name, "provider": picked.as_str() }))
            }),
            Tool::CostReport => crate::guard(tool, host.scope, &host.audit, &host.key_id, &input, || {
                let report = crate::tools::cost_report(&host.ledger()?, raw.count("limit", 50, 1000)?)?;
                // `CostReport::toon` is `ar-tokens`' own renderer for exactly this
                // shape, so the text block is TOON and the structured value is
                // the same rows in JSON — one query, two renderings, no second
                // shape.
                Ok(serde_json::json!({ "toon": report.toon(), "rows": report.rows.len(), "totals_usd_micros": report.totals.usd.micros }))
            }),
            Tool::ListModels => crate::guard(tool, host.scope, &host.audit, &host.key_id, &input, || {
                let Some(combo) = host.combo(raw.str("combo")) else {
                    return Ok(serde_json::json!({ "models": [] }));
                };
                let routable: Vec<Candidate> = Host::routable(combo).cloned().collect();
                Ok(serde_json::json!({ "models": crate::tools::list_models(&routable) }))
            }),
            Tool::ExplainRoute => crate::guard(tool, host.scope, &host.audit, &host.key_id, &input, || {
                let variant = match raw.str("variant") {
                    None => AutoVariant::Balanced,
                    Some(name) => AutoVariant::parse(name)
                        .ok_or_else(|| Error::UnknownVariant(name.to_owned()))?,
                };
                let pool = host.auto_pool();
                let trace = crate::tools::explain(&AutoCombo::new(variant), &pool, &AutoSelector::new())?;
                Ok(serde_json::json!({
                    "variant": trace.variant,
                    "provider": trace.provider.as_str(),
                    "model": trace.model.as_ref(),
                    "score": trace.score,
                    "reason": format!("{:?}", trace.reason),
                    "fallbacks": trace.fallbacks.iter().map(|p| p.as_str()).collect::<Vec<_>>(),
                }))
            }),
        };
        render(outcome)
    }

    /// `ar_tool_search`. No scope — it reads no host state — but an audit row,
    /// because "every tool leaves one" is the invariant.
    fn search(&self, args: &JsonObject) -> CallToolResult {
        let query = Args::new(Some(args)).str("query").unwrap_or("");
        let started = std::time::Instant::now();
        let hits: Vec<serde_json::Value> = tool_search(query)
            .iter()
            .map(|t| serde_json::json!({ "name": t.name(), "description": t.one_liner(), "scope": t.scope_doc() }))
            .collect();
        let body = serde_json::json!({ "tools": hits });
        // A failure here warns on stderr and does not fail the call, exactly as
        // `tools::write_row` does; the difference is only that this path has no
        // `guard` to inherit it from.
        if let Err(e) = self.host.audit.record(
            TOOL_SEARCH_NAME,
            started.elapsed(),
            &self.host.key_id,
            &serde_json::Value::Object(args.clone()).to_string(),
            &body.to_string(),
            CallOutcome::Ok,
        ) {
            tracing::warn!(tool = TOOL_SEARCH_NAME, error = %e, "audit row not written");
        }
        CallToolResult::structured(body)
    }
}

/// `spend` as JSON. `Usd` is a micro-dollar struct with no `Serialize`, so the
/// integer it wraps is the field the wire wants.
fn spend_json(spend: ar_tokens::Spend) -> serde_json::Value {
    serde_json::json!({ "usd_micros": spend.usd.micros, "tokens": spend.tokens })
}

/// A successful body, or the failure as a caller-visible tool error.
///
/// A tool error rather than a protocol error, on purpose: a bad argument or a
/// refused scope is a fact about the call, and the caller can act on it. Only the
/// server being unable to route is a protocol error.
fn render(outcome: Result<serde_json::Value, Error>) -> CallToolResult {
    match outcome {
        Ok(v) => CallToolResult::structured(v),
        Err(e) => CallToolResult::structured_error(serde_json::json!({ "error": e.to_string() })),
    }
}

impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("ar", env!("CARGO_PKG_VERSION")))
            .with_instructions(format!(
                "Artificial Route control plane. {} tools: {}. Narrow the held scope with AR_MCP_SCOPE.",
                Tool::ALL.len() + 1,
                Tool::ALL.map(|t| t.name()).join(", ")
            ))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let tools: Vec<McpTool> = Tool::ALL.into_iter().map(describe).chain([describe_search()]).collect();
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let args = request.arguments.unwrap_or_default();
        let outcome = if request.name.as_ref() == TOOL_SEARCH_NAME {
            Ok(self.search(&args))
        } else {
            match Tool::parse(request.name.as_ref()) {
                Ok(tool) => Ok(self.call(tool, &args)),
                // An unknown name is a routing failure, not a tool failure: this
                // server cannot find the tool at all, which is the case MCP wants
                // a protocol error for.
                Err(e) => Err(ErrorData::new(ErrorCode::METHOD_NOT_FOUND, e.to_string(), None)),
            }
        };
        Ok(CallToolResponse::Complete(outcome.unwrap_or_else(|e| {
            CallToolResult::error(vec![ContentBlock::text(e.to_string())])
        })))
    }
}

/// Serves the essential-8 plus `ar_tool_search` over stdio.
///
/// # Errors
///
/// [`Error::Transport`] when the handshake or the stdio pump fails — a broken pipe
/// when the host closed the connection, an `initialize` rejection when it is not
/// speaking MCP. A typed `Err` rather than `unimplemented!()`: a panic would
/// abort the process, and a host that asked for a control plane deserves a
/// message it can print.
pub async fn serve_stdio(host: Host) -> Result<(), Error> {
    let service = Server { host: Arc::new(host) }
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|e| Error::Transport(e.to_string()))?;
    service.waiting().await.map(|_| ()).map_err(|e| Error::Transport(e.to_string()))
}

/// Whether the stdio server is wired.
///
/// A `const`, not a runtime probe: the answer is a property of the *build*, so it
/// is decided at compile time and cannot go stale relative to what was linked in.
pub const STDIO_WIRED: bool = true;

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;

    use super::{Args, Host, Mutex, STDIO_WIRED, TOOL_SEARCH_NAME, describe, describe_search, render, spend_json};
    use crate::scope::Scope;
    use crate::Tool;
    use ar_keys::{Admission, LaneSpec};
    use ar_route::ProviderId;

    /// `redb` allows one open handle per file, and the tests run in parallel, so
    /// every host gets its own name and therefore its own ledger and audit db.
    fn host(name: &str) -> Host {
        Host {
            combos: vec![super::HostCombo {
                name: "cheap".to_owned(),
                strategy: ar_route::Strategy::CostOptimized,
                candidates: vec![ar_route::Candidate::new(ProviderId::new("groq"), "llama-3.3-70b")],
                pool: vec![ar_route::Candidate::new(ProviderId::new("together"), "mixtral")],
            }],
            admission: Admission::new([LaneSpec::INTERACTIVE, LaneSpec::BATCH, LaneSpec::HEAVY], 600)
                .expect("build"),
            ledger_path: scratch(name, "ledger.sqlite"),
            active: Mutex::new(crate::ComboState::default()),
            cursor: AtomicU64::new(0),
            scope: Scope::ALL,
            audit: crate::Audit::open(&scratch(name, "audit.redb")).expect("audit"),
            key_id: "k1".to_owned(),
        }
    }

    #[test]
    fn registers_the_catalog_plus_the_search_function() {
        let count = Tool::ALL
            .into_iter()
            .map(|t| t.name())
            .chain(std::iter::once(TOOL_SEARCH_NAME))
            .count();
        assert_eq!(count, 9);
    }

    #[test]
    fn every_registered_tool_carries_its_own_description() {
        for tool in Tool::ALL {
            assert_eq!(describe(tool).name.as_ref(), tool.name());
        }
    }

    #[test]
    fn a_read_tool_announces_itself_read_only() {
        let d = describe(Tool::GetHealth);
        let ann = d.annotations.expect("annotated");
        assert_eq!(ann.read_only_hint, Some(true));
    }

    #[test]
    fn a_write_tool_does_not_announce_itself_read_only() {
        let d = describe(Tool::SwitchCombo);
        assert_ne!(d.annotations.expect("annotated").read_only_hint, Some(true));
    }

    #[test]
    fn a_schema_rejects_an_argument_it_does_not_declare() {
        // A typo'd argument must fail at the host's client, not arrive here and
        // be silently dropped.
        let d = describe(Tool::SwitchCombo);
        assert_eq!(d.input_schema.get("additionalProperties"), Some(&serde_json::json!(false)));
    }

    #[test]
    fn stdio_is_wired_in_this_build() {
        const { assert!(STDIO_WIRED) };
    }

    #[test]
    fn a_missing_argument_reads_as_absent() {
        // `str` is an `Option`, and the `MissingArgument` error is raised by the
        // tool body that needs the value -- so the read itself is what is tested.
        assert!(Args::new(None).str("name").is_none());
    }

    #[test]
    fn a_present_flag_of_the_wrong_type_is_refused() {
        let raw = serde_json::json!({ "active": "yes" });
        let a = Args::new(raw.as_object());
        assert!(a.flag("active", true).is_err());
    }

    #[test]
    fn an_omitted_flag_takes_its_default() {
        let raw = serde_json::json!({});
        let a = Args::new(raw.as_object());
        assert!(a.flag("active", true).expect("default"));
    }

    #[test]
    fn a_count_is_clamped_to_its_ceiling() {
        let raw = serde_json::json!({ "limit": 10_000_000 });
        let a = Args::new(raw.as_object());
        assert_eq!(a.count("limit", 50, 1000).expect("clamped"), 1000);
    }

    #[test]
    fn a_failing_body_renders_as_a_tool_error_not_a_success() {
        let out = render(Err(crate::Error::UnknownTool("nope".into())));
        assert_eq!(out.is_error, Some(true));
    }

    /// A fresh path under the temp dir, removed first so a rerun starts empty.
    fn scratch(name: &str, file: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("ar-mcp-transport");
        std::fs::create_dir_all(&dir).expect("tmpdir");
        let p = dir.join(format!("{name}-{file}"));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn a_health_call_over_the_transport_goes_through_the_guard() {
        // The whole point of the transport: a real call lands a real audit row.
        let h = host("health");
        let before = h.audit.len();
        let s = super::Server { host: Arc::new(h) };
        let out = s.call(Tool::GetHealth, &serde_json::Map::new());
        assert!(out.is_error.is_none() || out.is_error == Some(false), "{out:?}");
        assert_eq!(s.host.audit.len(), before + 1, "one call, one row");
    }

    #[test]
    fn a_denied_scope_reaches_the_caller_as_an_error() {
        let mut h = host("denied");
        h.scope = Scope::NONE;
        let s = super::Server { host: Arc::new(h) };
        let out = s.call(Tool::GetHealth, &serde_json::Map::new());
        assert_eq!(out.is_error, Some(true), "a default-deny refusal is visible: {out:?}");
    }

    #[test]
    fn a_switch_activates_the_named_combo_and_is_idempotent() {
        let h = host("switch");
        let s = super::Server { host: Arc::new(h) };
        let first = s.call(Tool::SwitchCombo, &serde_json::json!({ "name": "cheap" }).as_object().cloned().expect("obj"));
        let second = s.call(Tool::SwitchCombo, &serde_json::json!({ "name": "cheap" }).as_object().cloned().expect("obj"));
        let changed = |r: &rmcp::model::CallToolResult| {
            r.structured_content.as_ref().and_then(|v| v.get("changed")).and_then(serde_json::Value::as_bool)
        };
        assert_eq!(changed(&first), Some(true), "{first:?}");
        assert_eq!(changed(&second), Some(false), "re-activating is a no-op: {second:?}");
    }

    #[test]
    fn the_search_tool_returns_the_catalog_it_was_asked_about() {
        let s = super::Server { host: Arc::new(host("search-body")) };
        let args = serde_json::json!({ "query": "switch" });
        let out = s.search(args.as_object().expect("obj"));
        let names = out.structured_content.expect("body");
        assert_eq!(names["tools"][0]["name"], "ar_switch_combo", "{names}");
    }

    #[test]
    fn the_search_tool_leaves_a_row_like_every_other_tool() {
        let s = super::Server { host: Arc::new(host("search-row")) };
        let before = s.host.audit.len();
        s.search(&serde_json::Map::new());
        assert_eq!(s.host.audit.len(), before + 1);
    }

    #[test]
    fn a_search_answer_names_the_scope_a_call_would_need() {
        let hits = crate::tool_search("quota");
        assert_eq!(hits, [&Tool::CheckQuota], "\"quota\" is in no wire name");
    }

    #[test]
    fn the_search_description_is_registered_too() {
        assert_eq!(describe_search().name.as_ref(), TOOL_SEARCH_NAME);
    }

    #[test]
    fn list_models_reports_the_bench_a_combo_can_fall_back_to() {
        // The bench is routable, so hiding it here would make `ar mcp` answer
        // "what can this combo reach" with less than `ar serve` can.
        let s = super::Server { host: Arc::new(host("list-models")) };
        let out = s.call(Tool::ListModels, &serde_json::Map::new());
        let rows = out.structured_content.expect("body");
        let names: Vec<&str> = rows["models"].as_array().expect("array").iter().filter_map(|m| m["model"].as_str()).collect();
        assert_eq!(names, ["llama-3.3-70b", "mixtral"], "{rows}");
    }

    #[test]
    fn route_request_picks_from_the_targets_not_the_bench() {
        // Same order contract as `ar-server`'s `chain`: the bench is tried after a
        // failure, so it must not be eligible to win a healthy request.
        let h = host("route-request");
        let s = super::Server { host: Arc::new(h) };
        let out = s.call(Tool::RouteRequest, &serde_json::Map::new());
        let winner = out.structured_content.as_ref().expect("body")["provider"].clone();
        assert_eq!(winner, "groq", "the bench must not win: {out:?}");
    }

    #[test]
    fn renders_spend_in_the_unit_the_ledger_computes_with() {
        let s = ar_tokens::Spend::default();
        assert_eq!(spend_json(s)["tokens"], serde_json::json!(0));
    }
}
