//! `eval:compression` — fidelity vs. savings on a pinned corpus.
//!
//! The question this answers is the one a release gate needs: *what did the
//! tokens we saved cost us?* So every case yields three numbers and nothing
//! else —
//!
//! | column | meaning |
//! |---|---|
//! | `fidelity` | share of the answer's content terms that survived |
//! | `saved_tokens` | `before - after`, in tokens |
//! | `ratio` | `after / before`; `1.0` means nothing was saved |
//!
//! The reference implementation in `../OmniRoute` scores fidelity with a paid
//! LLM judge behind a USD cost cap. That is dropped for two reasons: it is
//! non-reproducible, and it costs money per run. The mechanical scorer here is
//! deterministic by construction — no RNG, no model call, no network — so a
//! fidelity figure is a property of the code that produced it, reproducible
//! years later. The trade is real and named: a term matcher cannot tell a
//! faithful paraphrase from a lossy one, only that the *substance* survived. It
//! is a regression guard, not a quality score.
//!
//! ```
//! use ar_compress::eval::{SEED_CORPUS, report_table, run};
//! use ar_compress::{Plan, Source, registered};
//!
//! let plan = Plan { steps: Vec::new(), source: Source::Default };
//! let report = run(SEED_CORPUS, &plan, registered(), Some(64)).unwrap();
//! assert!(report_table(&report).contains("saved_tokens"));
//! ```
//!
//! Ported from `../OmniRoute/open-sse/services/compression/eval/` —
//! `corpus.ts` (validation + PII gate), `seedCorpus.ts` (the 5 pinned cases),
//! `savings.ts` (`before`/`after`/`ratio`), `aggregate.ts` (the rollup) and
//! `report.ts` (the table). Dropped: `judge.ts`, `fidelityCheck.ts`,
//! `executorModelClient.ts`, `costMeter.ts` and `grader.ts` — the whole
//! LLM-judge tier. The 5 seed cases are byte-identical to the reference and
//! carry no user data.

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::fmt;

use crate::budget::clamp_to_budget;
use crate::error::CompressError;
use crate::plan::{Plan, Transform, apply_plan};
use ar_tokens::count_text;

/// Shortest run of characters counted as a content term. Two-letter tokens are
/// dominated by function words, so including them would measure function-word
/// overlap rather than content survival.
const MIN_TERM_CHARS: usize = 3;

/// Decimal places every reported ratio and fidelity figure is rounded to, so a
/// report is byte-stable across runs and platforms.
const SCALE: f64 = 10_000.0;

/// The content shape of a corpus case. Reported per case so a regression can be
/// attributed to a shape rather than to the corpus as a whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContentKind {
    /// Natural-language explanation.
    Prose,
    /// Timestamped runtime output.
    Logs,
    /// Source code.
    Code,
    /// Serialized tool result.
    ToolOutputJson,
    /// Several interleaved conversation turns.
    MultiTurn,
}

impl ContentKind {
    /// The kind's stable lowercase name, as it appears in the report table.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Prose => "prose",
            Self::Logs => "logs",
            Self::Code => "code",
            Self::ToolOutputJson => "tool-output-json",
            Self::MultiTurn => "multi-turn",
        }
    }
}

impl fmt::Display for ContentKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One corpus case: a context to compress, and the question that proves it
/// survived.
#[derive(Debug, Clone, Copy)]
pub struct EvalCase<'a> {
    /// Stable id, used in the report and in error messages.
    pub id: &'a str,
    /// The context's content shape.
    pub kind: ContentKind,
    /// The raw context to compress — one user turn's worth.
    pub context: &'a str,
    /// The question asked against the context. Unused by the mechanical scorer
    /// and carried so a case stays interchangeable with the reference corpus.
    pub question: &'a str,
    /// The expected answer. Fidelity is measured against this when present.
    pub gold: Option<&'a str>,
    /// `true` when the case was captured from real traffic rather than curated.
    /// Captured cases are PII-vetted on load; curated ones are trusted as
    /// already vetted.
    pub captured: bool,
}

