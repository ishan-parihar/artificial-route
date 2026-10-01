//! The twelve tool bodies, and the one guard every call goes through.
//!
//! # `guard()` is the only entry point
//!
//! Every body below is `pub(crate)`. A host cannot call `ar_mcp::route_request`
//! or `ar_mcp::list_models` — they do not resolve from outside the crate, and
//! the crate-visible doc test at the bottom of this module is what proves it.
//! [`guard`] is the sole public entry: scope check, then exactly one audit row,
//! then the body. A transport that wants a tool calls `guard` — or
//! [`guard_async`] for the one body that has to await the token exchange — and
//! passes a closure; there is no path that skips the check or loses the row.
//!
//! [`Tool::ALL`] stays public because a host must be able to *list* the
//! catalog. Listing is not calling.
//!
//! # No second implementation
//!
//! Every function delegates: routing is [`ar_route::pick`], the cost report is
//! [`ar_tokens::Ledger::report`], quota is [`ar_tokens::Ledger::spend`] plus
//! [`ar_tokens::Ledger::cap`], the trace is [`ar_route::explain_route`]. An
//! upstream type comes back *as is* rather than re-projected into a parallel
//! shape -- a second shape is a second implementation waiting to drift.
//!
//! The only shape this crate owns is lane pressure, and it reads
//! [`ar_keys::Admission`] rather than taking a number from the caller. The
//! [`Health::from_snapshot`] fallback is kept for a host that runs without
//! `ar-keys` at all, and it *says so* in [`Health::source`] — a health report
//! that cannot name its own provenance is a health report nobody should trust.
//!
//! # `ar_route_request` is `execute:*`-gated on purpose
//!
//! The body is a pure pick — no provider is contacted — and the obvious
//! question is why it needs [`Scope::EXECUTE`] rather than
//! [`Scope::READ | Scope::COMPLETIONS`]. Because a pick is the *only*
//! observable difference between a completion that was routed and one that was
//! not, and because the pick is what a caller would then use to address a
//! provider directly. A `read:*` token that could steer traffic is a
//! write in everything but the syscall. The scope stays as `docs/06` names it
//! (`execute:completions`); the bit values are host-visible and unchanged.

use std::sync::atomic::AtomicU64;
use std::time::Instant;

use ar_keys::{Admission, Lane};
use ar_route::{
    AutoCandidate, AutoCombo, AutoSelector, Candidate, LkgpPins, ProviderId, RouteError, RouteTrace,
    Strategy, explain_route, pick,
};
use ar_tokens::{Cap, CostReport, Ledger, Spend, TokenError};
use serde::Serialize;

use crate::audit::{Audit, CallOutcome};
use crate::scope::Scope;
use crate::{Error, Tool};

/// Writes one audit row, and never fails the call because of it.
///
/// Losing the trail is bad; failing a caller's request over the trail is worse.
/// A failure here is not silent, though: the old `let _ =` discarded it, so a
/// disk-full or a lock timeout left no trace at all and the "audited" claim
/// quietly stopped being true. Now it is a `warn` with the tool named, which is
/// what a reader of the logs needs to know the trail has a hole in it.
fn write_row(
    audit: &Audit,
    tool: &str,
    took: std::time::Duration,
    key_id: &str,
    input: &str,
    output: &str,
    outcome: CallOutcome,
) {
    if let Err(e) = audit.record(tool, took, key_id, input, output, outcome) {
        tracing::warn!(tool, key_id, outcome = outcome.as_str(), error = %e, "audit row not written");
    }
}

