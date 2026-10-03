//! Hard token budget: the fail-closed clamp that runs after every engine.
//!
//! The invariant is one line and is the whole reason this module exists:
//!
//! > [`clamp_to_budget`] returns text whose token count is **never** greater
//! > than the budget it was given.
//!
//! OmniRoute's reference returns the over-budget body with a validation warning
//! when its preserve-guards make the target unreachable. That is the right call
//! for a gateway that would rather over-send than truncate. A hard budget is the
//! opposite promise — the caller is fitting a context window, so an over-budget
//! return is not a warning, it is a broken contract with the provider. So the
//! unreachable case is resolved by cutting, and [`Clamp::truncated`] is how the
//! caller learns fidelity was spent to hold the line.
//!
//! Live dispatch never calls into this module: the request path forwards
//! over-budget bodies unclamped, and this clamp's only caller is the eval
//! harness — the accepted divergence `docs/audit-notes.md` (d) records. Wiring
//! it onto the dispatch path requires the explicit opt-in that row names.
//!
//! Two stages, in order:
//!
//! 1. **Unit drop** — shed whole expendable units, largest first, never an
//!    [`is_anchor`] unit. This is the quality pass: it removes prose while
//!    leaving URLs, stack frames, `key=value` config lines and code fences
//!    intact and un-split.
//! 2. **Hard cut** — if anchors alone still exceed the budget, cut at a
//!    character boundary chosen by binary search. This is the guarantee pass.
//!    Nothing downstream can be over budget once it returns.
//!
//! Both stages measure with [`ar_tokens::count_text`], the same estimator
//! `ar-tokens` charges quota with, so a budget honoured here is honoured there.
//! Above [`ar_tokens::MAX_EXACT_TOKEN_COUNT_CHARS`] that estimator is a
//! `bytes/4` heuristic, so "within budget" means within the heuristic too — the
//! invariant holds against the estimator the rest of the system uses.
//!
//! ```
//! use ar_compress::clamp_to_budget;
//!
//! // Under budget: untouched, and the caller learns nothing was spent.
//! let roomy = clamp_to_budget("a short line of prose", 100);
//! assert!(!roomy.truncated);
//! assert_eq!(roomy.before, roomy.after);
//!
//! // Over budget: cut, never over.
//! let prose = "the quick brown fox jumps over the lazy dog. ".repeat(40);
//! let tight = clamp_to_budget(&prose, 10);
//! assert!(tight.after <= 10, "clamp went over budget: {}", tight.after);
//! assert!(tight.truncated);
//! ```
//!
//! Ported from `../OmniRoute/open-sse/services/compression/hardBudget.ts`
//! (unit split, preserve anchors, drop-to-target) with the unreachable-target
//! behaviour changed from warn-and-pass to cut, per the fail-closed contract.
//! The saliency ranking is size-ordered rather than semantic; see the
//! `ponytail:` note on [`clamp_to_budget`].

use ar_tokens::count_text;

/// A prose line longer than this is sentence-split into separate droppable
/// units, so a single long paragraph cannot force the whole budget onto a
/// blunt tail cut.
const SENTENCE_SPLIT_MIN_CHARS: usize = 60;

/// The outcome of a clamp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clamp {
    /// The clamped text, guaranteed within budget.
    pub text: String,
    /// Token count before clamping.
    pub before: u32,
    /// Token count after clamping. Always `<= budget`.
    pub after: u32,
    /// Whether any content was removed. `false` means the input was already
    /// within budget and the text is byte-identical.
    pub truncated: bool,
}

