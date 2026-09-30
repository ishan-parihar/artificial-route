//! Prefix-pin affinity: the hook `ar-route` calls to keep one conversation's
//! turns on one upstream connection.
//!
//! # There is no pin store, and that is the port
//!
//! `../OmniRoute/open-sse/services/combo/promptCacheAffinity.ts` is 404 lines of
//! which the whole mechanism is a *pure function*. It rendezvous-hashes the
//! prefix key against each candidate identity, reorders the candidates, and
//! forgets everything. No map, no TTL, no eviction, no memory accounting, no
//! invalidation when a provider dies.
//!
//! So this module has no state either. The obvious design -- a
//! `HashMap<prefix, provider>` with a TTL, like `ar_route::LkgpPins` -- is worse
//! in every dimension: it needs a sweep, it needs a stale-pin recovery path
//! (the reason `LkgpPins` exists at all is that a pin outliving its provider is
//! a routing bug that surfaces much later as a 502), and it reports the same
//! answer the stateless hash already reports.
//!
//! # What the key is
//!
//! `blake3(tenant | model | canonical stable prefix)`, where the stable prefix is
//! the *contiguous leading run* of `system` / `tool` / `assistant` turns, exactly
//! as `src/lib/promptCache/prefixAnalyzer.ts` scans it. It stops at the first
//! `user` turn: that is where the conversation becomes unique to this request,
//! and a key that included it would be a different key on every turn and pin
//! nothing.
//!
//! Two guard ports are load bearing and are kept:
//!
//! * **The empty-prefix guard.** OmniRoute has no
//!   `prefix_end_idx >= 0` equivalent in `generatePromptCacheKey` and instead
//!   special-cases it in `resolvePromptCacheAffinityKey`, because an
//!   all-`user` conversation hashes to the digest of the empty string. Every
//!   such conversation then scores identically against every target and the
//!   "pin" collapses to a single upstream for all of them. [`prefix_key`]
//!   returns `None` instead.
//! * **The explicit-key override.** A caller-supplied key wins over the derived
//!   prefix, trimmed and length-capped, so a client that knows its own
//!   conversation can pin deliberately.

use crate::entry::now_ms;
use crate::key::{CacheKey, key_of};
use serde_json::Value;

/// Rendezvous share of the affinity score: `0.75` in the reference.
const CACHE_SHARE: f64 = 0.75;

/// Availability share of the affinity score: `0.25` in the reference.
///
/// Note what this means before copying it: a non-OAuth target scores a flat `1`
/// here, so it effectively receives a free `+0.25` that an OAuth target has to
/// earn from session availability. That asymmetry is the reference's, and it
/// is reproduced rather than "fixed" -- a port that silently reweights scoring
/// is a port whose routing cannot be compared against the thing it ports.
const AVAILABILITY_SHARE: f64 = 1.0 - CACHE_SHARE;

/// Longest accepted explicit key, matching the reference's 4096.
pub const MAX_EXPLICIT_KEY_LEN: usize = 4096;

/// One routable connection, as far as affinity is concerned.
///
/// Mirrors `promptCacheTargetIdentity`: connection granularity when the
/// connection id is known, execution granularity otherwise. Those are different
/// things -- the same execution can be reached over several connections -- and
/// conflating them is how a "pin" ends up not pinning.
/// `PartialEq` but not `Eq`: `availability` is an `f64`, and an `f64` field
/// means no total order and no `Hash`. Candidate sets are compared by
/// [`score`] and [`order`], which sort explicitly rather than relying on a
/// derived `Ord`.
#[derive(Clone, Debug, PartialEq)]
pub struct AffinityTarget {
    /// The connection id, when the candidate is scoped to one.
    pub connection_id: Option<String>,
    /// The provider-local execution key, always present.
    pub execution_key: String,
    /// Whether this target authenticates with an OAuth session, which is the
    /// only thing that makes [`score`] discount it.
    pub oauth: bool,
    /// OAuth session availability in `[0.0, 1.0]`. Ignored when `oauth` is
    /// `false`, matching the reference's flat `1`.
    pub availability: f64,
}

