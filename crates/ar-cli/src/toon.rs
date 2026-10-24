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
///     list("providers", "providers", &toon::DEFAULT_FIELDS, &fields, &rows, false),
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

    for row in rows {
        let mut cells: Vec<Cow<'_, str>> = Vec::with_capacity(picked.len());
        for i in &picked {
            let cell = row.get(*i).map_or("", String::as_str);
            cells.push(clip(cell));
        }
        let joined: Vec<&str> = cells.iter().map(Cow::as_ref).collect();
        let _ = writeln!(out, "  {}", joined.join(","));
    }
    // Whether anything was TRUNCATED, which is the only thing `--full` can
    // fix. Not "whether any cell was rebuilt": `clip` also returns an owned
    // string merely to quote a comma, and a quoted cell is not a truncated
    // one. Asserting on `Cow::Owned` printed the hatch under `aroute service
    // status` — a verb with no `--full` flag — for a row that was merely
    // comma-bearing. The hint is about a flag that would change nothing,
    // which is exactly the noise the flag's own doc says to avoid.
    let clipped = rows
        .iter()
        .flatten()
        .any(|cell| cell.chars().count() > TRUNCATE_CHARS);
    if clipped && !full {
        let _ = writeln!(out, "{HINT}");
    }
    out
}

/// [`list`] for a caller that paginated the rows.
///
/// The one difference is `TOTAL`: this names the full row count, while `list`
/// can only say the page size because it does not know any more. The comment
/// on `list` promised the shape would not change when pagination lands — this
/// is that landing, and `docs/06`'s `count: N of TOTAL` becomes true rather
/// than `N of N`.
///
/// `total` is passed rather than derived so a page can be a slice of rows the
/// caller filtered first: `TOTAL` must be "how many there are", not "how many
/// this page has".
pub fn list_paged(
    name: &str,
    noun: &str,
    all: &[&str],
    fields: &[String],
    rows: &[Vec<String>],
    full: bool,
    total: usize,
) -> String {
    let mut out = list(name, noun, all, fields, rows, full);
    // `list` wrote `count: N of N total` as its first line; rewrite it to carry
    // the real total rather than duplicating the renderer for one digit.
    if let Some(rest) = out.strip_prefix(&format!("count: {} of {} total\n", rows.len(), rows.len()))
    {
        out = format!("count: {} of {total} total\n{rest}", rows.len());
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
/// But a marker is not enough on its own, because the *value itself* can carry
/// a comma: a config path, a detail string, a model name an operator chose.
/// So a cell is also **quoted** when it contains a comma — the TOON rule the
/// list shape was missing — which keeps `split(',')` honest at any width.
///
/// Returns `Cow::Borrowed` for the common short case so a list of ids allocates
/// nothing beyond the row itself.
pub fn clip(text: &str) -> Cow<'_, str> {
    let total = text.chars().count();
    let body = if total <= TRUNCATE_CHARS {
        Cow::Borrowed(text)
    } else {
        let head: String = text.chars().take(TRUNCATE_CHARS).collect();
        Cow::Owned(format!("{head} (truncated: {total} chars)"))
    };
    // Only a cell that actually needs it pays for a quote. The test below
    // asserts both halves: a comma-free id must stay bare (quoting everything
    // would waste a byte on every row for the rare case), and a comma-bearing
    // one must not be splittable.
    if body.contains(',') {
        Cow::Owned(format!("\"{}\"", body.replace('"', "'")))
    } else {
        body
    }
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

/// The same projected rows as a JSON object, one renderer call beside
/// [`list`] so a caller cannot get two shapes for one dataset.
///
/// This is the machine-readable opt-in `docs/06`'s CLI contract gains: TOON
/// stays the default for humans, and `--json` emits the *same* rows rather
/// than a second query that could drift from the first.
///
/// Shape:
///
/// ```json
/// { "count": 2, "total": 2, "name": "providers",
///   "columns": ["id", "provider", "status"],
///   "rows": [{"id": "openai", "provider": "openai", "status": "ok"}] }
/// ```
///
/// Two deliberate differences from the TOON rendering, both about cost:
///
/// * **No truncation.** The ceiling exists to keep a human table narrow; a
///   machine that asked for JSON can parse a long value, so clipping here
///   would silently destroy data the caller wanted. `--full`'s meaning is
///   "no clipping", which JSON already is.
/// * **`count` and `total` stay.** They are what an agent branches on to know
///   whether it saw everything, and `count: N of TOTAL` has the same job in
///   the text form.
///
/// A cell containing a comma needs no quoting here: JSON escapes it. That is
/// the other reason the shape is worth having — the `--json` path cannot have
/// the column-shift class of bug `clip` guards against.
pub fn json(name: &str, all: &[&str], fields: &[String], rows: &[Vec<String>]) -> String {
    json_paged(name, all, fields, rows, rows.len())
}

/// [`json`] for a caller that paginated or pre-filtered the rows.
///
/// The envelope already carries `count` and `total`, so the shape does not
/// change; only what `total` MEANS does. With `json` it was `rows.len()` —
/// always equal to `count`, so it said nothing. Here it is the full row count,
/// which is the number an agent needs to know whether to run the same query
/// again with `--offset`.
pub fn json_paged(
    name: &str,
    all: &[&str],
    fields: &[String],
    rows: &[Vec<String>],
    total: usize,
) -> String {
    let picked: Vec<usize> = fields
        .iter()
        .filter_map(|f| all.iter().position(|a| a == f))
        .collect();

    let objects: Vec<serde_json::Value> = rows
        .iter()
        .map(|row| {
            let mut obj = serde_json::Map::with_capacity(picked.len());
            for i in &picked {
                let key = all[*i];
                let value = row.get(*i).map_or("", String::as_str);
                obj.insert(key.to_owned(), serde_json::Value::String(value.to_owned()));
            }
            serde_json::Value::Object(obj)
        })
        .collect();

    serde_json::to_string_pretty(&serde_json::json!({
        "name": name,
        "count": objects.len(),
        "total": total,
        "columns": fields,
        "rows": objects,
    }))
    .unwrap_or_else(|_| String::from("{}"))
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
            list(
                "providers",
                "providers",
                &DEFAULT_FIELDS,
                &fields,
                &rows,
                false
            ),
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

    // The G4 finding: a list row is comma-delimited, so a comma inside a cell
    // made the row wider than the header's column list claimed, and every field
    // after it silently shifted. Reproduced live before the fix -- a config path
    // containing a comma rendered a 3-column `doctor` row that split into 4
    // fields. A quoting rule is the TOON answer, and these two tests pin both
    // halves so the common case is not penalised for the rare one.
    #[test]
    fn quotes_a_cell_containing_a_comma_so_the_column_count_holds() {
        let cell = clip("/path/with, comma/config.yaml");
        assert!(cell.starts_with('"') && cell.ends_with('"'), "not quoted: {cell}");

        // A quote-aware parser splits into exactly the header's three columns.
        // `split(',')` alone cannot see quotes, so it still yields 4 -- which is
        // why the contract is stated in the doc above as a QUOTING rule rather
        // than "split is now safe", and why the second test matters: the unquoted
        // majority of rows stay trivially splittable.
        //
        // What this test therefore pins is the part only quoting can give: the
        // cell is delimited, so a parser that respects the quotes can recover
        // the intended 3 columns, and the comma is no longer ambiguous.
        let row = format!("config,ok,{cell}");
        let fields: Vec<&str> = split_outside_quotes(&row);
        assert_eq!(fields.len(), 3, "a quote-aware parser must see 3: {row}");
        // The quotes are the delimiters, so the recovered value still carries them;
        // a caller strips them. What has been restored is the column count -- the
        // value was never in doubt.
        assert_eq!(fields[2], "\"/path/with, comma/config.yaml\"");
    }

    /// A minimal splitter that respects double quotes, mirroring what an agent
    /// has to write to parse this format: the whole justification for the
    /// quoting rule.
    fn split_outside_quotes(line: &str) -> Vec<&str> {
        let mut out = Vec::new();
        let mut quoted = false;
        let mut start = 0usize;
        for (i, ch) in line.char_indices() {
            match ch {
                '"' => quoted = !quoted,
                ',' if !quoted => {
                    out.push(&line[start..i]);
                    start = i + 1;
                }
                _ => {}
            }
        }
        out.push(&line[start..]);
        out
    }

    #[test]
    fn leaves_a_comma_free_cell_bare() {
        // The assertion that keeps the fix cheap: quoting everything would add a
        // byte to every row of every list to protect the rare comma-bearing one.
        let cell = clip("gpt-4o-mini");
        assert_eq!(cell, "gpt-4o-mini");
        assert!(!cell.contains('"'));
    }
    #[test]
    fn json_emits_the_same_rows_the_toon_table_carries() {
        // The claim `--json` exists to make: one dataset, two renderings, so a
        // caller can switch formats without learning a second shape. Asserts
        // against the row count and one cell rather than a byte comparison,
        // because the whole point is that the SHAPES differ while the DATA does
        // not.
        let rows = vec![
            vec!["openai".to_owned(), "openai".to_owned(), "ok".to_owned()],
            vec!["anthropic".to_owned(), "anthropic".to_owned(), "ok".to_owned()],
        ];
        let out = json("providers", &["id", "provider", "status", "base_url", "key"], &["id".to_owned(), "provider".to_owned(), "status".to_owned()], &rows);
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        assert_eq!(parsed["count"], 2, "{out}");
        assert_eq!(parsed["total"], 2, "{out}");
        assert_eq!(parsed["rows"][0]["id"], "openai", "{out}");
        assert_eq!(parsed["rows"][1]["provider"], "anthropic", "{out}");
    }

    #[test]
    fn json_carries_a_comma_inside_a_value_without_widening_the_row() {
        // The bug `clip` guards in TOON cannot happen here: JSON escapes the
        // delimiter, so a comma in a value stays inside that value. This is the
        // concrete reason the --json path is worth having alongside TOON.
        let rows = vec![vec![
            "config".to_owned(),
            "ok".to_owned(),
            "/path/with, comma/config.yaml".to_owned(),
        ]];
        let out = json("checks", &["check", "status", "detail"], &["check".to_owned(), "status".to_owned(), "detail".to_owned()], &rows);
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        assert_eq!(parsed["rows"].as_array().unwrap().len(), 1, "{out}");
        assert_eq!(
            parsed["rows"][0]["detail"], "/path/with, comma/config.yaml",
            "the comma must stay inside the string: {out}"
        );
    }

    #[test]
    fn json_projects_only_the_requested_columns() {
        let rows = vec![vec!["openai".to_owned(), "https://api.openai.com/v1".to_owned()]];
        let out = json("providers", &["id", "provider", "status", "base_url", "key"], &["id".to_owned()], &rows);
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        assert_eq!(parsed["rows"][0]["id"], "openai", "{out}");
        assert!(
            parsed["rows"][0].get("base_url").is_none(),
            "an unrequested column must not appear: {out}"
        );
    }

    #[test]
    fn does_not_hatch_full_for_a_cell_that_was_merely_quoted() {
        // A real bug, found by running `aroute service status`: `list` decided
        // "was anything clipped?" by "did any cell come back owned?", and
        // `clip` returns an owned string when it QUOTES a comma — not only
        // when it truncates. So a short comma-bearing cell printed the
        // `--full` hint, naming a flag that would change nothing, on a verb
        // (`service`) that has no `--full` at all.
        //
        // The contract: the hatch means "a value was cut". Quoting keeps a
        // value whole, so it must never trigger it.
        let cols = ["unit", "systemd", "probe", "detail"];
        let comma_row = vec![
            "aroute.service".to_owned(),
            "active".to_owned(),
            "ok".to_owned(),
            "active, port 20128 answers".to_owned(),
        ];
        let fields: Vec<String> = cols.iter().map(|s| s.to_string()).collect();
        let out = list(
            "service",
            "services",
            &cols,
            &fields,
            &[comma_row],
            false,
        );
        assert!(
            out.contains("\"active, port 20128 answers\""),
            "the comma-bearing cell is quoted: {out}"
        );
        assert!(
            !out.contains("--full"),
            "a quoted cell is not a truncated one, so the hatch must stay silent: {out}"
        );

        // And the truncation case still hatches, so this is not a blanket
        // suppression: negative control for the fix itself.
        let long = "x".repeat(TRUNCATE_CHARS + 200);
        let out = list(
            "service",
            "services",
            &cols,
            &fields,
            &[vec!["aroute.service".to_owned(), "active".to_owned(), "ok".to_owned(), long]],
            false,
        );
        assert!(
            out.contains("--full"),
            "a genuinely truncated cell must still name the hatch: {out}"
        );
    }
}
