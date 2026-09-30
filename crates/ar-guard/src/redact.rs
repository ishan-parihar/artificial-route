//! Stage 1 — PII and credential redaction.
//!
//! One `regex-automata::meta::Regex` carries every pattern, and each match
//! carries the `PatternID` of the one that fired, so a single pass reports both
//! the span and the class. The automaton is a DFA/NFA hybrid: linear in the
//! input, and no input can make it backtrack.
//!
//! Boundary anchors (`\b`) are on every digit class on purpose. Without them a
//! timestamp like `2024-01-1234` reads as an SSN and `risk-management-config`
//! reads as an `sk-` key. Over-redaction is the safe direction, but a guard that
//! eats half of every log is not deployable, so the cheap fix is anchors.

use std::sync::OnceLock;

use regex_automata::meta::Regex;

use crate::Error;

/// The classes stage 1 removes, in the order they are tried.
///
/// Order is the pattern order: at any one position the first listed pattern
/// that matches wins, which is what makes the output a deterministic function
/// of the input. Captures the class for [`regex_automata::PatternID`] *i*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum Kind {
    /// An email address, `local@domain.tld`.
    Email,
    /// A US social security number, `NNN-NN-NNNN`.
    Ssn,
    /// A 13-19 digit payment card number, with or without separators.
    Card,
    /// A vendor key of the `sk-` family (OpenAI and friends).
    ApiKey,
    /// An AWS access key id, `AKIA` + 16 base32 characters.
    AwsKey,
    /// An `Authorization: Bearer <token>` credential.
    Bearer,
}

impl Kind {
    /// The `kind` this class contributes to its `[REDACTED:kind]` placeholder.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Email => "email",
            Self::Ssn => "ssn",
            Self::Card => "card",
            Self::ApiKey => "api_key",
            Self::AwsKey => "aws_key",
            Self::Bearer => "bearer",
        }
    }
}

/// Pattern order is load-bearing: index *i* of this table is the class of
/// pattern *i*.
const KINDS: &[Kind] = &[
    Kind::Email,
    Kind::Ssn,
    Kind::Card,
    Kind::ApiKey,
    Kind::AwsKey,
    Kind::Bearer,
];

/// One pattern per [`Kind`], all case-insensitive.
///
/// Every alternation is a fixed character class or a bounded repetition, so
/// there is nothing here a DFA cannot run. `\b` is dropped from the email
/// pattern only: its leading class admits `.` and `%`, which are not word
/// characters, so a leading boundary would be a coin flip rather than a rule.
const PATTERNS: &[&str] = &[
    r"(?i)[a-z0-9._%+\-]+@[a-z0-9.\-]+\.[a-z]{2,}",
    r"(?i)\b[0-9]{3}-[0-9]{2}-[0-9]{4}\b",
    r"(?i)\b[0-9](?:[ -]?[0-9]){12,18}\b",
    r"(?i)\bsk-[a-z0-9_\-]{16,}",
    r"(?i)\bakia[0-9a-z]{16}\b",
    r"(?i)\bbearer[ \t]{1,8}[a-z0-9._~+/-]{8,}",
];

static STAGE1: OnceLock<std::result::Result<Regex, String>> = OnceLock::new();

fn build() -> std::result::Result<Regex, String> {
    Regex::new_many(PATTERNS).map_err(|e| {
        // `BuildError`'s own `Display` is just "error parsing pattern N"; the
        // reason is the `source`, and a message that cannot name the reason is
        // not worth building.
        let n = e
            .pattern()
            .map_or_else(|| "?".to_owned(), |p| p.as_usize().to_string());
        let why = std::error::Error::source(&e).map_or_else(String::new, ToString::to_string);
        format!("pattern {n}: {why}")
    })
}

fn stage1() -> Result<&'static Regex, Error> {
    // One pass, one automaton: `new_many` numbers the capture groups to match
    // the pattern indices, so a match reports both its span and its class.
    match STAGE1.get_or_init(build) {
        Ok(re) => Ok(re),
        Err(msg) => Err(Error::Stage1(msg.clone())),
    }
}

/// A redacted string plus the classes removed from it.
///
/// The classes record what was cut, so a caller can assert a *kind* was removed
/// without the removed bytes ever being retained anywhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redaction {
    text: String,
    hits: Vec<Kind>,
}

impl Redaction {
    /// The text with every hit replaced by `[REDACTED:<kind>]`.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// One entry per replacement, in the order they were made.
    #[must_use]
    pub fn hits(&self) -> &[Kind] {
        &self.hits
    }

    /// Whether anything was replaced.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.hits.is_empty()
    }

    /// Whether at least one hit of `kind` was replaced.
    #[must_use]
    pub fn has(&self, kind: Kind) -> bool {
        self.hits.contains(&kind)
    }
}

/// Remove PII and credentials from `text`.
///
/// The same call serves both directions of a proxied request — the inbound
/// prompt and the outbound completion — so a secret cannot survive by being on
/// the other side of the exchange. Callers must feed the returned
/// [`Redaction::text`] to every sink; the input is never retained here.
///
/// One pass, `O(n)` in `text.len()`.
pub fn redact_bidi(text: &str) -> Result<Redaction, Error> {
    let re = stage1()?;
    let mut out = String::with_capacity(text.len());
    let mut hits: Vec<Kind> = Vec::new();
    let mut last = 0usize;

    for m in re.find_iter(text) {
        out.push_str(&text[last..m.start()]);
        // `find_iter` and not `captures_iter`: the class comes from the match's
        // own `PatternID`, so there are no capture slots to fill or allocate,
        // and the kind cannot be misread from a group that happened to be
        // unset.
        let kind = KINDS[m.pattern().as_usize()];
        out.push_str("[REDACTED:");
        out.push_str(kind.as_str());
        out.push(']');
        hits.push(kind);
        last = m.end();
    }
    out.push_str(&text[last..]);

    Ok(Redaction { text: out, hits })
}
