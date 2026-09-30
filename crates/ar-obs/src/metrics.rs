//! Prometheus counters and histograms, with a hard cardinality cap.
//!
//! `docs/03` lists the `prometheus` client crate and this still does not need
//! it, for the reason `ar-server`'s exposition gives: the client adds a registry,
//! a label-validation layer and a protobuf dependency to print text a bounded
//! counter set renders in a dozen `write!`s, against the "minimal-RAM" budget in
//! `docs/00`. The cap is this crate's job either way -- a client validates label
//! cardinality, it does not bound it.
//!
//! Cardinality is `provider|family|decision` per `docs/04`, and it is structural
//! rather than reviewed: [`Request`] has one free-form field (`provider`, a
//! registry id) and every other field is an enum or an integer, so there is
//! nothing here a caller could fill with a model name or a prompt. Past
//! [`MAX_SERIES`] a new label triple collapses into one `other` series.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

/// Hard ceiling on distinct series.
pub const MAX_SERIES: usize = 512;

/// Bucket that every over-cap label triple collapses into.
const OTHER: &str = "other";

/// Fixed duration buckets in microseconds. Fixed so the exposition shape is
/// stable: a Prometheus counter that renames buckets is one that resets.
const DURATION_US: [u64; 8] = [
    1_000, 5_000, 25_000, 100_000, 500_000, 2_000_000, 30_000_000, 120_000_000,
];
/// Fixed queue-wait buckets. The zero bound is real: a request that never
/// entered a lane is a different case from a shed one.
const QUEUE_WAIT_US: [u64; 7] = [0, 1_000, 10_000, 50_000, 250_000, 1_000_000, 5_000_000];
/// Widest bucket set plus the `+Inf` slot.
const MAX_BUCKETS: usize = DURATION_US.len() + 1;

const DECISIONS: [&str; 6] = ["primary", "failover", "retry", "reject", "defer", OTHER];
const FAMILIES: [&str; 5] = ["frontier", "balanced", "economy", "unpriced", OTHER];
const CACHES: [&str; 4] = ["hit", "miss", "stale", "bypass"];
const QUEUES: [&str; 4] = ["direct", "admitted", "waiting", "shedding"];

/// The routing decision that produced a response.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Decision {
    /// The first choice served it.
    Primary = 0,
    /// Another provider served it after the first failed.
    Failover = 1,
    /// The same provider was retried after a 429.
    Retry = 2,
    /// Refused upstream of any provider call.
    Reject = 3,
    /// The named strategy is not implemented yet.
    Defer = 4,
    /// Over the cardinality cap.
    Other = 5,
}

/// Model family, bucketed. Deliberately not a model name: a family is a
/// price/latency class, so adding a model cannot add a series.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Family {
    /// Top of the catalog.
    Frontier = 0,
    /// The default middle.
    Balanced = 1,
    /// Cheapest known tier.
    Economy = 2,
    /// No known price.
    Unpriced = 3,
    /// Over the cardinality cap.
    Other = 4,
}

/// Cache disposition for the request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Cache {
    /// Served from memory or disk.
    Hit = 0,
    /// No entry; forwarded upstream.
    Miss = 1,
    /// Served stale while revalidating.
    Stale = 2,
    /// Bypass requested.
    Bypass = 3,
}

/// Lane admission outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Queue {
    /// No lane layer in the path.
    Direct = 0,
    /// Admitted at position 0.
    Admitted = 1,
    /// Waited for a lease.
    Waiting = 2,
    /// Shed for capacity.
    Shedding = 3,
}

/// `as_str` from a label table, shared by all four enums.
///
/// `self as usize` is the discriminant, which is why every variant carries an
/// explicit value and `repr(u8)` is pinned: the index and the label can then
/// never drift apart, the way a hand-written `match` can.
macro_rules! labels {
    ($t:ty, $table:expr) => {
        impl $t {
            /// Label as it appears in the exposition and the headers.
            #[must_use]
            pub fn as_str(self) -> &'static str {
                $table[self as usize]
            }
        }
    };
}

labels!(Decision, DECISIONS);
labels!(Family, FAMILIES);
labels!(Cache, CACHES);
labels!(Queue, QUEUES);

