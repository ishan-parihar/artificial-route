//! Integration tests for the lane admission controller.
//!
//! The headline test is [`protects_interactive_when_heavy_floods`]. It is worth
//! stating what it does and does not show: because each lane owns its own
//! semaphore, a heavy request cannot reach an interactive permit at all. The
//! test asserts that structural fact rather than a latency distribution, which
//! is why it can be deterministic — there is no scheduling race to be lucky
//! about.

use std::time::Duration;

use ar_keys::{AdmitError, Admission, Audit, Lane, LaneSpec};

/// A lane spec. `wait_ms` of 0 with `never_queue: false` gives a lane that
/// immediately times out, which keeps a test that is only about the fast path
/// from waiting at all.
fn spec(capacity: usize, queue_cap: usize, wait_ms: u64, never_queue: bool) -> LaneSpec {
    LaneSpec { capacity, queue_cap, wait: Duration::from_millis(wait_ms), never_queue }
}

/// interactive 8 + batch 4 + heavy 60. The other two lanes sum to 12, and a
/// fifth of a total that *includes* heavy solves to `12 / 4 = 3`, so heavy is
/// clamped from 60 to 3 and in-force capacity is 15. The clamp being the only
/// thing holding the guarantee is what makes this a test of the mechanism rather
/// than of the numbers.
fn clamped() -> Admission {
    Admission::new([spec(8, 64, 50, false), spec(4, 4, 0, true), spec(60, 1, 200, false)], 10_000)
        .expect("build admission")
}

/// Saturate `lane` and return the leases holding it.
async fn saturate(admission: &Admission, lane: Lane, key: &str) -> Vec<ar_keys::Lease> {
    let mut held = Vec::new();
    while held.len() < 256 {
        match admission.acquire(lane, key).await {
            Ok(lease) => held.push(lease),
            Err(_) => break,
        }
    }
    held
}

#[tokio::test]
async fn protects_interactive_when_heavy_floods() {
    let admission = clamped();
    assert_eq!(admission.total_capacity(), 15);

    let heavy = saturate(&admission, Lane::Heavy, "flood").await;
    assert_eq!(heavy.len(), 3, "heavy must saturate at its clamped 20% share");
    assert!(admission.capacity(Lane::Heavy) * 100 <= admission.total_capacity() * 20);

    // Every interactive permit is untouched, so this is served on the fast path
    // with no wait at all. If lanes shared a queue, this is where the 60 queued
    // heavy requests would be sitting.
    let interactive = admission.acquire(Lane::Interactive, "human").await.expect("interactive must be admitted");
    assert_eq!(interactive.waited(), Duration::ZERO);
}

#[tokio::test]
async fn heavy_load_does_not_delay_interactive_waits() {
    // The same guarantee with an interactive request that has to *wait* for its
    // own lane: heavy saturation must still leave the whole interactive lane
    // reachable, so only interactive-vs-interactive contention delays it.
    let admission = clamped();
    let _heavy = saturate(&admission, Lane::Heavy, "flood").await;

    let mut interactive = Vec::new();
    while interactive.len() < admission.capacity(Lane::Interactive) {
        interactive.push(admission.acquire(Lane::Interactive, "human").await.expect("interactive admitted"));
    }
    assert!(interactive.iter().all(|l| l.waited() == Duration::ZERO));
    assert_eq!(admission.in_flight(Lane::Heavy), 3);
}

#[tokio::test]
async fn sheds_with_429_worthy_retry_after_rather_than_growing() {
    // Batch never queues: it sheds the instant a permit is not free.
    let admission =
        Admission::new([spec(1, 1, 0, false), spec(1, 1, 0, true), spec(1, 1, 0, false)], 10_000).expect("build");
    let _held = admission.acquire(Lane::Batch, "c1").await.expect("first batch");

    let err = admission.acquire(Lane::Batch, "c2").await.expect_err("batch must shed");
    assert!(matches!(err, AdmitError::Busy { lane: Lane::Batch, .. }), "{err}");
    // A shed is a rate limit, so the caller can emit a Retry-After. Never zero.
    assert_eq!(err.retry_after(), Some(1));
}

#[tokio::test]
async fn a_full_queue_sheds_and_keeps_the_oldest_entry() {
    let admission = clamped();
    let _heavy = saturate(&admission, Lane::Heavy, "flood").await;

    let queued = tokio::spawn({
        let admission = admission.clone();
        async move { admission.acquire(Lane::Heavy, "queued").await }
    });
    // Let it reach the semaphore so it holds the lane's single queue slot.
    while admission.queued(Lane::Heavy) == 0 {
        tokio::task::yield_now().await;
    }

    let err = admission.acquire(Lane::Heavy, "late").await.expect_err("the queue slot is taken");
    queued.abort();
    assert!(matches!(err, AdmitError::QueueFull { position: 1, queue_cap: 1, .. }), "{err}");
    assert_eq!(err.retry_after(), Some(1), "Retry-After: 0 invites the storm");
}