/// The pinned seed corpus: five cases, one per content kind.
///
/// Every `context` is synthetic. A captured case appends to this list with
/// `captured: true` and is vetted by [`load_corpus`].
pub const SEED_CORPUS: &[EvalCase<'static>] = &[
    EvalCase {
        id: "prose-1",
        kind: ContentKind::Prose,
        context: "The deployment pipeline has three stages. First, the build stage compiles the \
                  TypeScript sources and bundles them. Second, the test stage runs the unit \
                  suite and the integration suite in parallel. Third, the deploy stage pushes \
                  the artifact to the staging environment and waits for a manual approval \
                  before promoting to prod.",
        question: "How many stages does the deployment pipeline have, and what is the last stage?",
        gold: Some("Three stages; the last is the deploy stage."),
        captured: false,
    },
    EvalCase {
        id: "code-1",
        kind: ContentKind::Code,
        context: "export function clamp(n: number, lo: number, hi: number): number {\n  if (n < lo) \
                  return lo;\n  if (n > hi) return hi;\n  return n;\n}",
        question: "What does clamp(5, 0, 3) return?",
        gold: Some("3"),
        captured: false,
    },
    EvalCase {
        id: "tool-output-json-1",
        kind: ContentKind::ToolOutputJson,
        context: "{\"status\":\"ok\",\"results\":[{\"id\":1,\"name\":\"alpha\",\"score\":0.91},\
                  {\"id\":2,\"name\":\"beta\",\"score\":0.42},{\"id\":3,\"name\":\"gamma\",\
                  \"score\":0.77}]}",
        question: "Which result has the highest score?",
        gold: Some("alpha (score 0.91)"),
        captured: false,
    },
    EvalCase {
        id: "logs-1",
        kind: ContentKind::Logs,
        context: "2026-06-22T10:00:01Z INFO  worker started pid=4821\n\
                  2026-06-22T10:00:02Z WARN  retrying upstream attempt=1\n\
                  2026-06-22T10:00:03Z ERROR upstream timeout after=30000ms\n\
                  2026-06-22T10:00:04Z INFO  fell back to secondary provider",
        question: "What error occurred and what happened after it?",
        gold: Some("An upstream timeout after 30000ms; the worker then fell back to the secondary provider."),
        captured: false,
    },
    EvalCase {
        id: "multi-turn-1",
        kind: ContentKind::MultiTurn,
        context: "User: I need to rename the column user_name to username in the users table.\n\
                  Assistant: You can run ALTER TABLE users RENAME COLUMN user_name TO username.\n\
                  User: Will that drop the data in the column?\n\
                  Assistant: No — RENAME COLUMN only changes the column name; the data is \
                  preserved.",
        question: "Does renaming the column drop its data?",
        gold: Some("No; RENAME COLUMN preserves the data."),
        captured: false,
    },
];

/// Token savings for one full-vs-compressed pair.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Savings {
    /// Tokens before compression.
    pub before: u32,
    /// Tokens after compression.
    pub after: u32,
    /// `before - after`. Zero when compression did not fire or inflated.
    pub saved: u32,
    /// `after / before`, rounded to 4 dp. `1.0` when `before` is zero.
    pub ratio: f64,
}

/// Measures token savings between two texts.
///
/// Uses the same estimator [`clamp_to_budget`] enforces with, so a ratio of
/// `1.0` here and "within budget" there cannot disagree.
///
/// ```
/// use ar_compress::eval::savings;
///
/// assert_eq!(savings("a b c d", "a b c d").ratio, 1.0);
/// ```
#[must_use]
pub fn savings(before: &str, after: &str) -> Savings {
    let before = count_text(before);
    let after = count_text(after);
    let ratio = if before == 0 {
        1.0
    } else {
        round(f64::from(after) / f64::from(before))
    };
    Savings {
        before,
        after,
        saved: before.saturating_sub(after),
        ratio,
    }
}

