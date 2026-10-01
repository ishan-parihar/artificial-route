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
//! **Not ported:** sentence recapitalisation (cosmetic, and it is
//! what mangles unfenced code casing upstream) and language packs.
//!
//! ## Intensity
//!
//! The reference gates its rule table by rank — `getRulesForContext` keeps a
//! rule when `INTENSITY_RANK[rule.minIntensity] <= INTENSITY_RANK[intensity]` —
//! and that gate is the whole dial. This port carries the gate as a `tier` on
//! each rule, and the tiers are ordered by how much a deletion can cost:
//!
//! | rung | level | what it may delete |
//! |---|---|---|
//! | 0 | [`Intensity::Lite`] | noise with no referent — a hedge, a bare adverb |
//! | 1 | [`Intensity::Full`] | any filler phrase, including ones that restate a clause |
//! | 2 | [`Intensity::Ultra`] | the filler, plus the noun abbreviations below |
//!
//! `full` is the reference default and exactly the table this crate ran before
//! the dial existed, so no config written against either changes behaviour. The
//! `ultra` abbreviations were previously dropped outright because they rewrite a
//! noun the model may read back into code — a real risk, and the reason they
//! stay opt-in at the top rung rather than joining the default table.

use std::borrow::Cow;

use crate::plan::{Engine, Intensity};
use crate::stats::Stats;

/// A filler rule and the lowest [`Intensity`] rung that may apply it.
struct Phrase {
    /// The text to match, case-insensitively, on word boundaries.
    text: &'static str,
    /// The rung on `caveman`'s ladder. 0 is referent-free, 1 is a clause that
    /// restates something, 2 is reserved for [`ABBREVIATIONS`].
    tier: u8,
}

/// Shorthand for a rung-0 (referent-free) rule.
const NOISE: u8 = 0;
/// Shorthand for a rung-1 (restates a clause) rule.
const FILLER: u8 = 1;

/// Filler phrases, deleted along with the single space that follows.
///
/// Ordered longest-phrase-first within each concern so that a longer match is
/// never pre-empted by one of its own prefixes (`"in order to"` before any
/// stray `"order"`). All ASCII, all matched on word boundaries, so a rule can
/// never fire inside an identifier.
///
/// Ported from `caveman.ts::RULE_KEYWORDS`. These are the words a prompt
/// carries without them meaning anything; the `tier` is this port's
/// [`Intensity`] gate.
const PHRASES: [Phrase; 46] = [
    // Purpose clauses that restate the next clause.
    Phrase { text: "in order to", tier: FILLER },
    Phrase { text: "so as to", tier: FILLER },
    Phrase { text: "due to the fact that", tier: FILLER },
    Phrase { text: "the reason is because", tier: FILLER },
    Phrase { text: "at this point in time", tier: FILLER },
    Phrase { text: "for the purpose of", tier: FILLER },
    Phrase { text: "with the goal of", tier: FILLER },
    Phrase { text: "in an effort to", tier: FILLER },
    // Meta-commentary about the message itself.
    Phrase { text: "it is important to note that", tier: FILLER },
    Phrase { text: "it should be noted that", tier: FILLER },
    Phrase { text: "please note that", tier: FILLER },
    Phrase { text: "keep in mind that", tier: FILLER },
    Phrase { text: "note that the", tier: FILLER },
    Phrase { text: "note that this", tier: FILLER },
    Phrase { text: "as you may know", tier: FILLER },
    Phrase { text: "as we discussed earlier", tier: FILLER },
    Phrase { text: "as previously mentioned", tier: FILLER },
    Phrase { text: "as mentioned before", tier: FILLER },
    Phrase { text: "as previously stated", tier: FILLER },
    // Politeness that carries no instruction.
    Phrase { text: "could you please", tier: FILLER },
    Phrase { text: "would you please", tier: FILLER },
    Phrase { text: "can you please", tier: FILLER },
    Phrase { text: "i would like you to", tier: FILLER },
    Phrase { text: "thank you so much", tier: FILLER },
    Phrase { text: "thanks in advance", tier: FILLER },
    Phrase { text: "i really appreciate", tier: FILLER },
    Phrase { text: "you're welcome", tier: FILLER },
    Phrase { text: "glad to help", tier: FILLER },
    Phrase { text: "feel free to", tier: FILLER },
    Phrase { text: "let me know if", tier: FILLER },
    Phrase { text: "at your convenience", tier: FILLER },
    Phrase { text: "when you get a chance", tier: FILLER },
    // Hedging. Referent-free: the sentence behind it says the same thing.
    Phrase { text: "it seems like", tier: NOISE },
    Phrase { text: "it appears that", tier: NOISE },
    Phrase { text: "i think that", tier: NOISE },
    Phrase { text: "i believe that", tier: NOISE },
    Phrase { text: "to summarize", tier: NOISE },
    Phrase { text: "in summary", tier: NOISE },
    Phrase { text: "to recap", tier: NOISE },
    // Bare filler adverbs.
    Phrase { text: "of course", tier: NOISE },
    Phrase { text: "certainly", tier: NOISE },
    Phrase { text: "absolutely", tier: NOISE },
    Phrase { text: "basically", tier: NOISE },
    Phrase { text: "essentially", tier: NOISE },
    Phrase { text: "literally", tier: NOISE },
    Phrase { text: "actually", tier: NOISE },
];