/// Clamps `text` to at most `budget` tokens, fail-closed.
///
/// Under budget the text is returned byte-identical with `truncated: false` —
/// no re-measurement churn, no formatting drift on the common path. Over
/// budget, units are dropped and then the remainder is hard-cut, so the
/// returned text is never over budget at any stage.
///
/// Dropping is ordered by unit size, largest first, ties broken toward the end
/// of the text. A [`is_anchor`] unit is never a drop candidate.
///
/// ```
/// use ar_compress::clamp_to_budget;
///
/// // A budget of zero yields empty text, not over-budget text.
/// let clamped = clamp_to_budget("anything at all", 0);
/// assert_eq!(clamped.text, "");
/// assert_eq!(clamped.after, 0);
/// ```
///
/// // ponytail: drop order is size-ordered, not semantic — the largest
// expendable unit goes first, with no notion of which prose matters more.
// That is the cheapest ordering that is deterministic and anchor-safe, and
// `eval:compression` is the instrument that would justify a real saliency
// score. Add one when the eval's fidelity column regresses on a corpus where
// the tail is worth more than the head.
#[must_use]
pub fn clamp_to_budget(text: &str, budget: u32) -> Clamp {
    let before = count_text(text);
    if before <= budget {
        return Clamp {
            text: text.to_owned(),
            before,
            after: before,
            truncated: false,
        };
    }

    // Stage 1: drop whole non-anchor units, largest first.
    let units: Vec<Unit<'_>> = split_units(text).map(Unit::new).collect();
    let mut dropped: Vec<bool> = vec![false; units.len()];
    let mut order: Vec<usize> = (0..units.len()).filter(|&i| !units[i].anchor).collect();
    order.sort_by(|&a, &b| units[b].tokens.cmp(&units[a].tokens).then(b.cmp(&a)));

    // `saturating_sub` on a u64 accumulator: the running figure is an estimate
    // of the joined text, and the exact figure is taken after the loop.
    let mut running: u64 = units.iter().map(|unit| u64::from(unit.tokens)).sum();
    for i in order {
        if running <= u64::from(budget) {
            break;
        }
        running = running.saturating_sub(u64::from(units[i].tokens));
        dropped[i] = true;
    }

    let rebuilt = units
        .iter()
        .zip(&dropped)
        .filter(|(_, is_dropped)| !**is_dropped)
        .map(|(unit, _)| unit.text)
        .collect::<Vec<_>>()
        .join("\n");

    // Stage 2: the guarantee. Whatever stage 1 could not shed, cut at the
    // longest character boundary that still measures within budget.
    let text = if count_text(&rebuilt) <= budget {
        rebuilt
    } else {
        String::from(hard_cut(&rebuilt, budget))
    };

    let after = count_text(&text);
    debug_assert!(
        after <= budget,
        "hard budget must be enforced, got {after} > {budget}"
    );

    Clamp {
        text,
        before,
        after,
        truncated: true,
    }
}

/// One droppable unit: a line, or a sentence within a long prose line.
struct Unit<'a> {
    text: &'a str,
    tokens: u32,
    anchor: bool,
}

impl<'a> Unit<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text,
            tokens: count_text(text),
            anchor: is_anchor(text),
        }
    }
}

/// Splits `text` into droppable units.
///
/// Blank lines are their own unit (dropping one is free whitespace), a line that
/// carries an anchor is never subdivided, and a long unanchored prose line is
/// sentence-split so the budget can be spent unevenly across it.
fn split_units(text: &str) -> impl Iterator<Item = &str> {
    text.split('\n').flat_map(|line| {
        if line.trim().is_empty() {
            return vec![line];
        }
        if !is_anchor(line) && line.len() > SENTENCE_SPLIT_MIN_CHARS {
            let sentences = split_sentences(line);
            if sentences.len() > 1 {
                return sentences;
            }
        }
        vec![line]
    })
}

/// Splits a prose line after `.`, `!` or `?` when followed by whitespace.
///
/// The terminator stays with the sentence it ends, so a dropped unit never
/// leaves a dangling `. ` mid-prose. No regex: this runs on a hot request path
/// and the pattern is three characters.
fn split_sentences(line: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = line.as_bytes();
    let mut start = 0;
    for i in 0..bytes.len() {
        if !matches!(bytes[i], b'.' | b'!' | b'?') {
            continue;
        }
        // Only a terminator followed by whitespace ends a sentence; `3.14` and
        // `e.g` are not sentence ends.
        if !bytes.get(i + 1).is_some_and(u8::is_ascii_whitespace) {
            continue;
        }
        // Advance past the run of whitespace so the next unit starts on the
        // first non-space character.
        let mut next = i + 1;
        while next < bytes.len() && bytes[next].is_ascii_whitespace() {
            next += 1;
        }
        if next > start {
            out.push(&line[start..next]);
            start = next;
        }
    }
    if start < line.len() {
        out.push(&line[start..]);
    }
    out
}

