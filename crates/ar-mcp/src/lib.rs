//! `ar-mcp` — the optional MCP control plane for Artificial Route.
//!
//! The essential-12 catalog from `docs/06-axi-mcp.md`, as thin wrappers over the
//! same `ar-route` / `ar-tokens` / `ar-keys` / `ar-exec` functions the data plane
//! calls. No second routing implementation, no second pricing implementation, no
//! second admission controller, no second OAuth wire format.
//!
//! [`Tool`] is the catalog and [`tool_search`] the discovery query over it,
//! `scope` the default-deny check, `audit` the append-only `redb` row per
//! guarded call, and [`guard`] the **only** entry point into the bodies —
//! the bodies themselves are `pub(crate)` and cannot be reached from outside
//! this crate. The `rmcp` server module is present only with `--features mcp`,
//! and with it [`transport::serve_stdio`], the wiring `ar mcp` calls.
//!
//! `mcp` is off by default, so `cargo tree -e no-dev` on a default build shows
//! no `rmcp`. Everything else compiles and is callable in-process with the SDK
//! absent, which is what makes it testable without a running server.
//!
//! # The catalog is twelve; discovery is a function
//!
//! `docs/06` lists `ar_tool_search` "from day one", so the search exists — as
//! [`tool_search`], not as a thirteenth [`Tool`] variant. The enum stays the
//! twelve that `docs/06` numbers, and the transport registers [`tool_search`]
//! alongside it. Folding it into the enum would make `Tool::ALL` thirteen long
//! and every count a host reports off-by-one against `TOTAL_MCP_TOOL_COUNT`.

#![deny(missing_docs)]

mod audit;
mod login;
mod scope;
mod tools;

#[cfg(feature = "mcp")]
pub mod transport;

#[cfg(feature = "mcp")]
pub use transport::{Host, HostCombo, STDIO_WIRED, TOOL_SEARCH_NAME, serve_stdio};

pub use audit::{AUDIT_OUTPUT_LIMIT, Audit, AuditError, CallOutcome, input_hash};
pub use login::{AuthTarget, LOGIN_TTL, PendingLogin, PendingLogins, PendingRow};
pub use scope::Scope;
pub use tools::{
    Combo, ComboState, Health, HealthSource, KeyPressure, LaneLoad, Lanes, ModelRow, Quota, Switch,
    get_health, guard, guard_async,
};