/// The bounded facts one request contributes.
///
/// Every field is an enum or an integer except `provider`, a registry id. That
/// is what makes "no raw prompt in metrics" a property of the type rather than
/// something a reviewer has to notice.
#[derive(Clone, Copy, Debug)]
pub struct Request<'a> {
    /// Registry id of the provider that served the request.
    pub provider: &'a str,
    /// The model's price/latency class.
    pub family: Family,
    /// The routing decision.
    pub decision: Decision,
    /// Cache disposition.
    pub cache: Cache,
    /// Lane admission outcome.
    pub queue: Queue,
    /// Position in the lane queue at admission.
    pub queue_pos: u16,
    /// Provider attempts, including the successful one.
    pub attempts: u16,
    /// Prompt tokens consumed.
    pub tokens_in: u64,
    /// Completion tokens produced.
    pub tokens_out: u64,
    /// Cost in millionths of a USD. A flat-rate deployment reports `0` here and
    /// still reports full quota usage.
    pub cost_micros: u64,
    /// Total request duration.
    pub duration_us: u64,
    /// Time spent waiting for a lane lease.
    pub queue_wait_us: u64,
}

impl Request<'_> {
    /// The five headers `docs/04` names: `Decision | Usage | Cost | Cache |
    /// Queue`.
    ///
    /// Values come only from the enum labels and integers above. The provider
    /// appears because it is a registry id the caller already puts in a response
    /// header; nothing else about the request does.
    #[must_use]
    pub fn headers(&self) -> [(&'static str, String); 5] {
        [
            (
                "x-ar-decision",
                format!(
                    "outcome={};provider={};attempts={}",
                    self.decision.as_str(),
                    self.provider,
                    self.attempts
                ),
            ),
            (
                "x-ar-usage",
                format!(
                    "in={};out={};total={}",
                    self.tokens_in,
                    self.tokens_out,
                    self.tokens_in.saturating_add(self.tokens_out)
                ),
            ),
            ("x-ar-cost", format!("usd_micros={}", self.cost_micros)),
            ("x-ar-cache", self.cache.as_str().to_string()),
            (
                "x-ar-queue",
                format!("lane={};pos={}", self.queue.as_str(), self.queue_pos),
            ),
        ]
    }
}

/// Series identity. [`MAX_SERIES`] bounds the whole map, not each dimension.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Key {
    provider: Box<str>,
    family: Family,
    decision: Decision,
}

/// Per-series counters. Cache and queue are deliberately *not* per-series: they
/// describe the process, and multiplying them across series is exactly the
/// cardinality blow-up the cap exists to prevent.
#[derive(Clone, Copy, Debug, Default)]
struct Cell {
    requests: u64,
    tokens_in: u64,
    tokens_out: u64,
    cost_micros: u64,
}

/// A fixed-bucket histogram. A fixed array, not a `Vec`, so `new` stays const
/// and a histogram costs no allocation.
#[derive(Debug)]
struct Histogram {
    bounds: &'static [u64],
    counts: [u64; MAX_BUCKETS],
    sum: u64,
    total: u64,
}

impl Histogram {
    const fn new(bounds: &'static [u64]) -> Self {
        Self {
            bounds,
            counts: [0; MAX_BUCKETS],
            sum: 0,
            total: 0,
        }
    }

    fn observe(&mut self, v: u64) {
        let mut idx = self.bounds.len();
        for (i, bound) in self.bounds.iter().enumerate() {
            if v <= *bound {
                idx = i;
                break;
            }
        }
        if let Some(slot) = self.counts.get_mut(idx) {
            *slot = slot.saturating_add(1);
        }
        self.sum = self.sum.saturating_add(v);
        self.total = self.total.saturating_add(1);
    }

    fn render(&self, out: &mut String, name: &str, help: &str) {
        let _ = writeln!(out, "# HELP {name} {help}");
        let _ = writeln!(out, "# TYPE {name} histogram");
        // Buckets are cumulative, so a scrape reads a running total.
        let mut run = 0u64;
        for (bound, count) in self.bounds.iter().zip(&self.counts) {
            run = run.saturating_add(*count);
            let _ = writeln!(out, "{name}_bucket{{le=\"{bound}\"}} {run}");
        }
        let _ = writeln!(out, "{name}_bucket{{le=\"+Inf\"}} {}", self.total);
        let _ = writeln!(out, "{name}_sum {}", self.sum);
        let _ = writeln!(out, "{name}_count {}", self.total);
    }
}

/// Recovers a poisoned lock instead of propagating it.
///
/// A panic mid-`observe` leaves the counters readable and the exposition
/// well-formed; failing every later scrape over one bad observation is worse
/// than a slightly stale number.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The metric set. Cheap to share behind an `Arc`.
#[derive(Debug)]
pub struct Metrics {
    series: Mutex<HashMap<Key, Cell>>,
    cache: [AtomicU64; CACHES.len()],
    queue: [AtomicU64; QUEUES.len()],
    duration_us: Mutex<Histogram>,
    queue_wait_us: Mutex<Histogram>,
}