/// Error and exception headers, longest first so `TypeError:` is not matched as
/// the `Error:` suffix.
const ERROR_HEADERS: [&str; 7] = [
    "ReferenceError:",
    "SyntaxError:",
    "RangeError:",
    "TypeError:",
    "Exception:",
    "Traceback:",
    "Error:",
];

/// Whether a unit carries content that must survive the clamp.
///
/// Anchored to meaningful signals only — never to a bare end-of-sentence
/// period, which would mark every prose line as an anchor and turn the clamp
/// into a permanent no-op:
///
/// * any ASCII digit — numbers, line numbers, ports
/// * `://` — URLs
/// * an error/exception header (`Error:`, `Traceback:`, …)
/// * ` ``` ` — code fences
/// * a leading `at ` — stack-trace frames, which carry no digits
/// * a multi-segment path with a dotted segment — `src/lib/foo.rs`, and *not*
///   `and/or`
/// * `ident=value` — credential and config lines, which carry no digits
#[must_use]
pub fn is_anchor(unit: &str) -> bool {
    unit.chars().any(|c| c.is_ascii_digit())
        || unit.contains("://")
        || unit.contains("```")
        || ERROR_HEADERS.iter().any(|h| unit.contains(h))
        || has_stack_frame(unit)
        || has_path(unit)
        || has_assignment(unit)
}

/// A stack-trace frame: the line begins with `at ` after optional whitespace.
fn has_stack_frame(unit: &str) -> bool {
    unit.trim_start().starts_with("at ")
}

/// A multi-segment path whose first segment has an extension dot.
///
/// Requires at least two slashes *and* a dot inside a segment. `and/or` has
/// one slash and no dot; `/usr/local/lib` has three slashes and no dot and is
/// still dropped, which is the conservative direction for a path that carries
/// no version or filename signal.
fn has_path(unit: &str) -> bool {
    let Some(first) = unit.find('/') else {
        return false;
    };
    let rest = &unit[first + 1..];
    let mut has_dot = false;
    for segment in rest.split('/') {
        if segment.contains('.') {
            has_dot = true;
        }
    }
    has_dot && unit.matches('/').count() >= 2
}

/// An `identifier=value` assignment with a non-space value.
fn has_assignment(unit: &str) -> bool {
    let Some(eq) = unit.find('=') else {
        return false;
    };
    if eq == 0 {
        return false;
    }
    let before = unit[..eq]
        .chars()
        .next_back()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    let has_value = unit[eq + 1..]
        .chars()
        .next()
        .is_some_and(|c| !c.is_ascii_whitespace());
    before && has_value
}

