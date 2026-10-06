//! Registry-canonical provider precedence, for catalog ordering.
//!
//! Ported from OmniRoute's `src/shared/constants/canonicalProviderOrder.ts` and
//! `src/app/api/v1/models/catalogOrder.ts`. The rule upstream states in two
//! sentences: the combo block is pinned first, then providers in registry
//! precedence **OAUTH → NOAUTH → APIKEY**, then unknown providers by
//! code-unit order; input order is preserved within a group.
//!
//! # Where the precedence comes from
//!
//! Upstream builds the rank from `Object.keys(OAUTH_PROVIDERS)` then
//! `NOAUTH_PROVIDERS` then `APIKEY_PROVIDERS` — three TS registry files. The
//! equivalent fact is already in this crate's `registry.json` as each provider's
//! `auth_kind`, which the importer takes from the very same upstream entries
//! (`omniroute.rs` `to_def`). So the rank is *derived* from the catalog rather
//! than generated as a third file: there is no table to drift out of step with
//! the catalog, and adding a provider needs no regeneration.
//!
//! | `auth_kind` | upstream section |
//! |---|---|
//! | `oauth` | `OAUTH_PROVIDERS` |
//! | `optional`, `none` | `NOAUTH_PROVIDERS` — keyless or anonymous |
//! | `apikey` | `APIKEY_PROVIDERS` |
//!
//! Within a section upstream keeps each file's declaration order. This crate
//! keeps id order, because declaration order is not something
//! `ar import --from omniroute` records: `registry.json` is a `BTreeMap`, so the
//! declaration order it was built from is gone. The difference is cosmetic —
//! both orderings group the same providers — and this one is stable across
//! regenerations, which a declaration order read back out of a `BTreeMap` would
//! not be.

use std::collections::HashMap;
use std::sync::LazyLock;

/// The combo bucket's key, distinct from any provider id.
///
/// A leading space is upstream's own trick (`canonicalProviderOrder.ts:41`) and
/// it is kept: no provider id begins with a space, so the sentinel cannot
/// collide with one, and a client that groups on `owned_by` sees a bucket
/// neither `combo` nor any provider name.
pub const COMBO_GROUP: &str = " combo";

/// The three registry sections, in the precedence order they are listed under.
///
/// A section index, so an unrecognised `auth_kind` is a fact rather than a
/// guess: it lands in the API-key section, which is where the overwhelming
/// majority of the catalog already is and where a newly-added auth kind
/// belongs until someone says otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Section {
    OAuth,
    NoAuth,
    ApiKey,
}

/// Which section a provider's `auth_kind` belongs to.
fn section_of(auth_kind: &str) -> Section {
    match auth_kind {
        "oauth" => Section::OAuth,
        // `optional` is a key that may be absent and `none` is a genuinely
        // anonymous provider; upstream files both as no-auth nodes.
        "optional" | "none" => Section::NoAuth,
        _ => Section::ApiKey,
    }
}

/// The id→rank map, derived from the catalog on first use.
///
/// Lazy on the same reasoning as [`crate::global`]: the parse is already lazy
/// and this adds one pass over 276 ids to it, which no dispatch path touches.
static RANKS: LazyLock<HashMap<&'static str, usize>> = LazyLock::new(|| {
    let mut ordered: Vec<(Section, &str)> = crate::global()
        .iter()
        .map(|(id, def)| (section_of(def.auth_kind.as_ref()), id.as_ref()))
        .collect();
    ordered.sort_unstable();
    ordered
        .into_iter()
        .enumerate()
        .map(|(rank, (_, id))| (id, rank))
        .collect()
});

/// The id→rank map.
fn ranks() -> &'static HashMap<&'static str, usize> {
    &RANKS
}

/// The canonical order itself: every provider id, most-precedented first.
///
/// Exposed so `ar doctor` and a failing ordering test can read the list rather
/// than infer it from ranks.
#[must_use]
pub fn canonical_order() -> Vec<&'static str> {
    let mut all: Vec<(usize, &'static str)> = ranks().iter().map(|(id, r)| (*r, *id)).collect();
    // Rank first, id second: a tuple's `Ord` compares left to right, so the id
    // has to be the second element or this sorts by name and returns the raw
    // catalog order the whole module exists to replace.
    all.sort_unstable();
    all.into_iter().map(|(_, id)| id).collect()
}

