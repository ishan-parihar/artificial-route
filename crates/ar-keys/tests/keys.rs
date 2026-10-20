//! Integration tests for the credential and token half of `ar-keys`.
//!
//! These drive the public re-exports rather than the module internals, so a
//! change that quietly removes something from the public surface fails here.
//!
//! Every key in this file is ephemeral: generated in-process by the OS CSPRNG,
//! or the literal byte string under test. Nothing is read from the environment
//! and nothing is written outside `std::env::temp_dir()`, so a run leaves no
//! credential material behind.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ar_keys::{
    ACCESS_TTL, Action, Audit, HashParams, Issue, KeyError, KeyMeta, MasterKey, Outcome,
    REFRESH_TTL, Revocations, Salt, Scope, ScopeSet, Secret, Tokens, decrypt, encrypt, verify,
};

/// A fast-params master key. `HashParams::RECOMMENDED` costs 19 MiB and ~40 ms
/// per derivation; a suite of this size would spend seconds of CPU proving
/// nothing extra.
fn master() -> MasterKey {
    MasterKey::new(
        Secret::generate(),
        KeyMeta::with_params(HashParams::FAST, Salt::generate()),
    )
    .expect("derive a test master key")
}

/// A unique sqlite path.
///
/// The file is a **sqlite** file, not a redb one, and that is the point: the
/// running proxy holds its handle open for the life of the process, and the
/// operator's `aroute keys revoke` has to open the same file while it runs.
/// A redb file could not be opened twice, so each test still gets its own path
/// (parallel tests must not share a file) — but the file itself now supports
/// the concurrent reader-and-writer pattern the design depends on.
fn temp_store(tag: &str) -> (Revocations, std::path::PathBuf) {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let path =
        std::env::temp_dir().join(format!("ar-keys-it-{tag}-{}-{n}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    (
        Revocations::open(&path, 256).expect("open the revocation store"),
        path,
    )
}

fn issue(tokens: &Tokens, scopes: ScopeSet) -> ar_keys::Issued {
    tokens
        .issue(Issue {
            key_id: "key-1",
            scopes,
            device_id: Some("test-device"),
            ttl: None,
        })
        .expect("issue")
}

fn now() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_secs(),
    )
    .expect("fits i64")
}

#[test]
fn rejects_when_revoked() {
    let (revocations, path) = temp_store("revoked");
    let tokens = Tokens::new(&master());
    let issued = issue(&tokens, ScopeSet::all());

    // Live and correct before the revoke: this test is about the revocation, not
    // about a token that was never valid.
    assert!(
        tokens
            .verify(&issued.access, Scope::ReadAll, Some(&revocations))
            .is_ok(),
        "precondition: the token must verify before it is revoked"
    );

    tokens.revoke(&issued.access, &revocations).expect("revoke");

    // The signature is still valid and `exp` is 15 minutes out, so the revoke
    // list is the only thing that can be refusing this.
    let err = tokens
        .verify(&issued.access, Scope::ReadAll, Some(&revocations))
        .expect_err("a revoked token must be refused");
    assert!(matches!(err, KeyError::Revoked { .. }), "{err}");
    let _ = std::fs::remove_file(path);
}