/// Runs `f` under `tool`'s scope check, recording exactly one audit row.
///
/// The row is written on **both** outcomes, including the refusal: a scope
/// denial is recorded before the `Err` is returned, because a denial is the one
/// event an operator most wants to see and the one most likely to be the whole
/// point of running the audit. The refusal's duration is `0` because the body
/// never started — the elapsed time between asking and being told no is not the
/// tool's cost.
///
/// An audit failure never masks the tool's result; see [`write_row`].
///
/// The four tools whose upstream return type is not `Serialize` are still
/// callable in-process; a transport renders them from the returned value's
/// public fields rather than this crate inventing a second shape.
///
/// ```compile_fail
/// // The bodies are crate-private. This is the whole point: there is no way to
/// // reach a tool without the scope check and the audit row.
/// use ar_mcp::route_request;
/// ```
pub fn guard<T: Serialize>(
    tool: Tool,
    have: Scope,
    audit: &Audit,
    key_id: &str,
    input: &serde_json::Value,
    f: impl FnOnce() -> Result<T, Error>,
) -> Result<T, Error> {
    permit(tool, have, audit, key_id, input)?;
    let started = Instant::now();
    let outcome = f();
    record(audit, tool, key_id, input, started.elapsed(), &outcome);
    outcome
}

/// [`guard`] for a body that has to await something — the token exchange in
/// `ar_auth_complete`.
///
/// The same three steps in the same order (scope check, body, exactly one audit
/// row) rather than a second spelling of them: an async path that did its own
/// scope check would be a scope check someone can forget to update when a tool
/// moves between the two, and the failure mode is a write tool reachable under a
/// read grant. It is a separate function only because [`guard`]'s closure is
/// `FnOnce() -> Result<T, Error>` and a body that does I/O returns a future.
///
/// The exchange is the only awaiting body in the catalog, which is why this is
/// not a generic `impl Future` guard: there is nothing else to generalise for.
pub async fn guard_async<T, F, Fut>(
    tool: Tool,
    have: Scope,
    audit: &Audit,
    key_id: &str,
    input: &serde_json::Value,
    f: F,
) -> Result<T, Error>
where
    T: Serialize,
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<T, Error>>,
{
    permit(tool, have, audit, key_id, input)?;
    let started = Instant::now();
    let outcome = f().await;
    record(audit, tool, key_id, input, started.elapsed(), &outcome);
    outcome
}

/// The scope half of [`guard`], shared with [`guard_async`].
///
/// Refused before the body starts, and recorded before the `Err` is returned: a
/// denial is the event an operator most wants to see and the one most likely to
/// be the whole point of running the audit. Its duration is `0` because the body
/// never began — the elapsed time between asking and being told no is not the
/// tool's cost.
fn permit(
    tool: Tool,
    have: Scope,
    audit: &Audit,
    key_id: &str,
    input: &serde_json::Value,
) -> Result<(), Error> {
    let need = tool.scope();
    if have.permits(need) {
        return Ok(());
    }
    let denied = Error::ScopeDenied { tool: tool.name(), need };
    write_row(
        audit,
        tool.name(),
        std::time::Duration::ZERO,
        key_id,
        &input.to_string(),
        &denied.to_string(),
        CallOutcome::Denied,
    );
    Err(denied)
}

/// The audit half of [`guard`], shared with [`guard_async`].
fn record<T: Serialize>(
    audit: &Audit,
    tool: Tool,
    key_id: &str,
    input: &serde_json::Value,
    took: std::time::Duration,
    outcome: &Result<T, Error>,
) {
    let (body, call) = match outcome {
        Ok(v) => (
            serde_json::to_string(v).unwrap_or_else(|_| "<unserializable>".into()),
            CallOutcome::Ok,
        ),
        Err(e) => (format!("error: {e}"), CallOutcome::Error),
    };
    write_row(audit, tool.name(), took, key_id, &input.to_string(), &body, call);
}

/// One key's admission state, supplied by a host that runs **without**
/// `ar-keys`. Only [`Health::from_snapshot`] consumes it; a host that has an
/// [`Admission`] should read that instead, so the numbers are the real ones
/// rather than whatever the caller felt like passing.
#[derive(Debug, Clone, Serialize)]
pub struct KeyPressure {
    /// Opaque key identifier, never the secret.
    pub key_id: String,
    /// Whether the key is currently admitted for traffic.
    pub admitted: bool,
    /// `0.0..=1.0`, the host's own admission-pressure reading.
    pub pressure: f32,
}

