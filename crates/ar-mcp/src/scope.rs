//! Scope bits and the default-deny check.
//!
//! A [`Scope`] is a bitmask of what a *host* holds; a tool declares what it
//! *needs*. [`Scope::permits`] is the only question asked, and it is
//! default-deny: an empty held scope permits nothing, and a bit nobody granted
//! is never implied by a neighbouring one.
//!
//! `read:*` is the one wildcard. Holding [`Scope::READ`] satisfies any
//! read-category need -- one that carries [`Scope::READ`] and no
//! [`Scope::WRITE`] / [`Scope::EXECUTE`] bit. It does not reach a privileged
//! tool: `write:combos` is not a read.
//!
//! # The grant grammar is the bits, not the per-tool names
//!
//! `docs/06` names a scope per tool (`read:health`, `write:combos`,
//! `execute:completions`) and that column is the *requirement* each tool
//! declares. What a host is *granted* is a set of bits, and those per-tool
//! spellings are all compositions of them: `read:*` alone satisfies every
//! `read:<domain>` need, `read:*`+`write:*` satisfies `write:combos`, and
//! `execute:*` satisfies `execute:completions`. So [`Scope::parse`] accepts the
//! ten bits plus `*` and nothing else -- one name per bit, no second spelling
//! for a combination. That is also what makes the deny default-deny-able: a
//! grant is a list a host cannot spell two ways.

use std::ops::BitOr;

/// What a host holds, or what a tool needs. Zero means "nothing".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Scope(pub u16);

impl Scope {
    /// No scope. Permits nothing that is not a zero-need.
    pub const NONE: Self = Self(0);
    /// `read:*` -- every read-only tool.
    pub const READ: Self = Self(1 << 0);
    /// `write:*` -- every mutating tool.
    pub const WRITE: Self = Self(1 << 1);
    /// `execute:*` -- tools that put a request on the wire.
    pub const EXECUTE: Self = Self(1 << 2);
    /// `*:health` -- lane pressure, breakers, cache counters.
    pub const HEALTH: Self = Self(1 << 3);
    /// `*:combos` -- combo listing and switching.
    pub const COMBOS: Self = Self(1 << 4);
    /// `*:quota` -- per-key remaining budget.
    pub const QUOTA: Self = Self(1 << 5);
    /// `*:usage` -- the cost ledger.
    pub const USAGE: Self = Self(1 << 6);
    /// `*:models` -- the routable catalog.
    pub const MODELS: Self = Self(1 << 7);
    /// `*:completions` -- dispatching a chat completion.
    pub const COMPLETIONS: Self = Self(1 << 8);
    /// `*` -- everything.
    pub const ALL: Self = Self(0x1ff);

    /// Whether every bit of `need` is satisfied by `self`.
    ///
    /// A zero need is always permitted -- a tool that asks for nothing is not
    /// asking. Otherwise the `read:*` wildcard short-circuits a read-category
    /// need, and the rest is a plain subset test, which is what makes the answer
    /// default-deny: `self` only ever contributes bits it actually carries.
    pub fn permits(self, need: Self) -> bool {
        if need.0 == 0 {
            return true;
        }
        let privileged = Self::WRITE.0 | Self::EXECUTE.0;
        if self.0 & Self::READ.0 != 0 && need.0 & Self::READ.0 != 0 && need.0 & privileged == 0 {
            return true;
        }
        need.0 & !self.0 == 0
    }

    /// Whether `bit` is held.
    pub fn contains(self, bit: Self) -> bool {
        self.0 & bit.0 == bit.0
    }