/// Share of a reference answer's content terms that survived compression, in
/// `[0.0, 1.0]`, rounded to 4 dp.
///
/// The reference is `gold` — the answer is what must survive, and measuring
/// against the whole context would let a lossy compressor score well by keeping
/// everything *except* the answer. It is then **intersected with the original**,
/// which is what makes the number mean anything: a gold answer is a paraphrase
/// of the context, so its terms are not all verbatim in the input, and scoring
/// against the raw gold would report an *uncompressed* baseline below `1.0` and
/// make the column unreadable as a regression signal. An uncompressed run is
/// therefore exactly `1.0` by construction.
///
/// With no usable overlap — a gold answer of `"3"`, say, which has no term of
/// three characters — the reference falls back to the original's own terms, so
/// the case still measures truncation instead of scoring a vacuous pass.
///
/// Matching is exact per term, so a morphological variant (`deploy` vs
/// `deploys`) counts as lost. That understates a faithful paraphrase; it is
/// the accepted cost of a scorer with no model behind it.
///
/// ```
/// use ar_compress::eval::fidelity;
///
/// assert_eq!(fidelity("all of this survives", "all of this survives", None), 1.0);
/// ```
#[must_use]
pub fn fidelity(original: &str, compressed: &str, gold: Option<&str>) -> f64 {
    let original_terms = terms(original);
    let mut reference: BTreeSet<String> = match gold {
        Some(gold) => terms(gold).intersection(&original_terms).cloned().collect(),
        None => original_terms.clone(),
    };
    if reference.is_empty() {
        reference = original_terms;
    }
    if reference.is_empty() {
        // Nothing to lose: an empty reference cannot be unfaithfully kept.
        return 1.0;
    }
    let kept = terms(compressed);
    let matched = reference.iter().filter(|term| kept.contains(*term)).count();
    round_ratio(matched, reference.len())
}

fn round(value: f64) -> f64 {
    (value * SCALE).round() / SCALE
}

/// Same rounding, for a count over a count.
fn round_ratio(numerator: usize, denominator: usize) -> f64 {
    round(numerator as f64 / denominator as f64)
}

/// Lowercased alphanumeric terms of at least [`MIN_TERM_CHARS`] characters, with
/// duplicates collapsed.
///
/// A `BTreeSet` rather than a `HashSet` so iteration order — and therefore any
/// future per-term diff — is stable across runs.
fn terms(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut current = String::new();
    // The sentinel separator flushes the final term without a second code path.
    for ch in text.chars().chain(std::iter::once(' ')) {
        if ch.is_alphanumeric() {
            current.extend(ch.to_lowercase());
        } else if current.len() >= MIN_TERM_CHARS {
            out.insert(std::mem::take(&mut current));
        } else {
            current.clear();
        }
    }
    out
}

/// One case's row in the report.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CaseReport<'a> {
    /// The case's id.
    pub id: &'a str,
    /// The case's content kind.
    pub kind: ContentKind,
    /// Content-term retention, in `[0.0, 1.0]`.
    pub fidelity: f64,
    /// Token savings for this case.
    pub savings: Savings,
    /// Whether the hard budget had to cut this case.
    pub truncated: bool,
}

/// A whole run: one row per case, plus the rollup the report prints.
#[derive(Debug, Clone, PartialEq)]
pub struct Report<'a> {
    /// Per-case rows, in corpus order.
    pub cases: Vec<CaseReport<'a>>,
    /// Mean case fidelity, rounded to 4 dp.
    pub fidelity_mean: f64,
    /// Total tokens saved across every case.
    pub saved_total: u32,
    /// `after / before` summed over the whole corpus, rounded to 4 dp. This is
    /// the corpus-level ratio and is *not* the mean of the per-case ratios.
    pub ratio_total: f64,
}

impl Report<'_> {
    /// Tokens the corpus occupied before compression.
    #[must_use]
    pub fn before_total(&self) -> u32 {
        self.cases.iter().map(|c| c.savings.before).sum()
    }

    /// Tokens the corpus occupies after compression.
    #[must_use]
    pub fn after_total(&self) -> u32 {
        self.cases.iter().map(|c| c.savings.after).sum()
    }
}

