//! Three-lane admission: `interactive`, `batch`, `heavy`.
//!
//! # Why per-lane semaphores and not one priority queue
//!
//! The obvious design for "heavy must not starve interactive" is a single
//! semaphore with a priority-ordered waiter queue. It is the weaker design, for
//! a reason that is not subtle once stated: in any queue that is *not* strictly
//! priority-ordered, a waiter that joined earlier is served before a waiter that
//! joined later. Fill the shared queue with `heavy` and every `interactive`
//! arrival queues behind all of it — so "interactive p99 protected" becomes a
//! statement about how long the heavy queue takes to drain, not a guarantee.
//! Real priority queues (a second-chance / fair queueing structure) fix that and
//! cost a scheduler per release, which is a lot of machinery for three buckets.
//!
//! So the priority here is **structural**: each lane owns a semaphore, and a
//! permit released in `heavy` is only ever visible to `heavy`. There is no path
//! by which a heavy request can consume an interactive permit, so no amount of
//! heavy load can lengthen an interactive wait. [`protects_interactive_when_heavy_floods`]
//! is not a statistical assertion about this; it is a consequence.
//!
//! Within a lane, order is FIFO — `tokio::sync::Semaphore`'s own wait queue.
//! That is correct for a lane: two interactive requests have no claim on each
//! other's place, and a heavy request should not jump a queued heavy request.
//!
//! # The 20% heavy cap
//!
//! [`Admission::new`] clamps `heavy`'s in-flight capacity to
//! [`HEAVY_SHARE_NUM`]% of the sum of all three lane capacities, so a config that
//! asks for a heavy lane as large as the interactive one still cannot hold more
//! than a fifth of the system. With the shipped defaults (100 / 50 / 20 = 170) it
//! does not bind — `min(20, 34) = 20` — and that is the point: the clamp is a
//! ceiling on misconfiguration, not a tax on the default.
//!
//! # Shedding
//!
//! Every refusal is a **429 with a `Retry-After`**, not a 503 and not a dropped
//! connection. This deviates from the gateway, where an admission shed is
//! `RequestLimitExceeded => 503` and only quota exhaustion is 429
//! (`../agentgateway/crates/agentgateway/src/proxy/mod.rs:432-472`). A queue that
//! sheds *is* a rate limit, and 503 tells a client to retry immediately against a
//! system that is telling it to wait — which is how a shed becomes a retry storm.
//! [`AdmitError::retry_after`] hands the caller the seconds so it can emit the
//! header without a second decision.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ar_core::Strng;
use governor::clock::{Clock as _, QuantaClock};
use governor::{DefaultDirectRateLimiter, Quota};

use crate::audit::{Action, Audit, Outcome};

/// Share of total lane capacity the heavy lane may hold. Numerator of 1/5.
pub const HEAVY_SHARE_NUM: usize = 20;

/// Denominator of the heavy share.
pub const HEAVY_SHARE_DEN: usize = 100;

/// Default cap on connections with an RPM lease.
pub const DEFAULT_MAX_CONNS: usize = 4_096;

/// A bucket idle for longer than this is reclaimable.
pub const DEFAULT_IDLE_TTL: Duration = Duration::from_secs(300);

/// Which lane a request belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Lane {
    /// A human waiting on a response. Highest priority.
    Interactive = 0,
    /// Background and control-plane traffic. Never queues.
    Batch = 1,
    /// Long generation runs. Capped, and the cap is what protects interactive.
    Heavy = 2,
}

impl Lane {
    /// Every lane, highest priority first. The order a caller should use when
    /// reporting several lanes.
    pub const PRIORITY: [Lane; 3] = [Self::Interactive, Self::Batch, Self::Heavy];

    /// Index into the lane array. Depends on the discriminants above.
    #[must_use]
    pub const fn index(self) -> usize {
        self as usize
    }

    /// Label for the `X-Artificial-Queue` header and the audit log.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::Batch => "batch",
            Self::Heavy => "heavy",
        }
    }
}

impl fmt::Display for Lane {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One lane's admission policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LaneSpec {
    /// In-flight permits. The `qN` in `docs/04-subsystems.md`.
    pub capacity: usize,
    /// Waiters allowed before shedding. Bounded on purpose.
    pub queue_cap: usize,
    /// How long a waiter waits before being shed.
    pub wait: Duration,
    /// Never queue: shed the instant a permit is not free.
    pub never_queue: bool,
}