/// Everything a tool call can fail with.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The held scope does not cover the tool's requirement. Checked before the
    /// body runs, so nothing has happened behind it — and audited before the
    /// `Err` is returned, so the refusal leaves a row.
    #[error("scope denied for {tool}: need {need:?}")]
    ScopeDenied {
        /// The tool that was refused.
        tool: &'static str,
        /// What it needed.
        need: Scope,
    },
    /// A name that is not in the catalog.
    #[error("unknown tool: {0}")]
    UnknownTool(String),
    /// The router refused the request.
    #[error("route: {0}")]
    Route(#[from] ar_route::RouteError),
    /// The usage ledger refused the read.
    #[error("tokens: {0}")]
    Tokens(#[from] ar_tokens::TokenError),
    /// The encrypted credential store refused a read or a write. Redacted by
    /// construction: `ar_keys` never names a stored value in its own errors.
    #[error("store: {0}")]
    Store(#[from] ar_keys::KeyError),
    /// The audit row could not be written.
    #[error("audit: {0}")]
    Audit(#[from] AuditError),
    /// Arguments did not render as JSON.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    /// A required argument was absent.
    #[error("missing argument: {name}")]
    MissingArgument {
        /// The argument's name.
        name: &'static str,
    },
    /// An argument was present but of a type this tool cannot read. Distinct from
    /// [`Error::MissingArgument`] because the caller's fix differs: one is "send
    /// it", the other is "send it as the declared type".
    #[error("argument {name:?} must be a string, a number or a boolean, not {got}")]
    BadArgument {
        /// The argument's name.
        name: &'static str,
        /// What the caller actually sent, by name.
        got: &'static str,
    },
    /// A combo the call named is not in the host's table.
    #[error("unknown combo: {0}")]
    UnknownCombo(String),
    /// An `auto/*` variant the call named is not one of the six.
    #[error("unknown auto variant: {0}")]
    UnknownVariant(String),
    /// The transport itself failed: a closed pipe, or a host that is not speaking
    /// MCP. Carries the SDK's own message rather than "an error occurred", because
    /// the two cases have different fixes — reconnect, or fix the host config.
    #[error("mcp transport: {0}")]
    Transport(String),
    /// A transport is not wired in this build. Carries *what* is missing, so
    /// the message says which one rather than "an error occurred".
    ///
    /// Reachable only from a build without the `mcp` feature, where
    /// [`crate::transport`] does not exist at all — which is the point: the
    /// variant names the gap instead of a stub pretending to serve.
    #[error("transport not wired: {what}")]
    NotWired {
        /// The missing piece, e.g. `"stdio"`.
        what: &'static str,
    },
    /// A login the call named is not in the config's `oauth:` blocks, or it has
    /// no `authorize_url` to send a person to.
    #[error("auth: {0}")]
    Auth(#[from] ar_exec::LoginError),
    /// A pending login id that is unknown, already used, or past its budget.
    ///
    /// One variant for all three on purpose: from the host's side they are the
    /// same fact — this session id cannot be completed — and a caller that could
    /// tell them apart could probe which ids other logins are holding.
    #[error("auth: no pending login {0:?}: unknown, already completed, or expired")]
    NoPendingSession(String),
    /// A login was redeemed but there is nowhere to put the tokens.
    ///
    /// Carries the path rather than a bare "no store", because the fix is a
    /// specific one — export `$AR_MASTER_KEY`, or point `--config` at a directory
    /// that holds a `credentials.db` — and a sentence without the path does not
    /// say which of the two it is.
    #[error("auth: no credential store at {0}; set $AR_MASTER_KEY, then start a fresh login")]
    StoreRequired(String),
}

/// The essential-12 tool catalog, from `docs/06-axi-mcp.md`. Each variant carries
/// the scope the same table names for it.
///
/// The wire names and the [`Scope`] bit values are **host-visible and frozen**:
/// a host config or an OmniRoute-derived `mcpScopes` mapping names them. Adding
/// a variant is a wire change; changing a bit assignment silently re-points
/// every host at the wrong permission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tool {
    /// Lane pressure, breakers, cache counters.
    GetHealth,
    /// Chains, strategies, metrics.
    ListCombos,
    /// Activate or deactivate a combo. Idempotent.
    SwitchCombo,
    /// Per-provider remaining budget and token health.
    CheckQuota,
    /// A chat completion, routed.
    RouteRequest,
    /// Session / day / week / month spend, per provider.
    CostReport,
    /// Catalog, capabilities, pricing.
    ListModels,
    /// Provider, model, score, factors, fallbacks.
    ExplainRoute,
    /// Start a browser login: the authorize URL and a pending session id.
    AuthLoginUrl,
    /// Redeem a pasted redirect for tokens and store them.
    AuthComplete,
    /// Which logins are pending, and which providers are armed.
    AuthStatus,
    /// Forget a provider's stored tokens.
    AuthLogout,
}

impl Tool {
    /// The catalog, in the order `docs/06` numbers it. The single source of
    /// truth for the tool count: nothing derives a second list.
    pub const ALL: [Self; 12] = [
        Self::GetHealth,
        Self::ListCombos,
        Self::SwitchCombo,
        Self::CheckQuota,
        Self::RouteRequest,
        Self::CostReport,
        Self::ListModels,
        Self::ExplainRoute,
        Self::AuthLoginUrl,
        Self::AuthComplete,
        Self::AuthStatus,
        Self::AuthLogout,
    ];

    /// The wire name, e.g. `"ar_get_health"`.
    pub fn name(self) -> &'static str {
        match self {
            Self::GetHealth => "ar_get_health",
            Self::ListCombos => "ar_list_combos",
            Self::SwitchCombo => "ar_switch_combo",
            Self::CheckQuota => "ar_check_quota",
            Self::RouteRequest => "ar_route_request",
            Self::CostReport => "ar_cost_report",
            Self::ListModels => "ar_list_models",
            Self::ExplainRoute => "ar_explain_route",
            Self::AuthLoginUrl => "ar_auth_login_url",
            Self::AuthComplete => "ar_auth_complete",
            Self::AuthStatus => "ar_auth_status",
            Self::AuthLogout => "ar_auth_logout",
        }
    }

    /// The scope this tool needs. See [`Scope::permits`].
    ///
    /// `Scope::EXECUTE` on [`Tool::RouteRequest`] is deliberate even though the
    /// body is a pure pick — see the `ar-mcp/src/tools.rs` module docs.
    ///
    /// The four login tools ask for the bare [`Scope::READ`] / [`Scope::WRITE`]
    /// categories and no domain bit: an authorize URL is public by construction
    /// (that is what makes it printable on another device), and completing a
    /// login writes credentials. Naming a per-domain bit for them would add an
    /// eleventh scope name to the grant grammar for a capability that is exactly
    /// a read or exactly a write.
    pub fn scope(self) -> Scope {
        match self {
            Self::GetHealth => Scope::READ | Scope::HEALTH,
            Self::ListCombos => Scope::READ | Scope::COMBOS,
            Self::SwitchCombo => Scope::WRITE | Scope::COMBOS,
            Self::CheckQuota => Scope::READ | Scope::QUOTA,
            Self::RouteRequest => Scope::EXECUTE | Scope::COMPLETIONS,
            Self::CostReport => Scope::READ | Scope::USAGE,
            Self::ListModels => Scope::READ | Scope::MODELS,
            Self::ExplainRoute => Scope::READ | Scope::HEALTH | Scope::USAGE,
            Self::AuthLoginUrl | Self::AuthStatus => Scope::READ,
            Self::AuthComplete | Self::AuthLogout => Scope::WRITE,
        }
    }

    /// One line of help, for token-efficient discovery.
    pub fn one_liner(self) -> &'static str {
        match self {
            Self::GetHealth => "lane pressure, breakers and cache counters",
            Self::ListCombos => "list routing combos with their strategies and providers",
            Self::SwitchCombo => "activate or deactivate a combo (idempotent)",
            Self::CheckQuota => "per-key remaining budget and token health",
            Self::RouteRequest => "route a chat completion and report the chosen provider",
            Self::CostReport => "spend by session, day, week, month and provider",
            Self::ListModels => "routable model catalog with capabilities and pricing",
            Self::ExplainRoute => "why a provider won: score, factors, fallbacks",
            Self::AuthLoginUrl => "start a browser login: authorize URL plus a pending session id",
            Self::AuthComplete => "redeem a pasted redirect URL or bare code and store the tokens",
            Self::AuthStatus => "which logins are pending and which providers are armed",
            Self::AuthLogout => "forget a provider's stored access and refresh tokens",
        }
    }

    /// The scope this tool needs, in `docs/06`'s spelling.
    ///
    /// The *requirement* per tool, which is what a reader of the catalog and a
    /// host's own scope config both want to see. The *grant* is a set of bits
    /// ([`Scope::parse`]) of which this mask is a composition, so the two are
    /// deliberately different vocabularies and neither derives the other.
    ///
    /// [`Scope::parse`]: crate::Scope::parse
    #[must_use]
    pub fn scope_doc(self) -> &'static str {
        match self {
            Self::GetHealth => "read:health",
            Self::ListCombos => "read:combos",
            Self::SwitchCombo => "write:combos",
            Self::CheckQuota => "read:quota",
            Self::RouteRequest => "execute:completions",
            Self::CostReport => "read:usage",
            Self::ListModels => "read:models",
            Self::ExplainRoute => "read:health+read:usage",
            Self::AuthLoginUrl | Self::AuthStatus => "read:*",
            Self::AuthComplete | Self::AuthLogout => "write:*",
        }
    }

    /// Looks a tool up by wire name.
    ///
    /// # Errors
    /// [`Error::UnknownTool`] if the name is not in the catalog.
    pub fn parse(name: &str) -> Result<Self, Error> {
        Self::ALL
            .into_iter()
            .find(|t| t.name() == name)
            .ok_or_else(|| Error::UnknownTool(name.to_string()))
    }
}

