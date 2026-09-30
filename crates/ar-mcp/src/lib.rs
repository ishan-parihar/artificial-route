//! `ar-mcp` — the optional MCP control plane for Artificial Route.
//!
//! The essential-8 catalog from `docs/06-axi-mcp.md`, as thin wrappers over the
//! same `ar-route` / `ar-tokens` / `ar-keys` functions the data plane calls. No
//! second routing implementation, no second pricing implementation, no second
//! admission controller.
//!
//! [`Tool`] is the catalog and [`tool_search`] the discovery query over it,
//! `scope` the default-deny check, `audit` the append-only `redb` row per
//! guarded call, and [`guard`] the **only** entry point into the eight bodies —
//! the bodies themselves are `pub(crate)` and cannot be reached from outside
//! this crate. The `rmcp` server module is present only with `--features mcp`.
//!
//! `mcp` is off by default, so `cargo tree -e no-dev` on a default build shows
//! no `rmcp`. Everything else compiles and is callable in-process with the SDK
//! absent, which is what makes it testable without a running server.
//!
//! # The catalog is eight; discovery is a function
//!
//! `docs/06` lists `ar_tool_search` "from day one", so the search exists — as
//! [`tool_search`], not as a ninth [`Tool`] variant. The enum stays the
//! essential-8 that `docs/04` and the OmniRoute reference count, and the
//! transport registers [`tool_search`] alongside it when it lands. Folding it
//! into the enum would make `Tool::ALL` nine long and every count a host
//! reports off-by-one against `TOTAL_MCP_TOOL_COUNT`.

#![deny(missing_docs)]

mod audit;
mod scope;
mod tools;

#[cfg(feature = "mcp")]
pub mod transport;

pub use audit::{AUDIT_OUTPUT_LIMIT, Audit, AuditError, CallOutcome, input_hash};
pub use scope::Scope;
pub use tools::{
    Combo, ComboState, Health, HealthSource, KeyPressure, LaneLoad, Lanes, ModelRow, Quota, Switch, get_health,
    guard,
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
    /// The audit row could not be written.
    #[error("audit: {0}")]
    Audit(#[from] AuditError),
    /// Arguments did not render as JSON.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    /// A transport is not wired in this build. Carries *what* is missing, so
    /// the message says which one rather than "an error occurred".
    ///
    /// A typed error rather than `unimplemented!()`: a transport stub is not
    /// reachable on a live request path, but a panic is a promise that the
    /// process can die, and the caller of a half-wired control plane deserves
    /// an `Err` it can map to a 501.
    #[error("transport not wired: {what}")]
    NotWired {
        /// The missing piece, e.g. `"stdio"`.
        what: &'static str,
    },
}

/// The essential-8 tool catalog, from `docs/06-axi-mcp.md`. Each variant carries
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
}

impl Tool {
    /// The catalog, in the order `docs/06` numbers it. The single source of
    /// truth for the tool count: nothing derives a second list.
    pub const ALL: [Self; 8] = [
        Self::GetHealth,
        Self::ListCombos,
        Self::SwitchCombo,
        Self::CheckQuota,
        Self::RouteRequest,
        Self::CostReport,
        Self::ListModels,
        Self::ExplainRoute,
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
        }
    }

    /// The scope this tool needs. See [`Scope::permits`].
    ///
    /// `Scope::EXECUTE` on [`Tool::RouteRequest`] is deliberate even though the
    /// body is a pure pick — see the `ar-mcp/src/tools.rs` module docs.
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
        assert_eq!(names.len(), 8);
    }

    #[test]
    fn parse_round_trips() {
        assert_eq!(Tool::parse("ar_cost_report").expect("known"), Tool::CostReport);
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
        let positions: Vec<usize> =
            hits.iter().map(|t| Tool::ALL.iter().position(|x| x == *t).expect("in ALL")).collect();
        assert!(positions.windows(2).all(|w| w[0] < w[1]), "{positions:?}");
    }

    #[test]
    fn not_wired_names_the_missing_piece() {
        let msg = Error::NotWired { what: "stdio" }.to_string();
        assert!(msg.contains("stdio"), "{msg}");
    }
}