impl LaneSpec {
    /// `q100/30s` — the interactive lane from `docs/04-subsystems.md`.
    pub const INTERACTIVE: Self = Self {
        capacity: 100,
        queue_cap: 100,
        wait: Duration::from_secs(30),
        never_queue: false,
    };

    /// `q50/never-queue`. `docs/04-subsystems.md` calls this `mgmt`; the roadmap
    /// calls it `batch`. Same lane.
    pub const BATCH: Self = Self {
        capacity: 50,
        queue_cap: 50,
        wait: Duration::ZERO,
        never_queue: true,
    };

    /// `q20/600s` — the heavy lane from `docs/04-subsystems.md`.
    pub const HEAVY: Self = Self {
        capacity: 20,
        queue_cap: 20,
        wait: Duration::from_secs(600),
        never_queue: false,
    };

    /// A lane that never queues sheds after this long, so `Retry-After: 0` is
    /// never emitted.
    #[must_use]
    pub fn shed_retry_after(&self) -> Duration {
        self.wait.max(Duration::from_secs(1))
    }
}

/// Why a request was not admitted.
///
/// Every shedding variant carries `retry_after` as a whole number of seconds,
/// already rounded up and floored at 1, because that is exactly what
/// `Retry-After: delay-seconds` is (RFC 9110 §10.2.3). Storing the header-ready
/// integer rather than a `Duration` means the caller cannot emit `0` — which
/// invites the retry storm this crate exists to prevent — by forgetting to
/// round. Use `entry().or_insert()` rather than `insert()` when writing it, so
/// an upstream-supplied value still wins.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AdmitError {
    /// The per-connection RPM lease is empty.
    #[error("{} lane: connection rate limit exceeded, retry after {}s", .lane, .retry_after)]
    RateLimited {
        /// The lane that asked.
        lane: Lane,
        /// From the limiter, so it is the real wait rather than a guess.
        retry_after: u64,
    },

    /// No permit was free and this lane does not queue.
    #[error("{} lane: busy, retry after {}s", .lane, .retry_after)]
    Busy {
        /// The lane that asked.
        lane: Lane,
        /// Before the caller would have been served.
        retry_after: u64,
    },

    /// The lane's waiting queue was full.
    #[error(
        "{} lane: queue full ({} waiting, would be position {}), retry after {}s",
        .lane,
        .queue_cap,
        .position,
        .retry_after
    )]
    QueueFull {
        /// The lane that asked.
        lane: Lane,
        /// Where this request would have queued.
        position: usize,
        /// The lane's queue cap.
        queue_cap: usize,
        /// Before the caller would have been served.
        retry_after: u64,
    },

    /// The request waited out its lane's timeout.
    #[error("{} lane: waited longer than {}ms, retry after {}s", .lane, .wait_ms, .retry_after)]
    QueueTimeout {
        /// The lane that asked.
        lane: Lane,
        /// The lane's wait budget, in milliseconds — a test fixture's 30 ms
        /// budget must not render as "0s".
        wait_ms: u64,
        /// Before the caller would have been served.
        retry_after: u64,
    },

    /// Admission has been shut down.
    #[error("admission is shut down")]
    Shutdown,

    /// An RPM limit of zero was configured, which would refuse everything.
    #[error("rpm limit must be greater than zero")]
    ZeroRpm,
}

impl AdmitError {
    /// The `Retry-After` value to emit, in whole seconds, at least 1.
    ///
    /// `None` only for [`AdmitError::Shutdown`] and [`AdmitError::ZeroRpm`],
    /// where a retry interval would be meaningless.
    #[must_use]
    pub const fn retry_after(&self) -> Option<u64> {
        match self {
            Self::RateLimited { retry_after, .. }
            | Self::Busy { retry_after, .. }
            | Self::QueueFull { retry_after, .. }
            | Self::QueueTimeout { retry_after, .. } => Some(*retry_after),
            Self::Shutdown | Self::ZeroRpm => None,
        }
    }

