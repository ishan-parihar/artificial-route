//! The panel's **live** gate: a cached token engine that notices when the
//! gate row changes.
//!
//! # The problem this solves
//!
//! A8: the panel resolved its gate state **once at boot** and held it for the
//! life of the process. Arming the gate with `aroute keys arm-gate` while the
//! proxy was running did not turn the panel's gate on, and breaking the
//! master did not turn it `Broken`.
//!
//! The obvious fix — re-resolve per request — is **not viable**, and the
//! reason is worth stating because it is not obvious:
//!
//! Resolving the gate means **decrypting** the gate row, and decrypting means
//! deriving the master key: argon2id at [`ar_keys::HashParams::RECOMMENDED`]
//! is **19 MiB and ~40 ms** (`crates/ar-keys/src/hash.rs`, whose own comment
//! says "it runs once per key load, never per request"). A per-request
//! re-resolve would spend 40 ms and 19 MiB of transient allocation on every
//! `/ui` request — three orders of magnitude more than the revocation lookup
//! it would sit beside, and the panel would become the slowest surface in
//! the proxy by a wide margin.
//!
//! # The shape that works
//!
//! **Cache the engine; invalidate it on a cheap signal.**
//!
//! The signing key is the master bytes themselves
//! ([`ar_keys::MasterKey::signing_key`] returns `&self.master`), so a
//! [`ar_keys::Tokens`] built from a master keeps verifying correctly for
//! exactly as long as that master is the right one. And the master changes
//! only when the **gate row** changes — which is observable without
//! decrypting anything: the row's *ciphertext* is a cheap sqlite point read.
//!
//! So the check is: read the gate row's envelope, compare it with the
//! envelope the cached engine was built from, and re-derive **only** when
//! they differ. A request that does not change the row costs one sqlite
//! point read and nothing else.
//!
//! The file's mtime is deliberately **not** the signal. The revocation table
//! lives in the same sqlite file, so every `aroute keys revoke` would change
//! the mtime and force a 40 ms re-derivation of a gate that had not changed.
//! The mtime is a hint; the row's ciphertext is the fact.
//!
//! # The same master the server uses
//!
//! The panel's gate and `/v1`'s gate must resolve to the **same** master, or
//! the two surfaces disagree about the same token — and a panel that refuses
//! what the proxy accepts is a panel the operator cannot use.
//!
//! The server's master is ([`ar_server::app::Components::into_state`]):
//!
//! 1. the store's gate row, **if it decrypts**;
//! 2. otherwise `AR_HTTP_MASTER_KEY`;
//! 3. otherwise no gate at all.
//!
//! So the panel resolves the same way, and the env master is captured **once**
//! at construction — an environment variable cannot change under a running
//! process, so there is nothing to watch and nothing to re-derive.
//!
//! The store itself needs `$AR_MASTER_KEY` to **open**, so a host with no
//! store has no `LiveGate` over a store at all. That is the one case where
//! "no store" genuinely means "no gate row to read", and it is decided at
//! construction rather than inferred from a failed read.
//!
//! # The three arms are preserved
//!
//! A live resolution can produce all three states, and the **badge** is
//! resolved from the same value the **guard** uses. The old shape computed
//! the badge from one call and the gate from another, and the two could
//! disagree — a store that opened for one and not the other produced a badge
//! saying "unarmed" on a panel that was refusing every request.
//!
//! # The safe direction
//!
//! A failed re-derivation goes to [`GateState::Broken`], never back to
//! [`GateState::Unarmed`]. A panel that stays unarmed for the life of the
//! process is safer than a gate that turns itself off, and the panel must
//! never confuse "I could not read the gate" with "there is no gate". A
//! `Broken` panel refuses **writes**; an `Unarmed` one permits them, so the
//! difference is the difference between a panel that is cautious and a panel
//! that is open.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use ar_keys::{CredentialStore, KeyMeta, MasterKey, Revocations, Secret, Tokens};

use super::auth::GateState;

/// The gate's live state: a cached engine plus the ciphertext it was built
/// from.
struct Cached {
    /// The gate row's ciphertext when the engine was built. `None` means the
    /// row was absent, which is a different state from "present and
    /// unchanged".
    envelope: Option<String>,
    /// The engine, when the row decrypted.
    tokens: Option<Arc<Tokens>>,
    /// Why the last resolution failed, when it did.
    ///
    /// The `Broken` arm has to be **representable**, not just returned once.
    /// An earlier shape returned `Broken` from the failure path but left the
    /// cache saying "no row", so the NEXT request saw an unchanged row and
    /// answered `Unarmed` — which permits writes. The broken state has to
    /// live in the cache to survive a request.
    broken: Option<String>,
}

