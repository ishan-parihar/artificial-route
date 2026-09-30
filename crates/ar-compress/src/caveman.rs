//! `caveman` — terse prose rewrite.
//!
//! Removes English filler: politeness, hedging, throat-clearing, and the
//! purpose-clauses that restate the sentence that follows. One left-to-right
//! pass, no allocation per rule, no second parse.
//!
//! Ported from `../OmniRoute/open-sse/services/compression/caveman.ts` — the
//! `RULE_KEYWORDS` phrase table, `applyRulesToText`, and `cleanupArtifacts`.
//!
//! ## Two guards, both fail-closed
//!
//! The reference mutates whole chat messages and has a validation gate that
//! reverts a message when compression breaks a structure it recognises. This
//! function gets a `&str` and no message context, so it cannot run a post-hoc
//! gate — the guards move *in front* of the rewrite instead:
//!
//! 1. **Protected structure.** Text carrying a fence, a URL, a markdown
//!    heading, a path, or an `Error:` line is left alone. Those are the things
//!    the reference's validation gate checks for, checked here before any byte
//!    moves.
//! 2. **Code-dominant text.** When a third of the non-blank lines look like
//!    code, nothing is rewritten. Ported verbatim from the reference's
//!    `isCodeDominantText` / `isCodeLikeLine` pair, which exists for unfenced
//!    code — a `#file` reference, a raw diff — that preservation cannot see.
//!
//! Fail-closed means the failure mode is a slightly larger prompt, never a
//! corrupted one. Both guards return [`Cow::Borrowed`].
//!
//! **Not ported:** the ultra abbreviation table (`database` → `db`). Rewriting a
//! noun the model is about to read back into code is a correctness risk traded
//! for six characters; the reference gates it behind an `ultra` intensity we
//! do not have. Also not ported: sentence recapitalisation (cosmetic, and it is
//! what mangles unfenced code casing upstream), language packs, and intensity
//! tiers. Every rule here is on or off.

use std::borrow::Cow;

use crate::stats::Stats;

/// Filler phrases, deleted along with the single space that follows.
///
/// Ordered longest-phrase-first within each concern so that a longer match is
/// never pre-empted by one of its own prefixes (`"in order to"` before any
/// stray `"order"`). All ASCII, all matched on word boundaries, so a rule can
/// never fire inside an identifier.
///
/// Ported from `caveman.ts::RULE_KEYWORDS`, minus the `ultra_*` abbreviations.
/// These are the words a prompt carries without them meaning anything.
const PHRASES: [&str; 46] = [
    // Purpose clauses that restate the next clause.
    "in order to",
    "so as to",
    "due to the fact that",
    "the reason is because",
    "at this point in time",
    "for the purpose of",
    "with the goal of",
    "in an effort to",
    // Meta-commentary about the message itself.
    "it is important to note that",
    "it should be noted that",
    "please note that",
    "keep in mind that",
    "note that the",
    "note that this",
    "as you may know",
    "as we discussed earlier",
    "as previously mentioned",
    "as mentioned before",
    "as previously stated",
    // Politeness that carries no instruction.
    "could you please",
    "would you please",
    "can you please",
    "i would like you to",
    "thank you so much",
    "thanks in advance",
    "i really appreciate",
    "you're welcome",
    "glad to help",
    "feel free to",
    "let me know if",
    "at your convenience",
    "when you get a chance",
    // Hedging.
    "it seems like",
    "it appears that",
    "i think that",
    "i believe that",
    "to summarize",
    "in summary",
    "to recap",
    // Bare filler adverbs.
    "of course",
    "certainly",
    "absolutely",
    "basically",
    "essentially",
    "literally",
    "actually",
];

/// Structural markers that mean "hands off".
///
/// A cheap prefilter from `caveman.ts::PROTECTED_STRUCTURE_PREFILTER_RE`: if
/// none of these characters appear, none of the expensive patterns below can
/// match either, so most prose skips the scan entirely.
const PREFILTER: [char; 8] = ['`', '~', '[', ']', '|', '$', '#', '/'];

/// Everything [`protected`] could match. `\n` is handled by the line scan.
const PROTECTED: [&str; 10] = [
    "http://",
    "https://",
    "Error:",
    "Exception:",
    "Traceback",
    "process.env",
    "=>",
    "::",
    "()",
    "fn ",
];

/// Fraction of lines that must look like code before the rewrite is skipped.
///
/// Ported from `caveman.ts::isCodeDominantText`. `provisional:` inherited from
/// the reference, not fitted here. It biases toward *less* compression, which
/// is the direction a wrong answer does not live.
const CODE_DOMINANT_RATIO: f64 = 0.3;

/// Shortest input worth rewriting.
///
/// Below this a rewrite cannot pay for the words it introduces. Mirrors the
/// `minMessageLength` guard in `caveman.ts`.
const MIN_LENGTH: usize = 32;

/// Rewrites filler out of `text`.
///
/// Returns [`Cow::Borrowed`] when a guard declines the text, or when no rule
/// matched — in both cases the input is unchanged and nothing was allocated.
#[must_use]
pub fn caveman(text: &str) -> Cow<'_, str> {
    if text.len() < MIN_LENGTH || protected(text) || code_dominant(text) {
        return Cow::Borrowed(text);
    }
    let stripped = strip_phrases(text);
    let cleaned = cleanup(&stripped);
    if cleaned == text {
        return Cow::Borrowed(text);
    }
    Cow::Owned(cleaned)
}