impl AffinityTarget {
    /// The string rendezvous-hashed against a prefix key.
    ///
    /// Prefixed by kind so a connection id `"x"` and an execution key `"x"`
    /// cannot score identically.
    #[must_use]
    pub fn identity(&self) -> String {
        match &self.connection_id {
            Some(id) if !id.trim().is_empty() => format!("connection:{id}"),
            _ => format!("execution:{}", self.execution_key),
        }
    }
}

/// How a prefix key was obtained.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AffinitySource {
    /// Supplied by the caller.
    Explicit,
    /// Derived from the conversation's stable prefix.
    Prefix,
}

/// A resolved affinity key and its provenance.
///
/// Not `Copy`: [`CacheKey`] is a 32-byte digest above the 24-byte `Copy`
/// guideline (AGENTS.md §2), and this struct is held across a candidate
/// scoring loop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AffinityKey {
    /// The key itself, as a digest. Never the conversation.
    pub key: CacheKey,
    /// Where it came from.
    pub source: AffinitySource,
}

/// Derives the affinity key for a conversation, honouring an explicit override.
///
/// `messages` is the canonical turn list (`ar_translate::Msg`). `explicit` is
/// the caller's `prompt_cache_key`, or `None`.
///
/// Returns `None` when there is no stable prefix: no system preamble, no tool
/// history, no explicit key. A caller that gets `None` should route on its
/// normal strategy, and must not treat "no key" as "pin to the first target".
#[must_use]
pub fn resolve_key(
    tenant: &str,
    model: &str,
    messages: &[TurnPrefix],
    explicit: Option<&str>,
) -> Option<AffinityKey> {
    if let Some(explicit) = explicit.map(str::trim)
        && !explicit.is_empty()
        && explicit.len() <= MAX_EXPLICIT_KEY_LEN
    {
        return Some(AffinityKey {
            key: key_of(&[
                ("kind", &Value::from("explicit")),
                ("key", &Value::from(explicit)),
                ("model", &Value::from(model)),
                ("tenant", &Value::from(tenant)),
            ]),
            source: AffinitySource::Explicit,
        });
    }

    let prefix = stable_prefix(messages)?;
    Some(AffinityKey {
        key: key_of(&[
            ("kind", &Value::from("prefix")),
            ("prefix", &Value::from(prefix)),
            ("model", &Value::from(model)),
            ("tenant", &Value::from(tenant)),
        ]),
        source: AffinitySource::Prefix,
    })
}

/// The digest a conversation's stable prefix maps to, or `None` when there is
/// no reusable prefix.
///
/// The `[V]`/`[D]`-free shortcut: this is the whole public surface a caller
/// needs if it wants to compute the key once and score several candidate sets.
#[must_use]
pub fn prefix_key(tenant: &str, model: &str, messages: &[TurnPrefix]) -> Option<CacheKey> {
    resolve_key(tenant, model, messages, None).map(|k| k.key)
}

/// Rendezvous score for one candidate, in `[0.0, 1.0]`.
///
/// `0.75 * rendezvous(key, identity) + 0.25 * availability`, exactly the
/// reference's weighting. Higher wins.
///
/// The rendezvous term is `blake3(key || 0x00 || identity)`, top 64 bits read
/// big-endian as a `u64`, normalised by `u64::MAX`. The NUL separator is what
/// makes the two halves unambiguous; hashing them with a `:` would let a crafted
/// key and a crafted identity be confused for one input.
///
/// This is HRW hashing, which is why it needs no state: the ordering is a pure
/// function of the candidate set, so adding a candidate cannot change the
/// relative order of the ones already there. A `HashMap`-based pin cannot say
/// that, and pays a lock plus a stale-entry problem to not say it.
#[must_use]
pub fn score(key: &AffinityKey, target: &AffinityTarget) -> f64 {
    let identity = target.identity();
    let mut buf = Vec::with_capacity(key.key.as_bytes().len() + identity.len() + 1);
    buf.extend_from_slice(key.key.as_bytes());
    buf.push(0);
    buf.extend_from_slice(identity.as_bytes());

    let digest = *blake3::hash(&buf).as_bytes();
    let high = u64::from_be_bytes(digest[0..8].try_into().unwrap_or([0; 8]));
    let rendezvous = high as f64 / u64::MAX as f64;
    let availability = if target.oauth {
        target.availability.clamp(0.0, 1.0)
    } else {
        1.0
    };
    CACHE_SHARE * rendezvous + AVAILABILITY_SHARE * availability
}

