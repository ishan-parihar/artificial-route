//! `lite` — the always-on, lossless-in-intent baseline.
//!
//! Strips two things from a prompt and nothing else:
//!
//! * **whitespace** — runs of three or more newlines collapse to one blank
//!   line, and trailing spaces/tabs go.
//! * **comments** — `//` and `#` line comments outside fenced blocks.
//!
//! Fenced code (` ``` ` / `~~~`) is copied **byte for byte**. That is the whole
//! contract of this transform: it runs on every request, so it must never be the
//! thing that breaks a prompt. A trailing-space or comment rule that reaches
//! inside a fence changes code, and changed code is a wrong answer the model
//! cannot tell from a right one.
//!
//! Ported from `../OmniRoute/open-sse/services/compression/lite.ts` —
//! `normalizeMessageWhitespace`, plus the fenced-block guard that
//! `engines/rtk/index.ts` applies before its `stripCode` pass. Not ported: the
//! image-URL placeholder (needs request JSON, not a string) and the tool-result
//! truncation cap (a budget decision, owned by the plan resolver).

use std::borrow::Cow;

use crate::stats::Stats;

/// Strips trailing whitespace and line comments outside fenced code blocks.
///
/// Returns [`Cow::Borrowed`] when the text has nothing to strip, so the
/// no-op case costs one read-only scan and zero allocations.
#[must_use]
pub fn lite(text: &str) -> Cow<'_, str> {
    if !needs_rewrite(text) {
        return Cow::Borrowed(text);
    }
    Cow::Owned(rewrite(text))
}

/// [`lite`] plus the savings it produced.
#[must_use]
pub fn lite_with_stats(text: &str) -> (Cow<'_, str>, Stats) {
    let out = lite(text);
    let stats = Stats::between(text, &out);
    (out, stats)
}

/// Cheap read-only scan for anything [`rewrite`] could remove.
///
/// Exists so the common case — an already-clean prompt — never allocates. It is
/// deliberately over-eager: a false positive costs one [`String::with_capacity`]
/// and the final `== text` check hands the borrow back, so a sloppy scan here is
/// only ever slower, never wrong.
fn needs_rewrite(text: &str) -> bool {
    let b = text.as_bytes();
    let mut newline_run = 0usize;
    for (i, &c) in b.iter().enumerate() {
        match c {
            b'\n' => {
                newline_run += 1;
                if newline_run >= 3 {
                    return true;
                }
            }
            b' ' | b'\t' => {
                if b.get(i + 1).is_none_or(|&n| n == b'\n') {
                    return true;
                }
                newline_run = 0;
            }
            b'/' => {
                if b.get(i + 1) == Some(&b'/') && (i == 0 || b[i - 1].is_ascii_whitespace()) {
                    return true;
                }
                newline_run = 0;
            }
            b'#' => {
                if hash_is_comment(&text[i + 1..]) {
                    return true;
                }
                newline_run = 0;
            }
            _ => newline_run = 0,
        }
    }
    false
}

/// One pass over the lines. See [`rewrite`] for why fences are copied verbatim.
fn rewrite(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut fenced = false;
    let mut wrote_any = false;
    let mut prev_blank = false;

    for line in text.split('\n') {
        let is_fence = is_fence_line(line);
        // Unterminated fence (more common than it should be in a tool result) leaves
        // `fenced` set: the rest of the text is then treated as code. Wrong in one
        // direction only — too conservative.
        if is_fence {
            fenced = !fenced;
        }
        let verbatim = fenced || is_fence;

        let kept = if verbatim {
            line
        } else {
            strip_comment(line).trim_end()
        };
        // A blank line inside a fence is code, so the run cap must not see it.
        let blank = !verbatim && kept.is_empty();
        if blank && prev_blank {
            continue;
        }
        if wrote_any {
            out.push('\n');
        }
        out.push_str(kept);
        wrote_any = true;
        prev_blank = blank;
    }

    if out == text {
        // `needs_rewrite` was over-eager (a protected `#` heading, a `//` inside a
        // URL). Give the borrow back rather than shipping an identical copy.
        return text.to_owned();
    }
    out
}

fn is_fence_line(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("```") || t.starts_with("~~~")
}

/// Byte offset of the first line comment on `line`, if any.
fn strip_comment(line: &str) -> &str {
    comment_cut(line).map_or(line, |i| &line[..i])
}

/// Walks `line` once, tracking string literals, and returns where the first real
/// comment begins.
///
/// The literal tracking is not decoration: `https://example.com/x` and
/// `"a // b"` both contain `//`, and stripping either one silently corrupts a
/// prompt. A single pass is also the only way to know which one you hit.
fn comment_cut(line: &str) -> Option<usize> {
    let b = line.as_bytes();
    let mut quote: Option<u8> = None;
    let mut i = 0usize;
    while i < b.len() {
        let c = b[i];
        if let Some(open) = quote {
            if c == b'\\' {
                i += 2;
                continue;
            }
            if c == open {
                quote = None;
            }
            i += 1;
            continue;
        }
        match c {
            b'"' | b'\'' => {
                quote = Some(c);
                i += 1;
            }
            // Word-boundary only: `path//x` and `http://h` are not comments.
            b'/' if b.get(i + 1) == Some(&b'/') && (i == 0 || b[i - 1].is_ascii_whitespace()) => {
                return Some(i);
            }
            b'#' if (i == 0 || b[i - 1].is_ascii_whitespace())
                && hash_is_comment(&line[i + 1..]) =>
            {
                return Some(i);
            }
            _ => i += 1,
        }
    }
    None
}

/// Is the `#` at the start of `rest` a comment rather than structure?
///
/// Rejects markdown headings (`# Title`), shebangs, and the preprocessor /
/// shell / Julia words where `#` is syntax. `rest` is everything after the `#`.
fn hash_is_comment(rest: &str) -> bool {
    let mut chars = rest.chars();
    let Some(first) = chars.next() else {
        // A lone trailing `#` (an anchor link) is worth nothing.
        return false;
    };
    if first.is_whitespace() {
        return false;
    }
    const DIRECTIVE: [&str; 9] = [
        "!", "include", "import", "set", "show", "let", "pragma", "define", "f",
    ];
    !DIRECTIVE.iter().any(|d| rest.starts_with(d))
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::{comment_cut, lite, lite_with_stats};

    #[test]
    fn keeps_code_when_lite() {
        let src = "before\n```rust\nlet x  =  1;  // keep me\n\n\nfn f() {}\n```\nafter   \n";
        let out = lite(src);
        assert!(out.contains("```rust\nlet x  =  1;  // keep me\n\n\nfn f() {}\n```"));
    }

    #[test]
    fn borrows_when_text_already_clean() {
        assert!(matches!(
            lite("fn main() { println!(\"hi\"); }"),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn strips_url_when_it_looks_like_a_comment() {
        assert_eq!(comment_cut("see https://x.dev/y"), None);
    }

    #[test]
    fn keeps_heading_when_hash_is_markdown() {
        assert_eq!(comment_cut("# Title"), None);
    }

    #[test]
    fn reports_no_savings_when_only_fences_present() {
        assert_eq!(lite_with_stats("```\n a \n```").0, "```\n a \n```");
    }
}