/// Validates a corpus, then runs it through `plan` and reports the table.
///
/// `budget` is applied after the plan, matching the live order: engines first,
/// then the hard clamp. `None` measures the engines alone.
///
/// The corpus is validated first and the whole run is rejected on the first
/// malformed case, so a broken corpus fails loudly instead of scoring as if it
/// were valid.
///
/// # Errors
///
/// Returns [`CompressError::CorpusCase`] when a case is missing an id, context
/// or question, or — if it was captured from real traffic — matches a PII marker.
pub fn run<'a>(
    cases: &[EvalCase<'a>],
    plan: &Plan,
    transforms: &dyn Transform,
    budget: Option<u32>,
) -> Result<Report<'a>, CompressError> {
    load_corpus(cases)?;

    let rows: Vec<CaseReport<'a>> = cases
        .iter()
        .map(|case| {
            let compressed = apply_plan(plan, case.context, transforms);
            let (text, truncated) = match budget {
                Some(budget) => {
                    let clamped = clamp_to_budget(&compressed, budget);
                    (Cow::Owned(clamped.text), clamped.truncated)
                }
                None => (compressed, false),
            };
            CaseReport {
                id: case.id,
                kind: case.kind,
                fidelity: fidelity(case.context, &text, case.gold),
                savings: savings(case.context, &text),
                truncated,
            }
        })
        .collect();

    let before: u32 = rows.iter().map(|c| c.savings.before).sum();
    let after: u32 = rows.iter().map(|c| c.savings.after).sum();
    let mean = if rows.is_empty() {
        1.0
    } else {
        round(rows.iter().map(|c| c.fidelity).sum::<f64>() / rows.len() as f64)
    };

    Ok(Report {
        fidelity_mean: mean,
        saved_total: before.saturating_sub(after),
        ratio_total: if before == 0 {
            1.0
        } else {
            round(f64::from(after) / f64::from(before))
        },
        cases: rows,
    })
}

/// Renders the fidelity-vs-savings table as markdown.
///
/// The TOTAL row's `ratio` is corpus-level (`after / before` over summed
/// tokens), not the mean of the per-case ratios, so a report reads the way the
/// request would actually be billed. A case the budget had to cut is marked
/// `(truncated)`: its fidelity is a floor the clamp paid for, not an engine's
/// doing.
#[must_use]
pub fn report_table(report: &Report<'_>) -> String {
    let mut out =
        String::from("| case | kind | fidelity | saved_tokens | ratio |\n|---|---|---|---|---|\n");
    for case in &report.cases {
        let mark = if case.truncated { " (truncated)" } else { "" };
        out.push_str(&format!(
            "| {}{mark} | {} | {:.4} | {} | {:.4} |\n",
            case.id,
            case.kind,
            case.fidelity,
            case.savings.saved,
            case.savings.ratio
        ));
    }
    out.push_str(&format!(
        "| **TOTAL** | {} cases | **{:.4}** | **{}** | **{:.4}** |\n",
        report.cases.len(),
        report.fidelity_mean,
        report.saved_total,
        report.ratio_total
    ));
    out
}

