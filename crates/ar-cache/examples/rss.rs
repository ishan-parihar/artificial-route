//! Re-measures the memory tier's real footprint as it fills past its ceiling.
//!
//! The byte cap in `CacheConfig` is a claim; this is the check that turns it into
//! a number. `evicts_when_full` in `tests/cache_contract.rs` proves the tier
//! reports a weight at or under its capacity, which is the *mechanism*. This
//! proves the process's `VmRSS` stops growing, which is the thing `docs/00`
//! budgets.
//!
//! ```sh
//! cargo run -p ar-cache --release --example rss
//! ```
//!
//! `provisional:` re-measure on the target machine. The absolute figure moves
//! with allocator arena sizing; the *shape* -- flat after the first couple of
//! rounds -- is the property.
//!
//! Measured on the P1 build (release, `lto = true`, `codegen-units = 1`),
//! 8000 distinct 64 KB bodies (512 MB pushed through a 32 MB cache):
//!
//! ```text
//! baseline_vmRSS_kb=2568 mem_weight=0
//! round=10 entries=384 weight=25208832 capacity=33554432 vmRSS_kb=27592
//! round=20 entries=384 weight=25208832 capacity=33554432 vmRSS_kb=27828
//! round=30 entries=384 weight=25208832 capacity=33554432 vmRSS_kb=27920
//! round=40 entries=384 weight=25208832 capacity=33554432 vmRSS_kb=27924
//! ```
//!
//! Two things to read out of that. `VmRSS` is flat from round 10 onward after
//! 4x more data has gone through, and the settled `weight` is ~75% of the
//! nominal `capacity` -- `quick_cache` shards internally and evicts per shard,
//! so each shard reserves its own headroom and the aggregate lands below the
//! total. Both are expected: the ceiling is a ceiling, not a fill target.

use ar_cache::{Cache, CacheConfig, CacheKey};

/// `VmRSS` in kB, from `/proc/self/status`. `0` off Linux.
fn vmrss_kb() -> u64 {
    let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
        return 0;
    };
    status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

/// Distinct 64 KB bodies per round.
const PER_ROUND: u64 = 200;
const ROUNDS: u64 = 40;

fn main() {
    let cache =
        Cache::with_config(CacheConfig::memory_only().with_mem_bytes(ar_cache::DEFAULT_MEM_BYTES))
            .expect("memory-only cache");

    println!(
        "baseline_vmRSS_kb={} mem_weight={}",
        vmrss_kb(),
        cache.mem().weight()
    );

    let payload = "x".repeat(64 * 1024);
    for round in 1..=ROUNDS {
        for i in 0..PER_ROUND {
            let key = CacheKey::hash(format!("{round}-{i}").as_bytes());
            cache.store(
                &key,
                200,
                "application/json",
                bytes::Bytes::copy_from_slice(payload.as_bytes()),
            );
        }
        if round % 10 == 0 {
            println!(
                "round={round} entries={} weight={} capacity={} vmRSS_kb={}",
                cache.mem().len(),
                cache.mem().weight(),
                cache.mem().capacity(),
                vmrss_kb(),
            );
        }
    }

    let weight = cache.mem().weight();
    assert!(
        weight <= cache.mem().capacity(),
        "memory tier held {weight} bytes, over its {} byte ceiling",
        cache.mem().capacity(),
    );
    println!("final weight={weight} vmRSS_kb={}", vmrss_kb());
}
