//! TOON rendering.
//!
//! `docs/06-axi-mcp.md` is normative: TOON on stdout, ~40% fewer tokens than
//! the equivalent JSON, list rows defaulting to 3-4 fields, and an explicit
//! `count: N of TOTAL` so an agent never has to re-run to learn the total.
//! Bodies over [`TRUNCATE_CHARS`] are cut with a `(truncated, TOTAL chars)`
//! marker and, only then, a `--full` hint. Conversion happens here, at the
//! output boundary; nothing upstream of this module emits TOON.

use std::borrow::Cow;
use std::fmt::Write as _;

/// Default column set for a list, per `docs/06`.
///
/// Every list command must keep these three columns in positions 0..=2 (a unit
/// test in `commands` enforces it) so one default serves all of them.
pub const DEFAULT_FIELDS: [&str; 3] = ["id", "provider", "status"];

/// Ceiling on one rendered value, in chars. `docs/06` allows 500-1500.
///
/// `provisional:` 1000 sits mid-range; nothing measures where an LLM body stops
/// being useful to an agent. Raise it when a real transcript overflows badly.
pub const TRUNCATE_CHARS: usize = 1000;

/// Renders `rows` as a TOON table, or a definitive-empty line when there are
/// none.
///
/// `all` is every column a row carries, in order; `fields` selects which of them
/// appear. Selection is positional against `all`, so callers validate the names
/// first (see `commands::columns`) and a typo fails loud rather than emitting a
/// header that disagrees with its rows.
///
/// An empty list is never blank: `docs/06` requires the zero to be stated with
/// context, so the agent can tell "no rows" from "the command failed".
///
/// ```
/// use crate::toon;
///
/// let rows = vec![vec!["openai".to_string(), "openai".to_string(), "configured".to_string()]];
/// let fields: Vec<String> = ["id", "status"].iter().map(|s| s.to_string()).collect();
/// assert_eq!(
///     toon::list("providers", "providers", &toon::DEFAULT_FIELDS, &fields, &rows, false),
///     "count: 1 of 1 total\nproviders[1]{id,status}:\n  openai,configured\n",
/// );
/// ```
pub fn list(
    name: &str,
    noun: &str,
    all: &[&str],
    fields: &[String],
    rows: &[Vec<String>],
    full: bool,
) -> String {
    if rows.is_empty() {
        return format!("{name}: 0 {noun} found\n");
    }

    let picked: Vec<usize> = fields
        .iter()
        .filter_map(|f| all.iter().position(|a| a == f))
        .collect();

    let mut out = String::new();
    // TOTAL is the full row count. There is no pagination at P0, so it equals
    // the page size, but the field is what agents branch on, so it is always
    // emitted and the shape does not change when pagination lands.
    let _ = writeln!(out, "count: {} of {} total", rows.len(), rows.len());
    let _ = writeln!(out, "{name}[{}]{{{}}}:", rows.len(), fields.join(","));

    let mut clipped = false;
    for row in rows {
        let mut cells: Vec<Cow<'_, str>> = Vec::with_capacity(picked.len());
        for i in &picked {
            let cell = row.get(*i).map_or("", String::as_str);
            let clipped_cell = clip(cell);
            clipped |= matches!(clipped_cell, Cow::Owned(_));
            cells.push(clipped_cell);
        }
        let joined: Vec<&str> = cells.iter().map(Cow::as_ref).collect();
        let _ = writeln!(out, "  {}", joined.join(","));
    }
    if clipped && !full {
        let _ = writeln!(out, "{HINT}");
    }
    out
}

/// Renders one value, cutting it to [`TRUNCATE_CHARS`] with a comma-free
/// marker.
///
/// The marker must not contain a comma: a list row is comma-delimited, so a
/// comma in a cell makes the row one column wider than the header claims. The
/// standalone body marker below does not share that constraint.
///
/// Returns `Cow::Borrowed` for the common short case so a list of ids allocates
/// nothing beyond the row itself.
pub fn clip(text: &str) -> Cow<'_, str> {
    let total = text.chars().count();
    if total <= TRUNCATE_CHARS {
        return Cow::Borrowed(text);
    }
    let head: String = text.chars().take(TRUNCATE_CHARS).collect();
    Cow::Owned(format!("{head} (truncated: {total} chars)"))
}

/// Renders a free-standing body, cut to [`TRUNCATE_CHARS`] with the marker
/// `docs/06` names, plus the `--full` hatch.
///
/// The hatch is emitted only when something was actually truncated: a hint
/// about a flag that would change nothing is noise an agent has to parse past.
pub fn body(text: &str, full: bool) -> String {
    let total = text.chars().count();
    if full || total <= TRUNCATE_CHARS {
        return text.to_owned();
    }
    let head: String = text.chars().take(TRUNCATE_CHARS).collect();
    format!("{head}\n(truncated, {total} chars)\n{HINT}")
}

/// The `--full` escape hatch. One constant so the list and body paths cannot
/// drift into two different wordings.
const HINT: &str = "help: re-run with --full for the whole value";

/// Every column of `cols`, in order.
///
/// The projection for a table that has no `--fields` and no meaningful default —
/// `doctor`'s `check,status,detail` does not share `DEFAULT_FIELDS`, so asking
/// `list` for the default there would print a header and no cells.
pub fn every_field(cols: &[&str]) -> Vec<String> {
    cols.iter().map(|s| (*s).to_string()).collect()
}

/// Parses a `--fields a,b,c` value, falling back to [`DEFAULT_FIELDS`].
///
/// An empty or whitespace-only value means "unspecified", not "no columns", so it
/// falls back rather than producing a header with nothing after the braces.
pub fn fields_or_default(raw: Option<&str>) -> Vec<String> {
    let picked: Vec<String> = raw
        .map(|r| {
            r.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    if picked.is_empty() {
        DEFAULT_FIELDS.iter().map(|s| (*s).to_string()).collect()
    } else {
        picked
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> Vec<String> {
        DEFAULT_FIELDS.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn states_zero_when_list_empty() {
        assert_eq!(
            list("models", "models", &DEFAULT_FIELDS, &defaults(), &[], false),
            "models: 0 models found\n"
        );
    }

    #[test]
    fn falls_back_to_defaults_when_fields_blank() {
        assert_eq!(fields_or_default(Some("  ")), DEFAULT_FIELDS.to_vec());
    }

    #[test]
    fn projects_requested_columns_only() {
        let rows = vec![vec![
            "openai".to_string(),
            "openai".to_string(),
            "configured".to_string(),
        ]];
        let fields = fields_or_default(Some("id,status"));
        assert_eq!(
            list("providers", "providers", &DEFAULT_FIELDS, &fields, &rows, false),
            "count: 1 of 1 total\nproviders[1]{id,status}:\n  openai,configured\n"
        );
    }

    #[test]
    fn borrows_when_value_fits() {
        assert!(matches!(clip("openai"), Cow::Borrowed("openai")));
    }

    #[test]
    fn marker_omits_comma_when_value_clipped() {
        let long = "x".repeat(TRUNCATE_CHARS + 1);
        let clipped = clip(&long);
        assert!(!clipped.contains(','));
        assert!(clipped.ends_with(&format!("(truncated: {} chars)", long.chars().count())));
    }

    #[test]
    fn hatches_full_flag_only_when_truncated() {
        let long = "x".repeat(TRUNCATE_CHARS + 1);
        assert!(body(&long, true).ends_with(long.chars().last().unwrap()));
    }

    #[test]
    fn omits_hatch_when_body_fits() {
        assert_eq!(body("ok", false), "ok");
    }
}
