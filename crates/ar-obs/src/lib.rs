//! `ar-obs` — metrics, JSON tracing, and the redacted audit ledger.
//!
//! The P5 half of Artificial Route, against `docs/04-subsystems.md`:
//!
//! * [`metrics`] — Prometheus counters and histograms capped at
//!   `provider|family|decision`, plus the `Decision | Usage | Cost | Cache |
//!   Queue` headers. No prompt, model name, key, or session reaches it.
//! * [`trace`] — a `tracing` JSON subscriber fed by a 128k lossy channel into a
//!   dedicated writer thread, rotated daily and pruned at 7d.
//! * [`audit`] — a `redb` ledger over `ar_keys::AuditLine`, already redacted by
//!   type. A raw path exists behind an admin grant, stamped with a hard 24h TTL.
//!
//! No BigQuery and no cloud export. `docs/05` put BigQuery in P5 and this drops
//! it: shipping every prompt to a warehouse is a data-egress decision, not an
//! observability one, and it would make "a stolen `read` token can't dump
//! traffic" depend on a second system's access control. If the numbers have to
//! leave the box, the answer is a scrape of `/metrics`, not a write path.
//!
//! ```no_run
//! use ar_obs::{Cache, Decision, Family, Metrics, Queue, Request, TraceWriter};
//!
//! let metrics = Metrics::new();
//! let trace = TraceWriter::start(std::path::Path::new("./trace"))?;
//! trace.install()?; // RUST_LOG, or `warn`
//!
//! let r = Request {
//!     provider: "groq",
//!     family: Family::Balanced,
//!     decision: Decision::Primary,
//!     cache: Cache::Miss,
//!     queue: Queue::Direct,
//!     queue_pos: 0,
//!     attempts: 1,
//!     tokens_in: 12,
//!     tokens_out: 40,
//!     cost_micros: 0,
//!     duration_us: 830,
//!     queue_wait_us: 0,
//! };
//! metrics.observe(&r);
//! for (name, value) in r.headers() {
//!     println!("{name}: {value}");
//! }
//! # Ok::<(), ar_obs::ObsError>(())
//! ```

#![deny(missing_docs)]

pub mod audit;
pub mod error;
pub mod metrics;
pub mod trace;

pub use audit::{AuditLedger, RAW_TTL_SECS};
pub use error::ObsError;
pub use metrics::{Cache, Decision, Family, Metrics, Queue, Request, MAX_SERIES};
pub use trace::{CHANNEL_CAP, RETENTION_DAYS, TraceWriter};

#[cfg(test)]
mod tests {
    use super::{AuditLedger, Metrics, TraceWriter};

    /// The `Send + Sync` audit AGENTS.md asks for on shared state.
    ///
    /// All three are shared behind an `Arc` across request workers and the
    /// writer thread, so this is the property that makes that legal. A
    /// compile-time check is the only version that cannot rot: it fails the build
    /// rather than a test run someone can skip.
    #[test]
    fn shared_state_is_send_and_sync() {
        fn assert<T: Send + Sync>() {}
        assert::<Metrics>();
        assert::<AuditLedger>();
        assert::<TraceWriter>();
    }
}