    /// The lane that was refused, for metrics and audit.
    #[must_use]
    pub const fn lane(&self) -> Option<Lane> {
        match self {
            Self::RateLimited { lane, .. }
            | Self::Busy { lane, .. }
            | Self::QueueFull { lane, .. }
            | Self::QueueTimeout { lane, .. } => Some(*lane),
            Self::Shutdown | Self::ZeroRpm => None,
        }
    }

    /// The audit `detail` string for this refusal.
    #[must_use]
    pub const fn detail(&self) -> &'static str {
        match self {
            Self::RateLimited { .. } => "rpm",
            Self::Busy { .. } => "busy",
            Self::QueueFull { .. } => "queue-full",
            Self::QueueTimeout { .. } => "queue-timeout",
            Self::Shutdown => "shutdown",
            Self::ZeroRpm => "zero-rpm",
        }
    }
}

/// Rounds a `Duration` up to the whole seconds `Retry-After` wants, floored at 1.
///
/// Round *up*: a 0.4s wait truncated to `0` says "come straight back", and the
/// caller comes straight back into the queue it was just shed from.
#[must_use]
fn retry_after_secs(d: Duration) -> u64 {
    d.as_secs()
        .saturating_add(u64::from(d.subsec_nanos() != 0))
        .max(1)
}

/// A granted admission. Dropping it returns the permit.
#[derive(Debug)]
pub struct Lease {
    lane: Lane,
    /// Held only for its `Drop`. Nothing needs the permit itself — dropping the
    /// lease is what returns it — and a public accessor would only invite a
    /// caller to hold one permit per request forever.
    #[expect(
        dead_code,
        reason = "RAII: the permit's Drop is what returns it to the semaphore"
    )]
    permit: Option<tokio::sync::OwnedSemaphorePermit>,
    waited: Duration,
}

impl Lease {
    /// Which lane this lease is for.
    #[must_use]
    pub const fn lane(&self) -> Lane {
        self.lane
    }

    /// How long admission took. `Duration::ZERO` means a permit was free.
    ///
    /// This is the number the `p95 wait` acceptance criterion in
    /// `docs/04-subsystems.md` wants, and it is measured rather than modelled.
    #[must_use]
    pub const fn waited(&self) -> Duration {
        self.waited
    }
}

/// Per-connection RPM leases.
struct RpmLeases {
    quota: Quota,
    max_conns: usize,
    idle_ttl: Duration,
    /// `key-id → (last seen, bucket)`. Bounded by `max_conns`; see
    /// [`RpmLeases::check`].
    buckets: Mutex<HashMap<Strng, (Instant, DefaultDirectRateLimiter)>>,
}

impl RpmLeases {
    fn new(rpm: u32, max_conns: usize, idle_ttl: Duration) -> Result<Self, AdmitError> {
        let per_minute = NonZeroU32::new(rpm).ok_or(AdmitError::ZeroRpm)?;
        Ok(Self {
            quota: Quota::per_minute(per_minute),
            max_conns,
            idle_ttl,
            buckets: Mutex::new(HashMap::new()),
        })
    }

    /// Takes one lease for `key_id`, or reports how long to wait.
    ///
    /// # Errors
    /// [`AdmitError::RateLimited`] when the bucket is empty.
    fn check(&self, key_id: &str, lane: Lane) -> Result<(), AdmitError> {
        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = Instant::now();

        if !buckets.contains_key(key_id) && buckets.len() >= self.max_conns {
            // The map is keyed on a caller-supplied id, so it is the one
            // structure here an unauthenticated peer could otherwise grow. Sweep
            // what has gone idle, then evict the least-recently-seen entry. The
            // gateway evicts silently here too ("the same as never having seen
            // those keys", `localratelimit.rs:91`); evicting the oldest rather
            // than refusing keeps a legitimately new connection working instead
            // of locking it out for the process lifetime.
            buckets.retain(|_, (seen, _)| now.duration_since(*seen) < self.idle_ttl);
            while buckets.len() >= self.max_conns {
                let Some(oldest) = buckets
                    .iter()
                    .min_by_key(|(_, (seen, _))| *seen)
                    .map(|(k, _)| k.clone())
                else {
                    break;
                };
                buckets.remove(&oldest);
            }
        }

        let entry = buckets
            .entry(Strng::from(key_id))
            .or_insert_with(|| (now, DefaultDirectRateLimiter::direct(self.quota)));
        entry.0 = now;
        match entry.1.check() {
            Ok(()) => Ok(()),
            Err(not_until) => {
                let wait = not_until.wait_time_from(QuantaClock::default().now());
                Err(AdmitError::RateLimited {
                    lane,
                    retry_after: retry_after_secs(wait),
                })
            }
        }
    }