/// Noun abbreviations, gated at [`Intensity::Ultra`].
///
/// Ported from the `ultra` category of `cavemanRules.ts`'s rule table. Unlike a
/// deletion these are substitutions, so the space after the word is kept — the
/// word gets shorter, it does not disappear. The guards in front of the rewrite
/// still hold, so this never touches a fence, a URL, or code-dominant text.
const ABBREVIATIONS: [(&str, &str); 11] = [
    ("database", "db"),
    ("configuration", "config"),
    ("function", "fn"),
    ("request", "req"),
    ("response", "res"),
    ("implementation", "impl"),
    ("authentication", "auth"),
    ("authorization", "authz"),
    ("application", "app"),
    ("dependency", "dep"),
    ("dependencies", "deps"),
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
    caveman_at(text, Intensity::Full)
}

/// [`caveman`] at an explicit intensity.
///
/// `level` selects which rules may fire; see the module header for the three
/// rungs. [`Intensity::Full`] is the reference default and the behaviour this
/// crate always had, so [`caveman`] is this function at `full`.
#[must_use]
pub fn caveman_at(text: &str, level: Intensity) -> Cow<'_, str> {
    if text.len() < MIN_LENGTH || protected(text) || code_dominant(text) {
        return Cow::Borrowed(text);
    }
    let stripped = strip(text, Engine::Caveman.rung(level));
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

/// [`caveman_at`] plus the savings it produced.
#[must_use]
pub fn caveman_at_with_stats(text: &str, level: Intensity) -> (Cow<'_, str>, Stats) {
    let out = caveman_at(text, level);
    let stats = Stats::between(text, &out);
    (out, stats)
}

