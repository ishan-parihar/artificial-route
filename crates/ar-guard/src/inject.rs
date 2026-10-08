//! Stage 2 — prompt-injection verdict.
//!
//! The trigger set is literal phrases, so this is `aho-corasick` rather than a
//! regex: one linear pass, and no way to later introduce a backtracking pattern.
//! Whitespace is normalised (lower-cased, runs collapsed to one space) before
//! matching, so `"Ignore   ALL previous  instructions"` costs one needle rather
//! than forty — one `O(n)` pass over the haystack, not one per pattern.
//!
//! The override family is *composed*, not enumerated: four verbs x four
//! modifiers x five targets x six nouns is 480 needles, which `aho-corasick`
//! absorbs without noticing and which would be an unmaintainable literal table.
//! The system-prompt-probe family is composed the same way from upstream's
//! `system_prompt_leak` regex, and chat-template delimiters carry upstream's
//! `delimiter_injection` set under their own rule name.

use std::sync::OnceLock;

use aho_corasick::AhoCorasick;

use crate::Error;

/// What stage 2 decided about a piece of text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Verdict {
    /// No injection signal. Forward as-is.
    Allow,
    /// A real signal that is not a directive override — red-team framings and
    /// role hijacks. Redact before logging, forward the request.
    Redact,
    /// An instruction override or a system-prompt probe. Refuse.
    Deny,
}

/// The family of trigger that fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Rule {
    /// "ignore all previous instructions" and its whole family.
    Override,
    /// "reveal your system prompt" — upstream's `system_prompt_leak` set,
    /// composed. The `(system|initial|hidden|original)` qualifier is
    /// load-bearing: without it, "show the instructions" is ordinary
    /// coding traffic (upstream issue #4041).
    SystemLeak,
    /// Chat-template delimiters — upstream's `delimiter_injection` set,
    /// verbatim. Its own name so a refusal says what actually fired instead
    /// of blaming a system-prompt probe for a pasted template header.
    DelimiterInjection,
    /// "you are now", "act as if you are", ...
    RoleHijack,
    /// "do anything now", "developer mode", ...
    Jailbreak,
}

impl Rule {
    /// A stable, log-safe name for this family. Deliberately never the matched
    /// text: the point of the guard is that nothing attacker-controlled reaches
    /// a sink.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Override => "override",
            Self::SystemLeak => "system_leak",
            Self::DelimiterInjection => "delimiter_injection",
            Self::RoleHijack => "role_hijack",
            Self::Jailbreak => "jailbreak",
        }
    }

    /// The verdict this family alone produces.
    #[must_use]
    pub const fn verdict(self) -> Verdict {
        match self {
            Self::Override | Self::SystemLeak | Self::DelimiterInjection => Verdict::Deny,
            Self::RoleHijack | Self::Jailbreak => Verdict::Redact,
        }
    }
}

/// A stage-2 outcome: the verdict plus the families behind it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Injection {
    verdict: Verdict,
    rules: Vec<Rule>,
}

impl Injection {
    /// The strongest verdict across every family that fired.
    #[must_use]
    pub const fn verdict(&self) -> Verdict {
        self.verdict
    }

    /// Every family that fired, in match order, without duplicates.
    #[must_use]
    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }
}

const OVERRIDE_VERBS: &[&str] = &["ignore", "disregard", "forget", "override"];
const OVERRIDE_MODS: &[&str] = &["", "all ", "all the ", "the "];
const OVERRIDE_TARGETS: &[&str] = &["previous", "prior", "above", "earlier", "preceding"];
const OVERRIDE_NOUNS: &[&str] = &[
    "instructions",
    "instruction",
    "prompts",
    "prompt",
    "rules",
    "directives",
];

/// Upstream `system_prompt_leak` (`inputSanitizer.ts`), composed rather than
/// enumerated so the set stays the regex's exact meaning. The
/// `(system|initial|hidden|original)` qualifier is load-bearing: upstream
/// issue #4041 exists because an unqualified "show the instructions" fires on
/// ordinary coding-agent traffic. Never add a probe needle without a
/// qualifier.
const PROBE_VERBS: &[&str] = &[
    "reveal", "reveals", "show", "shows", "display", "displays", "print", "prints", "output",
    "outputs", "repeat", "repeats",
];
const PROBE_POSSESSIVES: &[&str] = &["", "your ", "the "];
const PROBE_QUALIFIERS: &[&str] = &["system", "initial", "hidden", "original"];
const PROBE_NOUNS: &[&str] = &["prompt", "instruction", "instructions"];

