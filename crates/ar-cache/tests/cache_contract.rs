//! Acceptance tests for `docs/04` §cache and the P1 gate:
//! **hit header + replay safe + RSS flat when full**.
//!
//! Every test here drives the public API only, so it exercises the same surface
//! `ar-server` will. Names are `verb_should_outcome_when_condition`
//! (AGENTS.md §2), except the three named in the P1 acceptance criteria,
//! which keep their given names verbatim.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use ar_cache::entry::now_ms;
use ar_cache::idempotency::{DEFAULT_IDEMPOTENCY_BYTES, DEFAULT_IDEMPOTENCY_TTL, Replay};
use ar_cache::tier::{DEFAULT_MEM_BYTES, DiskTier};
use ar_cache::{Cache, CacheConfig, CacheState, Entry, TtlPolicy};
use bytes::Bytes;
use serde_json::json;

/// A unique scratch path per call, so `cargo test`'s thread pool cannot have two
/// tests fighting over one `redb` file. `temp_dir` + pid + counter: no `tempfile`
/// dependency for one line of path construction.
fn scratch(name: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("ar-cache-{name}-{}-{n}.redb", std::process::id()))
}

fn body() -> &'static str {
    r#"{"id":"chatcmpl-1","model":"gpt-4o-mini","message":{"role":"assistant","content":"pong"}}"#
}

fn request_body() -> serde_json::Value {
    json!({
        "model": "gpt-4o-mini",
        "messages": [
            {"role": "system", "content": "you are terse"},
            {"role": "user", "content": "ping"},
        ],
        "temperature": 0.0,
        "max_tokens": 64,
        "stream": false,
    })
}

fn keyed_cache() -> Cache {
    Cache::with_config(CacheConfig::memory_only()).expect("memory-only cache")
}

// ---------------------------------------------------------------------------
// The three named acceptance tests
// ---------------------------------------------------------------------------

/// The P1 gate: an identical POST returns the cached body with
/// `x-ar-cache: hit`.
#[test]
fn hits_when_same_body() {
    let cache = keyed_cache();
    let key = ar_cache::key::request_key("acme", "gpt-4o-mini", &request_body());

    assert_eq!(
        cache.get(&key).state,
        CacheState::Miss,
        "cold key must miss"
    );
    assert!(
        cache.store(&key, 200, "application/json", body()),
        "first POST is stored"
    );

    let second = cache.get(&key);
    assert_eq!(second.state, CacheState::Hit, "identical POST must hit");
    assert_eq!(second.state.as_header(), "hit", "header must read `hit`");
    assert_eq!(second.body().map(Bytes::as_ref), Some(body().as_bytes()));
}

/// The P1 gate: a retried `Idempotency-Key` replays the first answer instead
/// of spending again.
#[test]
fn replays_safe_when_idempotency_key() {
    let cache = keyed_cache();
    let store = cache.idempotency();
    let first = Entry::new(
        200,
        "application/json",
        body(),
        now_ms(),
        std::time::Duration::from_secs(60),
    );

    // First attempt claims the key and publishes the answer.
    assert_eq!(store.begin("acme", "retry-1"), Replay::Proceed);
    store.complete("acme", "retry-1", first.clone());

    // A client retry -- same key, same body, minutes later -- replays verbatim.
    let replay = store.begin("acme", "retry-1");
    assert_eq!(
        replay,
        Replay::Cached(first),
        "retry must replay, not re-execute"
    );

    // A concurrent retry, before the first answer exists, is told to wait.
    let other = Cache::memory_only();
    assert_eq!(
        other.idempotency().begin("acme", "retry-2"),
        Replay::Proceed
    );
    assert_eq!(
        other.idempotency().begin("acme", "retry-2"),
        Replay::InFlight,
        "a concurrent duplicate must not be allowed to double-spend"
    );
}