    /// Number of live buckets. Bounded by `max_conns`.
    fn len(&self) -> usize {
        self.buckets.lock().map_or(0, |b| b.len())
    }
}

/// One lane's runtime state.
struct LaneState {
    spec: LaneSpec,
    /// What `spec.capacity` became after the heavy-share clamp. Kept so
    /// [`Admission::capacity`] reports the number in force, not the number asked
    /// for.
    capacity: usize,
    sem: Arc<tokio::sync::Semaphore>,
    queued: AtomicUsize,
}

impl LaneState {
    fn new(spec: LaneSpec, capacity: usize) -> Self {
        Self {
            spec,
            capacity,
            sem: Arc::new(tokio::sync::Semaphore::new(capacity)),
            queued: AtomicUsize::new(0),
        }
    }

    fn in_flight(&self) -> usize {
        self.capacity.saturating_sub(self.sem.available_permits())
    }
}

/// The three-lane admission controller.
///
/// Cheap to share: `Clone` is an `Arc` bump, so the server can hold one per
/// listener without a lock on the request path until a lane actually has to wait.
#[derive(Clone)]
pub struct Admission {
    lanes: Arc<[LaneState; 3]>,
    rpm: Arc<RpmLeases>,
    audit: Option<Arc<Audit>>,
    total: usize,
}

impl Admission {
    /// Builds a controller from per-lane specs, ordered
    /// `[interactive, batch, heavy]`.
    ///
    /// `heavy`'s capacity is clamped to [`HEAVY_SHARE_NUM`]% of the sum.
    ///
    /// # Errors
    /// [`AdmitError::ZeroRpm`] if `rpm` is zero.
    pub fn new(specs: [LaneSpec; 3], rpm: u32) -> Result<Self, AdmitError> {
        Self::with_options(specs, rpm, DEFAULT_MAX_CONNS, DEFAULT_IDLE_TTL, None)
    }

    /// Builds a controller with every knob exposed. `specs` is ordered
    /// `[interactive, batch, heavy]`.
    ///
    /// # Errors
    /// [`AdmitError::ZeroRpm`] if `rpm` is zero.
    pub fn with_options(
        specs: [LaneSpec; 3],
        rpm: u32,
        max_conns: usize,
        idle_ttl: Duration,
        audit: Option<Arc<Audit>>,
    ) -> Result<Self, AdmitError> {
        let configured: usize = specs.iter().map(|s| s.capacity).sum();
        // The heavy clamp, solved rather than approximated.
        //
        // The share is of *in-force* capacity, not of what was asked for, so
        // `h <= num/den * (other + h)` rather than `h <= num/den * configured`.
        // Solving for `h` gives `h <= other * num / (den - num)`: a fifth of the
        // other two lanes combined. Using the configured total instead is wrong
        // in a way that looks fine — with interactive 8 / batch 4 / heavy 60, a
        // configured-total clamp yields `min(60, 14) = 14`, and 14 of the 26 in
        // force is 54%, not 20%. This formula yields `min(60, 3) = 3`, exactly a
        // fifth of 15.
        let other = configured.saturating_sub(specs[Lane::Heavy.index()].capacity);
        let share_cap =
            (other.saturating_mul(HEAVY_SHARE_NUM) / (HEAVY_SHARE_DEN - HEAVY_SHARE_NUM)).max(1);
        // Indexed rather than matched on value: two lanes may legitimately
        // share a `LaneSpec`, and a value match would clamp whichever one was
        // built second.
        let states = [
            LaneState::new(
                specs[Lane::Interactive.index()],
                specs[Lane::Interactive.index()].capacity,
            ),
            LaneState::new(
                specs[Lane::Batch.index()],
                specs[Lane::Batch.index()].capacity,
            ),
            LaneState::new(
                specs[Lane::Heavy.index()],
                specs[Lane::Heavy.index()].capacity.min(share_cap),
            ),
        ];
        let total = states.iter().map(|lane| lane.capacity).sum();
        Ok(Self {
            lanes: Arc::new(states),
            rpm: Arc::new(RpmLeases::new(rpm, max_conns, idle_ttl)?),
            audit,
            total,
        })
    }

