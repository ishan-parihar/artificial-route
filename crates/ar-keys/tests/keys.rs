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
    ACCESS_TTL, Action, Audit, HashParams, Issue, KeyError, KeyMeta, MasterKey, Outcome, REFRESH_TTL, Revocation, Salt,
    Scope, ScopeSet, Secret, Tokens, decrypt, encrypt, verify,
};

/// A fast-params master key. `HashParams::RECOMMENDED` costs 19 MiB and ~40 ms
/// per derivation; a suite of this size would spend seconds of CPU proving
/// nothing extra.
fn master() -> MasterKey {
    MasterKey::new(Secret::generate(), KeyMeta::with_params(HashParams::FAST, Salt::generate()))
        .expect("derive a test master key")
}

/// A unique redb path. redb takes an exclusive lock, so parallel tests must not
/// share a file.
fn temp_store(tag: &str) -> (Revocation, std::path::PathBuf) {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("ar-keys-it-{tag}-{}-{n}.redb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    (Revocation::open(&path, 256).expect("open the revocation store"), path)
}

fn issue(tokens: &Tokens, scopes: ScopeSet) -> ar_keys::Issued {
    tokens.issue(Issue { key_id: "key-1", scopes, device_id: Some("test-device"), ttl: None }).expect("issue")
}

fn now() -> i64 {
    i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH).expect("clock").as_secs()).expect("fits i64")
}

#[test]
fn rejects_when_revoked() {
    let (revocations, path) = temp_store("revoked");
    let tokens = Tokens::new(&master());
    let issued = issue(&tokens, ScopeSet::all());

    // Live and correct before the revoke: this test is about the revocation, not
    // about a token that was never valid.
    assert!(
        tokens.verify(&issued.access, Scope::ReadAll, &revocations).is_ok(),
        "precondition: the token must verify before it is revoked"
    );

    tokens.revoke(&issued.access, &revocations).expect("revoke");

    // The signature is still valid and `exp` is 15 minutes out, so the revoke
    // list is the only thing that can be refusing this.
    let err = tokens.verify(&issued.access, Scope::ReadAll, &revocations).expect_err("a revoked token must be refused");
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
        .issue(Issue { key_id: "key-1", scopes: ScopeSet::all(), device_id: None, ttl: Some(Duration::from_secs(1)) })
        .expect("issue");

    // The attacker's copy: taken while the token is still live, so it is a
    // verbatim working credential and nothing distinguishes it from the original.
    let stolen = issued.access;
    assert!(
        tokens.verify(&stolen, Scope::ReadAll, &revocations).is_ok(),
        "precondition: the stolen copy starts valid"
    );

    // Past `exp`. Two whole seconds of slack, not one: `exp` is
    // `now + ttl.as_secs()` (a sub-second TTL truncates to zero, leaving the
    // token alive for exactly the second it was minted in) and expiry is a
    // strict `exp < now`, so the verify has to land in the *second after* `exp`.
    // One second of sleep cleared that for 90% of sub-second offsets and failed
    // for the rest; 2100ms is deterministic for every one of them.
    std::thread::sleep(Duration::from_millis(2_100));

    let err = tokens.verify(&stolen, Scope::ReadAll, &revocations).expect_err("an expired token must be refused");
    assert!(matches!(err, KeyError::Expired), "{err}");
    let _ = std::fs::remove_file(path);
}