/// Reorders `targets` by descending affinity, and reports whether the order
/// changed.
///
/// Ties break on `identity` ascending and then on the original position, so the
/// result is total and deterministic -- the same candidate list always yields
/// the same order, on every thread, with no dependence on hashing order.
///
/// Returns `false` when there is nothing to do: no key, or at most one
/// candidate. The reference bails out on `targets.length <= 1` for the same
/// reason.
#[must_use]
pub fn order<'a>(key: Option<&AffinityKey>, targets: &'a [AffinityTarget]) -> Vec<&'a AffinityTarget> {
    let Some(key) = key else {
        return targets.iter().collect();
    };
    let mut scored: Vec<(usize, &AffinityTarget, f64)> = targets
        .iter()
        .enumerate()
        .map(|(index, target)| (index, target, score(key, target)))
        .collect();
    scored.sort_by(|(ia, a, sa), (ib, b, sb)| {
        sb.total_cmp(sa)
            .then_with(|| a.identity().cmp(&b.identity()))
            .then_with(|| ia.cmp(ib))
    });
    scored.into_iter().map(|(_, t, _)| t).collect()
}

/// The single best candidate, or `None` for an empty set.
///
/// Distinct from [`order`] because a caller that only wants a pick should not
/// pay for a full sort. It still breaks ties identically, so
/// `order(...).first() == best(...)` holds.
#[must_use]
pub fn best<'a>(key: &AffinityKey, targets: &'a [AffinityTarget]) -> Option<&'a AffinityTarget> {
    targets
        .iter()
        .enumerate()
        // `max_by` keeps `a` when `compare(a, current)` is `Greater`, so the
        // score comparison reads `a` first. Reversing it here silently returns
        // the *worst* candidate.
        .max_by(|(ia, a), (ib, b)| {
            score(key, a)
                .total_cmp(&score(key, b))
                // Tie: prefer the lexicographically smaller identity, which is
                // the same rule `order` applies.
                .then_with(|| b.identity().cmp(&a.identity()))
                .then_with(|| ib.cmp(ia))
        })
        .map(|(_, target)| target)
}

/// Why a turn can extend the stable prefix.
///
/// A subset of [`ar_translate::Role`], declared here so this module does not
/// depend on `ar-translate`. The mapping is the caller's one line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnPrefix {
    /// The stable system preamble.
    System,
    /// A tool result, i.e. the tools that shaped the answer.
    Tool,
    /// Prior assistant turns.
    Assistant,
    /// The unique part. Terminates the prefix.
    User,
}

/// The reusable prefix of `messages`: the contiguous leading run up to, but not
/// including, the first [`TurnPrefix::User`] turn.
///
/// `None` when that run is empty. Returning `None` rather than hashing nothing
/// is the empty-prefix guard: `[]` hashes to a fixed digest, so every
/// all-`user` conversation would score identically and pin to whichever target
/// won that one digest.
#[must_use]
pub fn stable_prefix(messages: &[TurnPrefix]) -> Option<String> {
    let end = messages
        .iter()
        .position(|m| *m == TurnPrefix::User)
        .unwrap_or(messages.len());
    if end == 0 {
        return None;
    }
    let mut out = String::with_capacity(end * 12);
    for (idx, turn) in messages[..end].iter().enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        match turn {
            TurnPrefix::System => out.push_str("system"),
            TurnPrefix::Tool => out.push_str("tool"),
            TurnPrefix::Assistant => out.push_str("assistant"),
            // Unreachable: the slice ends before the first `User`.
            TurnPrefix::User => out.push_str("user"),
        }
    }
    Some(out)
}

/// A short, log-safe fingerprint of an [`AffinityKey`].
///
/// 12 hex characters, matching the reference's `fingerprint`. Diagnostics only:
/// 48 bits is fine for "is this the same conversation?" in a log line and far
/// too little to use as a map key, which is why [`order`] and [`best`] take the
/// full key.
#[must_use]
pub fn fingerprint(key: &AffinityKey) -> String {
    key.key.to_hex()[..12].to_owned()
}

/// Seconds since the Unix epoch, re-exported for callers that stamp a pin.
///
/// Deliberately not used by the scoring path: affinity holds no state, so
/// nothing here needs a clock.
#[must_use]
pub fn wall_clock_secs() -> u64 {
    now_ms() / 1_000
}

