//! Vendor model-lifecycle snapshot: which model ids a vendor has retired.
//!
//! Generated from `../OmniRoute/config/quality/model-lifecycle.json` by
//! `ar import --from omniroute`, embedded here with `include_str!` and parsed
//! once, lazily — the same shape and the same reason as
//! [`crate::free::FreeBudgets`].
//!
//! # What this hides, and what it does not
//!
//! Upstream's `isModelSelectable` (`open-sse/services/modelLifecycle.ts:287-304`)
//! has three verdicts — `untracked`, `deprecated`, `shutdown` — and only the
//! last two can hide a model, and only when the caller opts in
//! (`includeDeprecated` / `includeShutdown`). The catalog passes neither, so
//! both default `false` and the *effective* rule is one bit: **hide a model whose
//! vendor has retired it.** That is [`ModelLifecycle::is_selectable`].
//!
//! The upstream snapshot carries three statuses, and only one of them vetoes:
//!
//! | status | count (2026-08-25) | catalog verdict |
//! |---|---|---|
//! | `retired` | 68 | hidden |
//! | `retiring` | 37 | advertised — upstream warns, it does not reject |
//! | `deprecated` | 1 | advertised — same |
//!
//! So only the `retired` ids are carried here. The upstream record also holds
//! `vendor`, `retiredOn` and `replacement`, and those are dropped: nothing in
//! this workspace reads them, and a snapshot that carried fields nothing
//! consumed would be a second thing to keep in step with the vendor pages.
//!
//! # Matching is id-scoped, not provider-scoped
//!
//! `isVendorRetiredId` (`:177-184`) lowercases the id and checks it whole, then
//! its last `/` segment — so `openrouter/claude-opus-4-1-20250805` is hidden by
//! an entry recorded as `claude-opus-4-1-20250805`. That is deliberate upstream
//! (`#11625`): an aggregator still serving a retired id should not re-expose it.
//! The provider-scoped `MODEL_LIFECYCLE_RECORDS` table, which carries dated
//! OpenAI shutdowns, is *not* ported — those ids are in the snapshot's sibling
//! table, are provider-scoped rather than id-scoped, and none of them is a
//! catalog id this snapshot does not already cover.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use ar_core::Strng;
use serde::{Deserialize, Serialize};

/// The generated snapshot, compact JSON for the same reason `registry.json` is.
const MODEL_LIFECYCLE_JSON: &str = include_str!("modelLifecycle.json");

/// How many ids the upstream snapshot marks `retired`, as of this build.
///
/// Asserted against [`ModelLifecycle::retired`] so a snapshot that lost rows
/// fails a test instead of quietly hiding fewer models.
pub const RETIRED_MODEL_IDS: usize = 68;

/// The date the snapshot was last curated against the vendor pages.
pub const LIFECYCLE_GENERATED_AT: &str = "2026-08-25";

/// Whether a generated-at stamp is absent, so the file omits it rather than shipping `""`.
fn is_blank(v: &Strng) -> bool {
    v.is_empty()
}

/// The vendor lifecycle snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelLifecycle {
    /// When upstream last regenerated the file.
    #[serde(default, skip_serializing_if = "is_blank")]
    pub generated_at: Strng,
    /// Lowercased ids whose vendor has retired them.
    ///
    /// A set rather than a map because the upstream record's remaining fields
    /// are not read here — see the module docs.
    #[serde(default)]
    pub retired: BTreeSet<Strng>,
}

impl ModelLifecycle {
    /// Whether a vendor has retired this model id.
    ///
    /// Port of `isVendorRetiredId` (`modelLifecycle.ts:177-184`): the whole
    /// lowercased id, then its last `/` segment.
    #[must_use]
    pub fn is_retired_id(&self, model_id: &str) -> bool {
        if model_id.is_empty() {
            return false;
        }
        let lower = model_id.to_lowercase();
        self.retired.contains(lower.as_str())
            || lower
                .rsplit_once('/')
                .is_some_and(|(_, tail)| self.retired.contains(tail))
    }

    /// Whether this model may be advertised — the catalog's `isModelSelectable`
    /// with upstream's default options.
    ///
    /// The `provider` argument is accepted and ignored, because the catalog's
    /// only lifecycle call site passes one (`catalog.ts:1093`) and a reader who
    /// has not seen the upstream signature would reasonably expect the provider
    /// to matter. It does not here: see the module docs.
    #[must_use]
    pub fn is_selectable(&self, _provider: &str, model_id: &str) -> bool {
        !self.is_retired_id(model_id)
    }
}

/// The process-wide snapshot, parsed on first use.
///
/// # Panics
///
/// Panics if the embedded snapshot is malformed. It is a generated source file,
/// not user input, so a panic at first use is the honest signal.
#[must_use]
pub fn global() -> &'static ModelLifecycle {
    static TABLE: LazyLock<ModelLifecycle> = LazyLock::new(|| {
        serde_json::from_str(MODEL_LIFECYCLE_JSON)
            .expect("embedded modelLifecycle.json is malformed")
    });
    &TABLE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_hide_the_catalog_when_a_vendor_has_retired_the_id() {
        let l = ModelLifecycle {
            generated_at: Strng::from("2026-08-25"),
            retired: BTreeSet::from([Strng::from("gpt-3.5-turbo")]),
        };
        assert!(!l.is_selectable("openai", "gpt-3.5-turbo"));
        assert!(
            l.is_selectable("openai", "gpt-4o"),
            "untracked stays visible"
        );
    }

    #[test]
    fn should_hide_by_the_last_path_segment_when_the_id_carries_a_provider_prefix() {
        let l = ModelLifecycle {
            generated_at: Strng::from(""),
            retired: BTreeSet::from([Strng::from("claude-opus-4-1-20250805")]),
        };
        assert!(l.is_retired_id("openrouter/claude-opus-4-1-20250805"));
        assert!(l.is_retired_id("some/deep/prefix/claude-opus-4-1-20250805"));
    }

    #[test]
    fn should_ignore_case_when_matching_a_retired_id() {
        let l = ModelLifecycle {
            generated_at: Strng::from(""),
            retired: BTreeSet::from([Strng::from("gpt-3.5-turbo")]),
        };
        assert!(l.is_retired_id("GPT-3.5-Turbo"));
    }

    #[test]
    fn should_keep_an_empty_id_visible_when_nothing_retired_it() {
        // Upstream returns false for an empty id before consulting the snapshot
        // (`modelLifecycle.ts:178`), so an empty id is never a retired one.
        let l = ModelLifecycle {
            generated_at: Strng::from(""),
            retired: BTreeSet::from([Strng::from("")]),
        };
        assert!(!l.is_retired_id(""));
    }

    #[test]
    fn should_match_a_bare_segment_only_when_it_is_the_tail() {
        let l = ModelLifecycle {
            generated_at: Strng::from(""),
            retired: BTreeSet::from([Strng::from("a-b")]),
        };
        // The tail is `b`, not `a-b`, so the prefix form does not match.
        assert!(!l.is_retired_id("a-b/c-d"));
        assert!(l.is_retired_id("x/a-b"));
    }

    #[test]
    fn should_carry_the_upstream_retired_set_verbatim() {
        let l = global();
        assert_eq!(
            l.retired.len(),
            RETIRED_MODEL_IDS,
            "regenerated snapshot disagrees with the recorded count"
        );
        assert_eq!(l.generated_at.as_ref(), LIFECYCLE_GENERATED_AT);
        assert!(l.is_retired_id("claude-opus-4-1-20250805"));
        assert!(l.is_retired_id("openai/claude-opus-4-1-20250805"));
    }
}
