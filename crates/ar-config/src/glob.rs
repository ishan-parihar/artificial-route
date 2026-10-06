//! Glob matching for the operator's model allow/deny lists and per-provider
//! exclusion lists.
//!
//! Ported from OmniRoute's `src/shared/utils/globPattern.ts`, which compiles a
//! glob to an anchored case-insensitive regex supporting `*` (any run of
//! characters) and `?` (exactly one). The semantics that matter for parity are
//! the two the regex spells out and this port reproduces: the match is over the
//! **whole** string (both ends anchored), and it is **case-insensitive**.
//!
//! Why a hand-rolled matcher rather than a regex crate: upstream deliberately
//! escapes every regex metacharacter before substituting the two wildcards, so
//! the pattern can never produce a nested-quantifier construction. The same
//! property holds here for free — there is no regex engine at all, and
//! [`glob_match`] is linear in the pattern length rather than exponential in
//! the worst case, because a `*` backtracks over its own tail only.
//!
//! The scan is over `char`s, not bytes, so a multi-byte character counts as the
//! single "character" `?` promises. Upstream's regex engine is UTF-16 and would
//! count a surrogate pair as two; a model id is ASCII in every registry this
//! port reads, and `char` is the right unit for a Rust caller.

/// Whether `text` matches `pattern`, anchored at both ends, case-insensitively.
///
/// `*` matches any run of characters including none, `?` matches exactly one.
/// Every other character matches only itself — `.`, `+`, `[`, `{` and the rest
/// are literals here exactly as they are after upstream's escape pass.
///
/// ```
/// use ar_config::glob::glob_match;
///
/// assert!(glob_match("claude-*", "claude-sonnet-4-6"));
/// assert!(!glob_match("claude-*", "openai/claude-sonnet-4-6"));
/// assert!(glob_match("GPT-4?", "gpt-4o"));
/// ```
#[must_use]
pub fn glob_match(pattern: &str, text: &str) -> bool {
    // Two cursors and a backtrack point, which is the whole of the algorithm:
    // `star` is the index into `pattern` of the last `*` seen, `resume` the
    // index into `text` to retry from when a tail comparison fails. The
    // alternative — collecting every wildcard position up front — allocates for
    // a matcher called once per catalog row per list entry.
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    // `usize::MAX` rather than `Option`: one compare per step beats a branch on
    // `Option` in a loop this hot, and `MAX` is not a valid pattern index.
    let (mut star, mut resume) = (usize::MAX, 0usize);

    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || eq_ignore_case(p[pi], t[ti])) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            // Defer the decision: remember where the wildcard is, consume
            // nothing yet, and let the next iteration either eat a character or
            // fall through to the backtrack branch.
            star = pi;
            resume = ti;
            pi += 1;
        } else if star != usize::MAX {
            // The tail did not match here. Let the last `*` absorb one more
            // character and retry the tail from the same point.
            pi = star + 1;
            resume += 1;
            ti = resume;
        } else {
            return false;
        }
    }
    // The text is exhausted; a pattern of trailing `*`s still matches it all.
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// ASCII case-insensitive equality.
///
/// The patterns that reach here are provider and model ids — ASCII in every
/// registry this crate reads — so a full Unicode fold would be a per-character
/// table lookup for a case that never occurs. [`char::to_ascii_lowercase`] is
/// the whole rule, and it leaves a non-ASCII character equal only to itself
/// rather than silently folding `İ` onto `i`.
#[inline]
fn eq_ignore_case(a: char, b: char) -> bool {
    a == b || a.eq_ignore_ascii_case(&b)
}

/// Whether an entry names a wildcard at all.
///
/// The cheap pre-filter [`is_model_exposed`] applies before compiling anything:
/// upstream (`modelExposureList.ts:38-50`) tries an exact string compare first
/// and only reaches `globToRegex` when the entry actually contains `*` or `?`,
/// because the common entry is a plain id and a regex on it would be cost paid
/// for nothing.
#[must_use]
pub fn has_wildcard(pattern: &str) -> bool {
    pattern.contains(['*', '?'])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_match_the_whole_text_when_the_pattern_is_an_exact_id() {
        assert!(glob_match("gpt-4o", "gpt-4o"));
        assert!(!glob_match("gpt-4", "gpt-4o"), "anchored at the end");
        assert!(
            !glob_match("gpt-4o-mini", "gpt-4o"),
            "anchored at the start"
        );
    }

    #[test]
    fn should_match_any_run_when_the_pattern_is_a_star() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("*-search", "grok-4-search"));
        assert!(glob_match("openai/*", "openai/gpt-4o"));
        assert!(!glob_match("openai/*", "openrouter/openai/gpt-4o"));
        assert!(glob_match("a*b*c", "axxbyyc"), "two stars share the text");
    }

    #[test]
    fn should_match_exactly_one_character_when_the_pattern_is_a_question_mark() {
        assert!(glob_match("gpt-4?", "gpt-4o"));
        assert!(!glob_match("gpt-4?", "gpt-4"));
        assert!(!glob_match("gpt-4?", "gpt-4oo"));
    }

    #[test]
    fn should_ignore_case_when_both_sides_differ() {
        assert!(glob_match("GPT-4O", "gpt-4o"));
        assert!(glob_match("claude-*", "CLAUDE-sonnet-4-6"));
    }

    #[test]
    fn should_treat_regex_metacharacters_as_literals() {
        // Upstream escapes `[.+^${}()|[\]\\]` before substituting wildcards, so a
        // pattern naming a literal bracket is a literal match, not a class.
        assert!(glob_match("gpt-[4]o", "gpt-[4]o"));
        assert!(!glob_match("gpt-[4]o", "gpt-4o"));
        assert!(glob_match("a.b", "a.b"));
        assert!(!glob_match("a.b", "axb"));
        assert!(glob_match("v1.0+", "v1.0+"));
    }

    #[test]
    fn should_backtrack_when_a_star_must_give_back_a_character() {
        // The tail after `*` cannot match at the position the star first tried,
        // so the star has to absorb one more character and retry.
        assert!(glob_match("*ab", "aaab"));
        assert!(glob_match("*-b", "aa-b"));
        assert!(glob_match("a*b?c", "a1b2c"));
        assert!(!glob_match("a*b?c", "a1b2cd"));
    }

    #[test]
    fn should_match_an_empty_text_when_the_pattern_is_all_stars() {
        assert!(glob_match("*", ""));
        assert!(glob_match("**", ""));
        assert!(glob_match("", ""));
        assert!(!glob_match("?", ""));
    }

    #[test]
    fn should_report_a_wildcard_only_when_the_entry_has_one() {
        assert!(has_wildcard("gpt-*"));
        assert!(has_wildcard("gpt-4?"));
        assert!(!has_wildcard("gpt-4o"));
        assert!(!has_wildcard(""));
    }
}