/// Where a [`Health`] reading came from. Every `ar_get_health` answer carries
/// one, because the two sources are not comparable: a snapshot is a host's
/// claim, a live reading is [`ar_keys::Admission`]'s own state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum HealthSource {
    /// Read from a real [`Admission`] — in-flight, queued and capacity per lane.
    Live,
    /// Assembled from caller-supplied [`KeyPressure`]. Lane detail is all zero
    /// and the aggregate is whatever the caller passed.
    Snapshot,
}

/// One lane's load.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct LaneLoad {
    /// Lane name, as `ar-keys` spells it.
    pub lane: &'static str,
    /// Permits currently held.
    pub in_flight: usize,
    /// Requests waiting for a permit.
    pub queued: usize,
    /// Permits in force, i.e. after the heavy-share clamp.
    pub capacity: usize,
}

impl LaneLoad {
    /// `0.0..=1.0` lane utilisation, or `0.0` for a lane with no capacity in
    /// force. Never divides by zero and never exceeds 1.0: in-flight cannot
    /// exceed capacity, but a caller that rebuilt the controller mid-read
    /// should not be able to render `inf`.
    fn utilisation(self) -> f32 {
        if self.capacity == 0 {
            return 0.0;
        }
        (self.in_flight as f32 / self.capacity as f32).clamp(0.0, 1.0)
    }
}

/// Aggregate lane pressure. The field names are shared by both sources and
/// mean what [`Health::source`] says they mean:
///
/// | field | [`HealthSource::Live`] | [`HealthSource::Snapshot`] |
/// |---|---|---|
/// | `keys` | in-force permit capacity | keys the host knows about |
/// | `admitted` | requests in flight, all lanes | how many are admitted |
/// | `mean_pressure` | mean per-lane utilisation | mean admitted-key pressure |
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Lanes {
    /// Capacity, or key count. See the table above.
    pub keys: usize,
    /// In flight, or admitted keys.
    pub admitted: usize,
    /// Mean utilisation, or mean admitted-key pressure.
    pub mean_pressure: f32,
}

/// `ar_get_health`: lane pressure, breaker count, cache counters.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Health {
    /// Aggregate lane pressure.
    pub lanes: Lanes,
    /// Per-lane load, `Lane::PRIORITY` order (interactive, batch, heavy). All
    /// zero under [`HealthSource::Snapshot`], which is why `source` is on the
    /// struct rather than left to the caller to remember.
    pub per_lane: [LaneLoad; 3],
    /// Open breakers, as counted by the host. No lane-wide breaker exists in P0
    /// (`ar-route` carries one resilience layer), so this is a host number in
    /// both sources.
    pub breakers_open: u32,
    /// Cache hits, host counter.
    pub cache_hits: u64,
    /// Cache misses, host counter.
    pub cache_misses: u64,
    /// Which of the two readings this is.
    pub source: HealthSource,
}

/// `ar_get_health`, from the real three-lane admission state.
///
/// This is the reading to use: every number comes from [`Admission`] rather than
/// from the caller, so it cannot be stale by the time it is rendered, and
/// `per_lane` is the actual in-flight/queued/capacity of each lane. Cache
/// counters and breakers have no `ar-keys` equivalent and stay host-supplied.
pub fn get_health(adm: &Admission, breakers_open: u32, cache: (u64, u64)) -> Health {
    let per_lane = Lane::PRIORITY.map(|lane| LaneLoad {
        lane: lane.as_str(),
        in_flight: adm.in_flight(lane),
        queued: adm.queued(lane),
        capacity: adm.capacity(lane),
    });
    let capacity: usize = per_lane.iter().map(|l| l.capacity).sum();
    let in_flight: usize = per_lane.iter().map(|l| l.in_flight).sum();
    let mean = if capacity == 0 {
        0.0
    } else {
        per_lane.iter().copied().map(LaneLoad::utilisation).sum::<f32>() / 3.0
    };
    Health {
        lanes: Lanes { keys: capacity, admitted: in_flight, mean_pressure: mean },
        per_lane,
        breakers_open,
        cache_hits: cache.0,
        cache_misses: cache.1,
        source: HealthSource::Live,
    }
}

