//! Prometheus text exposition, hand-rolled.
//!
//! `docs/03-crates-and-deps.md` lists the `prometheus` client crate, and P0 does
//! not need it: there are four counters, all of which are `u64` adds behind an
//! atomic, and the exposition format for a fixed set of counters is a dozen
//! lines of `write!`. A client would add a registry, a label-validation layer
//! and a protobuf dependency to print the same four lines — against the
//! "minimal-RAM" budget in `docs/00-overview.md`.
//!
//! Cardinality is capped at `path|status|provider|decision`, per
//! `docs/04-subsystems.md`. **No prompt, model name, key, or session ever
//! reaches this module** — `observe_*` takes only the four bounded labels, so
//! there is no field a caller could accidentally fill with user content.

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
    fn never_renders_a_label_field() {
        // The P0 gate from docs/04-obs: nothing here can carry a prompt, so
        // the exposition has no `{...}` label syntax at all.
        assert!(!Metrics::new().render().contains('{'));
    }
}