/// The panel's live gate.
///
/// One per process, behind the panel's own `Arc`. Every read of the gate
/// state goes through [`Self::resolve`], which is what keeps the badge and
/// the guard from disagreeing.
pub struct LiveGate {
    /// The credential store, when one could be opened.
    ///
    /// `None` means there is no store on this host at all — the supported
    /// loopback install. The effective master is then the env one, and the
    /// gate row is not watched because there is no file to watch.
    ///
    /// An `Arc` because the panel's **request path** reads it — the envelope
    /// check on every API call — and so do the mint handler and the health
    /// view. One connection, one lock, shared: two connections would be two
    /// read snapshots that could disagree about a just-written gate row.
    store: Option<Arc<Mutex<CredentialStore>>>,
    /// The env-derived master, resolved **once** at construction.
    ///
    /// An environment variable cannot change under a running process, so
    /// there is nothing to watch and nothing to re-derive. The engine is
    /// built here rather than per request, for the same reason the store's
    /// engine is cached.
    env: Option<Arc<Tokens>>,
    /// The revocation table, shared with `/v1`'s gate.
    revocations: Option<Arc<Revocations>>,
    /// The cache, and the derivation counter that proves the cache works.
    cached: Mutex<Cached>,
    /// Counts master derivations.
    ///
    /// Not test-only: the number is the **evidence** that the cache works,
    /// and a cache that silently stopped working would cost 40 ms and 19 MiB
    /// on every request. It is read by the test that pins the property.
    derivations: AtomicU64,
}

impl LiveGate {
    /// Builds a live gate over a credential store, with the env-derived
    /// master as the fallback.
    ///
    /// `store` is `None` when the host has no credential store at all, and
    /// `env_master` is `None` when the gate is not armed from the
    /// environment either. The two are the same two sources the server's
    /// own gate uses, in the same order, so the panel and `/v1` agree.
    ///
    /// The env master is **derived here**, once. That is one argon2id
    /// derivation at boot, the same cost the server's gate pays.
    #[must_use]
    pub fn new(
        store: Option<Arc<Mutex<CredentialStore>>>,
        env_master: Option<Secret>,
        revocations: Option<Arc<Revocations>>,
    ) -> Self {
        let env = env_master.map(|master| {
            Arc::new(engine_for(master, &AtomicU64::new(0)))
        });
        Self {
            store,
            env,
            revocations,
            cached: Mutex::new(Cached {
                envelope: None,
                tokens: None,
                broken: None,
            }),
            derivations: AtomicU64::new(0),
        }
    }