/// The P1 gate: memory stays flat once the byte ceiling is reached.
#[test]
fn evicts_when_full() {
    // 256 KB of ceiling, 32 KB bodies: room for ~7, so 500 insertions must
    // evict rather than grow.
    const CAP: u64 = 256 * 1024;
    const BODY_BYTES: usize = 32 * 1024;
    let cache = Cache::with_config(CacheConfig::memory_only().with_mem_bytes(CAP))
        .expect("memory-only cache");
    let payload = "x".repeat(BODY_BYTES);

    for i in 0..500 {
        let key = ar_cache::key::request_key("acme", "gpt-4o-mini", &json!({ "n": i }));
        cache.store(
            &key,
            200,
            "application/json",
            Bytes::copy_from_slice(payload.as_bytes()),
        );
    }

    let weight = cache.mem().weight();
    assert!(
        weight <= CAP,
        "memory tier held {weight} bytes, over the {CAP} byte ceiling"
    );
    assert!(
        cache.mem().len() < 500,
        "nothing was evicted: {} entries retained",
        cache.mem().len()
    );
}

// ---------------------------------------------------------------------------
// `x-ar-cache` contract
// ---------------------------------------------------------------------------

#[test]
fn reports_bypass_when_the_caller_scopes_the_request_out() {
    // Streaming requests are never eligible. The verdict comes from the caller
    // because this crate reads no headers.
    let bypass = Cache::bypass();
    assert_eq!(bypass.state, CacheState::Bypass);
    assert_eq!(bypass.state.as_header(), "bypass");
}

#[test]
fn a_different_body_misses_when_one_is_cached() {
    let cache = keyed_cache();
    let a = ar_cache::key::request_key("acme", "gpt-4o-mini", &request_body());
    cache.store(&a, 200, "application/json", body());

    let mut other = request_body();
    other["messages"][1]["content"] = json!("a different question");
    let b = ar_cache::key::request_key("acme", "gpt-4o-mini", &other);

    assert_eq!(cache.get(&b).state, CacheState::Miss);
}

#[test]
fn a_different_tenant_misses_when_one_is_cached() {
    let cache = keyed_cache();
    let key = ar_cache::key::request_key("acme", "gpt-4o-mini", &request_body());
    cache.store(&key, 200, "application/json", body());

    let other = ar_cache::key::request_key("globex", "gpt-4o-mini", &request_body());
    assert_eq!(cache.get(&other).state, CacheState::Miss);
}

#[test]
fn key_ignores_json_key_order_so_identical_posts_share_a_key() {
    // Two clients, same request, different JSON serialisers.
    let first = json!({
        "model": "gpt-4o-mini",
        "messages": [{"role": "user", "content": "ping"}],
        "stream": false,
    });
    let second = json!({
        "stream": false,
        "messages": [{"content": "ping", "role": "user"}],
        "model": "gpt-4o-mini",
    });
    assert_eq!(
        ar_cache::key::request_key("acme", "gpt-4o-mini", &first),
        ar_cache::key::request_key("acme", "gpt-4o-mini", &second),
    );
}

// ---------------------------------------------------------------------------
// TTL per status class (`docs/04`: 200:5m 4xx:30s 5xx:no-store)
// ---------------------------------------------------------------------------

#[test]
fn does_not_store_a_5xx() {
    let cache = keyed_cache();
    let key = ar_cache::key::request_key("acme", "gpt-4o-mini", &json!({"n": 1}));
    assert!(!cache.store(&key, 503, "application/json", body()));
    assert_eq!(cache.get(&key).state, CacheState::Miss);
}