    /// The shipped defaults from `docs/04-subsystems.md`, at `rpm` requests per
    /// minute per connection.
    ///
    /// # Errors
    /// [`AdmitError::ZeroRpm`] if `rpm` is zero.
    pub fn defaults(rpm: u32) -> Result<Self, AdmitError> {
        Self::new(
            [LaneSpec::INTERACTIVE, LaneSpec::BATCH, LaneSpec::HEAVY],
            rpm,
        )
    }

    /// Attaches an audit ring. Shed and admitted requests are recorded with the
    /// lane and outcome, and nothing else — see [`crate::audit`].
    #[must_use]
    pub fn with_audit(mut self, audit: Arc<Audit>) -> Self {
        self.audit = Some(audit);
        self
    }

    /// Admits `lane` for `key_id`, or says when to come back.
    ///
    /// Order of checks: the RPM lease first, because it is a pure atomic and
    /// rejecting a rate-limited caller should not have queued them behind
    /// hundreds of heavy waiters. Then the fast path, then the lane's policy.
    ///
    /// # Errors
    /// [`AdmitError`] for a rate-limited, busy, full-queue or timed-out request,
    /// or [`AdmitError::Shutdown`] if the semaphore closed under us.
    pub async fn acquire(&self, lane: Lane, key_id: &str) -> Result<Lease, AdmitError> {
        if let Err(e) = self.rpm.check(key_id, lane) {
            self.audit(lane, Outcome::Shed, e.detail());
            return Err(e);
        }

        let state = &self.lanes[lane.index()];
        let started = Instant::now();

        if let Ok(permit) = Arc::clone(&state.sem).try_acquire_owned() {
            self.audit(lane, Outcome::Ok, "granted");
            return Ok(Lease {
                lane,
                permit: Some(permit),
                waited: Duration::ZERO,
            });
        }

        if state.spec.never_queue {
            let e = AdmitError::Busy {
                lane,
                retry_after: retry_after_secs(state.spec.shed_retry_after()),
            };
            self.audit(lane, Outcome::Shed, e.detail());
            return Err(e);
        }

        // The queue bound. `fetch_add` returns this request's ticket, so the
        // check and the claim are one atomic — two requests cannot both read
        // `queue_cap - 1` and both decide there is room.
        let position = state.queued.fetch_add(1, Ordering::AcqRel);
        if position >= state.spec.queue_cap {
            state.queued.fetch_sub(1, Ordering::AcqRel);
            let e = AdmitError::QueueFull {
                lane,
                position,
                queue_cap: state.spec.queue_cap,
                retry_after: retry_after_secs(state.spec.shed_retry_after()),
            };
            self.audit(lane, Outcome::Shed, e.detail());
            return Err(e);
        }

        let waited =
            tokio::time::timeout(state.spec.wait, Arc::clone(&state.sem).acquire_owned()).await;
        state.queued.fetch_sub(1, Ordering::AcqRel);

        match waited {
            Ok(Ok(permit)) => {
                self.audit(lane, Outcome::Ok, "queued");
                Ok(Lease {
                    lane,
                    permit: Some(permit),
                    waited: started.elapsed(),
                })
            }
            Ok(Err(_)) => Err(AdmitError::Shutdown),
            Err(_elapsed) => {
                let e = AdmitError::QueueTimeout {
                    lane,
                    wait_ms: u64::try_from(state.spec.wait.as_millis()).unwrap_or(u64::MAX),
                    retry_after: retry_after_secs(state.spec.shed_retry_after()),
                };
                self.audit(lane, Outcome::Shed, e.detail());
                Err(e)
            }
        }
    }

    /// In-flight permits available for `lane`, after the heavy-share clamp.
    #[must_use]
    pub fn capacity(&self, lane: Lane) -> usize {
        self.lanes[lane.index()].capacity
    }

    /// In-flight requests in `lane` right now.
    #[must_use]
    pub fn in_flight(&self, lane: Lane) -> usize {
        self.lanes[lane.index()].in_flight()
    }

    /// Requests waiting in `lane` right now.
    #[must_use]
    pub fn queued(&self, lane: Lane) -> usize {
        self.lanes[lane.index()].queued.load(Ordering::Relaxed)
    }