/// A provider's registry rank, or [`usize::MAX`] when the catalog has none.
///
/// `usize::MAX` rather than a sentinel count, so a caller sorting on it needs no
/// knowledge of how many providers there are — the same reason upstream returns
/// `Infinity` (`canonicalProviderOrder.ts:47`) and sorts unknown groups by a
/// separate comparator.
#[must_use]
pub fn provider_rank(id: &str) -> usize {
    ranks().get(id).copied().unwrap_or(usize::MAX)
}

/// Catalog sort priority for one grouping key: -1 for combos, a rank for a
/// known provider, [`usize::MAX`] for anything else.
///
/// The `-1` is what pins the combo block above every provider, and it has to be
/// a *lower* number rather than a first-class variant because a card's group is
/// a `&str` and there is nowhere to hang an enum.
#[must_use]
pub fn group_sort_priority(group: &str) -> i64 {
    if group == COMBO_GROUP {
        return -1;
    }
    match ranks().get(group) {
        Some(rank) => *rank as i64,
        None => i64::MAX,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds the table the real one is derived from, without the
    /// `include_str!` parse — the derivation is the thing under test, and a test
    /// that needed the 276-row document would fail for a reason that has nothing
    /// to do with ordering.
    fn ranks_of(rows: &[(&str, &str)]) -> HashMap<String, usize> {
        let mut ordered: Vec<(Section, &str)> = rows
            .iter()
            .map(|(id, kind)| (section_of(kind), *id))
            .collect();
        ordered.sort_unstable();
        ordered
            .into_iter()
            .enumerate()
            .map(|(rank, (_, id))| (id.to_owned(), rank))
            .collect()
    }

    #[test]
    fn should_rank_oauth_before_noauth_before_apikey() {
        let r = ranks_of(&[
            ("zed", "apikey"),
            ("beta", "oauth"),
            ("alpha", "apikey"),
            ("gamma", "none"),
        ]);
        assert!(r["beta"] < r["gamma"], "oauth outranks no-auth");
        assert!(r["gamma"] < r["alpha"], "no-auth outranks api-key");
        assert!(r["alpha"] < r["zed"], "api-key section is id-ordered");
    }

    #[test]
    fn should_file_an_optional_key_with_the_noauth_section() {
        assert_eq!(section_of("optional"), Section::NoAuth);
        assert_eq!(section_of("none"), Section::NoAuth);
        assert_eq!(section_of("oauth"), Section::OAuth);
        assert_eq!(section_of("apikey"), Section::ApiKey);
    }

    #[test]
    fn should_file_an_unknown_auth_kind_with_the_api_key_section() {
        // A new auth kind must land somewhere deterministic, not panic and not
        // sort ahead of the 21 oauth providers it is not one of.
        assert_eq!(section_of("mcp"), Section::ApiKey);
        assert_eq!(section_of(""), Section::ApiKey);
    }

    #[test]
    fn should_pin_the_combo_group_above_every_provider() {
        assert_eq!(group_sort_priority(COMBO_GROUP), -1);
        for id in canonical_order() {
            assert!(
                group_sort_priority(id) > -1,
                "{id} outranked the combo block"
            );
        }
    }

    #[test]
    fn should_put_an_unknown_group_after_every_known_one() {
        for id in canonical_order() {
            assert!(group_sort_priority(id) < group_sort_priority("no-such-provider"));
        }
    }

    #[test]
    fn should_read_the_real_catalog_and_rank_every_entry() {
        let order = canonical_order();
        assert_eq!(order.len(), crate::global().len());
        assert_eq!(provider_rank(order[0]), 0);
        assert_eq!(provider_rank("no-such-provider"), usize::MAX);
    }

    #[test]
    fn should_keep_the_combo_group_out_of_the_provider_namespace() {
        // The sentinel is only safe while no provider can produce it. A provider
        // id with a leading space is not in the catalog, and `check_alias`
        // rejects one in a combo id, so this cannot collide today.
        assert!(COMBO_GROUP.starts_with(' '));
        assert!(!crate::global().iter().any(|(id, _)| id.starts_with(' ')));
    }
}