impl Health {
    /// `ar_get_health` for a host with no `ar-keys` admission controller — a
    /// keyless deployment, a CLI preview, a test.
    ///
    /// The fallback, and visibly the fallback: `source` is
    /// [`HealthSource::Snapshot`] and `per_lane` is all zero. Reach for
    /// [`get_health`] whenever there is an [`Admission`] to read.
    #[must_use]
    pub fn from_snapshot(keys: &[KeyPressure], breakers_open: u32, cache: (u64, u64)) -> Self {
        let (mut admitted, mut sum) = (0usize, 0.0f32);
        for k in keys.iter().filter(|k| k.admitted) {
            admitted += 1;
            sum += k.pressure;
        }
        Self {
            lanes: Lanes {
                keys: keys.len(),
                admitted,
                mean_pressure: if admitted == 0 { 0.0 } else { sum / admitted as f32 },
            },
            per_lane: [LaneLoad::default(); 3],
            breakers_open,
            cache_hits: cache.0,
            cache_misses: cache.1,
            source: HealthSource::Snapshot,
        }
    }
}

/// One combo: a name, the strategy it routes with, and the providers it reaches.
#[derive(Debug, Clone, Serialize)]
pub struct Combo {
    /// Host-assigned combo name, e.g. `"auto/cheap"`.
    pub name: String,
    /// The strategy the combo routes with, rendered for the wire.
    pub strategy: String,
    /// Distinct provider ids, in first-seen order.
    pub providers: Vec<String>,
}

/// Which combo is live. The host owns it; `ar_switch_combo` only flips it.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ComboState {
    /// The active combo name, or `None` for none.
    pub active: Option<String>,
}

/// The result of an `ar_switch_combo`. `changed: false` is the idempotent
/// no-op, and it is a success, not an error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Switch {
    /// The combo the call named.
    pub name: String,
    /// Whether it is active after the call.
    pub active: bool,
    /// Whether this call changed anything.
    pub changed: bool,
}

/// `ar_check_quota`: what a key has spent against what it is allowed.
#[derive(Debug, Clone)]
pub struct Quota {
    /// Spend so far.
    pub spend: Spend,
    /// The configured cap, if one is set.
    pub cap: Option<Cap>,
}

/// One row of `ar_list_models`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelRow {
    /// Provider id.
    pub provider: String,
    /// Provider-local model name.
    pub model: String,
}

/// `ar_list_combos`. Distinct providers in the candidate set, first-seen order.
pub(crate) fn list_combos(name: &str, strategy: &Strategy, candidates: &[Candidate]) -> Combo {
    let mut providers: Vec<String> = Vec::new();
    for c in candidates {
        let p = c.provider.as_str().to_string();
        if !providers.contains(&p) {
            providers.push(p);
        }
    }
    Combo { name: name.to_string(), strategy: format!("{strategy:?}"), providers }
}

/// `ar_switch_combo`. Idempotent: re-activating the live combo is a no-op
/// reporting `changed: false`, never an error.
pub(crate) fn switch_combo(state: &mut ComboState, name: &str, active: bool) -> Switch {
    let want = active.then(|| name.to_string());
    let changed = state.active != want;
    if changed {
        state.active = want;
    }
    Switch { name: name.to_string(), active, changed }
}

/// `ar_check_quota`. Reads the ledger; it decides nothing.
pub(crate) fn check_quota(ledger: &Ledger, key_id: &str) -> Result<Quota, TokenError> {
    Ok(Quota { spend: ledger.spend(key_id)?, cap: ledger.cap(key_id)? })
}