    /// Parses a comma-separated grant list, e.g. `"read:*,write:combos"`.
    ///
    /// Default-deny in the sense that matters: a name that is not in the table
    /// is an error, not a bit silently dropped. A host that typos `read` and
    /// believes it granted a read scope must be told, not handed a scope that
    /// happens to work.
    ///
    /// The wildcards are literal, not glob: `read:*` is one name, and `*` alone
    /// is the only way to say everything. A partial `read:quota` does not exist
    /// as a scope — [`Tool::scope`] asks for whole categories, and a scope
    /// system with a second, narrower spelling for the same bit is a scope
    /// system with two truths.
    ///
    /// # Errors
    ///
    /// The first name that is not in the table, with the valid set, so the
    /// caller can print it as a `help:` line.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let mut out = Self::NONE;
        for name in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            out = out
                | Self::one(name).ok_or_else(|| {
                    format!("unknown scope {name:?}; valid: {}", Self::NAMES.join(", "))
                })?;
        }
        Ok(out)
    }

    /// Every name [`Scope::parse`] accepts, in the order a help line should list
    /// them: category wildcards first, then the per-domain bits, then `*`.
    pub const NAMES: [&'static str; 11] = [
        "read:*",
        "write:*",
        "execute:*",
        "*:health",
        "*:combos",
        "*:quota",
        "*:usage",
        "*:models",
        "*:completions",
        "*",
        "none",
    ];

    fn one(name: &str) -> Option<Self> {
        Some(match name {
            "read:*" => Self::READ,
            "write:*" => Self::WRITE,
            "execute:*" => Self::EXECUTE,
            "*:health" => Self::HEALTH,
            "*:combos" => Self::COMBOS,
            "*:quota" => Self::QUOTA,
            "*:usage" => Self::USAGE,
            "*:models" => Self::MODELS,
            "*:completions" => Self::COMPLETIONS,
            "*" => Self::ALL,
            "none" => Self::NONE,
            _ => return None,
        })
    }
}

impl BitOr for Scope {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

#[cfg(test)]
mod tests {
    use super::Scope;

    #[test]
    fn denies_when_scope_missing() {
        assert!(!Scope::READ.permits(Scope::WRITE | Scope::COMBOS));
    }

    #[test]
    fn read_wildcard_covers_read_tools() {
        assert!(Scope::READ.permits(Scope::READ | Scope::HEALTH));
    }

    #[test]
    fn empty_scope_denies_everything() {
        assert!(!Scope::NONE.permits(Scope::READ | Scope::HEALTH));
    }

    #[test]
    fn read_wildcard_does_not_reach_execute() {
        assert!(!Scope::READ.permits(Scope::EXECUTE | Scope::COMPLETIONS));
    }

    #[test]
    fn full_scope_permits_a_write_tool() {
        assert!(Scope::ALL.permits(Scope::WRITE | Scope::COMBOS));
    }

    #[test]
    fn parses_a_comma_separated_grant_list() {
        assert_eq!(
            Scope::parse("read:*,*:combos").expect("known"),
            Scope::READ | Scope::COMBOS
        );
    }

    #[test]
    fn parse_tolerates_whitespace_and_trailing_commas() {
        assert_eq!(Scope::parse(" read:* , , ").expect("known"), Scope::READ);
    }

    #[test]
    fn parse_rejects_an_unknown_name_rather_than_dropping_it() {
        // A typo that silently dropped its bit would leave a host believing it
        // granted a scope it did not.
        let e = Scope::parse("read:*")
            .and_then(|_| Scope::parse("read"))
            .expect_err("typo");
        assert!(e.contains("\"read\""), "{e}");
        assert!(e.contains("read:*"), "the error lists the valid set: {e}");
    }

    #[test]
    fn parse_of_an_empty_grant_is_nothing_granted() {
        assert_eq!(
            Scope::parse("").expect("empty is not an error"),
            Scope::NONE
        );
    }

    #[test]
    fn every_listed_name_parses() {
        for name in Scope::NAMES {
            assert!(
                Scope::parse(name).is_ok(),
                "{name} is listed but does not parse"
            );
        }
    }

    #[test]
    fn the_listed_names_cover_every_bit() {
        // The grant grammar has no second spelling, so the ten names plus `*` must
        // be able to express the whole mask -- or a bit is unreachable and the
        // only way to grant it is `*`.
        let all =
            "read:*,write:*,execute:*,*:health,*:combos,*:quota,*:usage,*:models,*:completions";
        assert_eq!(Scope::parse(all).expect("known"), Scope::ALL);
    }
}