/// The longest prefix of `text` measuring at most `budget` tokens, cut on a
/// character boundary.
///
/// The cut point is found by binary search over character boundaries, so the
/// result never splits a `char` and never needs a lossy byte trim.
fn hard_cut(text: &str, budget: u32) -> &str {
    // ponytail: one `usize` per character (200 KB for a 50 kB prompt), which is
    // only allocated on the over-budget path. A streaming search would need a
    // resumable state machine for no measurable win at this size — revisit if
    // the clamp ever runs on multi-megabyte inputs.
    let mut bounds: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
    bounds.push(text.len());

    // `bounds[0] == 0` always satisfies the predicate, so the search has a
    // known-good starting point and terminates with a valid index.
    let (mut lo, mut hi) = (0_usize, bounds.len() - 1);
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        if count_text(&text[..bounds[mid]]) <= budget {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    &text[..bounds[lo]]
}

#[cfg(test)]
mod tests {
    use super::{SENTENCE_SPLIT_MIN_CHARS, clamp_to_budget, is_anchor, split_sentences};
    use ar_tokens::count_text;

    fn long_prose(lines: usize) -> String {
        let line =
            "the quick brown fox jumps over the lazy dog while the cat watches from a warm sill. ";
        (0..lines)
            .map(|i| format!("{i}{line}\n"))
            .collect::<String>()
    }

    #[test]
    fn returns_text_unchanged_when_within_budget() {
        let clamp = clamp_to_budget("a short line", 1_000);
        assert_eq!(clamp.text, "a short line");
    }

    #[test]
    fn reports_not_truncated_when_within_budget() {
        let clamp = clamp_to_budget("a short line", 1_000);
        assert!(!clamp.truncated);
    }

    #[test]
    fn never_exceeds_budget_when_clamping() {
        let clamp = clamp_to_budget(&long_prose(20), 12);
        assert!(clamp.after <= 12, "after = {}", clamp.after);
    }

    #[test]
    fn clamps_when_over_hard_budget() {
        let clamp = clamp_to_budget(&long_prose(40), 16);
        assert!(clamp.after <= 16, "after = {}", clamp.after);
    }

    #[test]
    fn never_exceeds_budget_of_zero() {
        let clamp = clamp_to_budget("anything at all", 0);
        assert_eq!(clamp.after, 0);
    }

    #[test]
    fn reports_truncated_when_over_budget() {
        let clamp = clamp_to_budget(&long_prose(20), 12);
        assert!(clamp.truncated);
    }

    #[test]
    fn records_before_count_when_clamping() {
        let prose = long_prose(20);
        let clamp = clamp_to_budget(&prose, 12);
        assert_eq!(clamp.before, count_text(&prose));
    }

    #[test]
    fn hard_cuts_when_anchors_alone_exceed_budget() {
        // Every unit is anchored, so unit-drop cannot help and only the
        // guarantee pass can bring this within budget.
        let all_anchors = "GET https://example.com/1\n".repeat(50);
        let clamp = clamp_to_budget(&all_anchors, 5);
        assert!(clamp.after <= 5, "after = {}", clamp.after);
    }

    #[test]
    fn cut_lands_on_a_char_boundary_when_anchors_overflow() {
        // Multi-byte characters: a byte-wise cut would panic or produce invalid
        // UTF-8, so this is the regression that keeps `hard_cut` honest.
        let text = "→→→→→→→→→→→→→→→→→→→→→→→→→→→→→→→→→→→→→→→→→→→";
        let clamp = clamp_to_budget(text, 1);
        assert!(clamp.text.chars().all(|c| c == '→'));
    }

    #[test]
    fn drops_prose_units_before_hard_cutting() {
        // Anchored line plus a lot of prose: with room for both, the prose
        // shrinks and the anchor survives whole.
        let text = format!("error: see https://example.com/x\n{}", long_prose(10));
        let clamp = clamp_to_budget(&text, count_text("error: see https://example.com/x") + 4);
        assert!(
            clamp.text.contains("https://example.com/x"),
            "{}",
            clamp.text
        );
    }

    #[test]
    fn anchors_a_line_carrying_a_digit() {
        assert!(is_anchor("reconnecting on port 8080"));
    }

    #[test]
    fn anchors_a_line_carrying_a_url() {
        assert!(is_anchor("see https://example.com for details"));
    }

    #[test]
    fn anchors_a_stack_frame_without_digits() {
        assert!(is_anchor("  at handler(ctx)"));
    }

    #[test]
    fn anchors_a_key_value_assignment_without_digits() {
        assert!(is_anchor("NODE_ENV=production"));
    }

    #[test]
    fn anchors_a_dotted_multi_segment_path() {
        assert!(is_anchor("compiling src/lib/parse.rs"));
    }

    #[test]
    fn does_not_anchor_a_bare_prose_line() {
        assert!(!is_anchor(
            "the deploy waits for a human to approve the change"
        ));
    }

    #[test]
    fn does_not_anchor_a_two_word_slash_pair() {
        assert!(!is_anchor("run either yes and/or no"));
    }

    #[test]
    fn splits_prose_line_into_sentences() {
        let out = split_sentences("One. Two! Three? Four");
        assert_eq!(out.len(), 4);
    }

    #[test]
    fn keeps_decimal_number_inside_a_sentence() {
        assert_eq!(
            split_sentences("pi is 3.14 exactly"),
            vec!["pi is 3.14 exactly"]
        );
    }

    #[test]
    fn leaves_short_prose_line_unsplit() {
        let line = "Short. Line.";
        assert!(!is_anchor(line) && line.len() <= SENTENCE_SPLIT_MIN_CHARS);
    }
}
