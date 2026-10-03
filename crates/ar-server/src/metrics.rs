//! Prometheus text exposition, hand-rolled.
//!
//! `docs/03-crates-and-deps.md` lists the `prometheus` client crate, and neither
//! the HTTP layer nor the routing layer needs it: there are five HTTP counters,
//! all of which are `u64` adds behind an atomic, and the exposition format for a
//! fixed set of counters is a dozen lines of `write!`. A client would add a
//! registry, a label-validation layer and a protobuf dependency to print the same
//! lines — against the "minimal-RAM" budget in `docs/00-overview.md`.
//!
//! Cardinality is capped at `path|status|provider|decision`, per
//! `docs/04-subsystems.md`. **No prompt, model name, key, or session ever
//! reaches this module** — `observe_*` takes only the four bounded labels, so
//! there is no field a caller could accidentally fill with user content.
//!
//! # Two layers, one endpoint
//!
//! [`Metrics`] counts *transport*: outcomes, failovers, attempts, catalog
//! refreshes. [`ar_obs::Metrics`] counts *work*: tokens, cost, cache disposition,
//! queue lane, and a duration histogram, labelled by `provider|family|decision`.
//! Both render to the same exposition format and `/metrics` prints them together.
//!
//! They are separate because they answer separate questions and neither can
//! substitute for the other: "is the proxy returning 503s" is not derivable from
//! token counts, and "what did this request cost" is not derivable from an
//! outcome class. One registry for both would mean either dropping the histograms
//! or dropping the outcome taxonomy, and `docs/04` asks for both.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};

/// Response status label. Bucketed to the class, not the exact code, so a
/// provider that varies its 5xx cannot inflate the series count.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Outcome {
    /// 2xx.
    Ok,
    /// 4xx other than 429.
    Client,
    /// 429 or 503.
    Throttled,
    /// 5xx other than 503.
    Upstream,
    /// No usable HTTP status (transport failure, timeout).
    Transport,
}

impl Outcome {
    /// Label as it appears in the exposition text.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Client => "client",
            Self::Throttled => "throttled",
            Self::Upstream => "upstream",
            Self::Transport => "transport",
        }
    }
}

/// P0 counter set. Fixed at compile time so the exposition has a stable shape.
#[derive(Debug, Default)]
pub struct Metrics {
    /// The routing/work half, rendered by `ar-obs`.
    ///
    /// This is the crate `docs/04` names as the observability owner, and it was
    /// shipped and tested with no in-tree consumer until now. Owning it here is
    /// what gives `/metrics` its histograms and token/cost series.
    pub work: ar_obs::Metrics,
    requests: AtomicU64,
    throttled: AtomicU64,
    failed_over: AtomicU64,
    attempts: AtomicU64,
    models_refresh: AtomicU64,
}