/// Case-insensitive substring search over the catalog's wire names and one-liners.
///
/// This is the `ar_tool_search` of `docs/06`: the whole of what a search index
/// over eight static entries would hold is their name and their one-liner, so
/// the index is a `contains`. An empty (or whitespace) query returns the whole
/// catalog, which is the useful reading — "what can you do?" answered by the
/// full list rather than by an empty result.
///
/// Returns borrows into [`Tool::ALL`], in catalog order, so the caller spends
/// no allocation beyond the `Vec`. Matching is `ASCII`-case-insensitive on both
/// sides; a host that has been handed `ar_switch_combo` in lower case still
/// finds it.
///
/// ```
/// let hits = ar_mcp::tool_search("combo");
/// assert!(hits.iter().any(|t| t.name() == "ar_switch_combo"));
/// ```
#[must_use]
pub fn tool_search(query: &str) -> Vec<&'static Tool> {
    let q = query.trim().to_ascii_lowercase();
    Tool::ALL
        .iter()
        .filter(|t| {
            q.is_empty()
                || t.name().to_ascii_lowercase().contains(&q)
                || t.one_liner().to_ascii_lowercase().contains(&q)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{Error, Tool, tool_search};

    #[test]
    fn every_tool_has_a_unique_name() {
        let mut names: Vec<&str> = Tool::ALL.iter().map(|t| t.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), 12);
    }

    #[test]
    fn parse_round_trips() {
        assert_eq!(
            Tool::parse("ar_cost_report").expect("known"),
            Tool::CostReport
        );
    }

    #[test]
    fn parse_rejects_a_stranger() {
        assert!(Tool::parse("ar_web_search").is_err());
    }

    #[test]
    fn search_finds_a_tool_by_wire_name_fragment() {
        let hits = tool_search("switch_combo");
        assert_eq!(hits, [&Tool::SwitchCombo]);
    }

    #[test]
    fn search_is_case_insensitive() {
        assert_eq!(tool_search("AR_COST_REPORT"), [&Tool::CostReport]);
    }

    #[test]
    fn search_matches_the_one_liner_too() {
        // "quota" is in no wire name but is in `ar_check_quota`'s help text.
        let hits = tool_search("quota");
        assert!(hits.contains(&&Tool::CheckQuota), "{hits:?}");
    }

    #[test]
    fn search_can_return_more_than_one_hit() {
        assert_eq!(tool_search("combo").len(), 2);
    }

    #[test]
    fn search_with_no_query_lists_the_whole_catalog() {
        assert_eq!(tool_search("   ").len(), Tool::ALL.len());
    }

    #[test]
    fn search_returns_nothing_for_a_word_no_tool_uses() {
        assert!(tool_search("kubernetes").is_empty());
    }

    #[test]
    fn search_preserves_catalog_order() {
        let hits = tool_search("a");
        let positions: Vec<usize> = hits
            .iter()
            .map(|t| Tool::ALL.iter().position(|x| x == *t).expect("in ALL"))
            .collect();
        assert!(positions.windows(2).all(|w| w[0] < w[1]), "{positions:?}");
    }

    #[test]
    fn not_wired_names_the_missing_piece() {
        let msg = Error::NotWired { what: "stdio" }.to_string();
        assert!(msg.contains("stdio"), "{msg}");
    }

    #[test]
    fn a_bad_argument_says_what_it_got() {
        let msg = Error::BadArgument {
            name: "active",
            got: "a string",
        }
        .to_string();
        assert!(msg.contains("active") && msg.contains("a string"), "{msg}");
    }

    #[test]
    fn a_missing_argument_names_only_the_argument() {
        assert_eq!(
            Error::MissingArgument { name: "name" }.to_string(),
            "missing argument: name"
        );
    }

    #[test]
    fn a_transport_failure_carries_the_sdk_message() {
        let msg = Error::Transport("broken pipe".into()).to_string();
        assert!(msg.contains("broken pipe"), "{msg}");
    }
}
