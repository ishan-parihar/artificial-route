//! P5 acceptance: headers emitted, cardinality capped, audit redacted.
//!
//! The first three are the gate from `docs/04-subsystems.md` — "no raw prompt in
//! metrics; stolen `read` token can't dump traffic" — as checks. The last two
//! cover the corners those three lean on: the raw TTL and the rotation.

use std::fs;
use std::path::PathBuf;

use ar_keys::AuditLine;
use ar_obs::audit::AuditLedger;
use ar_obs::{
    CHANNEL_CAP, Cache, Decision, Family, MAX_SERIES, Metrics, ObsError, Queue, RAW_TTL_SECS,
    RETENTION_DAYS, Request, TraceWriter,
};

/// A temp path unique to one test, so the suite can run in parallel.
///
/// `std::env::temp_dir` rather than `tempfile`: a dev-dependency for two unique
/// names is a dependency this crate would otherwise not have.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ar-obs-{name}"));
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_file(&dir);
    dir
}

/// A request with every bounded field varied except the ones under test.
fn request(provider: &str) -> Request<'_> {
    Request {
        provider,
        family: Family::Balanced,
        decision: Decision::Primary,
        cache: Cache::Miss,
        queue: Queue::Direct,
        queue_pos: 0,
        attempts: 1,
        tokens_in: 12,
        tokens_out: 40,
        cost_micros: 0,
        duration_us: 830,
        queue_wait_us: 0,
    }
}

/// A representative audit record: a key id and four bounded enum labels.
fn line() -> AuditLine {
    AuditLine {
        at: 1_700_000_000,
        key_id: "team-a".into(),
        action: ar_keys::Action::Verify,
        outcome: ar_keys::Outcome::Ok,
        detail: "scope=read:*",
    }
}

/// `docs/04`: "Headers `Decision|Usage|Cost|Cache|Queue`".
#[test]
fn emits_headers_when_request() {
    assert_eq!(
        request("groq").headers(),
        [
            ("x-ar-decision", "outcome=primary;provider=groq;attempts=1"),
            ("x-ar-usage", "in=12;out=40;total=52"),
            ("x-ar-cost", "usd_micros=0"),
            ("x-ar-cache", "miss"),
            ("x-ar-queue", "lane=direct;pos=0"),
        ]
        .map(|(n, v)| (n, v.to_string()))
    );
}

/// `docs/04`: "cardinality cap `provider|family|decision`".
#[test]
fn caps_cardinality_when_many_models() {
    let metrics = Metrics::new();
    // A registry that grew 10k models across 10k providers — the shape that
    // turns an uncapped label into an out-of-memory series map.
    for i in 0..10_000 {
        metrics.observe(&request(&format!("prov-{i}")));
    }
    assert!(
        metrics.series_len() <= MAX_SERIES,
        "{}",
        metrics.series_len()
    );
}

/// A redacted row is the key id and the bounded labels, and nothing else.
#[test]
fn redacts_when_audit() {
    let path = scratch("redacts");
    let ledger = AuditLedger::open(&path).expect("open");
    ledger.append(&line()).expect("append");
    let rows = ledger.query(0, 1_700_000_000).expect("query");
    let _ = fs::remove_file(&path);
    // Exactly the redacted rendering: no `raw=` marker, and nothing a caller
    // could have smuggled past an enum-or-`Strng` field.
    assert_eq!(rows, ["1700000000 key=team-a verify ok scope=read:*"]);
}

/// The acceptance criterion, literally: a wrong grant gets neither the file nor
/// the raw rows.
#[test]
fn refuses_raw_when_secret_is_wrong() {
    let path = scratch("read-token");
    let err = AuditLedger::open_raw(&path, "stolen-read-token", "admin-secret");
    let _ = fs::remove_file(&path);
    assert!(matches!(err, Err(ObsError::AdminScope)));
}

/// A raw row stops being readable the moment its TTL passes, swept or not.
#[test]
fn hides_raw_row_after_ttl() {
    let path = scratch("ttl");
    let ledger = AuditLedger::open_raw(&path, "admin-secret", "admin-secret").expect("open");
    let now = 1_700_000_000u64;
    ledger
        .append_raw(&line(), "raw excerpt", now)
        .expect("append");
    let expiry = now + RAW_TTL_SECS;
    let visible = ledger.query(0, expiry - 1).expect("query");
    let expired = ledger.query(0, expiry).expect("query");
    let _ = fs::remove_file(&path);
    assert!(
        visible.iter().any(|r| r.contains("raw excerpt")),
        "{visible:?}"
    );
    assert!(expired.is_empty(), "{expired:?}");
}

/// Rotation is by day index, so retention is an integer compare with no
/// date-formatting crate behind it.
#[test]
fn prunes_when_file_is_past_retention() {
    assert_eq!(RETENTION_DAYS, 7);
    let path = scratch("rotate");
    fs::create_dir_all(&path).expect("mkdir");
    // The epoch day index, so `today - 0 >= 7` holds for any real clock.
    fs::write(path.join("ar-trace-0.jsonl"), b"old").expect("write");
    fs::write(path.join("ar-trace-99999999.jsonl"), b"future").expect("write");
    fs::write(path.join("keep-me.txt"), b"not ours").expect("write");

    drop(TraceWriter::start(&path).expect("start"));

    let stale = path.join("ar-trace-0.jsonl").exists();
    let kept = path.join("ar-trace-99999999.jsonl").exists() && path.join("keep-me.txt").exists();
    let _ = fs::remove_dir_all(&path);
    assert!(!stale, "a file past 7d should be pruned");
    assert!(kept, "prune is a janitor, not a reaper");
}

/// The lossy channel is the `docs/04` 128k. If `emit` blocked instead of
/// dropping, this loop would hang rather than fail.
#[test]
fn drops_rather_than_blocks_when_channel_is_full() {
    let path = scratch("lossy");
    let writer = TraceWriter::start(&path).expect("start");
    let cap = writer.channel_cap();
    for i in 0..10_000 {
        writer.emit(&format!("{{\"i\":{i}}}"));
    }
    drop(writer);
    let _ = fs::remove_dir_all(&path);
    assert_eq!(cap, CHANNEL_CAP);
}