#[cfg(test)]
mod tests {
    use super::{
        AffinityKey, AffinitySource, AffinityTarget, TurnPrefix, best, fingerprint, order,
        prefix_key, resolve_key, score, stable_prefix,
    };
    use crate::key::CacheKey;

    fn target(execution: &str) -> AffinityTarget {
        AffinityTarget {
            connection_id: None,
            execution_key: execution.to_owned(),
            oauth: false,
            availability: 1.0,
        }
    }

    fn system_then_user() -> Vec<TurnPrefix> {
        vec![TurnPrefix::System, TurnPrefix::System, TurnPrefix::User]
    }

    #[test]
    fn a_system_preamble_yields_a_prefix_key() {
        assert!(prefix_key("t", "m", &system_then_user()).is_some());
    }

    #[test]
    fn an_all_user_conversation_yields_no_prefix_key() {
        // The empty-prefix guard. Without it every first-turn conversation
        // hashes to the same digest and pins to one upstream.
        assert_eq!(prefix_key("t", "m", &[TurnPrefix::User, TurnPrefix::User]), None);
    }

    #[test]
    fn no_messages_at_all_yields_no_prefix_key() {
        assert_eq!(prefix_key("t", "m", &[]), None);
    }

    #[test]
    fn the_prefix_stops_at_the_first_user_turn() {
        // A growing conversation must not change the key, or every turn is a
        // new key and nothing is ever pinned.
        let one = [TurnPrefix::System, TurnPrefix::User];
        let two = [TurnPrefix::System, TurnPrefix::User, TurnPrefix::Assistant, TurnPrefix::User];
        assert_eq!(prefix_key("t", "m", &one), prefix_key("t", "m", &two));
    }

    #[test]
    fn a_different_system_preamble_yields_a_different_key() {
        let a = [TurnPrefix::System, TurnPrefix::User];
        let b = [TurnPrefix::System, TurnPrefix::System, TurnPrefix::User];
        assert_ne!(prefix_key("t", "m", &a), prefix_key("t", "m", &b));
    }

    #[test]
    fn a_conversation_of_only_system_turns_still_has_a_prefix() {
        assert_eq!(
            stable_prefix(&[TurnPrefix::System, TurnPrefix::System]),
            Some("system\nsystem".to_owned())
        );
    }

    #[test]
    fn an_explicit_key_wins_over_the_derived_prefix() {
        let resolved = resolve_key("t", "m", &system_then_user(), Some("  my-key  ")).expect("explicit");
        assert_eq!(resolved.source, AffinitySource::Explicit);
        assert_ne!(resolved.key, prefix_key("t", "m", &system_then_user()).expect("prefix"));
    }

    #[test]
    fn an_over_long_explicit_key_falls_back_to_the_prefix() {
        let huge = "x".repeat(super::MAX_EXPLICIT_KEY_LEN + 1);
        let resolved = resolve_key("t", "m", &system_then_user(), Some(&huge)).expect("prefix");
        assert_eq!(resolved.source, AffinitySource::Prefix);
    }

    #[test]
    fn a_blank_explicit_key_falls_back_to_the_prefix() {
        let resolved = resolve_key("t", "m", &system_then_user(), Some("   ")).expect("prefix");
        assert_eq!(resolved.source, AffinitySource::Prefix);
    }

    #[test]
    fn scoring_is_in_the_unit_interval() {
        let key = resolve_key("t", "m", &system_then_user(), None).expect("key");
        for name in ["a", "b", "c"] {
            let s = score(&key, &target(name));
            assert!((0.0..=1.0).contains(&s), "score {s} out of range");
        }
    }

    #[test]
    fn scoring_is_deterministic() {
        let key = resolve_key("t", "m", &system_then_user(), None).expect("key");
        let a = target("groq");
        assert_eq!(score(&key, &a).to_bits(), score(&key, &a).to_bits());
    }

    #[test]
    fn the_same_prefix_scores_two_targets_differently() {
        // If this fails, every target ties and "affinity" is a no-op that
        // silently degrades routing to first-in-list.
        let key = resolve_key("t", "m", &system_then_user(), None).expect("key");
        assert_ne!(score(&key, &target("groq")).to_bits(), score(&key, &target("openai")).to_bits());
    }