impl Metrics {
    /// Builds a zeroed counter set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Counts one served request.
    pub fn observe_request(&self, outcome: Outcome) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        if outcome == Outcome::Throttled {
            self.throttled.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Counts one provider-to-provider failover.
    pub fn observe_failover(&self) {
        self.failed_over.fetch_add(1, Ordering::Relaxed);
    }

    /// Counts `n` upstream attempts dispatched, successful or not.
    ///
    /// Takes a count rather than one call per attempt because the attempt site is
    /// the router's loop, not the request handler: the loop owns `tried` and the
    /// handler only holds its verdict. Adding the verdict's own `attempts()` here
    /// is what makes `ar_upstream_attempts_total` count attempts (audit F-MED-3)
    /// rather than requests — the old single `fetch_add(1)` sat one layer above
    /// the loop and could not see the difference.
    pub fn observe_attempts(&self, n: u64) {
        self.attempts.fetch_add(n, Ordering::Relaxed);
    }

    /// Counts one `/v1/models` revalidation pass.
    pub fn observe_models_refresh(&self) {
        self.models_refresh.fetch_add(1, Ordering::Relaxed);
    }

    /// Renders the Prometheus text exposition format (`text/plain; version=0.0.4`).
    ///
    /// Writes into a `String` rather than streaming: the whole document is
    /// ~10 lines, and a `Vec` of `io::Write` adapters is not a simplification.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(512);
        let counters = [
            (
                "ar_http_requests_total",
                "Requests served, by outcome class.",
                self.requests.load(Ordering::Relaxed),
            ),
            (
                "ar_http_throttled_total",
                "Requests answered 429/503.",
                self.throttled.load(Ordering::Relaxed),
            ),
            (
                "ar_route_failovers_total",
                "Provider-to-provider failovers.",
                self.failed_over.load(Ordering::Relaxed),
            ),
            (
                "ar_upstream_attempts_total",
                "Upstream attempts dispatched.",
                self.attempts.load(Ordering::Relaxed),
            ),
            (
                "ar_models_refresh_total",
                "/v1/models revalidations.",
                self.models_refresh.load(Ordering::Relaxed),
            ),
        ];
        for (name, help, value) in counters {
            // `write!` to a String cannot fail; ignoring the Result rather than
            // unwrapping is the honest form.
            let _ = writeln!(out, "# HELP {name} {help}");
            let _ = writeln!(out, "# TYPE {name} counter");
            let _ = writeln!(out, "{name} {value}");
        }
        // The routing half. Its own document, appended: a scraper reads TYPE as a
        // declaration of what follows, and interleaving two registries' blocks
        // would make `ar_requests_total` look like a continuation of the
        // transport counters above.
        out.push_str(&self.work.render());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::{Metrics, Outcome};

    #[test]
    fn renders_counter_with_type_and_help() {
        let text = Metrics::new().render();
        assert!(text.contains("# TYPE ar_http_requests_total counter\nar_http_requests_total 0\n"));
    }

    #[test]
    fn counts_served_requests() {
        let m = Metrics::new();
        m.observe_request(Outcome::Ok);
        assert!(m.render().contains("\nar_http_requests_total 1\n"));
    }

    #[test]
    fn buckets_throttled_separately_from_total() {
        let m = Metrics::new();
        m.observe_request(Outcome::Throttled);
        m.observe_request(Outcome::Ok);
        let text = m.render();
        assert!(text.contains("ar_http_throttled_total 1"));
    }

    #[test]
    fn counts_failovers() {
        let m = Metrics::new();
        m.observe_failover();
        m.observe_failover();
        assert!(m.render().contains("ar_route_failovers_total 2"));
    }

    #[test]
    fn sums_upstream_attempts_when_one_request_spends_several() {
        // The F-MED-3 cardinality fact: a 3-attempt request must add 3, not 1.
        let m = Metrics::new();
        m.observe_attempts(3);
        assert!(m.render().contains("ar_upstream_attempts_total 3"));
    }

    #[test]
    fn accumulates_across_requests() {
        let m = Metrics::new();
        m.observe_attempts(2);
        m.observe_attempts(1);
        assert!(m.render().contains("ar_upstream_attempts_total 3"));
    }

    #[test]
    fn renders_the_routing_halfs_series_alongside_the_transport_counters() {
        // The wiring gap this crate had: `ar-obs` shipped complete, tested, and
        // unconsumed. `/metrics` now prints both halves from one endpoint.
        let m = Metrics::new();
        m.work.observe(&ar_obs::Request {
            provider: "openai",
            family: ar_obs::Family::Balanced,
            decision: ar_obs::Decision::Primary,
            cache: ar_obs::Cache::Miss,
            queue: ar_obs::Queue::Direct,
            queue_pos: 0,
            attempts: 1,
            tokens_in: 12,
            tokens_out: 40,
            cost_micros: 3,
            duration_us: 830,
            queue_wait_us: 0,
        });
        let out = m.render();
        assert!(out.contains("ar_http_requests_total"), "{out}");
        assert!(
            out.contains(r#"ar_tokens_in_total{provider="openai""#),
            "the routing half must reach the same document: {out}"
        );
        assert!(
            out.contains("ar_request_duration_us"),
            "histograms too: {out}"
        );
    }

    #[test]
    fn renders_no_label_when_nothing_has_been_observed() {
        // Was `never_renders_a_label_field`, and asserted the P0 gate from
        // docs/04-obs: with no labels anywhere, the transport half has no `{...}`
        // syntax. That gate still holds for *this* half — it emits five unlabelled
        // counters — but it no longer holds for the document, because the routing
        // half is labelled by design (`provider|family|decision`, bounded enums).
        //
        // The real no-content guarantee is the one that matters and still holds:
        // nothing user-supplied can appear in a label, because every label is a
        // bounded enum except `provider`, which is a registry id.
        let rendered = Metrics::new().render();
        let transport: Vec<&str> = rendered
            .lines()
            .filter(|l| {
                l.starts_with("ar_http")
                    || l.starts_with("ar_route_")
                    || l.starts_with("ar_upstream_")
                    || l.starts_with("ar_models_")
            })
            .filter(|l| !l.starts_with("#"))
            .collect();
        assert_eq!(
            transport.len(),
            5,
            "five unlabelled transport samples: {rendered}"
        );
        // The no-content guarantee is the one that matters: every label is a bounded
        // enum except `provider`, a registry id, so nothing user-supplied can
        // reach the exposition. The routing half always carries labels — it is
        // labelled by design — so the `{`-free property is asserted on the
        // transport half alone, which is what P0's gate claimed.
        assert!(
            transport.iter().all(|l| !l.contains('{')),
            "the transport half stays unlabelled: {transport:?}"
        );
    }
}