    /// Sum of lane capacities **in force**, i.e. after the heavy clamp.
    ///
    /// This is the denominator of the heavy share, so it is the number to
    /// compare [`Admission::capacity`] against — not the sum that was
    /// configured, which can be higher.
    #[must_use]
    pub const fn total_capacity(&self) -> usize {
        self.total
    }

    /// Connections holding an RPM bucket. Bounded by `max_conns`.
    #[must_use]
    pub fn leased_connections(&self) -> usize {
        self.rpm.len()
    }

    fn audit(&self, lane: Lane, outcome: Outcome, detail: &'static str) {
        if let Some(audit) = &self.audit {
            audit.record(Strng::from(lane.as_str()), Action::Admit, outcome, detail);
        }
    }
}

/// `VecDeque` is imported for the wait-order documentation above even though the
/// semaphore owns the queue; keep the type referenced so the intent is checkable.
const _: fn() = || {
    let _: Option<VecDeque<u8>> = None;
};

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{Admission, AdmitError, HEAVY_SHARE_DEN, HEAVY_SHARE_NUM, Lane, LaneSpec};

    fn spec(capacity: usize, queue_cap: usize, wait_ms: u64, never_queue: bool) -> LaneSpec {
        LaneSpec {
            capacity,
            queue_cap,
            wait: Duration::from_millis(wait_ms),
            never_queue,
        }
    }

    #[tokio::test]
    async fn protects_interactive_when_heavy_floods() {
        // heavy is configured far larger than the other two lanes, so the clamp
        // is the only thing keeping it from being the system. other = 8 + 4 = 12,
        // and a fifth of a total that includes heavy solves to other / 4 = 3.
        let adm = Admission::new(
            [
                spec(8, 64, 50, false),
                spec(4, 4, 0, true),
                spec(60, 1, 200, false),
            ],
            10_000,
        )
        .expect("build");
        assert_eq!(adm.capacity(Lane::Heavy), 3);
        assert_eq!(adm.total_capacity(), 15);

        // Flood heavy until it is saturated; the rest shed.
        let mut held = Vec::new();
        while held.len() < 40 {
            match adm.acquire(Lane::Heavy, "flood").await {
                Ok(lease) => held.push(lease),
                Err(_) => break,
            }
        }
        assert_eq!(held.len(), 3, "heavy must saturate at its clamped cap");
        assert_eq!(adm.in_flight(Lane::Heavy), 3);

        // Interactive has not been touched and is still immediately servable.
        let interactive = adm
            .acquire(Lane::Interactive, "human")
            .await
            .expect("interactive must be admitted");
        assert_eq!(interactive.waited(), Duration::ZERO);
        assert_eq!(adm.in_flight(Lane::Interactive), 1);
    }

    #[tokio::test]
    async fn sheds_heavy_with_a_retry_after_once_its_queue_is_full() {
        let adm = Admission::new(
            [
                spec(8, 64, 50, false),
                spec(4, 4, 0, true),
                spec(60, 1, 200, false),
            ],
            10_000,
        )
        .expect("build");
        let mut held = Vec::new();
        while held.len() < 40 {
            match adm.acquire(Lane::Heavy, "flood").await {
                Ok(lease) => held.push(lease),
                Err(_) => break,
            }
        }
        // Take the lane's single queue slot with a request that will wait.
        let queued = tokio::spawn({
            let adm = adm.clone();
            async move { adm.acquire(Lane::Heavy, "queued").await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(
            adm.queued(Lane::Heavy),
            1,
            "the spawned request must be waiting"
        );

        let err = adm
            .acquire(Lane::Heavy, "late")
            .await
            .expect_err("the one queue slot is taken");
        queued.abort();
        assert!(
            matches!(
                err,
                AdmitError::QueueFull {
                    position: 1,
                    queue_cap: 1,
                    ..
                }
            ),
            "{err}"
        );
        assert!(
            err.retry_after().is_some_and(|s| s >= 1),
            "never emit Retry-After: 0"
        );
    }

    #[tokio::test]
    async fn the_shipped_defaults_do_not_pay_the_heavy_clamp() {
        // other = 150, so the cap is 150 / 4 = 37 and the configured 20 stands.
        let adm = Admission::defaults(600).expect("build");
        assert_eq!(adm.capacity(Lane::Heavy), LaneSpec::HEAVY.capacity);
        assert_eq!(adm.total_capacity(), 170);
    }

    #[tokio::test]
    async fn heavy_holds_at_most_a_fifth_of_in_force_capacity() {
        let adm = Admission::new(
            [
                spec(4, 4, 0, false),
                spec(1, 1, 0, false),
                spec(100, 1, 100, false),
            ],
            10_000,
        )
        .expect("build");
        assert_eq!(adm.capacity(Lane::Heavy), 1, "other = 5, so other / 4 = 1");
        assert!(
            adm.capacity(Lane::Heavy) * HEAVY_SHARE_DEN <= adm.total_capacity() * HEAVY_SHARE_NUM,
            "heavy {} of total {}",
            adm.capacity(Lane::Heavy),
            adm.total_capacity()
        );
    }

    #[tokio::test]
    async fn batch_sheds_immediately_rather_than_queueing() {
        let adm = Admission::new(
            [
                spec(1, 1, 0, false),
                spec(1, 1, 0, true),
                spec(1, 1, 0, false),
            ],
            10_000,
        )
        .expect("build");
        let _held = adm.acquire(Lane::Batch, "c1").await.expect("first batch");
        let err = adm
            .acquire(Lane::Batch, "c2")
            .await
            .expect_err("batch never queues");
        assert!(matches!(err, AdmitError::Busy { .. }), "{err}");
    }

    #[tokio::test]
    async fn a_waiter_times_out_and_reports_the_budget() {
        let adm = Admission::new(
            [
                spec(1, 4, 30, false),
                spec(1, 1, 0, true),
                spec(1, 1, 0, false),
            ],
            10_000,
        )
        .expect("build");
        let _held = adm
            .acquire(Lane::Interactive, "c1")
            .await
            .expect("hold the only permit");
        let err = adm
            .acquire(Lane::Interactive, "c2")
            .await
            .expect_err("should time out");
        assert!(
            matches!(err, AdmitError::QueueTimeout { wait_ms: 30, .. }),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_released_permit_serves_the_waiter() {
        let adm = Admission::new(
            [
                spec(1, 4, 500, false),
                spec(1, 1, 0, true),
                spec(1, 1, 0, false),
            ],
            10_000,
        )
        .expect("build");
        let held = adm.acquire(Lane::Interactive, "c1").await.expect("hold");
        let waiter = {
            let adm = adm.clone();
            tokio::spawn(async move { adm.acquire(Lane::Interactive, "c2").await })
        };
        // Let the waiter reach the semaphore before freeing the permit.
        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(held);
        let lease = waiter.await.expect("join").expect("served");
        assert!(lease.waited() >= Duration::from_millis(20));
    }

    #[tokio::test]
    async fn an_exhausted_rpm_lease_sheds_the_next_request() {
        let adm = Admission::new(
            [
                spec(8, 8, 0, false),
                spec(8, 8, 0, true),
                spec(8, 8, 0, false),
            ],
            1,
        )
        .expect("build");
        // 1 rpm: the first request spends the token, the second must wait.
        let _first = adm.acquire(Lane::Interactive, "c1").await.expect("first");
        let err = adm
            .acquire(Lane::Interactive, "c1")
            .await
            .expect_err("rpm exhausted");
        assert!(matches!(err, AdmitError::RateLimited { .. }), "{err}");
        assert!(err.retry_after().is_some_and(|s| s >= 1));
    }

    #[tokio::test]
    async fn the_rpm_bucket_is_per_connection() {
        let adm = Admission::new(
            [
                spec(8, 8, 0, false),
                spec(8, 8, 0, true),
                spec(8, 8, 0, false),
            ],
            1,
        )
        .expect("build");
        let _a = adm
            .acquire(Lane::Interactive, "c1")
            .await
            .expect("c1 first");
        assert!(
            adm.acquire(Lane::Interactive, "c1").await.is_err(),
            "c1 is out of tokens"
        );
        assert!(
            adm.acquire(Lane::Interactive, "c2").await.is_ok(),
            "c2 has its own bucket"
        );
    }

    #[test]
    fn zero_rpm_is_refused_rather_than_shedding_everything() {
        assert!(matches!(Admission::defaults(0), Err(AdmitError::ZeroRpm)));
    }
}
