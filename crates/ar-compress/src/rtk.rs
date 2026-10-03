//! `rtk` — redundant turn kill.
//!
//! Command output and tool logs repeat. A failing test prints the same
//! assertion forty times; a build prints the same warning from every crate in
//! the graph; an agent loop re-sends a file it just read. None of it is signal
//! the second time, and all of it is billed.
//!
//! This transform collapses a run of **identical consecutive non-empty lines**
//! to the line plus one marker saying how many copies went away. That is
//! lossless in the only sense that matters: the model still learns the line
//! occurred `n` times.
//!
//! Ported from `../OmniRoute/open-sse/services/compression/engines/rtk/
//! deduplicator.ts::deduplicateRepeatedLines` with the threshold from
//! `DEFAULT_RTK_CONFIG.deduplicateThreshold`.
//!
//! ## Intensity
//!
//! The reference's `minimal`/`standard`/`aggressive` scale a *line budget*
//! (`effectiveMaxLines`: 1.5x / 1.0x / 0.5x) around its truncation filters.
//! This engine has no truncation — only the dedup pass — so the same dial lands
//! on the run length it does have: a bigger budget means a longer run is worth
//! collapsing, so [`Intensity::Minimal`] raises the threshold and
//! [`Intensity::Aggressive`] lowers it. `standard` is the reference default and
//! the threshold the crate has always used, so [`rtk`] and
//! [`rtk_at`]`(_, Intensity::Standard)` are the same function.
//!
//! Two deliberate differences from the reference:
//!
//! * **One marker, not two.** The reference emits `[line repeated Nx]` *and*
//!   `[rtk:dropped N repeated lines]`. At the default threshold of 3 that
//!   replaces three lines with three lines and saves nothing, so the dedup pass
//!   — the entire point of the engine — was free to be a no-op. One marker
//!   always shortens the run.
//! * **No command detection, no filters, no truncation.** Those need to know
//!   *which* command produced the output. This function gets a `&str` and
//!   nothing else; a caller that has the command applies its own line filters
//!   before calling. Conflating the two is how a dedup pass ends up truncating
//!   a file read.
//!
//! What is **not** ported: the reference's "skip dedup for document-like reads"
//! guard. Detecting a document read needs the command, so it lives with the
//! caller too. When in doubt, do not call `rtk` on a file read — that is the
//! one input where collapsed structure lines are load-bearing.

use std::borrow::Cow;

use crate::plan::Intensity;
use crate::stats::Stats;

/// How many identical consecutive lines before a run is worth collapsing.
///
/// Two is the floor: one repeat is not a pattern, it is a coincidence. Three is
/// the reference's default and stays the default here — it is a
/// `provisional:` constant inherited from `DEFAULT_RTK_CONFIG`, not fitted
/// against any corpus in this repo.
pub const REPEAT_THRESHOLD: usize = 3;

/// Collapses runs of identical consecutive non-empty lines.
///
/// Returns [`Cow::Borrowed`] when no run reaches [`REPEAT_THRESHOLD`], so text
/// without repetition costs one read-only scan and zero allocations.
#[must_use]
pub fn rtk(text: &str) -> Cow<'_, str> {
    rtk_below(text, REPEAT_THRESHOLD)
}

/// [`rtk`] at an explicit intensity.
///
/// The run length [`rtk`] collapses at, for each rung of `rtk`'s ladder.
/// `provisional:` the two off-default values (4 and 2) are the reference's
/// 1.5x and 0.5x line-budget factors rounded onto a run of three, not fitted
/// against any corpus in this repo.
#[must_use]
pub fn rtk_at(text: &str, level: Intensity) -> Cow<'_, str> {
    rtk_below(text, run_length(level))
}

/// How many identical lines `level` needs before a run is worth collapsing.
const fn run_length(level: Intensity) -> usize {
    match level {
        Intensity::Minimal => 4,
        Intensity::Aggressive => 2,
        _ => REPEAT_THRESHOLD,
    }
}

fn rtk_below(text: &str, threshold: usize) -> Cow<'_, str> {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out = String::with_capacity(text.len());
    let mut wrote_any = false;
    let mut collapsed = 0usize;
    let mut i = 0usize;

    while i < lines.len() {
        let line = lines[i];
        let mut run = 1usize;
        while i + run < lines.len() && lines[i + run] == line {
            run += 1;
        }

        if !line.trim().is_empty() && run >= threshold {
            if wrote_any {
                out.push('\n');
            }
            out.push_str(line);
            out.push('\n');
            out.push_str(&marker(run - 1));
            wrote_any = true;
            collapsed += run - 1;
        } else {
            for _ in 0..run {
                if wrote_any {
                    out.push('\n');
                }
                out.push_str(line);
                wrote_any = true;
            }
        }
        i += run;
    }

    if collapsed == 0 {
        return Cow::Borrowed(text);
    }
    Cow::Owned(out)
}

/// [`rtk`] plus the savings it produced.
#[must_use]
pub fn rtk_with_stats(text: &str) -> (Cow<'_, str>, Stats) {
    let out = rtk(text);
    let stats = Stats::between(text, &out);
    (out, stats)
}

/// The stand-in for the lines that were removed.
///
/// Named for the engine, not for the count: `rtk` is in the prompt budget
/// exactly once, and the run length is right beside it.
fn marker(dropped: usize) -> String {
    format!("[rtk: {dropped} repeats dropped]")
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::{rtk, rtk_at};
    use crate::plan::Intensity;

    #[test]
    fn shrinks_when_redundant() {
        let out = rtk("err boom\nerr boom\nerr boom\nerr boom\nerr boom\n");
        assert!(out.len() < "err boom\n".repeat(5).len());
    }

    #[test]
    fn keeps_run_when_below_threshold() {
        assert_eq!(rtk("a\na\nrest"), Cow::Borrowed("a\na\nrest"));
    }

    #[test]
    fn keeps_blank_runs_which_carry_structure() {
        assert_eq!(rtk("\n\n\n\n\nx"), Cow::Borrowed("\n\n\n\n\nx"));
    }

    #[test]
    fn marker_names_the_dropped_count() {
        assert!(rtk("x\nx\nx").contains("2 repeats dropped"));
    }

    /// The dial has to be observable, not decorative: a pair of lines is below
    /// every off-default rung and above none, so `aggressive` collapses it and
    /// `minimal` declines. If the two ladders ever merged, this is what notices.
    #[test]
    fn collapses_a_pair_only_when_aggressive() {
        let pair = "warn\nwarn\n";
        assert_eq!(rtk_at(pair, Intensity::Minimal), Cow::Borrowed(pair));
        assert!(!rtk_at(pair, Intensity::Aggressive).contains("warn\nwarn"));
    }

    #[test]
    fn collapses_a_quadruple_at_every_rung() {
        let quad = "warn\nwarn\nwarn\nwarn\n";
        for level in [
            Intensity::Minimal,
            Intensity::Standard,
            Intensity::Aggressive,
        ] {
            assert!(
                !rtk_at(quad, level).contains("warn\nwarn\nwarn\nwarn"),
                "{level}"
            );
        }
    }

    #[test]
    fn behaves_like_the_bare_engine_at_the_default_level() {
        let src = "boom\nboom\nboom\nresult\n";
        assert_eq!(rtk_at(src, Intensity::Standard), rtk(src));
    }
}