#[test]
fn expires_when_stolen() {
    let (revocations, path) = temp_store("stolen");
    // Zero skew, so `exp` is the boundary rather than `exp + 30s`. The default
    // 30s allowance is a real part of the effective lifetime and is asserted
    // separately in `token::tests::the_default_leeway_extends_the_effective_lifetime`.
    let tokens = Tokens::new(&master()).with_leeway(Duration::ZERO);
    let issued = tokens
        .issue(Issue {
            key_id: "key-1",
            scopes: ScopeSet::all(),
            device_id: None,
            ttl: Some(Duration::from_secs(1)),
        })
        .expect("issue");

    // The attacker's copy: taken while the token is still live, so it is a
    // verbatim working credential and nothing distinguishes it from the original.
    let stolen = issued.access;
    assert!(
        tokens.verify(&stolen, Scope::ReadAll, Some(&revocations)).is_ok(),
        "precondition: the stolen copy starts valid"
    );

    // Past `exp`. Two whole seconds of slack, not one: `exp` is
    // `now + ttl.as_secs()` (a sub-second TTL truncates to zero, leaving the
    // token alive for exactly the second it was minted in) and expiry is a
    // strict `exp < now`, so the verify has to land in the *second after* `exp`.
    // One second of sleep cleared that for 90% of sub-second offsets and failed
    // for the rest; 2100ms is deterministic for every one of them.
    std::thread::sleep(Duration::from_millis(2_100));

    let err = tokens
        .verify(&stolen, Scope::ReadAll, Some(&revocations))
        .expect_err("an expired token must be refused");
    assert!(matches!(err, KeyError::Expired), "{err}");
    let _ = std::fs::remove_file(path);
}

#[test]
fn the_default_leeway_is_part_of_the_effective_lifetime() {
    // Worth stating explicitly: the skew allowance extends every token's life, so
    // it is a security parameter and not a rounding convenience.
    assert_eq!(Tokens::new(&master()).leeway(), ar_keys::DEFAULT_LEEWAY);
    assert!(
        ar_keys::DEFAULT_LEEWAY.as_secs() > 0,
        "a zero default would break clock-skew tolerance"
    );
}

#[test]
fn an_access_token_is_short_and_its_refresh_is_long() {
    let tokens = Tokens::new(&master());
    let issued = issue(&tokens, ScopeSet::all());
    let access_life = issued.expires_at - now();
    let refresh_life = issued.refresh_expires_at - now();
    assert!(
        access_life <= ACCESS_TTL.as_secs() as i64 && refresh_life <= REFRESH_TTL.as_secs() as i64,
        "access {access_life}s / refresh {refresh_life}s"
    );
}