/// [`caveman`] plus the savings it produced.
#[must_use]
pub fn caveman_with_stats(text: &str) -> (Cow<'_, str>, Stats) {
    let out = caveman(text);
    let stats = Stats::between(text, &out);
    (out, stats)
}

/// One left-to-right pass. No rule loop, no per-rule allocation.
///
/// Trying every rule at every offset is `O(n * phrases)`, which is the trade for
/// being order-independent: a rule can never reappear inside a region a later
/// rule deleted, so the result does not depend on table order the way a chain
/// of `str::replace` passes would.
fn strip_phrases(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;

    while i < b.len() {
        if let Some(end) = match_phrase_at(text, i) {
            // The phrase is deleted, so nothing is emitted for it. It takes the
            // space that followed it too, so "please help" becomes "help".
            i = end;
            if b.get(i) == Some(&b' ') {
                i += 1;
            }
            continue;
        }
        i = push_char(&mut out, text, i);
    }
    out
}

/// Pushes one whole character, not one byte: a phrase boundary must never land
/// inside a multi-byte scalar and panic the slice. Returns the index just past
/// the character, so the caller resumes on a boundary too — advancing by one
/// byte instead is what let an em-dash be sliced in half.
fn push_char(out: &mut String, text: &str, i: usize) -> usize {
    let mut end = i + 1;
    while !text.is_char_boundary(end) {
        end += 1;
    }
    out.push_str(&text[i..end]);
    end
}

/// The index just past the phrase starting at `i`, if one starts there.
///
/// Word boundaries are required on both sides: without the trailing check a rule
/// like `"actually"` fires inside `actually_pure`, and without the leading one
/// it fires inside `unbasically`.
fn match_phrase_at(text: &str, i: usize) -> Option<usize> {
    let b = text.as_bytes();
    if i > 0 && is_word_byte(b[i - 1]) {
        return None;
    }
    for phrase in PHRASES {
        let end = i + phrase.len();
        if end > b.len() || !text.is_char_boundary(end) {
            continue;
        }
        if !b[i..end].eq_ignore_ascii_case(phrase.as_bytes()) {
            continue;
        }
        if b.get(end).is_some_and(|&c| is_word_byte(c)) {
            continue;
        }
        return Some(end);
    }
    None
}

fn is_word_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// Does this text carry a structure that must survive byte-for-byte?
fn protected(text: &str) -> bool {
    if !text.contains(PREFILTER) && !text.contains('\\') && !text.contains(':') {
        return false;
    }
    PROTECTED.iter().any(|marker| text.contains(marker)) || text.contains("```")
}

/// Ported from `caveman.ts::isCodeDominantText`, over 3+ non-blank lines.
fn code_dominant(text: &str) -> bool {
    let mut lines = 0usize;
    let mut code = 0usize;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        lines += 1;
        if looks_like_code(line) {
            code += 1;
        }
    }
    lines >= 3 && f64::from(code as u32) / f64::from(lines as u32) >= CODE_DOMINANT_RATIO
}

/// Ported from `toolResultCompressor.ts::isCodeLikeLine`, trimmed to the shape
/// that matters here: code lines carry punctuation prose does not.
fn looks_like_code(line: &str) -> bool {
    const HINTS: [&str; 8] = [
        "fn ", "def ", "class ", "function ", "import ", "return ", "const ", "pub ",
    ];
    line.contains('{')
        || line.contains(';')
        || line.contains("()")
        || line.ends_with(')')
        || line.ends_with('{')
        || HINTS.iter().any(|hint| line.contains(hint))
}

/// Ported from `caveman.ts::cleanupArtifacts`: deleting words leaves the gaps
/// they leave behind, and a doubled `!!` or a space before a comma costs tokens
/// on its own.
fn cleanup(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev_space = false;
    let mut prev_bang = false;
    for ch in text.chars() {
        let tight = matches!(ch, ',' | '.' | ';' | ':' | '!' | '?');
        if ch == ' ' || ch == '\t' {
            // A space is only worth keeping when a word follows it.
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
            continue;
        }
        if tight {
            if prev_space {
                out.pop();
            }
            // `!!` -> `!`, `?!` -> `?`.
            if prev_bang && (ch == '!' || ch == '?' || ch == '.') {
                continue;
            }
            prev_bang = ch != '.';
        } else {
            prev_bang = false;
        }
        out.push(ch);
        prev_space = false;
    }
    out.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::caveman_with_stats;

    const VERBOSE: &str = "Sure! It seems like the cache layer is basically \
        re-validating the response headers on every single request, which is \
        probably why the p95 latency is so high in production.";

    #[test]
    fn reports_savings_when_caveman() {
        let (out, stats) = caveman_with_stats(VERBOSE);
        assert!(stats.saved_tokens > 0 && out.len() < VERBOSE.len());
    }

    #[test]
    fn ratio_matches_the_reported_saving() {
        let (_, stats) = caveman_with_stats(VERBOSE);
        assert!(stats.ratio > 0.0 && stats.ratio < 1.0);
    }

    #[test]
    fn skips_when_structure_is_protected() {
        let src = "It seems like the endpoint at https://api.dev/v1 is slow.";
        assert_eq!(caveman_with_stats(src).0, Cow::Borrowed(src));
    }

    #[test]
    fn keeps_identifier_when_rule_looks_inside_it() {
        let src = "The basically_pure helper is basically fine on all inputs today.";
        assert!(caveman_with_stats(src).0.contains("basically_pure"));
    }
}
