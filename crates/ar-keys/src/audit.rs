//! The audit ring. Key ids, never key material.
//!
//! Every field on [`AuditLine`] is either a `&'static str` chosen by this crate
//! or a [`Strng`] the caller built from an identifier. There is no field a
//! caller could fill with a credential, so redaction is a property of the type
//! rather than of a code review.
//!
//! That property is load-bearing because the alternative is what OmniRoute has:
//! six independent masking helpers with three different disclosure widths
//! (`../agentgateway` research; `OmniRoute/src/lib/services/apiKey.ts:48`,
//! `src/lib/apiKeyExposure.ts:16`, `src/mitm/maskSecrets.ts:79`), plus a decrypt
//! failure that logs `ciphertext.slice(0, 30)`. A hand-rolled `mask()` is a
//! function someone eventually calls with the wrong argument.
//!
//! This is an in-memory ring. Persisting it — redb, 24h raw TTL, `admin`-only
//! access per `docs/04-subsystems.md` — is `ar-obs`'s ledger, which writes the
//! [`to_text`] rendering.

use std::collections::VecDeque;
use std::fmt;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use ar_core::Strng;

/// Default number of lines retained.
pub const DEFAULT_CAP: usize = 1_024;

/// What was attempted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// A token was minted.
    Issue,
    /// A token was checked.
    Verify,
    /// A token was put on the revoke list.
    Revoke,
    /// A credential was encrypted into an `enc:v2:` envelope.
    Encrypt,
    /// A credential envelope was opened.
    Decrypt,
    /// A presented credential was compared against its envelope.
    Match,
    /// A request was admitted, or shed, by the lane controller.
    Admit,
}

impl Action {
    /// Lowercase label for the rendered line.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Issue => "issue",
            Self::Verify => "verify",
            Self::Revoke => "revoke",
            Self::Encrypt => "encrypt",
            Self::Decrypt => "decrypt",
            Self::Match => "match",
            Self::Admit => "admit",
        }
    }
}

/// How it ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Succeeded.
    Ok,
    /// Refused.
    Denied,
    /// Shed for capacity.
    Shed,
}

impl Outcome {
    /// Lowercase label for the rendered line.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Denied => "denied",
            Self::Shed => "shed",
        }
    }
}

/// One audit record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditLine {
    /// Unix seconds when the record was made.
    pub at: i64,
    /// The key id. An identifier, never the key.
    pub key_id: Strng,
    /// What was attempted.
    pub action: Action,
    /// How it ended.
    pub outcome: Outcome,
    /// A bounded, non-sensitive detail — a lane name, a `jti`, an error variant
    /// name. Never free-form caller text, so nothing unbounded can be logged.
    pub detail: &'static str,
}

impl fmt::Display for AuditLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} key={} {} {} {}",
            self.at,
            self.key_id,
            self.action.as_str(),
            self.outcome.as_str(),
            self.detail
        )
    }
}

/// A bounded ring of [`AuditLine`]s.
///
/// Bounded because this is an audit *buffer*, not an archive: at [`DEFAULT_CAP`]
/// lines of a few dozen bytes it is well under the 128k lossy channel budget in
/// `docs/04-subsystems.md`, and it drops the oldest rather than growing. The
/// oldest record is the one least likely to be the one an incident needs.
pub struct Audit {
    cap: usize,
    lines: Mutex<VecDeque<AuditLine>>,
}

impl Audit {
    /// A ring holding at most `cap` lines. A `cap` of 0 keeps nothing.
    #[must_use]
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            lines: Mutex::new(VecDeque::with_capacity(cap.min(DEFAULT_CAP))),
        }
    }

    /// Records one line.
    ///
    /// Never fails and never blocks: the lock is held only for a push. A poison
    /// (a panic in another holder) is recovered from rather than propagated,
    /// because losing an audit line is strictly better than failing a request
    /// that was about to be served.
    pub fn record(&self, key_id: Strng, action: Action, outcome: Outcome, detail: &'static str) {
        if self.cap == 0 {
            return;
        }
        let Ok(mut lines) = self.lines.lock() else {
            return;
        };
        if lines.len() == self.cap {
            lines.pop_front();
        }
        lines.push_back(AuditLine {
            at: now_epoch(),
            key_id,
            action,
            outcome,
            detail,
        });
    }

    /// A snapshot of the retained lines, oldest first.
    #[must_use]
    pub fn lines(&self) -> Vec<AuditLine> {
        self.lines.lock().map_or_else(
            |e| e.into_inner().iter().cloned().collect(),
            |l| l.iter().cloned().collect(),
        )
    }

    /// The retained lines rendered, oldest first.
    #[must_use]
    pub fn to_text(&self) -> String {
        self.lines()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Number of retained lines.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lines.lock().map_or(0, |l| l.len())
    }

    /// Whether nothing is retained.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for Audit {
    fn default() -> Self {
        Self::new(DEFAULT_CAP)
    }
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use ar_core::intern;

    use super::{Action, Audit, Outcome};

    #[test]
    fn renders_the_key_id_and_the_action() {
        let a = Audit::new(8);
        a.record(intern("team-a"), Action::Revoke, Outcome::Ok, "jti=abc");
        assert_eq!(
            a.to_text(),
            format!("{} key=team-a revoke ok jti=abc", a.lines()[0].at)
        );
    }

    #[test]
    fn drops_the_oldest_at_the_cap() {
        let a = Audit::new(2);
        a.record(intern("a"), Action::Verify, Outcome::Ok, "");
        a.record(intern("b"), Action::Verify, Outcome::Ok, "");
        a.record(intern("c"), Action::Verify, Outcome::Ok, "");
        let ids: Vec<_> = a
            .lines()
            .into_iter()
            .map(|l| l.key_id.to_string())
            .collect();
        assert_eq!(ids, ["b", "c"]);
    }

    #[test]
    fn a_zero_cap_keeps_nothing() {
        let a = Audit::new(0);
        a.record(intern("a"), Action::Verify, Outcome::Ok, "");
        assert!(a.is_empty());
    }

    #[test]
    fn default_cap_is_bounded() {
        let a = Audit::default();
        for _ in 0..(super::DEFAULT_CAP * 4) {
            a.record(intern("k"), Action::Admit, Outcome::Shed, "lane=heavy");
            assert!(
                a.len() <= super::DEFAULT_CAP,
                "the ring must not grow past its cap"
            );
        }
    }
}