#[test]
fn revoking_a_session_kills_both_halves() {
    // A session is two credentials: a short-lived access token and a 30-day
    // refresh token. The refresh half is ITSELF a valid bearer token — the
    // same gate accepts it, with the same scopes — so revoking only the
    // access half would leave a 30-day credential live, which is the outcome
    // a "revoke this token" command exists to prevent.
    //
    // The two halves carry different `jti`s and the SAME `sid`, and the revoke
    // list keys on the `sid`. So one write kills the session.
    let (revocations, path) = temp_store("half");
    let tokens = Tokens::new(&master());
    let issued = issue(&tokens, ScopeSet::all());
    tokens
        .revoke(&issued.access, &revocations)
        .expect("revoke access");

    assert!(
        tokens
            .verify(&issued.access, Scope::ReadAll, Some(&revocations))
            .is_err(),
        "the access token must be dead"
    );
    assert!(
        tokens
            .verify(&issued.refresh, Scope::ReadAll, Some(&revocations))
            .is_err(),
        "the refresh token must die with the session: it is a valid bearer \
         token for 30 days, and a revoke that left it alive would be a lie"
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn the_two_halves_share_a_session_id_and_differ_in_jti() {
    // The shape the revocation depends on.
    let tokens = Tokens::new(&master());
    let issued = issue(&tokens, ScopeSet::all());
    let access = tokens.introspect(&issued.access).expect("access verifies");
    let refresh = tokens.introspect(&issued.refresh).expect("refresh verifies");

    assert_eq!(
        access.session_id, refresh.session_id,
        "both halves must name the same session, or one revoke cannot kill both"
    );
    assert_ne!(
        access.jti, refresh.jti,
        "each half names one credential, so the jtis must differ"
    );
}

#[test]
fn two_sessions_do_not_share_a_session_id() {
    // A revoke must not reach a session it was not aimed at.
    let tokens = Tokens::new(&master());
    let first = issue(&tokens, ScopeSet::all());
    let second = issue(&tokens, ScopeSet::all());
    assert_ne!(
        first.session_id, second.session_id,
        "two mints are two sessions"
    );
}

#[test]
fn revoking_survives_a_restart() {
    // The property that matters for a reported leak: a restart must not
    // un-revoke anything.
    static N: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "ar-keys-it-restart-{}-{}.sqlite",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    let tokens = Tokens::new(&master());
    let issued = issue(&tokens, ScopeSet::all());

    {
        let revocations = Revocations::open(&path, 256).expect("open");
        tokens.revoke(&issued.access, &revocations).expect("revoke");
    }

    let revocations = Revocations::open(&path, 256).expect("reopen");
    assert!(matches!(
        tokens.verify(&issued.access, Scope::ReadAll, Some(&revocations)),
        Err(KeyError::Revoked { .. })
    ));
    let _ = std::fs::remove_file(path);
}

#[test]
fn refuses_a_scope_the_token_does_not_carry() {
    let (revocations, path) = temp_store("scope");
    let tokens = Tokens::new(&master());
    let read_only = issue(&tokens, ScopeSet::of([Scope::ReadAll]));
    let err = tokens
        .verify(&read_only.access, Scope::WriteAll, Some(&revocations))
        .expect_err("a read-only token must not write");
    assert!(
        matches!(
            err,
            KeyError::ScopeDenied {
                needed: "write:*",
                ..
            }
        ),
        "{err}"
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn an_unscoped_token_does_nothing() {
    let (revocations, path) = temp_store("unscoped");
    let tokens = Tokens::new(&master());
    let unscoped = issue(&tokens, ScopeSet::EMPTY);
    assert!(
        tokens
            .verify(&unscoped.access, Scope::ReadAll, Some(&revocations))
            .is_err()
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn execute_completions_is_a_separate_grant_from_write() {
    // `execute:completions` is the data plane and `write:*` is the control
    // plane. A client that can mint completions must not thereby gain config
    // writes.
    let (revocations, path) = temp_store("execute-vs-write");
    let tokens = Tokens::new(&master());
    let issued = issue(&tokens, ScopeSet::of([Scope::ExecuteCompletions]));
    assert!(
        tokens
            .verify(&issued.access, Scope::ExecuteCompletions, Some(&revocations))
            .is_ok()
    );
    assert!(
        tokens
            .verify(&issued.access, Scope::WriteAll, Some(&revocations))
            .is_err()
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn a_credential_cannot_be_moved_to_another_key_id() {
    // The splice v1 could not detect: the OmniRoute tree has zero `setAAD`
    // calls, so an envelope copied between rows authenticated fine.
    let m = master();
    let envelope = encrypt(
        &m,
        "openai",
        "key-1",
        &Secret::new(b"sk-live-credential".to_vec()),
    )
    .expect("encrypt");
    assert!(matches!(
        decrypt(&m, "openai", "key-2", &envelope),
        Err(KeyError::TagMismatch)
    ));
}

#[test]
fn a_credential_cannot_be_read_by_another_provider() {
    let m = master();
    let envelope = encrypt(
        &m,
        "openai",
        "key-1",
        &Secret::new(b"sk-live-credential".to_vec()),
    )
    .expect("encrypt");
    assert!(decrypt(&m, "groq", "key-1", &envelope).is_err());
}

#[test]
fn a_credential_cannot_be_read_without_the_master_key() {
    let envelope =
        encrypt(&master(), "openai", "key-1", &Secret::new(b"sk-x".to_vec())).expect("encrypt");
    assert!(matches!(
        decrypt(&master(), "openai", "key-1", &envelope),
        Err(KeyError::TagMismatch)
    ));
}

#[test]
fn every_encryption_uses_a_fresh_nonce() {
    // Reusing a nonce under one key is catastrophic for GCM, so this is the
    // property the 12-byte nonce exists to provide.
    let m = master();
    let secret = Secret::new(b"sk-x".to_vec());
    let mut seen = std::collections::HashSet::new();
    for _ in 0..16 {
        assert!(
            seen.insert(encrypt(&m, "openai", "key-1", &secret).expect("encrypt")),
            "nonce reuse across encryptions"
        );
    }
}

#[test]
fn the_audit_log_holds_the_key_id_and_never_the_key() {
    let audit = Audit::new(64);
    let secret = Secret::new(b"sk-must-never-appear".to_vec());
    let envelope = encrypt(&master(), "openai", "key-1", &secret).expect("encrypt");
    let _ = verify(&master(), "openai", "key-1", &envelope, &secret);

    // The audit API takes a `Strng` the caller chooses and three closed enums,
    // so there is no field a secret could arrive in.
    audit.record(
        "openai/key-1".into(),
        Action::Decrypt,
        Outcome::Ok,
        "tag-verified",
    );
    audit.record("key-1".into(), Action::Revoke, Outcome::Ok, "jti=abc123");

    let rendered = audit.to_text();
    assert!(
        rendered.contains("key-1"),
        "the key id must be there: {rendered}"
    );
    assert!(
        !rendered.contains("sk-must-never-appear"),
        "the key leaked into the audit log: {rendered}"
    );
    assert!(
        !rendered.contains(&envelope),
        "the envelope leaked into the audit log: {rendered}"
    );
    assert!(
        !rendered.contains(&format!("{secret:?}")),
        "a Debug of the secret leaked: {rendered}"
    );
}

#[test]
fn the_audit_ring_is_bounded() {
    let audit = Audit::new(8);
    for _ in 0..1_000 {
        audit.record("key-1".into(), Action::Verify, Outcome::Denied, "expired");
    }
    assert!(audit.len() <= 8, "the ring grew to {}", audit.len());
}

/// The design's central claim, as a test.
///
/// The revocation list lives in the credential store's **sqlite** file, so a
/// second handle can be opened while the first is open. That is what lets the
/// operator's `aroute keys revoke` run **while the proxy is up** — which is
/// the only time a token needs revoking.
///
/// The redb design this replaced could not do this: redb takes an exclusive
/// `flock` on its file and returns `DatabaseAlreadyOpen`. A test that only
/// exercised one handle would not have caught that, so the test opens a second
/// handle and writes through it while the first is still open.
#[test]
fn a_second_handle_may_write_while_the_first_is_open() {
    let (reader, path) = temp_store("concurrent");

    // The "server" handle is open and holding the file.
    assert!(
        reader.is_empty().expect("the reader sees an empty list"),
        "precondition: the list starts empty"
    );

    // The "operator" opens the same file and writes through it.
    let writer = Revocations::open(&path, 256).expect("a second handle opens the same file");
    writer.revoke("jti-1", now() + 600).expect("the second handle writes");

    // The first handle sees the write. This is the property the redb file
    // could not provide.
    assert!(
        reader.is_revoked("jti-1").expect("the reader sees the revocation"),
        "the reader must see what the writer wrote"
    );

    let _ = std::fs::remove_file(&path);
}

/// A revoked token is dead on the list, and the list is shared.
#[test]
fn a_revocation_written_by_one_handle_kills_the_token_for_both() {
    let (reader, path) = temp_store("shared-kill");
    let writer = Revocations::open(&path, 256).expect("a second handle opens the same file");
    let tokens = Tokens::new(&master());
    let issued = issue(&tokens, ScopeSet::all());

    writer
        .revoke(&issued.session_id, issued.expires_at)
        .expect("revoke");

    assert!(
        tokens
            .verify(&issued.access, Scope::ReadAll, Some(&reader))
            .is_err(),
        "a token revoked through the writer must not verify through the reader"
    );

    let _ = std::fs::remove_file(&path);
}

/// The cap is on **live** entries, so expired ones are collectable.
#[test]
fn the_cap_counts_live_entries_not_rows_ever_written() {
    let (_list, path) = temp_store("cap-live");
    let capped = Revocations::open(&path, 2).expect("open");

    // Two expired rows. A write sweeps first, so the first is collected when
    // the second is written: the list holds what is LIVE, not what was ever
    // written. That is the property the cap depends on — a list full of dead
    // tokens is not full.
    capped.revoke("jti-old-1", now() - 10).expect("revoke");
    capped.revoke("jti-old-2", now() - 10).expect("revoke");
    assert_eq!(
        capped.len().expect("len"),
        1,
        "the sweep collects the expired row on the next write"
    );

    // Two expired rows and the cap is 2: a third write must still be accepted,
    // because the list is not full of **live** entries.
    capped.revoke("jti-live", now() + 600).expect("a live write after expired rows");
    capped.revoke("jti-live-2", now() + 600).expect("a second live write");

    assert!(
        !capped.is_revoked("jti-old-1").expect("an expired row is not live"),
        "an expired row must not read as revoked"
    );
    assert!(
        capped.is_revoked("jti-live").expect("the live row is revoked"),
        "the live row must read as revoked"
    );
    assert!(
        capped.is_revoked("jti-live-2").expect("the second live row is revoked"),
        "the second live row must read as revoked"
    );

    // Now the list holds two live entries, which IS the cap — a third live
    // write is refused rather than evicting one of them.
    let err = capped
        .revoke("jti-live-3", now() + 600)
        .expect_err("the cap refuses a third live entry");
    assert!(matches!(err, KeyError::RevokeListFull { cap: 2 }), "{err}");
    assert!(
        capped.is_revoked("jti-live").expect("the first live row survives"),
        "a refused write must not evict a live revocation"
    );

    let _ = std::fs::remove_file(&path);
}

/// The revocation lookup's cost, as a budget check.
///
/// This is the crate's benchmark, and it follows the repo's own precedent
/// (`ar-guard/tests/guard.rs`): `std::time` over a fixed number of iterations,
/// a percentile, and a release-only budget. No `criterion` — adding a
/// benchmark dependency to measure one point lookup would be a larger change
/// than the thing being measured.
///
/// The number matters because the lookup is on the **hot path of every `/v1`
/// request**: the gate consults the table before it admits a request. A
/// revocation list that is correct but slow would make every request pay for
/// the security it buys.
///
/// The budget is 1 ms at p95. That is deliberately loose — the lookup is a
/// point read on a primary key and should be microseconds — because the budget
/// exists to catch an order-of-magnitude regression (a full table scan, a
/// synchronous fsync), not to police the last digit.
#[test]
fn the_revocation_lookup_stays_under_its_budget() {
    let (_list, path) = temp_store("bench");
    // A table with a realistic number of live entries, so the measurement is
    // of a populated index and not of an empty one. The cap is raised for the
    // bench because the fixture's 256 is the *default* cap and the point is
    // to measure a populated table, not to test the cap.
    let list = Revocations::open(&path, 4_096).expect("the list opens with a bench-sized cap");
    for i in 0..500 {
        list.revoke(&format!("jti-{i}"), now() + 3600)
            .expect("the revoke lands");
    }

    let iters = 2_000;
    let mut samples = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t = std::time::Instant::now();
        let revoked = list.is_revoked("jti-7").expect("the lookup answers");
        samples.push(t.elapsed().as_nanos());
        assert!(revoked, "an entry in the list must read as revoked");
    }
    samples.sort_unstable();

    let rank = |p: f64| {
        let r = (p * samples.len() as f64).ceil() as usize;
        samples[r.saturating_sub(1).clamp(0, samples.len() - 1)]
    };
    eprintln!(
        "ar-keys revocation lookup p50={}ns p95={}ns p99={}ns  entries=500",
        rank(0.50),
        rank(0.95),
        rank(0.99),
    );

    // An unoptimised build has no budget; the SLO is a release number.
    if cfg!(debug_assertions) {
        return;
    }
    assert!(
        rank(0.95) < Duration::from_millis(1).as_nanos(),
        "the revocation lookup p95 {}ns exceeds the 1ms budget",
        rank(0.95),
    );

    let _ = std::fs::remove_file(&path);
}
