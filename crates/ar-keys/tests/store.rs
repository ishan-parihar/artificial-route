//! Integration tests for the local credential store.
//!
//! The unit tests in `src/store.rs` cover the behaviour; this file exists for one
//! reason the crate's own `tests/keys.rs` states: to drive the **public**
//! re-exports, so a change that quietly un-exports [`ar_keys::CredentialStore`]
//! fails here rather than in a downstream crate's build.
//!
//! Every key in this file is ephemeral — drawn from the OS CSPRNG in-process —
//! and every database lives under the per-test unique path that is removed on
//! the way out. Nothing is read from the environment, so a run leaves no
//! credential material behind.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ar_keys::{CredentialStore, KeyError, Secret};

/// A unique database path per call site.
///
/// `sqlite` takes a write lock per file, so two tests must never share one.
/// The counter plus the pid keeps parallel runs of the same binary apart.
fn temp_db(tag: &str) -> std::path::PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "ar-keys-store-it-{tag}-{}-{n}-{}.db",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
    ));
    let _ = std::fs::remove_file(&path);
    path
}

#[test]
fn returns_a_credential_when_the_name_is_stored() {
    let store = CredentialStore::open_in_memory(&Secret::generate()).expect("open");
    store.insert("openai", "openai", &Secret::new(b"sk-it-round-trip".to_vec())).expect("insert");
    assert_eq!(store.get("openai").expect("get").expect("a row").as_bytes(), b"sk-it-round-trip");
}

#[test]
fn returns_nothing_when_the_name_is_absent() {
    let store = CredentialStore::open_in_memory(&Secret::generate()).expect("open");
    assert!(store.get("nope").expect("get").is_none());
}

#[test]
fn lists_every_stored_name() {
    let store = CredentialStore::open_in_memory(&Secret::generate()).expect("open");
    store.insert("openai", "openai", &Secret::generate()).expect("insert");
    store.insert("anthropic", "anthropic", &Secret::generate()).expect("insert");
    assert_eq!(store.list_names().expect("names"), ["anthropic", "openai"]);
}

#[test]
fn reads_back_after_the_store_is_closed_and_reopened() {
    // The one property a credential store exists for: the second process must
    // find the row. A store that re-derived from a fresh salt would return a tag
    // mismatch here, which is the whole failure this crate's `store_meta` row
    // prevents.
    let path = temp_db("reopen");
    let material = Secret::generate();
    {
        let first = CredentialStore::open_with_material(&path, &material).expect("open");
        first.insert("openai", "openai", &Secret::new(b"sk-it-reopen".to_vec())).expect("insert");
    }
    let second = CredentialStore::open_with_material(&path, &material).expect("reopen");
    assert_eq!(second.get("openai").expect("get").expect("a row").as_bytes(), b"sk-it-reopen");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn refuses_a_row_when_the_master_key_is_another_installs() {
    let path = temp_db("foreign");
    {
        let mine = CredentialStore::open_with_material(&path, &Secret::generate()).expect("open");
        mine.insert("openai", "openai", &Secret::new(b"sk-it-foreign".to_vec())).expect("insert");
    }
    let theirs = CredentialStore::open_with_material(&path, &Secret::generate()).expect("reopen");
    assert!(matches!(theirs.get("openai"), Err(KeyError::TagMismatch)));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn renders_no_credential_in_debug() {
    let store = CredentialStore::open_in_memory(&Secret::generate()).expect("open");
    store.insert("openai", "openai", &Secret::new(b"sk-it-debug".to_vec())).expect("insert");
    let rendered = format!("{store:?}");
    assert!(rendered.starts_with("CredentialStore"), "{rendered}");
    assert!(!rendered.contains("sk-it-debug"), "a credential reached Debug: {rendered}");
}

#[test]
fn reads_the_credential_as_header_text() {
    let store = CredentialStore::open_in_memory(&Secret::generate()).expect("open");
    store.insert("openai", "openai", &Secret::new(b"sk-it-text".to_vec())).expect("insert");
    assert_eq!(store.get_text("openai").expect("text").as_deref(), Some("sk-it-text"));
}

#[test]
fn reports_no_text_when_the_name_is_absent() {
    let store = CredentialStore::open_in_memory(&Secret::generate()).expect("open");
    assert!(store.get_text("nope").expect("text").is_none());
}