    /// How many times the master has been derived.
    ///
    /// The evidence that the cache works: it must not grow with the request
    /// count, and a test that asserts it is the only thing that would catch
    /// an edit that re-derives per request — every behavioural test passes
    /// even then, and the cost is 40 ms and 19 MiB on every panel request.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the cache test is the only reader")
    )]
    #[must_use]
    pub fn derivations(&self) -> u64 {
        self.derivations.load(Ordering::Relaxed)
    }

    /// The gate's state, as of now.
    ///
    /// Re-derives the master **only** when the gate row's ciphertext has
    /// changed since the last resolution. An unchanged row costs one sqlite
    /// point read.
    pub fn resolve(&self) -> GateState {
        let Some(store) = &self.store else {
            // No store on this host. The effective master is the env one,
            // fixed for the life of the process.
            return match &self.env {
                Some(tokens) => GateState::Armed(Arc::clone(tokens), self.revocations.clone()),
                None => GateState::Unarmed,
            };
        };

        // The envelope read happens BEFORE the cache is locked, and the
        // derivation happens BEFORE the cache is locked again. The lock
        // order is therefore always `store` and never `cached` at the same
        // time as a 40 ms derivation — so a gate change cannot stall every
        // other panel request behind one derivation, and there is no lock
        // cycle to fall into.
        let envelope = self.envelope(store);
        let cached = self.cached.lock().unwrap_or_else(|e| e.into_inner());

        // The row has not changed since the last resolution: the cached
        // engine is still the right one, and re-deriving would spend 40 ms
        // and 19 MiB to learn nothing.
        //
        // Only a **successful** read can hit the cache. A failed read is
        // never cached, so the next request retries — and a failed read must
        // never match the cache, or a transient failure would be answered
        // from a stale success.
        let cache_hit = match &envelope {
            Ok(read) => cached.broken.is_none() && cached.envelope.as_ref() == read.as_ref(),
            Err(_) => false,
        };
        if cache_hit {
            return state_of(&cached, self.revocations.as_ref());
        }
        drop(cached);

        match envelope {
            // The row could not be read at all. This is NOT "no gate row" —
            // it is "I could not tell", and the safe answer to that is the
            // **env master**, because the server's gate is working from the
            // env in exactly this situation. Only when there is no env
            // master either does the answer become `Broken`: a gate row
            // exists and no master can open it, which is the case the
            // server also serves ungated.
            //
            // Answering `Unarmed` here would OPEN the panel, which is the
            // failure the three-arm split exists to prevent.
            Err(reason) => {
                // The cache is NOT updated: the next request retries, and a
                // store that becomes readable again recovers on its own.
                self.cache_failure(reason.clone());
                match &self.env {
                    Some(tokens) => {
                        GateState::Armed(Arc::clone(tokens), self.revocations.clone())
                    }
                    None => GateState::Broken(reason),
                }
            }
            // No row: the gate is the env one, or there is none.
            Ok(None) => {
                self.cache_resolution(None, None);
                match &self.env {
                    Some(tokens) => GateState::Armed(Arc::clone(tokens), self.revocations.clone()),
                    None => GateState::Unarmed,
                }
            }
            Ok(Some(envelope)) => match self.decrypt(store) {
                Ok(tokens) => {
                    self.cache_resolution(Some(envelope), Some(Arc::clone(&tokens)));
                    GateState::Armed(tokens, self.revocations.clone())
                }
                Err(reason) => {
                    // The row is there and will not open.
                    //
                    // The **server** falls back to the env master in this
                    // case, so the panel does too — otherwise the two surfaces
                    // disagree and the panel refuses tokens the proxy
                    // accepts. With no env master to fall back to, the answer
                    // is `Broken`: the operator believes there is a gate (the
                    // row exists) and there is not one.
                    //
                    // The cache is NOT updated, so the next request retries
                    // the derivation. A broken gate is an operator emergency
                    // they will fix, and a panel that keeps trying is one
                    // that recovers when they do.
                    self.cache_failure(reason.clone());
                    match &self.env {
                        Some(tokens) => {
                            GateState::Armed(Arc::clone(tokens), self.revocations.clone())
                        }
                        None => GateState::Broken(reason),
                    }
                }
            },
        }
    }

    /// Installs a successful resolution in the cache.
    fn cache_resolution(&self, envelope: Option<String>, tokens: Option<Arc<Tokens>>) {
        let mut cached = self.cached.lock().unwrap_or_else(|e| e.into_inner());
        cached.envelope = envelope;
        cached.tokens = tokens;
        cached.broken = None;
    }

    /// Records a failed resolution.
    ///
    /// The envelope is deliberately **not** updated: the next request has to
    /// retry, or a transient failure would be cached as a permanent answer.
    fn cache_failure(&self, reason: String) {
        let mut cached = self.cached.lock().unwrap_or_else(|e| e.into_inner());
        cached.broken = Some(reason);
        cached.tokens = None;
    }

    /// The gate row's ciphertext.
    ///
    /// Three answers, and the third is the one that matters:
    ///
    /// * `Ok(Some(envelope))` — the row is present, with that ciphertext.
    /// * `Ok(None)` — the row is genuinely absent.
    /// * `Err(reason)` — **we could not tell**. The store is unreadable (no
    ///   master, a rotated master, sqlite locked), and that is not the same
    ///   thing as "there is no gate". The caller must not collapse it.
    fn envelope(&self, store: &Arc<Mutex<CredentialStore>>) -> Result<Option<String>, String> {
        let store = store.lock().unwrap_or_else(|e| e.into_inner());
        store.envelope(GATE_ROW).map_err(|e| e.to_string())
    }

    /// The master, decrypted from the gate row, and the engine built on it.
    ///
    /// This is the expensive half: an argon2id derivation at 19 MiB and
    /// ~40 ms. It runs only when the row's ciphertext changed.
    fn decrypt(
        &self,
        store: &Arc<Mutex<CredentialStore>>,
    ) -> Result<Arc<Tokens>, String> {
        let store = store.lock().unwrap_or_else(|e| e.into_inner());
        let master: Secret = store
            .get(GATE_ROW)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| {
                "the gate row vanished between the envelope read and the decryption".to_owned()
            })?;
        Ok(Arc::new(engine_for(master, &self.derivations)))
    }
}

/// The engine for a master.
///
/// A fresh salt: it only ever feeds the AEAD key. The SIGNING key is the
/// master bytes themselves, so the engine verifies exactly the tokens the
/// server's own gate verifies.
fn engine_for(master: Secret, derivations: &AtomicU64) -> Tokens {
    derivations.fetch_add(1, Ordering::Relaxed);
    let master = MasterKey::new(master.to_owned_secret(), KeyMeta::generate())
        .expect("a 32-byte master always derives");
    Tokens::new(&master)
}

/// The state a cached engine describes.
fn state_of(cached: &Cached, revocations: Option<&Arc<Revocations>>) -> GateState {
    // A cached failure is `Broken`, never `Unarmed`. The cache must never
    // claim "there is no gate" when the last resolution could not tell.
    if let Some(reason) = &cached.broken {
        return GateState::Broken(reason.clone());
    }
    match &cached.tokens {
        Some(tokens) => GateState::Armed(Arc::clone(tokens), revocations.cloned()),
        None => GateState::Unarmed,
    }
}

/// The gate row's name in the credential store.
///
/// The same name the CLI's `arm-gate` writes and the doctor's gate row reads,
/// so the panel and the CLI are looking at one row.
pub const GATE_ROW: &str = "http-gate";