#[tokio::test]
async fn a_waiter_is_served_when_a_permit_is_released() {
    let admission =
        Admission::new([spec(1, 4, 2_000, false), spec(1, 1, 0, true), spec(1, 1, 0, false)], 10_000).expect("build");
    let held = admission.acquire(Lane::Interactive, "c1").await.expect("hold the only permit");

    let waiter = tokio::spawn({
        let admission = admission.clone();
        async move { admission.acquire(Lane::Interactive, "c2").await }
    });
    while admission.queued(Lane::Interactive) == 0 {
        tokio::task::yield_now().await;
    }
    drop(held);

    let lease = waiter.await.expect("join").expect("served");
    assert!(lease.waited() > Duration::ZERO, "a queued waiter must have actually waited");
}

#[tokio::test]
async fn a_waiter_that_outlasts_its_budget_is_shed() {
    let admission =
        Admission::new([spec(1, 4, 20, false), spec(1, 1, 0, true), spec(1, 1, 0, false)], 10_000).expect("build");
    let _held = admission.acquire(Lane::Interactive, "c1").await.expect("hold");

    let err = admission.acquire(Lane::Interactive, "c2").await.expect_err("budget exhausted");
    assert!(matches!(err, AdmitError::QueueTimeout { wait_ms: 20, .. }), "{err}");
    assert_eq!(err.retry_after(), Some(1));
    // The slot is returned, so a later request is not queued behind a ghost.
    assert_eq!(admission.queued(Lane::Interactive), 0);
}

#[tokio::test]
async fn the_queue_is_bounded_so_a_flood_cannot_grow_memory() {
    // 4 waiters allowed, 32 concurrent attempts: the rest are shed rather than
    // accumulated.
    let admission =
        Admission::new([spec(1, 1, 5_000, false), spec(1, 1, 0, true), spec(1, 4, 5_000, false)], 10_000).expect("build");
    let _held = admission.acquire(Lane::Heavy, "holder").await.expect("hold the only heavy permit");

    let mut admissions = Vec::new();
    for i in 0..32 {
        let admission = admission.clone();
        admissions.push(tokio::spawn(async move { admission.acquire(Lane::Heavy, &format!("flood-{i}")).await }));
    }
    // Let every task reach the semaphore before reading the depth.
    while admission.queued(Lane::Heavy) < 4 {
        tokio::task::yield_now().await;
    }

    assert!(admission.queued(Lane::Heavy) <= 4, "the queue grew to {}", admission.queued(Lane::Heavy));
    for admission_task in admissions {
        admission_task.abort();
    }
}

#[tokio::test]
async fn rpm_leases_are_per_connection() {
    // 1 rpm: the second request from one connection is shed while a different
    // connection is unaffected.
    let admission =
        Admission::new([spec(8, 8, 0, false), spec(8, 8, 0, true), spec(8, 8, 0, false)], 1).expect("build");
    let _a = admission.acquire(Lane::Interactive, "c1").await.expect("c1 spends its token");
    assert!(matches!(admission.acquire(Lane::Interactive, "c1").await, Err(AdmitError::RateLimited { .. })));
    assert!(admission.acquire(Lane::Interactive, "c2").await.is_ok(), "c2 has its own bucket");
}

#[tokio::test]
async fn the_rpm_bucket_map_is_bounded() {
    // The bucket map is keyed on a caller-supplied id, so it is the one structure
    // here an unauthenticated peer could otherwise grow.
    let admission = Admission::with_options(
        [spec(8, 8, 0, false), spec(8, 8, 0, true), spec(8, 8, 0, false)],
        10_000,
        8,
        Duration::from_millis(1),
        None,
    )
    .expect("build");
    for i in 0..500 {
        let _ = admission.acquire(Lane::Interactive, &format!("conn-{i}")).await;
    }
    assert!(admission.leased_connections() <= 8, "bucket map grew to {}", admission.leased_connections());
}

#[tokio::test]
async fn admission_is_audited_by_lane_and_outcome() {
    let audit = std::sync::Arc::new(Audit::new(64));
    let admission = clamped().with_audit(audit.clone());
    let _held = admission.acquire(Lane::Interactive, "human").await.expect("admit");

    let rendered = audit.to_text();
    assert!(rendered.contains("interactive"), "the lane must be recorded: {rendered}");
    assert!(rendered.contains("admit"), "the action must be recorded: {rendered}");
}

#[test]
fn the_shipped_defaults_are_the_documented_ones() {
    let admission = Admission::defaults(600).expect("build");
    assert_eq!(admission.capacity(Lane::Interactive), 100, "docs/04: interactive q100");
    assert_eq!(admission.capacity(Lane::Batch), 50, "docs/04: mgmt q50");
    assert_eq!(admission.capacity(Lane::Heavy), 20, "docs/04: heavy q20");
}

#[test]
fn the_heavy_clamp_binds_only_when_configured_above_its_share() {
    // Defaults: other = 150, cap = 150 / 4 = 37, so the configured 20 stands.
    let defaults = Admission::defaults(600).expect("build");
    assert_eq!(defaults.capacity(Lane::Heavy), 20);
    // Misconfigured: heavy as large as the system, clamped back to a fifth.
    let greedy =
        Admission::new([spec(8, 8, 0, false), spec(2, 2, 0, true), spec(100, 1, 100, false)], 600).expect("build");
    assert_eq!(greedy.total_capacity(), 12, "8 + 2 + 2");
    assert_eq!(greedy.capacity(Lane::Heavy), 2, "other = 10, so other / 4 = 2");
}

#[test]
fn a_zero_rpm_limit_is_refused_at_construction() {
    assert!(matches!(Admission::defaults(0), Err(AdmitError::ZeroRpm)));
}