#[test]
fn the_default_leeway_is_part_of_the_effective_lifetime() {
    // Worth stating explicitly: the skew allowance extends every token's life, so
    // it is a security parameter and not a rounding convenience.
    assert_eq!(Tokens::new(&master()).leeway(), ar_keys::DEFAULT_LEEWAY);
    assert!(ar_keys::DEFAULT_LEEWAY.as_secs() > 0, "a zero default would break clock-skew tolerance");
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
fn revoking_one_half_does_not_revoke_the_other() {
    let (revocations, path) = temp_store("half");
    let tokens = Tokens::new(&master());
    let issued = issue(&tokens, ScopeSet::all());
    tokens.revoke(&issued.access, &revocations).expect("revoke access");
    assert!(tokens.verify(&issued.refresh, Scope::ReadAll, &revocations).is_ok(), "the refresh token must survive");
    let _ = std::fs::remove_file(path);
}

#[test]
fn revoking_survives_a_restart() {
    // The property that matters for a reported leak: a restart must not
    // un-revoke anything.
    static N: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir()
        .join(format!("ar-keys-it-restart-{}-{}.redb", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    let _ = std::fs::remove_file(&path);
    let tokens = Tokens::new(&master());
    let issued = issue(&tokens, ScopeSet::all());

    {
        let revocations = Revocation::open(&path, 256).expect("open");
        tokens.revoke(&issued.access, &revocations).expect("revoke");
    }

    let revocations = Revocation::open(&path, 256).expect("reopen");
    assert!(matches!(
        tokens.verify(&issued.access, Scope::ReadAll, &revocations),
        Err(KeyError::Revoked { .. })
    ));
    let _ = std::fs::remove_file(path);
}

#[test]
fn refuses_a_scope_the_token_does_not_carry() {
    let (revocations, path) = temp_store("scope");
    let tokens = Tokens::new(&master());
    let read_only = issue(&tokens, ScopeSet::of([Scope::ReadAll]));
    let err = tokens.verify(&read_only.access, Scope::WriteAll, &revocations).expect_err("a read-only token must not write");
    assert!(matches!(err, KeyError::ScopeDenied { needed: "write:*", .. }), "{err}");
    let _ = std::fs::remove_file(path);
}

#[test]
fn an_unscoped_token_does_nothing() {
    let (revocations, path) = temp_store("unscoped");
    let tokens = Tokens::new(&master());
    let unscoped = issue(&tokens, ScopeSet::EMPTY);
    assert!(tokens.verify(&unscoped.access, Scope::ReadAll, &revocations).is_err());
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
    assert!(tokens.verify(&issued.access, Scope::ExecuteCompletions, &revocations).is_ok());
    assert!(tokens.verify(&issued.access, Scope::WriteAll, &revocations).is_err());
    let _ = std::fs::remove_file(path);
}

#[test]
fn a_credential_cannot_be_moved_to_another_key_id() {
    // The splice v1 could not detect: the OmniRoute tree has zero `setAAD`
    // calls, so an envelope copied between rows authenticated fine.
    let m = master();
    let envelope = encrypt(&m, "openai", "key-1", &Secret::new(b"sk-live-credential".to_vec())).expect("encrypt");
    assert!(matches!(decrypt(&m, "openai", "key-2", &envelope), Err(KeyError::TagMismatch)));
}

#[test]
fn a_credential_cannot_be_read_by_another_provider() {
    let m = master();
    let envelope = encrypt(&m, "openai", "key-1", &Secret::new(b"sk-live-credential".to_vec())).expect("encrypt");
    assert!(decrypt(&m, "groq", "key-1", &envelope).is_err());
}

#[test]
fn a_credential_cannot_be_read_without_the_master_key() {
    let envelope = encrypt(&master(), "openai", "key-1", &Secret::new(b"sk-x".to_vec())).expect("encrypt");
    assert!(matches!(decrypt(&master(), "openai", "key-1", &envelope), Err(KeyError::TagMismatch)));
}

#[test]
fn every_encryption_uses_a_fresh_nonce() {
    // Reusing a nonce under one key is catastrophic for GCM, so this is the
    // property the 12-byte nonce exists to provide.
    let m = master();
    let secret = Secret::new(b"sk-x".to_vec());
    let mut seen = std::collections::HashSet::new();
    for _ in 0..16 {
        assert!(seen.insert(encrypt(&m, "openai", "key-1", &secret).expect("encrypt")), "nonce reuse across encryptions");
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
    audit.record("openai/key-1".into(), Action::Decrypt, Outcome::Ok, "tag-verified");
    audit.record("key-1".into(), Action::Revoke, Outcome::Ok, "jti=abc123");

    let rendered = audit.to_text();
    assert!(rendered.contains("key-1"), "the key id must be there: {rendered}");
    assert!(!rendered.contains("sk-must-never-appear"), "the key leaked into the audit log: {rendered}");
    assert!(!rendered.contains(&envelope), "the envelope leaked into the audit log: {rendered}");
    assert!(!rendered.contains(&format!("{secret:?}")), "a Debug of the secret leaked: {rendered}");
}

#[test]
fn the_audit_ring_is_bounded() {
    let audit = Audit::new(8);
    for _ in 0..1_000 {
        audit.record("key-1".into(), Action::Verify, Outcome::Denied, "expired");
    }
    assert!(audit.len() <= 8, "the ring grew to {}", audit.len());
}
