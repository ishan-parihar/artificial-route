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
}