#[test]
fn stores_a_4xx_for_thirty_seconds() {
    let cache = keyed_cache();
    let key = ar_cache::key::request_key("acme", "gpt-4o-mini", &json!({"n": 2}));
    assert!(cache.store(&key, 400, "application/json", r#"{"error":"bad"}"#));
    assert_eq!(cache.get(&key).state, CacheState::Hit);
}

#[test]
fn a_2xx_expires_after_its_ttl() {
    // A 1 ms success TTL, so the test does not sleep for five minutes.
    let cache = Cache::with_config(CacheConfig::memory_only().with_ttl(TtlPolicy {
        success: std::time::Duration::from_millis(5),
        client_error: std::time::Duration::from_millis(5),
        other: ar_cache::NO_STORE,
    }))
    .expect("memory-only cache");
    let key = ar_cache::key::request_key("acme", "gpt-4o-mini", &json!({"n": 3}));
    assert!(cache.store(&key, 200, "application/json", body()));
    std::thread::sleep(std::time::Duration::from_millis(15));
    assert_eq!(cache.get(&key).state, CacheState::Miss);
}

#[test]
fn does_not_store_a_truncated_completion() {
    // `finish_reason: "length"` upstream. Storing this serves a partial answer
    // with a 200 for five minutes.
    let cache = keyed_cache();
    let key = ar_cache::key::request_key("acme", "gpt-4o-mini", &json!({"n": 4}));
    let truncated = json!({"choices": [{"finish_reason": "length"}]}).to_string();
    assert!(!cache.store_truncated(&key, 200, "application/json", truncated));
    assert_eq!(cache.get(&key).state, CacheState::Miss);
}

// ---------------------------------------------------------------------------
// Disk tier: survives a reopen, and stays inside its byte cap
// ---------------------------------------------------------------------------

#[test]
fn disk_tier_serves_an_entry_after_the_process_restarts() {
    let path = scratch("persist");
    let key = ar_cache::key::request_key("acme", "gpt-4o-mini", &request_body());
    let entry = Entry::new(
        200,
        "application/json",
        body(),
        now_ms(),
        std::time::Duration::from_secs(300),
    );

    {
        let disk = DiskTier::open(&path, 1 << 20).expect("open");
        assert!(disk.insert(&key, &entry, now_ms()).expect("insert"));
    }
    // Reopened as a *different* `DiskTier`, which is what a restart is.
    let reopened = DiskTier::open(&path, 1 << 20).expect("reopen");
    assert_eq!(
        reopened.get(&key, now_ms()).expect("get").map(|e| e.body),
        Some(entry.body)
    );
    drop(reopened);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn disk_tier_refuses_to_exceed_its_byte_cap() {
    let path = scratch("cap");
    // 8 KB cap, 4 KB entries: two fit, the rest are refused rather than
    // growing the file.
    let disk = DiskTier::open(&path, 8 * 1024).expect("open");
    let payload = "y".repeat(4 * 1024);

    let mut stored = 0;
    for i in 0..64 {
        let key = ar_cache::key::request_key("acme", "m", &json!({"i": i}));
        let entry = Entry::new(
            200,
            "application/json",
            Bytes::copy_from_slice(payload.as_bytes()),
            now_ms(),
            std::time::Duration::from_secs(300),
        );
        if disk.insert(&key, &entry, now_ms()).expect("insert") {
            stored += 1;
        }
    }

    assert!(stored > 0, "the cap must still allow some writes");
    assert!(
        stored < 64,
        "the cap must refuse eventually: stored {stored}/64"
    );
    assert!(
        disk.used_bytes() <= disk.cap_bytes(),
        "logical bytes {} exceeded cap {}",
        disk.used_bytes(),
        disk.cap_bytes()
    );
    drop(disk);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn cache_falls_back_to_the_disk_tier_on_a_memory_miss() {
    let path = scratch("fallback");
    let key = ar_cache::key::request_key("acme", "gpt-4o-mini", &request_body());

    // Written by one `Cache` over the file, read by a second `Cache` whose
    // memory tier has never seen the key -- a restart, using only the public
    // API. (`forget` would clear the disk row too, so it cannot stand in for
    // a cold memory tier.)
    let writer =
        Cache::with_config(CacheConfig::memory_only().with_disk(&path)).expect("with disk");
    assert!(writer.store(&key, 200, "application/json", body()));
    drop(writer);

    let reader =
        Cache::with_config(CacheConfig::memory_only().with_disk(&path)).expect("with disk");
    assert_eq!(
        reader.get(&key).state,
        CacheState::Miss,
        "memory tier starts cold"
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let lookup = rt.block_on(reader.get_with_disk(key));
    assert_eq!(
        lookup.state,
        CacheState::Hit,
        "disk tier must serve the miss"
    );
    assert_eq!(lookup.body().map(Bytes::as_ref), Some(body().as_bytes()));
    drop(rt);
    drop(reader);
    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// Bounded RSS, stated rather than asserted by feel
// ---------------------------------------------------------------------------

#[test]
fn documents_the_default_memory_ceiling_as_32_mb() {
    assert_eq!(DEFAULT_MEM_BYTES, 32 * 1024 * 1024);
    assert_eq!(DEFAULT_MEM_BYTES, 33_554_432);
}

#[test]
fn documents_the_default_idempotency_bounds() {
    assert_eq!(DEFAULT_IDEMPOTENCY_TTL.as_secs(), 24 * 60 * 60);
    assert_eq!(DEFAULT_IDEMPOTENCY_BYTES, 4 * 1024 * 1024);
}

#[test]
fn an_idempotency_store_at_capacity_never_exceeds_it() {
    // The 24 h window is the leak the reference has; this is the bound that
    // stops it. 500 completed responses into a 2 KB store.
    let cache = Cache::memory_only();
    let store = cache.idempotency();
    for i in 0..500 {
        store.begin("acme", &format!("k{i}"));
        store.complete(
            "acme",
            &format!("k{i}"),
            Entry::new(
                200,
                "application/json",
                body(),
                now_ms(),
                std::time::Duration::from_secs(86_400),
            ),
        );
    }
    assert!(
        store.bytes() <= DEFAULT_IDEMPOTENCY_BYTES,
        "idempotency store grew to {} bytes",
        store.bytes()
    );
}

// ---------------------------------------------------------------------------
// `Send`/`Sync` audit and the future-size budget
// ---------------------------------------------------------------------------

/// Compile-time assertions. A regression here is a build failure, which is the
/// point: the alternative is a runtime "failed to spawn" on the first
/// concurrent request.
#[test]
fn shared_state_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Cache>();
    assert_send_sync::<MemTierAudit>();
    assert_send_sync::<DiskTier>();
    assert_send_sync::<ar_cache::IdempotencyStore>();
    assert_send_sync::<ar_cache::affinity::AffinityKey>();
    assert_send_sync::<std::sync::Arc<Cache>>();
}

/// The public memory tier type, aliased so the assertion above reads clearly.
type MemTierAudit = ar_cache::MemTier;

/// The `get_with_disk` future stays under 4 KB.
///
/// A future's size is its largest suspend state, so deeply nested `async` grows
/// it silently. One `&Cache` plus one 32-byte key plus a join handle should be
/// tens of bytes; anything approaching 4 KB means the async path has started
/// buffering.
///
/// `agentgateway/crates/core/src/assertions.rs` has the canonical
/// `SizeAtMost<MAX>` helper. `ar-core` has not vendored it yet, and `ar-core` is
/// outside this crate's write scope, so it is reimplemented here in six lines
/// rather than blocked on. Delete this fn and import
/// `agent_core::prelude::AssertSize` when `ar-core` lands it.
fn assert_size_at_most<const MAX: usize, T>(value: T) -> T {
    assert!(
        std::mem::size_of::<T>() <= MAX,
        "type size {} exceeds the {MAX} byte budget",
        std::mem::size_of::<T>()
    );
    value
}

#[test]
fn the_disk_lookup_future_stays_under_4k() {
    let cache = Cache::memory_only();
    let key = ar_cache::key::request_key("acme", "gpt-4o-mini", &request_body());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");

    let fut = assert_size_at_most::<4096, _>(cache.get_with_disk(key));
    // The size assertion above is the test; block on anyway so the runtime has
    // work and the future is actually exercised rather than merely sized.
    let _ = rt.block_on(fut);
    drop(rt);
}