/// `ar_route_request`. Pure delegation to the router. `execute:*`-gated by
/// design — see the module docs.
pub(crate) fn route_request(
    strategy: Strategy,
    session: Option<&str>,
    candidates: &[Candidate],
    cursor: &AtomicU64,
    lkgp: Option<&LkgpPins>,
) -> Result<ProviderId, RouteError> {
    pick(strategy, session, candidates, cursor, lkgp)
}

/// `ar_cost_report`. Pure delegation to the usage ledger.
pub(crate) fn cost_report(ledger: &Ledger, limit: usize) -> Result<CostReport, TokenError> {
    ledger.report(limit)
}

/// `ar_list_models`. The routable catalog is the candidate set; there is no
/// second catalog to consult.
pub(crate) fn list_models(candidates: &[Candidate]) -> Vec<ModelRow> {
    candidates
        .iter()
        .map(|c| ModelRow {
            provider: c.provider.as_str().to_string(),
            model: c.model.clone().to_string(),
        })
        .collect()
}

/// `ar_explain_route`. Pure delegation, with the neutral task-fitness value the
/// `ar-route` docs prescribe.
pub(crate) fn explain(
    combo: &AutoCombo,
    pool: &[AutoCandidate],
    selector: &AutoSelector,
) -> Result<RouteTrace, RouteError> {
    explain_route(combo, pool, selector, |_| 0.5)
}

#[cfg(test)]
mod tests {
    use super::{
        ComboState, Health, HealthSource, KeyPressure, get_health, guard, guard_async, switch_combo,
    };
    use crate::audit::{Audit, CallOutcome};
    use crate::scope::Scope;
    use crate::Tool;
    use ar_keys::{Admission, Lane, LaneSpec};
    use ar_route::{Candidate, ProviderId, Strategy};
    use std::sync::atomic::AtomicU64;

    fn pool() -> Vec<Candidate> {
        vec![
            Candidate::new(ProviderId::new("openai"), "gpt-4o").with_price(2.50).with_rank(0),
            Candidate::new(ProviderId::new("groq"), "llama-3.3-70b").with_price(0.59).with_rank(1),
        ]
    }

    fn audit(name: &str) -> Audit {
        let dir = std::env::temp_dir().join("ar-mcp-tests");
        std::fs::create_dir_all(&dir).expect("tmpdir");
        let p = dir.join(name);
        let _ = std::fs::remove_file(&p);
        Audit::open(&p).expect("open")
    }

    fn admission() -> Admission {
        Admission::new([LaneSpec::INTERACTIVE, LaneSpec::BATCH, LaneSpec::HEAVY], 600).expect("build")
    }

    #[test]
    fn route_request_calls_the_router() {
        let w = super::route_request(Strategy::CostOptimized, None, &pool(), &AtomicU64::new(0), None)
            .expect("pick");
        assert_eq!(w.as_str(), "groq");
    }

    #[test]
    fn switch_combo_is_idempotent() {
        let mut state = ComboState::default();
        switch_combo(&mut state, "auto/cheap", true);
        assert!(!switch_combo(&mut state, "auto/cheap", true).changed);
    }

    #[test]
    fn list_combos_dedups_providers_and_keeps_first_seen_order() {
        let combo = super::list_combos("auto/cheap", &Strategy::Priority, &pool());
        assert_eq!(combo.providers, ["openai", "groq"]);
    }

    #[test]
    fn list_combos_renders_the_strategy_for_the_wire() {
        let combo = super::list_combos("p", &Strategy::CostOptimized, &pool());
        assert_eq!(combo.strategy, "CostOptimized");
    }