/// Validates a corpus before it is scored.
///
/// Rejects a case with no id, context or question, or — for a case marked
/// `captured`, meaning it came from real traffic — an obvious PII marker. A
/// captured case that reached this function un-anonymized is a customer's
/// prompt about to be written into a report.
pub fn load_corpus(cases: &[EvalCase<'_>]) -> Result<(), CompressError> {
    for case in cases {
        if case.id.is_empty() || case.context.is_empty() || case.question.is_empty() {
            return Err(CompressError::CorpusCase {
                id: if case.id.is_empty() { "?" } else { case.id }.to_owned(),
                reason: "missing id, context, or question",
            });
        }
        if case.captured && looks_like_pii(case.context) {
            return Err(CompressError::CorpusCase {
                id: case.id.to_owned(),
                reason: "contains an obvious PII marker — anonymize before ingestion",
            });
        }
    }
    Ok(())
}

/// Whether `text` matches a best-effort PII marker.
///
/// Deliberately narrow in shape and loose in threshold: these patterns are
/// shapes that are almost never prose, so a false positive costs one case while
/// a false negative costs a leak. This is a gate on *ingestion*, not a redactor
/// — choosing to keep nothing identifying is the actual control.
fn looks_like_pii(text: &str) -> bool {
    has_email(text) || has_ssn(text) || has_card_number(text)
}

fn has_email(text: &str) -> bool {
    let Some(at) = text.find('@') else { return false };
    let local = &text[..at];
    // The domain ends at the first whitespace, not at the end of the text:
    // "ops@example.com for access" has a valid domain followed by prose, and
    // reading the prose as part of the domain rejects nothing at all.
    let domain = text[at + 1..]
        .split_whitespace()
        .next()
        .unwrap_or_default();
    local.chars().any(|c| c.is_alphanumeric())
        && domain.contains('.')
        && domain
            .split('.')
            .all(|label| !label.is_empty() && label.chars().all(|c| c.is_alphanumeric() || c == '-'))
}

/// A `NNN-NN-NNNN` run anywhere in the text.
///
/// Scans for a window rather than splitting on `-`, so a hyphenated identifier
/// elsewhere in the same text cannot mask a real SSN shape.
fn has_ssn(text: &str) -> bool {
    const SHAPE: [usize; 3] = [3, 2, 4];
    let bytes = text.as_bytes();
    for start in 0..bytes.len() {
        let mut at = start;
        let mut matched = true;
        for (group, len) in SHAPE.iter().enumerate() {
            if group > 0 {
                match bytes.get(at) {
                    Some(b'-') => at += 1,
                    _ => {
                        matched = false;
                        break;
                    }
                }
            }
            match bytes.get(at..at + len) {
                Some(run) if run.iter().all(u8::is_ascii_digit) => at += len,
                _ => {
                    matched = false;
                    break;
                }
            }
        }
        if matched {
            return true;
        }
    }
    false
}

/// A run of at least 13 digits, optionally separated by single spaces or dashes.
///
/// Loose on purpose: the reference regex caps the run at 16, and a 40-digit hash
/// is not less identifying than a 16-digit card number.
fn has_card_number(text: &str) -> bool {
    let mut run = 0_usize;
    for ch in text.chars() {
        if ch.is_ascii_digit() {
            run += 1;
            if run >= 13 {
                return true;
            }
        } else if ch != ' ' && ch != '-' {
            run = 0;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::{
        CompressError, ContentKind, EvalCase, SEED_CORPUS, fidelity, load_corpus, looks_like_pii,
        report_table, run, savings,
    };
    use crate::plan::{Engine, Plan, Source, Step, Transform, registered};

    /// An engine that discards everything, to prove the fidelity column moves.
    struct Wreck;

    impl Transform for Wreck {
        fn apply<'a>(&self, _step: Step, _text: &'a str) -> Cow<'a, str> {
            Cow::Owned(String::new())
        }
    }

    /// An engine that halves the text, to prove the savings column moves.
    struct Halve;

    impl Transform for Halve {
        fn apply<'a>(&self, _step: Step, text: &'a str) -> Cow<'a, str> {
            Cow::Owned(text.chars().step_by(2).collect())
        }
    }

    fn plan(steps: Vec<Engine>) -> Plan {
        let steps = steps.into_iter().map(Step::new).collect();
        Plan {
            steps,
            source: Source::Default,
        }
    }

    fn case<'a>(id: &'a str, context: &'a str) -> EvalCase<'a> {
        EvalCase {
            id,
            kind: ContentKind::Prose,
            context,
            question: "q",
            gold: None,
            captured: false,
        }
    }

    #[test]
    fn reports_fidelity_when_eval() {
        let report = run(SEED_CORPUS, &plan(vec![Engine::Caveman]), registered(), None).unwrap();
        assert_eq!(report.cases.len(), SEED_CORPUS.len());
    }

    #[test]
    fn reports_perfect_fidelity_for_identity_compression() {
        let report = run(SEED_CORPUS, &plan(Vec::new()), registered(), None).unwrap();
        assert_eq!(report.fidelity_mean, 1.0);
    }

    #[test]
    fn reports_no_savings_for_identity_compression() {
        let report = run(SEED_CORPUS, &plan(Vec::new()), registered(), None).unwrap();
        assert_eq!(report.ratio_total, 1.0);
    }

    #[test]
    fn reports_zero_fidelity_when_everything_is_dropped() {
        let report = run(SEED_CORPUS, &plan(vec![Engine::Caveman]), &Wreck, None).unwrap();
        assert_eq!(report.fidelity_mean, 0.0);
    }

    #[test]
    fn reports_savings_when_an_engine_shrinks_text() {
        let report = run(SEED_CORPUS, &plan(vec![Engine::Lite]), &Halve, None).unwrap();
        assert!(report.saved_total > 0);
    }

    #[test]
    fn reports_ratio_below_one_when_tokens_are_saved() {
        let report = run(SEED_CORPUS, &plan(vec![Engine::Lite]), &Halve, None).unwrap();
        assert!(report.ratio_total < 1.0);
    }

    #[test]
    fn marks_case_truncated_when_budget_cuts_it() {
        let report = run(SEED_CORPUS, &plan(Vec::new()), registered(), Some(4)).unwrap();
        assert!(report.cases.iter().all(|c| c.truncated));
    }

    #[test]
    fn sums_before_tokens_across_cases() {
        let report = run(SEED_CORPUS, &plan(Vec::new()), registered(), None).unwrap();
        assert_eq!(report.before_total(), report.after_total());
    }

    #[test]
    fn derives_total_ratio_from_sums_not_case_ratios() {
        let report = run(SEED_CORPUS, &plan(Vec::new()), registered(), None).unwrap();
        let derived = f64::from(report.after_total()) / f64::from(report.before_total());
        assert!((report.ratio_total - derived).abs() < 0.0001);
    }

    #[test]
    fn table_carries_all_three_columns() {
        let report = run(SEED_CORPUS, &plan(vec![Engine::Rtk]), registered(), None).unwrap();
        assert!(report_table(&report).contains("| case | kind | fidelity | saved_tokens | ratio |"));
    }

    #[test]
    fn table_has_one_row_per_case_plus_total() {
        let report = run(SEED_CORPUS, &plan(Vec::new()), registered(), None).unwrap();
        let rows = report_table(&report)
            .lines()
            .filter(|line| line.starts_with('|'))
            .count();
        assert_eq!(rows, SEED_CORPUS.len() + 3);
    }

    #[test]
    fn rejects_case_without_context() {
        assert!(load_corpus(&[case("x", "")]).is_err());
    }

    #[test]
    fn rejects_case_without_id() {
        assert!(load_corpus(&[case("", "some context")]).is_err());
    }

    #[test]
    fn rejects_captured_case_containing_an_email() {
        let cases = [EvalCase {
            captured: true,
            ..case("leak", "contact ops@example.com for access")
        }];
        assert!(load_corpus(&cases).is_err());
    }

    #[test]
    fn accepts_captured_case_without_pii() {
        let cases = [EvalCase {
            captured: true,
            ..case("clean", "the deploy waits for a human to approve it")
        }];
        assert!(load_corpus(&cases).is_ok());
    }

    #[test]
    fn accepts_the_seed_corpus() {
        assert!(load_corpus(SEED_CORPUS).is_ok());
    }

    #[test]
    fn flags_ssn_shape() {
        assert!(looks_like_pii("applicant 123-45-6789 filed"));
    }

    #[test]
    fn flags_card_shape() {
        assert!(looks_like_pii("paid with 4111 1111 1111 1111 today"));
    }

    #[test]
    fn clears_ordinary_prose() {
        assert!(!looks_like_pii("the deploy waits for a human to approve it"));
    }

    #[test]
    fn counts_ratio_as_one_for_identical_text() {
        assert_eq!(savings("a b c", "a b c").ratio, 1.0);
    }

    #[test]
    fn counts_saved_as_difference_when_shrunk() {
        assert_eq!(savings("a b c d e f", "a b").saved, 4);
    }

    #[test]
    fn counts_saved_as_zero_when_grown() {
        assert_eq!(savings("a b", "a b c d e f").saved, 0);
    }

    #[test]
    fn counts_fidelity_as_one_when_gold_survives() {
        let original = "the pipeline has three stages and the last one deploys";
        let gold = "three stages";
        assert_eq!(fidelity(original, "three stages", Some(gold)), 1.0);
    }

    #[test]
    fn counts_fidelity_as_one_for_an_uncompressed_baseline() {
        let original = "the deploy stage pushes the artifact to staging";
        assert_eq!(fidelity(original, original, Some("the deploy stage")), 1.0);
    }

    #[test]
    fn counts_fidelity_below_one_when_gold_is_dropped() {
        let original = "the pipeline has three stages and the last one deploys";
        let gold = "three stages";
        assert!(fidelity(original, "unrelated filler", Some(gold)) < 1.0);
    }

    #[test]
    fn falls_back_to_original_terms_when_gold_has_no_overlap() {
        // "3" has no term of three characters, so the case must measure
        // truncation rather than scoring a vacuous pass.
        let original = "export function clamp returns the bound";
        assert!(fidelity(original, "", Some("3")) < 1.0);
    }

    #[test]
    fn matches_error_variant_when_corpus_is_rejected() {
        let err = load_corpus(&[case("x", "")]).unwrap_err();
        assert!(matches!(err, CompressError::CorpusCase { .. }));
    }
}