impl Metrics {
    /// Builds a zeroed metric set.
    #[must_use]
    pub fn new() -> Self {
        Self {
            series: Mutex::new(HashMap::new()),
            cache: Default::default(),
            queue: Default::default(),
            duration_us: Mutex::new(Histogram::new(&DURATION_US)),
            queue_wait_us: Mutex::new(Histogram::new(&QUEUE_WAIT_US)),
        }
    }

    /// Records one served request.
    pub fn observe(&self, r: &Request<'_>) {
        let want = Key {
            provider: Box::from(r.provider),
            family: r.family,
            decision: r.decision,
        };
        let mut s = lock(&self.series);
        // `MAX_SERIES - 1` reserves the overflow slot, so the cap holds once the
        // map is full. Re-observing a known series is always allowed: the cap is
        // on distinct series, not on writes.
        let key = if s.contains_key(&want) || s.len() < MAX_SERIES - 1 {
            want
        } else {
            Key {
                provider: Box::from(OTHER),
                family: Family::Other,
                decision: Decision::Other,
            }
        };
        let c = s.entry(key).or_default();
        c.requests = c.requests.saturating_add(1);
        c.tokens_in = c.tokens_in.saturating_add(r.tokens_in);
        c.tokens_out = c.tokens_out.saturating_add(r.tokens_out);
        c.cost_micros = c.cost_micros.saturating_add(r.cost_micros);
        drop(s);

        // `as usize` is the discriminant and the arrays are sized to match, so
        // the clamp is belt-and-braces against a future variant without a label.
        self.cache[(r.cache as usize).min(CACHES.len() - 1)].fetch_add(1, Ordering::Relaxed);
        self.queue[(r.queue as usize).min(QUEUES.len() - 1)].fetch_add(1, Ordering::Relaxed);
        lock(&self.duration_us).observe(r.duration_us);
        lock(&self.queue_wait_us).observe(r.queue_wait_us);
    }

    /// Distinct series currently held, the number [`MAX_SERIES`] bounds.
    #[must_use]
    pub fn series_len(&self) -> usize {
        lock(&self.series).len()
    }

    /// Renders the Prometheus text exposition (`text/plain; version=0.0.4`).
    ///
    /// Into a `String`: the document is a few hundred bytes, and a `Vec` of
    /// `io::Write` adapters is not a simplification.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(4096);
        // Every HELP/TYPE block precedes any sample: a scraper reads TYPE as a
        // declaration of what follows, not a summary of what came before.
        for (name, help) in [
            ("ar_requests_total", "Requests, by provider|family|decision."),
            ("ar_tokens_in_total", "Prompt tokens, by provider|family|decision."),
            (
                "ar_tokens_out_total",
                "Completion tokens, by provider|family|decision.",
            ),
            (
                "ar_cost_usd_micros_total",
                "Cost in millionths of a USD, by provider|family|decision.",
            ),
            ("ar_cache_events_total", "Cache dispositions."),
            ("ar_queue_events_total", "Lane admission outcomes."),
        ] {
            let _ = writeln!(out, "# HELP {name} {help}");
            let _ = writeln!(out, "# TYPE {name} counter");
        }
        for (key, c) in lock(&self.series).iter() {
            let labels = format!(
                "provider=\"{}\",family=\"{}\",decision=\"{}\"",
                key.provider,
                key.family.as_str(),
                key.decision.as_str()
            );
            for (name, value) in [
                ("ar_requests_total", c.requests),
                ("ar_tokens_in_total", c.tokens_in),
                ("ar_tokens_out_total", c.tokens_out),
                ("ar_cost_usd_micros_total", c.cost_micros),
            ] {
                let _ = writeln!(out, "{name}{{{labels}}} {value}");
            }
        }
        // Cache and queue are single-label counters: one loop over both tables.
        for (name, dim, labels, counts) in [
            ("ar_cache_events_total", "cache", CACHES, &self.cache),
            ("ar_queue_events_total", "lane", QUEUES, &self.queue),
        ] {
            for (label, count) in labels.iter().zip(counts) {
                let _ = writeln!(
                    out,
                    "{name}{{{dim}=\"{label}\"}} {}",
                    count.load(Ordering::Relaxed)
                );
            }
        }
        lock(&self.duration_us).render(
            &mut out,
            "ar_request_duration_us",
            "Request duration in microseconds.",
        );
        lock(&self.queue_wait_us)
            .render(&mut out, "ar_queue_wait_us", "Lane wait in microseconds.");
        out
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}