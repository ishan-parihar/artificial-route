//! The dashboard's per-key request-rate ceiling, read from its integrated DB,
//! plus the one renderer for the config's `limits:` section.
//!
//! Two readers of this module, one writer's discipline: `aroute limits set`
//! and `clear` edit the section, and `aroute sync` carries the dashboard's
//! `rpm` into it — all three go through [`render`], so the section's spelling
//! and indentation have exactly one source.

use std::collections::BTreeMap;
use std::path::Path;

use ar_config::{Limit, Limits};

/// Reads every active dashboard key that names a per-minute request ceiling.
///
/// One column maps 1:1 onto the config block's `rpm` arm —
/// `api_keys.max_requests_per_minute` — and that is the whole carry. The
/// dashboard's dollar and token ceilings are *windows* (daily, weekly,
/// monthly); this engine's arms are cumulative, so carrying them would change
/// what the operator asked for rather than move it (docs/15's non-goals name
/// the rolling window as a different shape).
///
/// Read-only with the same graceful-absence posture as the other importers:
/// the dashboard may be running while sync runs, and a DB that cannot be
/// opened — or predates the column — carries nothing rather than taking the
/// sync down.
pub(crate) fn read_rpm(path: &Path) -> BTreeMap<String, u32> {
    let Ok(conn) = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    ) else {
        eprintln!("ar: dashboard db unreadable; limits not carried");
        return BTreeMap::new();
    };
    let Ok(mut rows) = conn.prepare(
        "SELECT name, max_requests_per_minute FROM api_keys \
         WHERE is_active = 1 AND revoked_at IS NULL \
           AND max_requests_per_minute IS NOT NULL AND max_requests_per_minute > 0",
    ) else {
        // A schema predating the column has nothing to say, which is not a
        // failure the sync should report.
        return BTreeMap::new();
    };
    let Ok(mapped) = rows.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?))
    }) else {
        return BTreeMap::new();
    };
    mapped.filter_map(Result::ok).collect()
}

/// Renders the whole `limits:` block: only the rows that exist, only the arms
/// that are set, `refuse_unpriced` only when true — the same elisions serde's
/// `skip_serializing_if` makes, in the block style a reader can scan.
///
/// Callers do not render an empty block: [`Limits::is_empty`] means there is
/// no section to write, and every caller checks before it splices.
pub(crate) fn render(limits: &Limits) -> String {
    let mut out: Vec<String> = vec!["limits:".to_owned()];
    if !limits.default.is_empty() {
        out.push("  default:".to_owned());
        arms(&limits.default, 4, &mut out);
    }
    if !limits.keys.is_empty() {
        out.push("  keys:".to_owned());
        for (name, row) in &limits.keys {
            out.push(format!("    {name}:"));
            arms(row, 6, &mut out);
        }
    }
    out.join("\n") + "\n"
}

/// One row's set arms, indented `depth` spaces.
fn arms(row: &Limit, depth: usize, out: &mut Vec<String>) {
    let pad = " ".repeat(depth);
    if let Some(rpm) = row.rpm {
        out.push(format!("{pad}rpm: {rpm}"));
    }
    if let Some(usd) = row.usd_micros {
        out.push(format!("{pad}usd_micros: {usd}"));
    }
    if let Some(tokens) = row.tokens {
        out.push(format!("{pad}tokens: {tokens}"));
    }
    if row.refuse_unpriced {
        out.push(format!("{pad}refuse_unpriced: true"));
    }
}