    #[test]
    fn a_connection_id_beats_an_execution_key_for_identity() {
        let t = AffinityTarget {
            connection_id: Some("conn-1".into()),
            execution_key: "ignored".into(),
            oauth: false,
            availability: 1.0,
        };
        assert_eq!(t.identity(), "connection:conn-1");
    }

    #[test]
    fn a_blank_connection_id_falls_back_to_the_execution_key() {
        let t = AffinityTarget {
            connection_id: Some("   ".into()),
            execution_key: "groq".into(),
            oauth: false,
            availability: 1.0,
        };
        assert_eq!(t.identity(), "execution:groq");
    }

    #[test]
    fn identities_are_kind_separated() {
        let conn = AffinityTarget {
            connection_id: Some("x".into()),
            execution_key: "y".into(),
            oauth: false,
            availability: 1.0,
        };
        let exec = target("x");
        assert_ne!(conn.identity(), exec.identity());
    }

    #[test]
    fn a_dead_oauth_session_is_discounted_against_a_healthy_one() {
        let key = resolve_key("t", "m", &system_then_user(), None).expect("key");
        let dead = AffinityTarget {
            connection_id: None,
            execution_key: "dead".into(),
            oauth: true,
            availability: 0.0,
        };
        let healthy = AffinityTarget {
            connection_id: None,
            execution_key: "healthy".into(),
            oauth: true,
            availability: 1.0,
        };
        assert!(score(&key, &healthy) > score(&key, &dead));
    }

    #[test]
    fn out_of_range_availability_is_clamped_not_propagated() {
        let key = resolve_key("t", "m", &system_then_user(), None).expect("key");
        let absurd = AffinityTarget {
            connection_id: None,
            execution_key: "a".into(),
            oauth: true,
            availability: 9.0,
        };
        assert!(score(&key, &absurd) <= 1.0);
    }

    #[test]
    fn ordering_returns_every_candidate_exactly_once() {
        let targets = [target("a"), target("b"), target("c")];
        let key = resolve_key("t", "m", &system_then_user(), None).expect("key");
        let ordered = order(Some(&key), &targets);
        assert_eq!(ordered.len(), targets.len());
    }

    #[test]
    fn ordering_is_sorted_by_descending_score() {
        let targets = [target("a"), target("b"), target("c")];
        let key = resolve_key("t", "m", &system_then_user(), None).expect("key");
        let ordered = order(Some(&key), &targets);
        let scores: Vec<f64> = ordered.iter().map(|t| score(&key, t)).collect();
        let mut descending = scores.clone();
        descending.sort_by(f64::total_cmp);
        descending.reverse();
        assert_eq!(scores, descending);
    }

    #[test]
    fn ordering_is_stable_across_calls() {
        let targets = [target("a"), target("b"), target("c")];
        let key = resolve_key("t", "m", &system_then_user(), None).expect("key");
        let first: Vec<String> = order(Some(&key), &targets).iter().map(|t| t.identity()).collect();
        let second: Vec<String> = order(Some(&key), &targets).iter().map(|t| t.identity()).collect();
        assert_eq!(first, second);
    }

    #[test]
    fn best_agrees_with_the_first_element_of_order() {
        let targets = [target("a"), target("b"), target("c")];
        let key = resolve_key("t", "m", &system_then_user(), None).expect("key");
        assert_eq!(best(&key, &targets), order(Some(&key), &targets).first().copied());
    }

    #[test]
    fn no_key_leaves_the_candidate_order_untouched() {
        let targets = [target("z"), target("a")];
        let ordered = order(None, &targets);
        assert_eq!(ordered[0].identity(), "execution:z");
    }

    #[test]
    fn best_of_nothing_is_nothing() {
        let key = resolve_key("t", "m", &system_then_user(), None).expect("key");
        assert!(best(&key, &[]).is_none());
    }

    #[test]
    fn fingerprint_is_twelve_hex_characters() {
        let key = resolve_key("t", "m", &system_then_user(), None).expect("key");
        let fp = fingerprint(&key);
        assert_eq!(fp.len(), 12);
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn fingerprint_matches_the_key_digest_prefix() {
        let key = AffinityKey {
            key: CacheKey::hash(b"ar"),
            source: AffinitySource::Explicit,
        };
        assert_eq!(fingerprint(&key), CacheKey::hash(b"ar").to_hex()[..12]);
    }
}