/// Upstream `delimiter_injection`, verbatim:
/// `(\[SYSTEM\]|\[INST\]|<<SYS>>|<\|im_start\|>|<\|system\|>|<\|user\|>)`.
/// `<|im_end|>`, `### system` and Zephyr template tokens are deliberately
/// absent: those appear in ordinary code, docs and dataset traffic, and
/// carrying them here is exactly the false-alarm class that interrupted
/// agent streams.
const DELIMITER_PHRASES: &[&str] = &[
    "[system]",
    "[inst]",
    "<<sys>>",
    "<|im_start|>",
    "<|system|>",
    "<|user|>",
];
const ROLE_PHRASES: &[&str] = &[
    "you are now",
    "act as if you are",
    "pretend to be",
    "pretend you are",
    "from now on you are",
    "roleplay as",
];

const JAILBREAK_PHRASES: &[&str] = &[
    "do anything now",
    "developer mode",
    "enable developer mode",
    "jailbreak",
    "uncensored mode",
    "without any restrictions",
];

struct Automaton {
    ac: AhoCorasick,
    rules: Vec<Rule>,
}

static STAGE2: OnceLock<std::result::Result<Automaton, String>> = OnceLock::new();

fn build() -> std::result::Result<Automaton, String> {
    let mut owned: Vec<String> = Vec::new();
    let mut rules: Vec<Rule> = Vec::new();

    for verb in OVERRIDE_VERBS {
        for m in OVERRIDE_MODS {
            for t in OVERRIDE_TARGETS {
                for noun in OVERRIDE_NOUNS {
                    owned.push(format!("{verb} {m}{t} {noun}"));
                    rules.push(Rule::Override);
                }
            }
        }
    }
    for verb in PROBE_VERBS {
        for possessive in PROBE_POSSESSIVES {
            for qualifier in PROBE_QUALIFIERS {
                for noun in PROBE_NOUNS {
                    owned.push(format!("{verb} {possessive}{qualifier} {noun}"));
                    rules.push(Rule::SystemLeak);
                }
            }
        }
    }
    for (phrases, rule) in [
        (DELIMITER_PHRASES, Rule::DelimiterInjection),
        (ROLE_PHRASES, Rule::RoleHijack),
        (JAILBREAK_PHRASES, Rule::Jailbreak),
    ] {
        for p in phrases {
            owned.push(normalize(p));
            rules.push(rule);
        }
    }

    // `AhoCorasick` copies the patterns it is handed, so `owned` can drop here
    // even though the type has no lifetime parameter.
    let ac = AhoCorasick::new(&owned).map_err(|e| e.to_string())?;
    Ok(Automaton { ac, rules })
}

fn stage2() -> Result<&'static Automaton, Error> {
    match STAGE2.get_or_init(build) {
        Ok(a) => Ok(a),
        Err(msg) => Err(Error::Stage2(msg.clone())),
    }
}

/// Lower-case and collapse every whitespace run to a single space.
///
/// Leading and trailing whitespace is dropped, so a phrase is only found where
/// it is actually surrounded by non-space characters.
fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut gap = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            gap = !out.is_empty();
        } else {
            if gap {
                out.push(' ');
                gap = false;
            }
            out.extend(ch.to_lowercase());
        }
    }
    out
}

/// Upstream `MAX_INJECTION_SCAN_BYTES`: the head window each string is
/// scanned over.
const MAX_SCAN_BYTES: usize = 16 * 1024;

/// Decide whether `text` is a prompt-injection attempt.
///
/// Scanning stops at the first [`Verdict::Deny`] family, so the common case
/// (nothing found) is one automaton walk and the refusal case is shorter than
/// the full match set.
///
/// `O(min(n, MAX_SCAN_BYTES))`.
pub fn inspect(text: &str) -> Result<Injection, Error> {
    let auto = stage2()?;
    // Upstream caps its scan window (`MAX_INJECTION_SCAN_BYTES`, 16 KiB):
    // directives sit near the top of a prompt, and an uncapped scan is O(body)
    // CPU per request. Upstream caps the joined buffer; this port walks each
    // string separately, so the window applies per string.
    let mut end = text.len().min(MAX_SCAN_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let hay = normalize(&text[..end]);

    let mut rules: Vec<Rule> = Vec::new();
    for m in auto.ac.find_iter(hay.as_bytes()) {
        let rule = auto.rules[m.pattern()];
        if !rules.contains(&rule) {
            rules.push(rule);
        }
        if rule.verdict() == Verdict::Deny {
            return Ok(Injection {
                verdict: Verdict::Deny,
                rules,
            });
        }
    }

    let verdict = if rules.is_empty() {
        Verdict::Allow
    } else {
        Verdict::Redact
    };
    Ok(Injection { verdict, rules })
}