/// One left-to-right pass over both tables. No rule loop, no per-rule allocation.
///
/// Trying every rule at every offset is `O(n * phrases)`, which is the trade for
/// being order-independent: a rule can never reappear inside a region a later
/// rule deleted, so the result does not depend on table order the way a chain
/// of `str::replace` passes would. Deletions take the space that followed them
/// ("please help" becomes "help"); substitutions do not, because a shorter word
/// still needs its separator.
fn strip(text: &str, rung: u8) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;

    while i < b.len() {
        if let Some((end, replacement)) = match_at(text, i, rung) {
            i = end;
            if let Some(replacement) = replacement {
                out.push_str(replacement);
                continue;
            }
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

/// What a rule at `i` does: the index just past it, and what to emit in its
/// place. `None` means delete (and eat the following space); `Some(text)` means
/// substitute.
///
/// The phrase table is tried first, so a deletion always wins over an
/// abbreviation at the same offset — a phrase that would have been deleted
/// outright must not become a stub instead.
fn match_at(text: &str, i: usize, rung: u8) -> Option<(usize, Option<&'static str>)> {
    match_phrase_at(text, i, rung)
        .map(|end| (end, None))
        .or_else(|| match_abbreviation_at(text, i, rung).map(|(end, to)| (end, Some(to))))
}

/// The index just past the phrase starting at `i`, if one starts there and its
/// tier is at or below `rung`.
///
/// Word boundaries are required on both sides: without the trailing check a rule
/// like `"actually"` fires inside `actually_pure`, and without the leading one
/// it fires inside `unbasically`.
fn match_phrase_at(text: &str, i: usize, rung: u8) -> Option<usize> {
    let b = text.as_bytes();
    if i > 0 && is_word_byte(b[i - 1]) {
        return None;
    }
    for phrase in &PHRASES {
        if phrase.tier > rung {
            continue;
        }
        let end = i + phrase.text.len();
        if end > b.len() || !text.is_char_boundary(end) {
            continue;
        }
        if !b[i..end].eq_ignore_ascii_case(phrase.text.as_bytes()) {
            continue;
        }
        if b.get(end).is_some_and(|&c| is_word_byte(c)) {
            continue;
        }
        return Some(end);
    }
    None
}

/// The index just past the abbreviable noun at `i` and its replacement, when the
/// rung unlocks [`ABBREVIATIONS`] and the match sits on word boundaries.
fn match_abbreviation_at(text: &str, i: usize, rung: u8) -> Option<(usize, &'static str)> {
    if rung < Engine::Caveman.rung(Intensity::Ultra) || (i > 0 && is_word_byte(text.as_bytes()[i - 1]))
    {
        return None;
    }
    let b = text.as_bytes();
    ABBREVIATIONS.iter().find_map(|(from, to)| {
        let end = i + from.len();
        if end > b.len()
            || !text.is_char_boundary(end)
            || !b[i..end].eq_ignore_ascii_case(from.as_bytes())
            || b.get(end).is_some_and(|&c| is_word_byte(c))
        {
            return None;
        }
        Some((end, *to))
    })
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

    use super::{caveman, caveman_at, caveman_with_stats};
    use crate::plan::Intensity;

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

    /// A `full` rule restates the clause that follows, so `lite` leaves it. This
    /// is the rung that makes `lite` a *weaker* engine rather than a renaming
    /// of `full`.
    #[test]
    fn keeps_a_restating_clause_at_the_lite_rung() {
        let src = "In order to save memory the cache drops stale entries, so nothing changes.";
        assert_eq!(caveman_at(src, Intensity::Lite), Cow::Borrowed(src));
    }

    #[test]
    fn deletes_a_referent_free_hedge_at_every_rung() {
        let src = "It seems like the cache is basically re-validating every response header.";
        for level in [Intensity::Lite, Intensity::Full, Intensity::Ultra] {
            assert!(caveman_at(src, level).len() < src.len(), "{level}");
        }
    }

    #[test]
    fn abbreviates_a_noun_only_at_the_ultra_rung() {
        let src = "The database configuration caches every request response header value.";
        assert!(!caveman_at(src, Intensity::Full).contains("db"), "full must not abbreviate");
        assert!(caveman_at(src, Intensity::Ultra).contains("db config"), "ultra did not abbreviate");
    }

    #[test]
    fn keeps_an_abbreviation_looking_like_an_identifier_intact() {
        let src = "The database_and_response path is written in the configuration file itself.";
        assert!(caveman_at(src, Intensity::Ultra).contains("database_and_response"));
    }

    #[test]
    fn behaves_like_the_bare_engine_at_the_default_level() {
        let src = "It seems like the cache is basically re-validating the response headers.";
        assert_eq!(caveman_at(src, Intensity::Full), caveman(src));
    }
}