    #[test]
    fn list_models_reads_the_candidate_set() {
        let rows = super::list_models(&pool());
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn check_quota_reports_spend_and_no_cap_for_a_bare_key() {
        let ledger = ar_tokens::Ledger::open_in_memory().expect("ledger");
        let q = super::check_quota(&ledger, "unmetered").expect("quota");
        assert!(q.cap.is_none());
    }

    #[test]
    fn check_quota_reports_the_cap_when_one_is_installed() {
        let ledger = ar_tokens::Ledger::open_in_memory().expect("ledger");
        let cap = ar_tokens::Cap {
            key_id: "k1".into(),
            usd_micros: Some(1_000),
            tokens: None,
            refuse_unpriced: false,
        };
        ledger.set_cap(&cap).expect("cap");
        assert_eq!(super::check_quota(&ledger, "k1").expect("quota").cap, Some(cap));
    }

    #[test]
    fn cost_report_delegates_to_the_ledger() {
        let ledger = ar_tokens::Ledger::open_in_memory().expect("ledger");
        assert_eq!(super::cost_report(&ledger, 10).expect("report").rows.len(), 0);
    }

    #[test]
    fn explain_produces_a_trace_for_an_auto_combo() {
        let pool = [ar_route::AutoCandidate::new(ar_route::ProviderId::new("groq"), "llama-3.3-70b")];
        let combo = ar_route::AutoCombo::new(ar_route::AutoVariant::Balanced);
        let trace = super::explain(&combo, &pool, &ar_route::AutoSelector::new()).expect("trace");
        assert_eq!(trace.provider.as_str(), "groq");
    }

    #[test]
    fn get_health_reads_real_lane_capacity() {
        let h = get_health(&admission(), 0, (0, 0));
        assert_eq!(h.lanes.keys, 170);
    }

    #[test]
    fn get_health_marks_its_source_live() {
        assert_eq!(get_health(&admission(), 0, (0, 0)).source, HealthSource::Live);
    }

    #[test]
    fn get_health_reports_lane_names_in_priority_order() {
        let h = get_health(&admission(), 0, (0, 0));
        let names: Vec<&str> = h.per_lane.iter().map(|l| l.lane).collect();
        assert_eq!(names, ["interactive", "batch", "heavy"]);
    }

    #[test]
    fn snapshot_health_averages_admitted_keys_only() {
        let h = Health::from_snapshot(
            &[
                KeyPressure { key_id: "a".into(), admitted: true, pressure: 0.2 },
                KeyPressure { key_id: "b".into(), admitted: false, pressure: 1.0 },
            ],
            0,
            (0, 0),
        );
        assert_eq!(h.lanes.mean_pressure, 0.2);
    }

    #[test]
    fn snapshot_health_says_it_is_a_snapshot() {
        assert_eq!(Health::from_snapshot(&[], 0, (0, 0)).source, HealthSource::Snapshot);
    }

    #[test]
    fn guard_denies_before_the_closure_runs() {
        let r = guard(
            Tool::SwitchCombo,
            Scope::READ,
            &audit("deny.redb"),
            "k1",
            &serde_json::json!({}),
            || Ok::<Vec<super::ModelRow>, crate::Error>(Vec::new()),
        );
        assert!(r.is_err());
    }

    #[test]
    fn guard_audits_a_successful_call() {
        let audit = audit("call.redb");
        let r = guard(
            Tool::ListModels,
            Scope::READ,
            &audit,
            "k1",
            &serde_json::json!({ "n": 2 }),
            || Ok(super::list_models(&pool())),
        );
        assert!(r.is_ok() && audit.len() == 1);
    }

    #[test]
    fn guard_audits_a_denial_before_returning_the_error() {
        let audit = audit("denial-row.redb");
        let r = guard(
            Tool::SwitchCombo,
            Scope::READ,
            &audit,
            "k1",
            &serde_json::json!({ "name": "auto/cheap" }),
            || Ok::<Vec<super::ModelRow>, crate::Error>(Vec::new()),
        );
        assert!(r.is_err(), "the scope check still refuses");
        let row = audit.get(0).expect("get").expect("row");
        assert!(row.contains("|denied|"), "a refusal must leave a row: {row}");
    }

    #[test]
    fn guard_audits_a_body_error_as_error_not_ok() {
        let audit = audit("error-row.redb");
        let r = guard(
            Tool::ListModels,
            Scope::READ,
            &audit,
            "k1",
            &serde_json::json!({}),
            || Err::<Vec<super::ModelRow>, crate::Error>(crate::Error::UnknownTool("nope".into())),
        );
        assert!(r.is_err());
        let row = audit.get(0).expect("get").expect("row");
        assert!(row.contains("|error|"), "a failed body must not read as ok: {row}");
    }

    #[test]
    fn every_tool_in_the_catalog_is_guarded_by_a_scope_check() {
        // Each catalog entry needs at least one bit, so `Scope::NONE` refuses
        // all twelve and the empty scope is the floor.
        for tool in Tool::ALL {
            let need = tool.scope();
            assert_ne!(need, Scope::NONE, "{} asks for nothing", tool.name());
            assert!(!Scope::NONE.permits(need), "{} is reachable unscoped", tool.name());
        }
    }

    #[test]
    fn every_write_or_execute_tool_keeps_a_privileged_bit() {
        for tool in Tool::ALL {
            let need = tool.scope();
            let privileged = need.0 & (Scope::WRITE.0 | Scope::EXECUTE.0);
            let named_write = matches!(tool, Tool::SwitchCombo | Tool::RouteRequest);
            if named_write || need.0 & Scope::READ.0 == 0 {
                assert_ne!(privileged, 0, "{} lost its privileged bit", tool.name());
            }
        }
    }

    #[test]
    fn a_read_grant_cannot_reach_the_two_login_writes() {
        // The narrow-grant refusal, stated on the catalog rather than on one tool.
        let read_only = Scope::parse("read:*").expect("known");
        for tool in [Tool::AuthComplete, Tool::AuthLogout] {
            assert!(!read_only.permits(tool.scope()), "{} is reachable under read:*", tool.name());
        }
    }

    #[tokio::test]
    async fn guard_async_audits_a_successful_call() {
        let audit = audit("async-call.redb");
        let r = guard_async(
            Tool::AuthStatus,
            Scope::READ,
            &audit,
            "k1",
            &serde_json::json!({}),
            || async { Ok::<Vec<super::ModelRow>, crate::Error>(Vec::new()) },
        )
        .await;
        assert!(r.is_ok() && audit.len() == 1);
    }

    #[tokio::test]
    async fn guard_async_refuses_before_the_closure_runs() {
        let audit = audit("async-deny.redb");
        let r = guard_async::<Vec<super::ModelRow>, _, _>(
            Tool::AuthComplete,
            Scope::READ,
            &audit,
            "k1",
            &serde_json::json!({ "session_id": "s1" }),
            || async {
                panic!("the body must not run")
            },
        )
        .await;
        assert!(r.is_err(), "a write tool is refused under a read grant");
    }

    #[tokio::test]
    async fn guard_async_leaves_one_row_for_a_failure_too() {
        let audit = audit("async-error.redb");
        let r = guard_async::<Vec<super::ModelRow>, _, _>(
            Tool::AuthComplete,
            Scope::ALL,
            &audit,
            "k1",
            &serde_json::json!({}),
            || async { Err(crate::Error::NoPendingSession("s1".into())) },
        )
        .await;
        assert!(r.is_err());
        assert!(audit.get(0).expect("get").expect("row").contains("|error|"));
    }

    #[tokio::test]
    async fn lane_load_reports_a_held_permit() {
        let adm = admission();
        let held = adm.acquire(Lane::Interactive, "k1").await.expect("interactive");
        let h = get_health(&adm, 0, (0, 0));
        assert_eq!(h.per_lane[Lane::Interactive.index()].in_flight, 1);
        drop(held);
    }

    #[test]
    fn audit_record_signature_keeps_its_outcome_last() {
        // `record` gained an outcome parameter; this pins the order so a
        // refactor cannot silently swap `tool` and `key_id`.
        let audit = audit("order.redb");
        let seq = audit
            .record("ar_cost_report", std::time::Duration::ZERO, "k9", "{}", "x", CallOutcome::Ok)
            .expect("record");
        let row = audit.get(seq).expect("get").expect("row");
        assert!(row.starts_with("ar_cost_report|0|k9|"), "{row}");
    }
}
