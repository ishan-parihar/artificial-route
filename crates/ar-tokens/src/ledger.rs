//! Append-only usage ledger with per-key caps.
//!
//! Ported from `../OmniRoute/src/lib/usage/{usageLedger,budgetGuard}.ts`.
//!
//! Two rules carry the whole design:
//!
//! * **Append-only.** A `BEFORE UPDATE` and a `BEFORE DELETE` trigger make it
//!   impossible to rewrite a usage row through this connection, so the SQLite
//!   file itself is the audit trail rather than a convention.
//! * **Quota is not dollars.** A subscription provider costs $0 and must still
//!   be capped, so a cap has a USD arm *and* a token arm and either can deny.
//!   The token arm is what stops a flat-rate key from running unbounded, and it
//!   is also the only arm that can deny a model with no pricing row at all.

use std::fmt;
use std::path::Path;

use rusqlite::{Connection, params};

use crate::error::TokenError;
use crate::meta::ResponseMeta;
use crate::pricing::{Cost, PricingTable, Usd};
use crate::usage::NormalizedUsage;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS usage (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    key_id            TEXT    NOT NULL,
    provider          TEXT    NOT NULL,
    model             TEXT    NOT NULL,
    prompt_tokens     INTEGER NOT NULL,
    completion_tokens INTEGER NOT NULL,
    total_tokens      INTEGER NOT NULL,
    cost_micros       INTEGER NOT NULL,
    priced            INTEGER NOT NULL,
    created_at        INTEGER NOT NULL,
    combo             TEXT    NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS usage_by_key ON usage (key_id, id);
CREATE TRIGGER IF NOT EXISTS usage_no_update BEFORE UPDATE ON usage
    BEGIN SELECT RAISE(ABORT, 'usage ledger is append-only'); END;
CREATE TRIGGER IF NOT EXISTS usage_no_delete BEFORE DELETE ON usage
    BEGIN SELECT RAISE(ABORT, 'usage ledger is append-only'); END;
CREATE TABLE IF NOT EXISTS key_caps (
    key_id     TEXT    PRIMARY KEY,
    usd_micros INTEGER,
    token_cap  INTEGER,
    refuse_unpriced INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS failures (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    key_id     TEXT    NOT NULL,
    provider   TEXT    NOT NULL DEFAULT '-',
    model      TEXT    NOT NULL,
    status     INTEGER NOT NULL,
    error      TEXT    NOT NULL,
    created_at INTEGER NOT NULL
);
";

/// Failed requests kept for the /ui request log. A table of its own, beside
/// `usage` rather than in it: a refusal costs nothing, so it must never reach
/// the spend sums, the caps or the bill the append-only rows exist to protect.
/// Trimmed on write, so a flood of bad requests cannot grow the file.
const FAILURE_KEEP: i64 = 10_000;
/// Longest error text kept per failure, in characters.
const FAILURE_TEXT_MAX: usize = 300;

/// Column added after the first release of the schema. `CREATE TABLE IF NOT
/// EXISTS` skips a table that already exists, so an existing ledger file keeps
/// the old shape and every `INSERT`/`SELECT` naming the new column would fail.
/// The guard is `PRAGMA table_info`, which is the cheapest way to ask "does this
/// column exist" without a version table nobody else would read.
const MIGRATE_REFUSE_UNPRICED: &str = "
CREATE TABLE key_caps_migrated (
    key_id     TEXT    PRIMARY KEY,
    usd_micros INTEGER,
    token_cap  INTEGER,
    refuse_unpriced INTEGER NOT NULL DEFAULT 0
);
INSERT INTO key_caps_migrated (key_id, usd_micros, token_cap, refuse_unpriced)
    SELECT key_id, usd_micros, token_cap, 0 FROM key_caps;
DROP TABLE key_caps;
ALTER TABLE key_caps_migrated RENAME TO key_caps;
";

/// `failures.provider`, added after the table first shipped. An existing ledger
/// keeps the old shape, so its old rows read as `-`, the value they always had.
const MIGRATE_FAILURE_PROVIDER: &str =
    "ALTER TABLE failures ADD COLUMN provider TEXT NOT NULL DEFAULT '-';";

/// `usage.combo`, added after the table first shipped.
///
/// The column names the COMBO the client asked for, which is a different fact
/// from `model` — that one is the serving target's own model, because the
/// pricing lookup and the spend row describe what the provider answered.
///
/// Both are needed and neither replaces the other. Per-combo figures cannot
/// be derived from the serving target: two combos may share a target, and
/// then a join attributes one request's spend to both of them and the panel
/// shows a spend that did not happen. The column is what makes the
/// attribution exact.
///
/// An existing ledger keeps the old shape, so its old rows read as `''` — the
/// value they always had, before the column existed.
const MIGRATE_USAGE_COMBO: &str = "ALTER TABLE usage ADD COLUMN combo TEXT NOT NULL DEFAULT '';";

/// The covering index a per-target budget reads through.
///
/// `usage_by_key` is `(key_id, id)`, so the key-wide `SUM` has to visit every
/// row the key ever recorded — on a busy key that is tens of thousands of rows
/// per request. This index carries every column the grouped query needs
/// (`total_tokens`, `created_at`, `cost_micros`) alongside the three it filters
/// on, so the aggregate never touches the table at all: the plan is
/// `SEARCH usage USING COVERING INDEX`.
///
/// It is **not** a replacement for `usage_by_key`: the per-key log still needs
/// the row order that one gives.
const INDEX_USAGE_BY_TARGET: &str = "CREATE INDEX IF NOT EXISTS usage_by_target \
     ON usage (key_id, provider, model, total_tokens, created_at, cost_micros);";

const INSERT_USAGE: &str = "
INSERT INTO usage
    (key_id, provider, model, prompt_tokens, completion_tokens, total_tokens,
     cost_micros, priced, created_at, combo)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
";

const UPSERT_CAP: &str = "
INSERT INTO key_caps (key_id, usd_micros, token_cap, refuse_unpriced) VALUES (?1, ?2, ?3, ?4)
ON CONFLICT(key_id) DO UPDATE SET
    usd_micros = ?2, token_cap = ?3, refuse_unpriced = ?4
";

const REPORT: &str = "
SELECT key_id, provider, model, total_tokens, cost_micros, priced
FROM usage ORDER BY id DESC LIMIT ?1
";

/// One completed request, ready to record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry<'a> {
    /// Which API key spent it. The unit every cap is enforced against.
    pub key_id: &'a str,
    /// Provider that served it.
    pub provider: &'a str,
    /// Provider-local model name.
    pub model: &'a str,
    /// The COMBO the client asked for, when the request named one.
    ///
    /// Empty for a request that named no combo — a flat provider request, or
    /// a row written before the column existed. It is a different fact from
    /// `model`, which is the serving target's own name: the combo is what the
    /// client asked for and the model is what answered, and a per-combo
    /// figure needs the first while the pricing lookup needs the second.
    pub combo: &'a str,
    /// What it cost in tokens.
    pub usage: NormalizedUsage,
    /// What it cost in dollars.
    pub cost: Cost,
    /// Unix seconds.
    pub created_at: i64,
}

/// Who a request was, and what it was asked for.
///
/// The four names a ledger row carries, gathered so the write path does not
/// pass four positional strings a caller can transpose. The `combo` is the
/// name the CLIENT asked for and the `model` is the one that ANSWERED —
/// different facts, and two combos sharing a target are exactly the case
/// where confusing them would double-count a spend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Who<'a> {
    /// Which API key spent it.
    pub key_id: &'a str,
    /// Provider that served it.
    pub provider: &'a str,
    /// Provider-local model name.
    pub model: &'a str,
    /// The combo the client asked for, or `""` when it named none.
    pub combo: &'a str,
}

/// What a key has spent so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Spend {
    /// Dollars recorded against the key.
    pub usd: Usd,
    /// Tokens recorded against the key.
    pub tokens: u64,
}

/// One target's share of a key's spend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TargetSpend {
    /// The spend this target accrued, over all time.
    pub total: Spend,
    /// The spend this target accrued inside the window, when one was asked for.
    pub windowed: Spend,
}

/// One target's row in a [`SpendBreakdown`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetSpendRow {
    /// The provider that served.
    pub provider: String,
    /// The provider-local model that served.
    pub model: String,
    /// The spend, split by window.
    pub spend: TargetSpend,
}

/// A key's whole spend, and each target's share of it.
///
/// The shape a budget needs: the total is the key-wide ceiling, and each
/// target's row is its own ceiling. One read of the ledger, so the two
/// ceilings cannot be evaluated against different views of the same table.
///
/// The rows are a `Vec` rather than a map because the lookup happens on the
/// request path: a map keyed by `(String, String)` would allocate a pair of
/// owned strings on every lookup, and there is one lookup per target in the
/// chain — a handful, against a ledger whose rows are already in memory.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SpendBreakdown {
    /// The key's total, over every target and all time.
    pub total: Spend,
    /// The key's total inside the window, when one was asked for.
    pub windowed: Spend,
    /// Each target's share, in the order the ledger grouped them.
    pub targets: Vec<TargetSpendRow>,
}

impl SpendBreakdown {
    /// One target's spend, or zero when the target has never served.
    #[must_use]
    pub fn target(&self, provider: &str, model: &str) -> TargetSpend {
        self.targets
            .iter()
            .find(|r| r.provider == provider && r.model == model)
            .map_or_else(TargetSpend::default, |r| r.spend)
    }
}

/// A per-key ceiling. `None` on either arm means that arm is unlimited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cap {
    /// The key this cap governs.
    pub key_id: String,
    /// Micro-dollar ceiling. Denied at or above it.
    pub usd_micros: Option<u64>,
    /// Token ceiling. Denied at or above it. The arm that still bites when
    /// every request through the key costs $0.
    pub tokens: Option<u64>,
    /// Fail closed on a model with no pricing row: deny the request rather than
    /// let it through at an unknown cost. Default `false`.
    ///
    /// Off by default because the two arms cannot police a cost of "unknown",
    /// and an unpriced model against a $0 balance reads as "free" -- which is
    /// how a flat-rate key quietly becomes unmetered. A caller that must not
    /// serve unpriced traffic (a stream that bills clients in dollars) opts in
    /// here; a caller that would rather serve and record than refuse leaves it
    /// off and reads [`DenyReason::Unpriced`] nowhere.
    pub refuse_unpriced: bool,
}

/// Why a request was refused. Always an HTTP 402.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DenyReason {
    /// The key's dollar ceiling is reached, counting the pending request.
    UsdCap {
        /// Dollars already spent.
        spent: Usd,
        /// The configured ceiling.
        cap: Usd,
    },
    /// The key's token ceiling is reached, counting the pending request.
    TokenCap {
        /// Tokens already spent.
        spent: u64,
        /// The configured ceiling.
        cap: u64,
    },
    /// The model has no pricing row and the key's cap asked to fail closed on
    /// that. Not a budget problem: a budget cannot be evaluated.
    Unpriced,
    /// The spend table could not be read at all, so whether the key is within
    /// its ceiling is unknowable. Carried so a policy layer can fail closed on
    /// a ledger that errors under the request rather than serving at an
    /// unevaluated cost.
    Unauditable,
}

impl DenyReason {
    /// HTTP status this refusal maps to.
    ///
    /// 402 Payment Required, not 429: the key is not being rate-limited, its
    /// budget is spent, and retrying the same request will keep failing.
    #[must_use]
    pub fn status(&self) -> http::StatusCode {
        http::StatusCode::PAYMENT_REQUIRED
    }

    /// The machine-readable code for the error envelope, one per variant.
    ///
    /// A client branching on this can tell "my budget is spent" from "the
    /// model is unpriced" without matching a sentence, which is the same
    /// contract the router's `code`/`reason` pair makes elsewhere.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::UsdCap { .. } => "usd_cap",
            Self::TokenCap { .. } => "token_cap",
            Self::Unpriced => "unpriced",
            Self::Unauditable => "unauditable",
        }
    }
}

/// The refusal a client can read. The ledger's own words, so every surface
/// that carries a verdict — the router's 402 body, a future CLI report —
/// renders the same denial identically.
impl fmt::Display for DenyReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UsdCap { spent, cap } => write!(
                f,
                "this key has spent ${}, which reaches its ${} ceiling",
                spent.as_decimal_string(),
                cap.as_decimal_string()
            ),
            Self::TokenCap { spent, cap } => write!(
                f,
                "this key has spent {spent} tokens, which reaches its {cap}-token ceiling"
            ),
            Self::Unpriced => write!(
                f,
                "the model has no pricing row, and this key refuses unpriced requests"
            ),
            Self::Unauditable => write!(
                f,
                "the spend ledger could not be read, so this key's ceilings cannot be checked"
            ),
        }
    }
}

/// Whether a request may proceed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Within every configured cap.
    Allow,
    /// At or past a cap. 402.
    Deny(DenyReason),
}

impl Verdict {
    /// The refusal, if this is a [`Verdict::Deny`].
    #[must_use]
    pub fn reason(self) -> Option<DenyReason> {
        match self {
            Self::Allow => None,
            Self::Deny(reason) => Some(reason),
        }
    }
}

/// One row of the `ar cost-report` listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerRow {
    /// API key that spent it.
    pub key_id: String,
    /// Provider that served it.
    pub provider: String,
    /// Provider-local model name.
    pub model: String,
    /// Prompt + completion tokens.
    pub total_tokens: u32,
    /// Dollars charged.
    pub cost_usd: Usd,
    /// `false` when the row had no pricing row behind it.
    pub priced: bool,
}

/// The `ar cost-report` data shape: the rows plus the all-key totals, which is
/// what the TOON renderer prints as its `totals` and `count` lines.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CostReport {
    /// Most recent first, capped at the requested limit.
    pub rows: Vec<LedgerRow>,
    /// Totals across every key in the ledger.
    pub totals: Spend,
}

impl CostReport {
    /// Renders the report as TOON: a header, one row per record, totals, count.
    ///
    /// The shape `ar cost-report` prints, kept here so the field list and the
    /// empty-state wording cannot drift from the query that fills it.
    ///
    /// ```
    /// let report = ar_tokens::CostReport::default();
    /// assert_eq!(report.toon(), "cost: 0 rows — no usage recorded\n");
    /// ```
    #[must_use]
    pub fn toon(&self) -> String {
        if self.rows.is_empty() {
            return "cost: 0 rows — no usage recorded\n".to_owned();
        }
        let mut out = String::with_capacity(80 * self.rows.len());
        out.push_str("cost[");
        out.push_str(&self.rows.len().to_string());
        out.push_str("]{key_id,provider,model,tokens,usd}\n");
        for row in &self.rows {
            out.push_str(&row.key_id);
            out.push('\t');
            out.push_str(&row.provider);
            out.push('\t');
            out.push_str(&row.model);
            out.push('\t');
            out.push_str(&row.total_tokens.to_string());
            out.push('\t');
            out.push_str(&format_usd(row.cost_usd));
            out.push('\n');
        }
        out.push_str(&format!(
            "totals{{tokens:{},usd:{}}}\ncount: {}\n",
            self.totals.tokens,
            format_usd(self.totals.usd),
            self.rows.len()
        ));
        out
    }
}

fn format_usd(usd: Usd) -> String {
    usd.as_decimal_string()
}

/// One spend slice on the /ui analytics charts: a calendar day, a model, or a
/// provider with its summary over that slice.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SpendBucket {
    /// Which slice this totals: `YYYY-MM-DD`, a model name, or a provider name.
    pub name: String,
    /// Completed requests in the slice.
    pub requests: u64,
    /// Prompt + completion tokens.
    pub tokens: u64,
    /// Micro-dollars charged.
    pub cost_usd_micros: u64,
}

/// The /ui usage pane's lens over the ledger: spend by calendar day (anchored
/// on the newest row so an idle ledger stays non-empty), by model, by
/// provider, and by the combo the client asked for. [`Ledger::report`] stays
/// wholesale; this is the aggregation.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Analytics {
    /// One bucket per calendar day with usage, oldest first, last 30 days.
    pub days: Vec<SpendBucket>,
    /// Per-model buckets, most expensive first, capped at the requested limit.
    pub models: Vec<SpendBucket>,
    /// Per-provider buckets, most expensive first, capped at the requested
    /// limit.
    pub providers: Vec<SpendBucket>,
    /// Per-combo buckets, most expensive first, capped at the requested limit.
    ///
    /// The combo is the name the CLIENT asked for, so this is the only figure
    /// that answers "what did this model name cost me". The `models` buckets
    /// answer a different question — what the providers charged for — and the
    /// two are not interchangeable when two combos share a target.
    pub combos: Vec<SpendBucket>,
}

const ANALYTICS_DAYS: &str = "
SELECT STRFTIME('%Y-%m-%d', created_at, 'unixepoch'), COUNT(*),
       COALESCE(SUM(total_tokens), 0), COALESCE(SUM(cost_micros), 0)
FROM usage
WHERE created_at >= ?1
GROUP BY 1 ORDER BY 1
";

// Ranked by dollars so a /ui table reads top-down like a bill, with the name
// as tiebreak so equal-cost rows do not shuffle between polls.
const ANALYTICS_MODELS: &str = "
SELECT model, COUNT(*), COALESCE(SUM(total_tokens), 0), COALESCE(SUM(cost_micros), 0)
FROM usage WHERE created_at >= ?1 GROUP BY model ORDER BY 4 DESC, 1 ASC LIMIT ?2
";
const ANALYTICS_PROVIDERS: &str = "
SELECT provider, COUNT(*), COALESCE(SUM(total_tokens), 0), COALESCE(SUM(cost_micros), 0)
FROM usage WHERE created_at >= ?1 GROUP BY provider ORDER BY 4 DESC, 1 ASC LIMIT ?2
";
/// The ledger rows grouped by the COMBO the client asked for.
///
/// Grouped by the recorded `combo`, not by a join against the serving
/// target: two combos may share a target, and a join would then attribute
/// one request's spend to both of them — a per-combo column that sums to more
/// than the total spend, which is a budget number that is wrong.
const ANALYTICS_COMBOS: &str = "
SELECT combo, COUNT(*), COALESCE(SUM(total_tokens), 0), COALESCE(SUM(cost_micros), 0)
FROM usage WHERE created_at >= ?1 AND combo <> '' GROUP BY combo ORDER BY 4 DESC, 1 ASC LIMIT ?2
";

/// Newest row across both tables: the anchor a date range counts back from, so
/// an idle ledger shows its last active window instead of an empty one.
const NEWEST: &str = "
SELECT MAX(m) FROM (SELECT MAX(created_at) AS m FROM usage
                    UNION ALL SELECT MAX(created_at) FROM failures)
";

// Answered rows and failures as one stream. A failure names the last provider
// the chain tried, or `-` when it tried none, and `200` marks an answered row.
const LOG: &str = "
SELECT * FROM (
    SELECT created_at, key_id, provider, model, total_tokens, cost_micros, priced,
           200 AS status, '' AS error FROM usage
    UNION ALL
    SELECT created_at, key_id, provider, model, 0, 0, 1, status, error FROM failures
)
WHERE created_at >= ?1
  AND (?2 IS NULL OR model = ?2)
  AND (?3 IS NULL OR provider = ?3)
  AND (?4 = 0 OR status >= 400)
ORDER BY created_at DESC LIMIT ?5 OFFSET ?6
";

/// What the /ui request log asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogFilter<'a> {
    /// Window length in days, counted back from the newest row.
    pub days: u32,
    /// Only this model (the combo id for a failure).
    pub model: Option<&'a str>,
    /// Only this provider.
    pub provider: Option<&'a str>,
    /// Only failed requests.
    pub errors_only: bool,
    /// Most rows returned, newest first.
    pub limit: usize,
    /// Rows skipped from the newest end, for paging back through the window.
    /// Ties on `created_at` may repeat or skip a row across pages: the union
    /// has no shared key to order them by.
    pub offset: usize,
}

/// One line of the request log: an answered request or a failed one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRow {
    /// Unix seconds.
    pub created_at: i64,
    /// Key that made the request.
    pub key_id: String,
    /// Provider that served it, `-` for a failure.
    pub provider: String,
    /// Model served, or the model asked for on a failure.
    pub model: String,
    /// Prompt + completion tokens.
    pub tokens: u32,
    /// Dollars charged.
    pub cost_usd: Usd,
    /// `false` when the row had no pricing row behind it.
    pub priced: bool,
    /// HTTP status: `200` for an answered row, the error status otherwise.
    pub status: u16,
    /// Why it failed; empty for an answered row.
    pub error: String,
}

/// SQLite-backed usage ledger.
///
/// Blocking: `rusqlite` is synchronous. Hold it behind a mutex or hand work to
/// `spawn_blocking` rather than calling it from an async task directly.
pub struct Ledger {
    conn: Connection,
}

impl Ledger {
    /// Opens (or creates) a ledger file, applying the schema if it is new.
    ///
    /// # Errors
    ///
    /// Returns [`TokenError::Sqlite`] if the file cannot be opened or the
    /// schema cannot be applied.
    pub fn open(path: &Path) -> Result<Self, TokenError> {
        Self::from_connection(Connection::open(path)?)
    }

    /// Opens a private in-memory ledger. Useful for tests and for a dry run
    /// that must not touch the on-disk audit trail.
    ///
    /// # Errors
    ///
    /// Returns [`TokenError::Sqlite`] if the schema cannot be applied.
    pub fn open_in_memory() -> Result<Self, TokenError> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self, TokenError> {
        // WAL keeps a reader from blocking a writer; the ledger is written on
        // request completion and read by `ar cost-report`.
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        conn.execute_batch(SCHEMA)?;
        if !has_column(&conn, "failures", "provider")? {
            conn.execute_batch(MIGRATE_FAILURE_PROVIDER)?;
        }
        if !has_column(&conn, "usage", "combo")? {
            conn.execute_batch(MIGRATE_USAGE_COMBO)?;
        }
        if !has_column(&conn, "key_caps", "refuse_unpriced")? {
            conn.execute_batch(MIGRATE_REFUSE_UNPRICED)?;
        }
        // The covering index the per-target budget reads through. `CREATE INDEX
        // IF NOT EXISTS` on an existing ledger is a no-op, so this is the one
        // statement that has to run on every open: the index is not a schema
        // change to a column, and there is no `PRAGMA table_info` question to
        // ask about it.
        conn.execute_batch(INDEX_USAGE_BY_TARGET)?;
        Ok(Self { conn })
    }

    /// Records one completed request.
    ///
    /// # Errors
    ///
    /// Returns [`TokenError::Sqlite`] on a write failure.
    pub fn record(&self, entry: &Entry<'_>) -> Result<(), TokenError> {
        self.record_batch(std::slice::from_ref(entry))
    }

    /// Records one completed dispatch and answers what it cost.
    ///
    /// The response path's entry point. [`Entry`] is unchanged — a caller that
    /// already knows its usage still builds the row by hand — so this is the same
    /// append with the two derivations the response path would otherwise repeat
    /// per request: [`NormalizedUsage::from_usage`] over the provider's own
    /// `usage` object, and [`PricingTable::cost`] over the resolved price row.
    ///
    /// Both halves come out of one [`ResponseMeta`], and that is the point: the
    /// cost a caller stamps on the response and the row written here cannot
    /// disagree, because they are the same numbers read twice.
    ///
    /// A provider that reports no usage is recorded as a zero-token row, not
    /// skipped: the request *was* served, and a gap would read downstream as "no
    /// request happened" rather than "no measurement".
    ///
    /// # Errors
    ///
    /// Returns [`TokenError::Sqlite`] on a write failure.
    pub fn record_response(
        &self,
        who: &Who<'_>,
        upstream_usage: &serde_json::Value,
        created_at: i64,
        prices: &PricingTable,
    ) -> Result<ResponseMeta, TokenError> {
        let meta = ResponseMeta::from_upstream(prices, who.provider, who.model, upstream_usage);
        self.record(&Entry {
            key_id: who.key_id,
            provider: who.provider,
            model: who.model,
            combo: who.combo,
            usage: meta.usage(),
            cost: meta.cost(),
            created_at,
        })?;
        Ok(meta)
    }

    /// Records many completions in one transaction against one prepared
    /// statement.
    ///
    /// # Errors
    ///
    /// Returns [`TokenError::Sqlite`] on a write failure. The transaction rolls
    /// back, so a partial batch is never persisted.
    pub fn record_batch(&self, entries: &[Entry<'_>]) -> Result<(), TokenError> {
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut stmt = tx.prepare_cached(INSERT_USAGE)?;
            for entry in entries {
                stmt.execute(params![
                    entry.key_id,
                    entry.provider,
                    entry.model,
                    entry.usage.prompt,
                    entry.usage.completion,
                    entry.usage.total,
                    i64::try_from(entry.cost.usd.micros).unwrap_or(i64::MAX),
                    i64::from(entry.cost.priced),
                    entry.created_at,
                    entry.combo,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Installs or replaces a key's cap.
    ///
    /// # Errors
    ///
    /// Returns [`TokenError::Sqlite`] on a write failure.
    pub fn set_cap(&self, cap: &Cap) -> Result<(), TokenError> {
        // `None` must land as SQL NULL, not 0: a NULL arm is unlimited, and a
        // stored 0 would deny every subsequent request.
        self.conn.execute(
            UPSERT_CAP,
            params![
                cap.key_id,
                cap.usd_micros.map(to_i64),
                cap.tokens.map(to_i64),
                i64::from(cap.refuse_unpriced)
            ],
        )?;
        Ok(())
    }

    /// Reads a key's cap, or `None` when the key has none and is therefore
    /// unlimited.
    ///
    /// # Errors
    ///
    /// Returns [`TokenError::Sqlite`] on a read failure.
    pub fn cap(&self, key_id: &str) -> Result<Option<Cap>, TokenError> {
        let mut stmt = self.conn.prepare(
            "SELECT usd_micros, token_cap, refuse_unpriced FROM key_caps WHERE key_id = ?1",
        )?;
        let mut rows = stmt.query([key_id])?;
        let Some(row) = rows.next()? else {
            return Ok(None);
        };
        Ok(Some(Cap {
            key_id: key_id.to_owned(),
            usd_micros: read_opt(row.get::<_, Option<i64>>(0)?),
            tokens: read_opt(row.get::<_, Option<i64>>(1)?),
            refuse_unpriced: row.get::<_, i64>(2)? != 0,
        }))
    }

    /// Totals recorded against one key.
    ///
    /// # Errors
    ///
    /// Returns [`TokenError::Sqlite`] on a read failure.
    pub fn spend(&self, key_id: &str) -> Result<Spend, TokenError> {
        let (usd, tokens) = self.conn.query_row(
            "SELECT COALESCE(SUM(cost_micros), 0), COALESCE(SUM(total_tokens), 0) FROM usage WHERE key_id = ?1",
            [key_id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )?;
        Ok(Spend {
            usd: Usd {
                micros: to_u64(usd),
            },
            tokens: to_u64(tokens),
        })
    }

    /// The key's whole spend, and each target's share of it, in ONE query.
    ///
    /// A budget needs both halves at once: the key-wide ceiling is the total,
    /// and the per-target ceiling is one row of the split. Two queries would
    /// mean two passes over the key's rows on every request, so this reads the
    /// whole key once and groups it.
    ///
    /// `since` bounds the windowed half only: the cumulative figures are every
    /// token the key ever spent, and the windowed figure is the spend inside
    /// the window. Both are returned because a ceiling may be measured over
    /// either, and the window is optional.
    ///
    /// Reads through the `usage_by_target` covering index, so neither the
    /// aggregate nor the grouping touches the table.
    ///
    /// # Errors
    ///
    /// Returns [`TokenError::Sqlite`] on a read failure.
    pub fn spend_breakdown(
        &self,
        key_id: &str,
        since: Option<i64>,
    ) -> Result<SpendBreakdown, TokenError> {
        let mut stmt = self.conn.prepare(
            "SELECT provider, model, \
                    COALESCE(SUM(total_tokens), 0), \
                    COALESCE(SUM(cost_micros), 0), \
                    COALESCE(SUM(CASE WHEN created_at >= ?2 THEN total_tokens ELSE 0 END), 0), \
                    COALESCE(SUM(CASE WHEN created_at >= ?2 THEN cost_micros ELSE 0 END), 0) \
             FROM usage WHERE key_id = ?1 GROUP BY provider, model",
        )?;
        let mut rows = stmt.query(params![key_id, since.unwrap_or(i64::MIN)])?;
        let mut breakdown = SpendBreakdown::default();
        while let Some(row) = rows.next()? {
            let provider: String = row.get(0)?;
            let model: String = row.get(1)?;
            let total = Spend {
                usd: Usd {
                    micros: to_u64(row.get::<_, i64>(3)?),
                },
                tokens: to_u64(row.get::<_, i64>(2)?),
            };
            let windowed = Spend {
                usd: Usd {
                    micros: to_u64(row.get::<_, i64>(5)?),
                },
                tokens: to_u64(row.get::<_, i64>(4)?),
            };
            breakdown.total.tokens = breakdown.total.tokens.saturating_add(total.tokens);
            breakdown.total.usd.micros =
                breakdown.total.usd.micros.saturating_add(total.usd.micros);
            breakdown.windowed.tokens = breakdown.windowed.tokens.saturating_add(windowed.tokens);
            breakdown.windowed.usd.micros = breakdown
                .windowed
                .usd
                .micros
                .saturating_add(windowed.usd.micros);
            breakdown.targets.push(TargetSpendRow {
                provider,
                model,
                spend: TargetSpend { total, windowed },
            });
        }
        Ok(breakdown)
    }

    /// Decides whether a key may spend `projected` on a request that will use
    /// `projected_tokens`.
    ///
    /// Pre-flight, not post-hoc: the verdict lands before the request is
    /// dispatched, so a refused request never reached the provider. A key with
    /// no cap is [`Verdict::Allow`]; a cap with neither arm set is also
    /// `Allow`, since it constrains nothing.
    ///
    /// `projected.priced` is consulted **only** when the key's cap sets
    /// [`Cap::refuse_unpriced`], which is off by default. A flat-rate key spends
    /// $0 and is stopped by the token arm; an unpriced model also spends $0 and
    /// is stopped by the token arm too. A caller that wants to refuse unknown
    /// pricing outright sets that flag rather than checking [`Cost::priced`]
    /// itself, so the decision is on the cap and travels with it.
    ///
    /// # Errors
    ///
    /// Returns [`TokenError::Sqlite`] on a read failure.
    pub fn admit(
        &self,
        key_id: &str,
        projected: Cost,
        projected_tokens: u32,
    ) -> Result<Verdict, TokenError> {
        let Some(cap) = self.cap(key_id)? else {
            return Ok(Verdict::Allow);
        };
        self.admit_with(key_id, &cap, projected, projected_tokens)
    }

    /// The cap check for a caller that holds its cap from somewhere other than
    /// the store — a policy layer enforcing a `limits:` row from config.
    ///
    /// The check still consults the ledger's spend totals, because that is
    /// where they live; the *source* of the cap is what differs. A cap with
    /// neither arm set is `Allow`, since it constrains nothing.
    pub fn admit_with(
        &self,
        key_id: &str,
        cap: &Cap,
        projected: Cost,
        projected_tokens: u32,
    ) -> Result<Verdict, TokenError> {
        let spend = self.spend(key_id)?;
        self.admit_against(key_id, cap, &spend, projected, projected_tokens)
    }

    /// The cap check against a spend figure the caller already holds.
    ///
    /// The same comparison [`Ledger::admit_with`] makes, with the spend
    /// supplied instead of read. A caller that needs the spend *and* the
    /// per-target split reads them together (one grouped query) and passes the
    /// total here, so the two halves of a budget are evaluated against the
    /// same view of the table rather than two queries that can disagree.
    ///
    /// The windowed case is the reason this exists: the caller measures the
    /// spend over a window, and the total it holds is the windowed one.
    ///
    /// # Errors
    ///
    /// Never fails: there is no I/O here. The `Result` is kept so the two
    /// entry points have the same shape.
    pub fn admit_against(
        &self,
        _key_id: &str,
        cap: &Cap,
        spend: &Spend,
        projected: Cost,
        projected_tokens: u32,
    ) -> Result<Verdict, TokenError> {
        if cap.refuse_unpriced && !projected.priced {
            return Ok(Verdict::Deny(DenyReason::Unpriced));
        }
        if cap.usd_micros.is_none() && cap.tokens.is_none() {
            return Ok(Verdict::Allow);
        }
        let pending_usd = to_u64(i64::try_from(projected.usd.micros).unwrap_or(i64::MAX));
        if let Some(limit) = cap.usd_micros {
            let spent = spend.usd.micros.saturating_add(pending_usd);
            if spent >= limit {
                return Ok(Verdict::Deny(DenyReason::UsdCap {
                    spent: Usd { micros: spent },
                    cap: Usd { micros: limit },
                }));
            }
        }
        if let Some(limit) = cap.tokens {
            let spent = spend.tokens.saturating_add(u64::from(projected_tokens));
            if spent >= limit {
                return Ok(Verdict::Deny(DenyReason::TokenCap { spent, cap: limit }));
            }
        }
        Ok(Verdict::Allow)
    }

    /// Aggregates the ledger into the /ui analytics shape: last 30 days of
    /// daily spend, plus per-model and per-provider ranks (most expensive
    /// first, capped at `rank_cap` rows each).
    ///
    /// # Errors
    ///
    /// Returns [`TokenError::Sqlite`] on a read failure.
    pub fn analytics(&self, rank_cap: usize) -> Result<Analytics, TokenError> {
        self.analytics_since(30, None, rank_cap)
    }

    /// [`Ledger::analytics`] over a chosen window: the daily buckets and both
    /// ranks cover only the last `days` days (counted back from the newest
    /// row), which is what a /ui range picker asks for.
    ///
    /// # Errors
    ///
    /// Returns [`TokenError::Sqlite`] on a read failure.
    pub fn analytics_range(&self, days: u32, rank_cap: usize) -> Result<Analytics, TokenError> {
        self.analytics_since(days, Some(days), rank_cap)
    }

    // `rank_days` of `None` ranks over all time, the bill-style view.
    fn analytics_since(
        &self,
        days: u32,
        rank_days: Option<u32>,
        rank_cap: usize,
    ) -> Result<Analytics, TokenError> {
        let Some(newest) = self
            .conn
            .query_row("SELECT MAX(created_at) FROM usage", [], |r| {
                r.get::<_, Option<i64>>(0)
            })?
        else {
            return Ok(Analytics::default());
        };
        let cap = i64::try_from(rank_cap).unwrap_or(i64::MAX);
        let rank_since = rank_days.map_or(0, |d| window_start(newest, d));
        Ok(Analytics {
            days: self.named_buckets(ANALYTICS_DAYS, params![window_start(newest, days)])?,
            models: self.named_buckets(ANALYTICS_MODELS, params![rank_since, cap])?,
            providers: self.named_buckets(ANALYTICS_PROVIDERS, params![rank_since, cap])?,
            combos: self.named_buckets(ANALYTICS_COMBOS, params![rank_since, cap])?,
        })
    }

    /// Records one failed request for the /ui log. Never touches `usage`.
    ///
    /// # Errors
    ///
    /// Returns [`TokenError::Sqlite`] on a write failure.
    pub fn record_failure(
        &self,
        key_id: &str,
        provider: &str,
        model: &str,
        status: u16,
        error: &str,
        created_at: i64,
    ) -> Result<(), TokenError> {
        let error: String = error.chars().take(FAILURE_TEXT_MAX).collect();
        self.conn.execute(
            "INSERT INTO failures (key_id, provider, model, status, error, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![key_id, provider, model, i64::from(status), error, created_at],
        )?;
        self.conn.execute(
            "DELETE FROM failures WHERE id <= (SELECT MAX(id) FROM failures) - ?1",
            params![FAILURE_KEEP],
        )?;
        Ok(())
    }

    /// The request log: answered and failed requests, newest first, inside the
    /// filter's window.
    ///
    /// # Errors
    ///
    /// Returns [`TokenError::Sqlite`] on a read failure.
    pub fn log(&self, filter: &LogFilter<'_>) -> Result<Vec<LogRow>, TokenError> {
        let Some(newest) = self
            .conn
            .query_row(NEWEST, [], |r| r.get::<_, Option<i64>>(0))?
        else {
            return Ok(Vec::new());
        };
        let mut stmt = self.conn.prepare(LOG)?;
        let mut rows = stmt.query(params![
            window_start(newest, filter.days),
            filter.model,
            filter.provider,
            i64::from(filter.errors_only),
            i64::try_from(filter.limit).unwrap_or(i64::MAX),
            i64::try_from(filter.offset).unwrap_or(i64::MAX),
        ])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(LogRow {
                created_at: row.get(0)?,
                key_id: row.get(1)?,
                provider: row.get(2)?,
                model: row.get(3)?,
                tokens: to_u32(row.get::<_, i64>(4)?),
                cost_usd: Usd {
                    micros: to_u64(row.get::<_, i64>(5)?),
                },
                priced: row.get::<_, i64>(6)? != 0,
                status: u16::try_from(row.get::<_, i64>(7)?).unwrap_or(0),
                error: row.get(8)?,
            });
        }
        Ok(out)
    }

    // One SQL over the usage table mapped to [`SpendBucket`] rows. Constants
    // name their own group column (a bound '?' would be a constant there and
    // collapse every row into one bucket); the ranked SQL binds only its cap.
    fn named_buckets<P: rusqlite::Params>(
        &self,
        sql: &str,
        params: P,
    ) -> Result<Vec<SpendBucket>, TokenError> {
        let mut stmt = self.conn.prepare(sql)?;
        let mut rows = stmt.query(params)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(SpendBucket {
                name: row.get(0)?,
                requests: to_u64(row.get::<_, i64>(1)?),
                tokens: to_u64(row.get::<_, i64>(2)?),
                cost_usd_micros: to_u64(row.get::<_, i64>(3)?),
            });
        }
        Ok(out)
    }

    /// Most recent `limit` usage rows plus the all-key totals, for
    /// `ar cost-report`.
    ///
    /// # Errors
    ///
    /// Returns [`TokenError::Sqlite`] on a read failure.
    pub fn report(&self, limit: usize) -> Result<CostReport, TokenError> {
        let mut rows = Vec::new();
        let mut stmt = self.conn.prepare(REPORT)?;
        let mut query = stmt.query([i64::try_from(limit).unwrap_or(i64::MAX)])?;
        while let Some(row) = query.next()? {
            rows.push(LedgerRow {
                key_id: row.get(0)?,
                provider: row.get(1)?,
                model: row.get(2)?,
                total_tokens: to_u32(row.get::<_, i64>(3)?),
                cost_usd: Usd {
                    micros: to_u64(row.get::<_, i64>(4)?),
                },
                priced: row.get::<_, i64>(5)? != 0,
            });
            if rows.len() >= limit {
                break;
            }
        }
        let (usd, tokens) = self.conn.query_row(
            "SELECT COALESCE(SUM(cost_micros), 0), COALESCE(SUM(total_tokens), 0) FROM usage",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )?;
        Ok(CostReport {
            rows,
            totals: Spend {
                usd: Usd {
                    micros: to_u64(usd),
                },
                tokens: to_u64(tokens),
            },
        })
    }
}

/// First second of a `days`-day window ending at `newest` (inclusive of the
/// newest day, so `days = 1` is the last 24 hours).
fn window_start(newest: i64, days: u32) -> i64 {
    newest.saturating_sub(i64::from(days.max(1) - 1).saturating_mul(86_400))
}

fn to_u64(n: i64) -> u64 {
    u64::try_from(n).unwrap_or(0)
}

fn to_i64(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

fn to_u32(n: i64) -> u32 {
    u32::try_from(n).unwrap_or(0)
}

fn read_opt(n: Option<i64>) -> Option<u64> {
    n.map(to_u64)
}

/// Whether `key_caps` already carries `column`. See
/// [`MIGRATE_REFUSE_UNPRICED`] for why the migration needs to ask.
fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool, TokenError> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        if row.get::<_, String>(1)? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::{Analytics, LogFilter, SpendBucket};
    use super::{Cap, CostReport, DenyReason, Entry, Ledger, Verdict, Who};
    use crate::pricing::{Cost, Prices, PricingTable, Usd};
    use crate::usage::NormalizedUsage;
    use rusqlite::Connection;
    use serde_json::json;

    fn entry<'a>(key_id: &'a str, provider: &'a str, model: &'a str, tokens: u32) -> Entry<'a> {
        Entry {
            key_id,
            provider,
            model,
            combo: "",
            usage: NormalizedUsage::new(tokens, 0),
            cost: Cost {
                usd: Usd { micros: 1_000 },
                priced: true,
            },
            created_at: 1_700_000_000,
        }
    }

    fn flat_rate_table() -> PricingTable {
        let mut t = PricingTable::default();
        t.set_flat_rate("claude");
        t
    }
    fn priced_table() -> PricingTable {
        let mut t = PricingTable::default();
        t.set(
            "openai",
            "gpt-4o",
            Prices {
                input_micros_per_mtok: 2_500_000,
                output_micros_per_mtok: 10_000_000,
            },
        );
        t
    }

    fn upstream_usage() -> serde_json::Value {
        json!({ "prompt_tokens": 1_000_000, "completion_tokens": 1_000_000 })
    }

    #[test]
    fn ledgers_usage_when_completed() {
        let ledger = Ledger::open_in_memory().expect("open");
        ledger
            .record(&entry("k1", "openai", "gpt-4o", 1_200))
            .expect("record");
        assert_eq!(ledger.spend("k1").expect("spend").tokens, 1_200);
    }

    #[test]
    fn analytics_should_bucket_days_models_and_providers_when_rows_exist() {
        let ledger = Ledger::open_in_memory().expect("open");
        ledger
            .record(&entry("k1", "openai", "gpt-4o", 100))
            .expect("record");
        // Same day, different key: the day bucket must merge them (2 requests).
        ledger
            .record(&entry("k2", "openai", "gpt-4o", 10))
            .expect("record");
        // Different day and provider, higher cost per test fixture is flat, so
        // order the model rank by that two-row provider instead.
        let mut later = entry("k3", "anthropic", "claude-sonnet-4", 5);
        later.created_at = 1_700_086_400; // fixture day + 1.
        ledger.record(&later).expect("record");

        let a = ledger.analytics(10).expect("analytics");
        assert_eq!(a.days.len(), 2, "one bucket per calendar day");
        assert_eq!(a.days[0].name, "2023-11-14");
        assert_eq!(a.days[0].requests, 2);
        assert_eq!(a.days[1].name, "2023-11-15");
        assert_eq!(
            a.providers[0],
            SpendBucket {
                name: "openai".into(),
                requests: 2,
                tokens: 110,
                cost_usd_micros: 2_000,
            }
        );
        assert_eq!(a.models[0].name, "gpt-4o");
    }

    #[test]
    fn analytics_should_rank_models_by_dollars_then_cap_the_list() {
        let ledger = Ledger::open_in_memory().expect("open");
        for (model, tokens) in [
            ("gpt-4o", 100),
            ("gpt-4o", 100),
            ("gpt-4o", 100),
            ("cheap", 5),
            ("cheap", 5),
        ] {
            ledger
                .record(&entry("k1", "openai", model, tokens))
                .expect("record");
        }
        let a = ledger.analytics(1).expect("analytics");
        assert_eq!(a.models.len(), 1, "rank_cap bounds the rank list");
        assert_eq!(a.models[0].name, "gpt-4o");
        assert_eq!(a.models[0].requests, 3);
    }

    fn all_rows() -> LogFilter<'static> {
        LogFilter {
            days: 30,
            model: None,
            provider: None,
            errors_only: false,
            limit: 50,
            offset: 0,
        }
    }

    #[test]
    fn log_should_merge_failures_and_answered_rows_newest_first() {
        let ledger = Ledger::open_in_memory().expect("open");
        ledger
            .record(&entry("k1", "openai", "gpt-4o", 100))
            .expect("record");
        ledger
            .record_failure("k1", "-", "jev", 502, "all providers failed", 1_700_000_100)
            .expect("failure");
        let statuses: Vec<u16> = ledger
            .log(&all_rows())
            .expect("log")
            .iter()
            .map(|r| r.status)
            .collect();
        assert_eq!(statuses, [502, 200]);
    }

    #[test]
    fn log_should_keep_only_failures_when_errors_only() {
        let ledger = Ledger::open_in_memory().expect("open");
        ledger
            .record(&entry("k1", "openai", "gpt-4o", 100))
            .expect("record");
        ledger
            .record_failure("k1", "-", "jev", 429, "throttled", 1_700_000_100)
            .expect("failure");
        let filter = LogFilter {
            errors_only: true,
            ..all_rows()
        };
        assert_eq!(ledger.log(&filter).expect("log").len(), 1);
    }

    #[test]
    fn log_should_filter_by_model_when_one_is_named() {
        let ledger = Ledger::open_in_memory().expect("open");
        ledger
            .record(&entry("k1", "openai", "gpt-4o", 100))
            .expect("record");
        ledger
            .record(&entry("k1", "anthropic", "claude-sonnet-4", 5))
            .expect("record");
        let filter = LogFilter {
            model: Some("gpt-4o"),
            ..all_rows()
        };
        assert_eq!(ledger.log(&filter).expect("log")[0].provider, "openai");
    }

    #[test]
    fn record_failure_should_leave_the_spend_report_untouched_when_it_writes() {
        let ledger = Ledger::open_in_memory().expect("open");
        ledger
            .record_failure("k1", "-", "jev", 502, "down", 1_700_000_100)
            .expect("failure");
        assert_eq!(ledger.report(10).expect("report").rows.len(), 0);
    }

    #[test]
    fn log_should_name_the_provider_on_a_failed_row() {
        let ledger = Ledger::open_in_memory().expect("open");
        ledger
            .record_failure("k1", "p1", "jev", 502, "down", 1_700_000_100)
            .expect("failure");
        let rows = ledger
            .log(&LogFilter {
                days: 30,
                model: None,
                provider: None,
                errors_only: true,
                limit: 10,
                offset: 0,
            })
            .expect("log");
        assert_eq!(rows[0].provider, "p1");
    }

    #[test]
    fn from_connection_should_add_the_provider_column_to_an_old_failures_table() {
        let conn = Connection::open_in_memory().expect("open");
        conn.execute_batch(
            "CREATE TABLE failures (
                id INTEGER PRIMARY KEY AUTOINCREMENT, key_id TEXT NOT NULL,
                model TEXT NOT NULL, status INTEGER NOT NULL,
                error TEXT NOT NULL, created_at INTEGER NOT NULL);
             INSERT INTO failures (key_id, model, status, error, created_at)
                VALUES ('k1', 'jev', 502, 'old', 1700000100);",
        )
        .expect("old shape");
        let ledger = Ledger::from_connection(conn).expect("migrates");
        let rows = ledger
            .log(&LogFilter {
                days: 30,
                model: None,
                provider: None,
                errors_only: true,
                limit: 10,
                offset: 0,
            })
            .expect("log");
        assert_eq!(rows[0].provider, "-");
    }

    #[test]
    fn log_should_page_back_through_the_window_when_offset_is_set() {
        let ledger = Ledger::open_in_memory().expect("open");
        for t in 0..5 {
            ledger
                .record_failure("k1", "-", "jev", 502, "down", 1_700_000_100 + t)
                .expect("failure");
        }
        let page = |offset| {
            ledger
                .log(&LogFilter {
                    days: 30,
                    model: None,
                    provider: None,
                    errors_only: true,
                    limit: 2,
                    offset,
                })
                .expect("log")
                .iter()
                .map(|r| r.created_at)
                .collect::<Vec<_>>()
        };
        assert_eq!(page(0), vec![1_700_000_104, 1_700_000_103]);
        assert_eq!(page(4), vec![1_700_000_100]);
        assert!(page(6).is_empty());
    }

    #[test]
    fn analytics_range_should_drop_older_days_when_the_window_is_one_day() {
        let ledger = Ledger::open_in_memory().expect("open");
        ledger
            .record(&entry("k1", "openai", "gpt-4o", 100))
            .expect("record");
        let mut later = entry("k3", "anthropic", "claude-sonnet-4", 5);
        later.created_at = 1_700_086_400;
        ledger.record(&later).expect("record");
        assert_eq!(ledger.analytics_range(1, 10).expect("range").days.len(), 1);
    }

    #[test]
    fn analytics_should_return_empty_buckets_when_the_ledger_is_new() {
        let ledger = Ledger::open_in_memory().expect("open");
        assert_eq!(
            ledger.analytics(10).expect("analytics"),
            Analytics::default()
        );
    }

    #[test]
    fn blocks_when_over_budget() {
        let ledger = Ledger::open_in_memory().expect("open");
        ledger
            .set_cap(&Cap {
                key_id: "k1".into(),
                usd_micros: Some(1_000),
                tokens: None,
                refuse_unpriced: false,
            })
            .expect("cap");
        ledger
            .record(&entry("k1", "openai", "gpt-4o", 10))
            .expect("record");
        let verdict = ledger
            .admit(
                "k1",
                Cost {
                    usd: Usd { micros: 500 },
                    priced: true,
                },
                10,
            )
            .expect("admit");
        assert_eq!(
            verdict.reason(),
            Some(DenyReason::UsdCap {
                spent: Usd { micros: 1_500 },
                cap: Usd { micros: 1_000 }
            })
        );
    }

    #[test]
    fn maps_denial_to_payment_required() {
        assert_eq!(
            DenyReason::TokenCap { spent: 2, cap: 1 }.status().as_u16(),
            402
        );
    }

    #[test]
    fn allows_when_no_cap_installed() {
        let ledger = Ledger::open_in_memory().expect("open");
        assert_eq!(
            ledger
                .admit("k1", Cost::UNPRICED, 1_000_000)
                .expect("admit"),
            Verdict::Allow
        );
    }

    #[test]
    fn enforces_token_cap_when_flat_rate_cost_is_zero() {
        let ledger = Ledger::open_in_memory().expect("open");
        ledger
            .set_cap(&Cap {
                key_id: "k1".into(),
                usd_micros: Some(1_000_000),
                tokens: Some(1_000),
                refuse_unpriced: false,
            })
            .expect("cap");
        let cost =
            flat_rate_table().cost("claude", "claude-sonnet-4", NormalizedUsage::new(1_000, 0));
        ledger
            .record(&Entry {
                cost,
                ..entry("k1", "claude", "claude-sonnet-4", 1_000)
            })
            .expect("record");
        let verdict = ledger
            .admit(
                "k1",
                Cost {
                    usd: Usd::ZERO,
                    priced: true,
                },
                100,
            )
            .expect("admit");
        assert_eq!(
            verdict.reason(),
            Some(DenyReason::TokenCap {
                spent: 1_100,
                cap: 1_000
            })
        );
    }

    #[test]
    fn enforces_token_cap_when_model_is_unpriced() {
        let ledger = Ledger::open_in_memory().expect("open");
        ledger
            .set_cap(&Cap {
                key_id: "k1".into(),
                usd_micros: Some(1_000_000),
                tokens: Some(500),
                refuse_unpriced: false,
            })
            .expect("cap");
        let verdict = ledger.admit("k1", Cost::UNPRICED, 500).expect("admit");
        assert_eq!(
            verdict.reason(),
            Some(DenyReason::TokenCap {
                spent: 500,
                cap: 500
            })
        );
    }

    #[test]
    fn ignores_cap_with_neither_arm_set() {
        let ledger = Ledger::open_in_memory().expect("open");
        ledger
            .set_cap(&Cap {
                key_id: "k1".into(),
                usd_micros: None,
                tokens: None,
                refuse_unpriced: false,
            })
            .expect("cap");
        assert_eq!(
            ledger
                .admit("k1", Cost::UNPRICED, 9_999_999)
                .expect("admit"),
            Verdict::Allow
        );
    }

    #[test]
    fn serves_an_unpriced_model_when_the_cap_does_not_ask_to_fail_closed() {
        // The default. A budget cannot be evaluated against "unknown", but
        // refusing by default would break every key whose pricing table has a
        // gap -- so the flag exists and defaults off.
        let ledger = Ledger::open_in_memory().expect("open");
        ledger
            .set_cap(&Cap {
                key_id: "k1".into(),
                usd_micros: Some(1_000_000),
                tokens: None,
                refuse_unpriced: false,
            })
            .expect("cap");
        assert_eq!(
            ledger.admit("k1", Cost::UNPRICED, 10).expect("admit"),
            Verdict::Allow
        );
    }

    #[test]
    fn refuses_an_unpriced_model_when_the_cap_asks_to_fail_closed() {
        let ledger = Ledger::open_in_memory().expect("open");
        ledger
            .set_cap(&Cap {
                key_id: "k1".into(),
                usd_micros: Some(1_000_000),
                tokens: None,
                refuse_unpriced: true,
            })
            .expect("cap");
        let verdict = ledger.admit("k1", Cost::UNPRICED, 10).expect("admit");
        assert_eq!(verdict.reason(), Some(DenyReason::Unpriced));
    }

    #[test]
    fn still_allows_a_priced_model_when_failing_closed() {
        // The flag is about *unknown* cost, not about spending: a priced request
        // on a fail-closed key is decided by the arms as usual.
        let ledger = Ledger::open_in_memory().expect("open");
        ledger
            .set_cap(&Cap {
                key_id: "k1".into(),
                usd_micros: Some(1_000_000),
                tokens: Some(500),
                refuse_unpriced: true,
            })
            .expect("cap");
        let priced = Cost {
            usd: Usd { micros: 1 },
            priced: true,
        };
        assert_eq!(
            ledger.admit("k1", priced, 10).expect("admit"),
            Verdict::Allow
        );
    }

    #[test]
    fn treats_a_flat_rate_model_as_priced_under_fail_closed() {
        // A flat-rate provider is a known $0, not an unknown price. Refusing it
        // would refuse every subscription key.
        let ledger = Ledger::open_in_memory().expect("open");
        ledger
            .set_cap(&Cap {
                key_id: "k1".into(),
                usd_micros: None,
                tokens: Some(500),
                refuse_unpriced: true,
            })
            .expect("cap");
        let flat = Cost {
            usd: Usd::ZERO,
            priced: true,
        };
        assert_eq!(ledger.admit("k1", flat, 10).expect("admit"), Verdict::Allow);
    }

    #[test]
    fn round_trips_the_fail_closed_flag_through_the_cap_table() {
        let ledger = Ledger::open_in_memory().expect("open");
        let cap = Cap {
            key_id: "k1".into(),
            usd_micros: None,
            tokens: None,
            refuse_unpriced: true,
        };
        ledger.set_cap(&cap).expect("cap");
        assert_eq!(ledger.cap("k1").expect("cap"), Some(cap));
    }

    #[test]
    fn maps_an_unpriced_denial_to_payment_required() {
        assert_eq!(DenyReason::Unpriced.status().as_u16(), 402);
    }

    #[test]
    fn migrates_a_cap_table_written_before_the_flag_existed() {
        // An old ledger file: the flag column is absent, so `open` has to add
        // it or every later `INSERT` naming it fails.
        let path = std::env::temp_dir().join("ar-tokens-migrate.redb");
        let _ = std::fs::remove_file(&path);
        {
            let conn = Connection::open(&path).expect("open");
            conn.execute_batch(
                "CREATE TABLE usage (id INTEGER PRIMARY KEY AUTOINCREMENT, key_id TEXT NOT NULL,
                 provider TEXT NOT NULL, model TEXT NOT NULL, prompt_tokens INTEGER NOT NULL,
                 completion_tokens INTEGER NOT NULL, total_tokens INTEGER NOT NULL,
                 cost_micros INTEGER NOT NULL, priced INTEGER NOT NULL, created_at INTEGER NOT NULL);
                 CREATE TABLE key_caps (key_id TEXT PRIMARY KEY, usd_micros INTEGER, token_cap INTEGER);
                 INSERT INTO key_caps (key_id, usd_micros, token_cap) VALUES ('k1', 5000, 900);",
            )
            .expect("seed");
        }
        let ledger = Ledger::open(&path).expect("reopen");
        let cap = ledger.cap("k1").expect("cap").expect("a row");
        assert_eq!(
            (cap.usd_micros, cap.tokens, cap.refuse_unpriced),
            (Some(5_000), Some(900), false)
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_breakdown_splits_a_keys_spend_across_its_targets() {
        // The property the per-target arm rests on: the sum over the targets
        // equals the key's whole spend, and each target carries its own share.
        // A breakdown whose parts did not add up would let a budget deny a
        // request on a spend that never happened.
        let ledger = Ledger::open_in_memory().expect("open");
        ledger.record(&entry("k1", "p", "m", 100)).expect("record");
        ledger.record(&entry("k1", "p", "m", 50)).expect("record");
        ledger.record(&entry("k1", "q", "m2", 25)).expect("record");
        ledger.record(&entry("k2", "p", "m", 999)).expect("record");
        let breakdown = ledger.spend_breakdown("k1", None).expect("breakdown");
        assert_eq!(breakdown.total.tokens, 175);
        assert_eq!(breakdown.target("p", "m").total.tokens, 150);
        assert_eq!(breakdown.target("q", "m2").total.tokens, 25);
        assert_eq!(
            breakdown.target("p", "m").total.tokens + breakdown.target("q", "m2").total.tokens,
            breakdown.total.tokens,
            "the parts add up to the whole"
        );
        assert_eq!(
            breakdown.target("q", "m2").total.tokens,
            25,
            "another key's spend is not counted against this one"
        );
    }

    #[test]
    fn a_target_that_never_served_reports_zero() {
        // A target with no spend is a target the budget must NOT refuse, so
        // the lookup has to answer zero rather than "unknown".
        let ledger = Ledger::open_in_memory().expect("open");
        ledger.record(&entry("k1", "p", "m", 100)).expect("record");
        let breakdown = ledger.spend_breakdown("k1", None).expect("breakdown");
        assert_eq!(breakdown.target("q", "m2").total.tokens, 0);
    }

    #[test]
    fn a_window_excludes_spend_recorded_before_it() {
        // The window is what makes a ceiling recoverable: a row older than the
        // window does not count against it, because the ledger is append-only
        // and the spend can never be un-spent.
        let ledger = Ledger::open_in_memory().expect("open");
        ledger
            .record(&Entry {
                created_at: 1_000,
                ..entry("k1", "p", "m", 100)
            })
            .expect("record");
        ledger
            .record(&Entry {
                created_at: 9_000,
                ..entry("k1", "p", "m", 10)
            })
            .expect("record");
        let breakdown = ledger
            .spend_breakdown("k1", Some(5_000))
            .expect("breakdown");
        assert_eq!(breakdown.total.tokens, 110, "the total is every row");
        assert_eq!(
            breakdown.windowed.tokens, 10,
            "the window is the recent one"
        );
        assert_eq!(breakdown.target("p", "m").windowed.tokens, 10);
        assert_eq!(
            breakdown.target("p", "m").total.tokens,
            110,
            "the cumulative figure is unaffected by the window"
        );
    }

    #[test]
    fn an_existing_ledger_gains_the_covering_index() {
        // The index is what makes the grouped query cheap, and an existing
        // ledger file has to get it on open — the same property the column
        // migrations carry.
        let path = std::env::temp_dir().join("ar-tokens-index.sqlite");
        let _ = std::fs::remove_file(&path);
        {
            let conn = Connection::open(&path).expect("open");
            conn.execute_batch(
                "CREATE TABLE usage (id INTEGER PRIMARY KEY AUTOINCREMENT, key_id TEXT NOT NULL,
                 provider TEXT NOT NULL, model TEXT NOT NULL, prompt_tokens INTEGER NOT NULL,
                 completion_tokens INTEGER NOT NULL, total_tokens INTEGER NOT NULL,
                 cost_micros INTEGER NOT NULL, priced INTEGER NOT NULL, created_at INTEGER NOT NULL);",
            )
            .expect("seed");
        }
        let _ledger = Ledger::open(&path).expect("reopen");
        let conn = Connection::open(&path).expect("reopen");
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='index' AND name='usage_by_target'")
            .expect("prepare");
        let mut rows = stmt.query([]).expect("query");
        assert!(
            rows.next().expect("a row").is_some(),
            "the covering index was not created on an existing ledger"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn records_the_usage_a_response_reported() {
        let ledger = Ledger::open_in_memory().expect("open");
        ledger
            .record_response(
                &Who {
                    key_id: "k1",
                    provider: "openai",
                    model: "gpt-4o",
                    combo: "",
                },
                &upstream_usage(),
                1_700_000_000,
                &priced_table(),
            )
            .expect("record");
        assert_eq!(ledger.spend("k1").expect("spend").tokens, 2_000_000);
    }

    #[test]
    fn attributes_a_row_to_the_combo_the_client_asked_for() {
        // The per-combo figure is only exact if the COMBO is recorded, not
        // derived from the serving target: two combos may share a target, and
        // a join would then charge one request's spend to both of them.
        let ledger = Ledger::open_in_memory().expect("open");
        ledger
            .record_response(
                &Who {
                    key_id: "k1",
                    provider: "openai",
                    model: "gpt-4o",
                    combo: "large-stack",
                },
                &upstream_usage(),
                1_700_000_000,
                &priced_table(),
            )
            .expect("record");
        let analytics = ledger.analytics_range(30, 10).expect("analytics");
        assert_eq!(analytics.combos.len(), 1);
        assert_eq!(analytics.combos[0].name, "large-stack");
    }

    #[test]
    fn two_combos_sharing_a_target_are_told_apart() {
        // The double-count the joined estimate would have produced: one
        // target, two combo names, and each row charged to the name it was
        // asked for under.
        let ledger = Ledger::open_in_memory().expect("open");
        for combo in ["large-stack", "small-stack"] {
            ledger
                .record_response(
                    &Who {
                        key_id: "k1",
                        provider: "openai",
                        model: "gpt-4o",
                        combo,
                    },
                    &upstream_usage(),
                    1_700_000_000,
                    &priced_table(),
                )
                .expect("record");
        }
        let analytics = ledger.analytics_range(30, 10).expect("analytics");
        assert_eq!(analytics.combos.len(), 2, "each combo is its own row");
        let by_combo: u64 = analytics.combos.iter().map(|c| c.cost_usd_micros).sum();
        let by_model: u64 = analytics
            .models
            .iter()
            .filter(|m| m.name == "gpt-4o")
            .map(|m| m.cost_usd_micros)
            .sum();
        assert_eq!(
            by_combo, by_model,
            "the per-combo column sums to the same spend as the per-model one"
        );
    }

    #[test]
    fn a_row_with_no_combo_is_not_counted_as_one() {
        // A flat provider request names no combo, and the per-combo view
        // must not grow a bucket called "" for it.
        let ledger = Ledger::open_in_memory().expect("open");
        ledger
            .record_response(
                &Who {
                    key_id: "k1",
                    provider: "openai",
                    model: "gpt-4o",
                    combo: "",
                },
                &upstream_usage(),
                1_700_000_000,
                &priced_table(),
            )
            .expect("record");
        let analytics = ledger.analytics_range(30, 10).expect("analytics");
        assert!(analytics.combos.is_empty(), "{:?}", analytics.combos);
    }

    #[test]
    fn normalizes_the_prompt_count_when_the_provider_splits_its_cache_counters() {
        // The Anthropic shape: `input_tokens` is the *non-cached* portion, so the
        // row has to carry the sum or a flat-rate provider's token cap is fed a
        // number half the real one.
        let ledger = Ledger::open_in_memory().expect("open");
        let usage = json!({ "input_tokens": 10, "cache_read_input_tokens": 4, "cache_creation_input_tokens": 1, "output_tokens": 2 });
        let meta = ledger
            .record_response(
                &Who {
                    key_id: "k1",
                    provider: "anthropic",
                    model: "claude-sonnet-4",
                    combo: "",
                },
                &usage,
                1_700_000_000,
                &priced_table(),
            )
            .expect("record");
        assert_eq!(meta.tokens_in(), 15);
        assert_eq!(ledger.spend("k1").expect("spend").tokens, 17);
    }

    #[test]
    fn answers_with_the_same_cost_it_stored() {
        // The contract the response headers lean on: the returned meta and the
        // persisted row are one computation read twice, never two that can drift.
        let ledger = Ledger::open_in_memory().expect("open");
        let meta = ledger
            .record_response(
                &Who {
                    key_id: "k1",
                    provider: "openai",
                    model: "gpt-4o",
                    combo: "",
                },
                &upstream_usage(),
                1_700_000_000,
                &priced_table(),
            )
            .expect("record");
        assert_eq!(
            meta.cost().usd.micros,
            ledger.report(1).expect("report").rows[0].cost_usd.micros
        );
    }

    #[test]
    fn records_a_zero_row_when_the_provider_reported_no_usage() {
        // A served request with no measurement is still a served request; a gap
        // would read downstream as "no request happened".
        let ledger = Ledger::open_in_memory().expect("open");
        let meta = ledger
            .record_response(
                &Who {
                    key_id: "k1",
                    provider: "openai",
                    model: "gpt-4o",
                    combo: "",
                },
                &json!({}),
                1_700_000_000,
                &priced_table(),
            )
            .expect("record");
        assert_eq!(meta.usage(), NormalizedUsage::new(0, 0));
        assert_eq!(ledger.report(1).expect("report").rows.len(), 1);
    }

    #[test]
    fn records_a_flat_rate_response_as_priced_zero() {
        let ledger = Ledger::open_in_memory().expect("open");
        let meta = ledger
            .record_response(
                &Who {
                    key_id: "k1",
                    provider: "claude",
                    model: "claude-sonnet-4",
                    combo: "",
                },
                &upstream_usage(),
                1_700_000_000,
                &flat_rate_table(),
            )
            .expect("record");
        assert_eq!(
            meta.cost(),
            Cost {
                usd: Usd::ZERO,
                priced: true
            }
        );
    }

    #[test]
    fn records_an_unpriced_model_as_unpriced_rather_than_free() {
        let ledger = Ledger::open_in_memory().expect("open");
        let meta = ledger
            .record_response(
                &Who {
                    key_id: "k1",
                    provider: "openai",
                    model: "no-such-model",
                    combo: "",
                },
                &upstream_usage(),
                1_700_000_000,
                &priced_table(),
            )
            .expect("record");
        assert_eq!(meta.cost(), Cost::UNPRICED);
    }

    #[test]
    fn records_every_response_of_a_session_separately() {
        let ledger = Ledger::open_in_memory().expect("open");
        for _ in 0..3 {
            ledger
                .record_response(
                    &Who {
                        key_id: "k1",
                        provider: "openai",
                        model: "gpt-4o",
                        combo: "",
                    },
                    &upstream_usage(),
                    1_700_000_000,
                    &priced_table(),
                )
                .expect("record");
        }
        assert_eq!(ledger.report(10).expect("report").rows.len(), 3);
    }

    #[test]
    fn records_batch_in_one_transaction_when_given_several() {
        let ledger = Ledger::open_in_memory().expect("open");
        let batch = [
            entry("k1", "openai", "gpt-4o", 10),
            entry("k1", "openai", "gpt-4o", 20),
        ];
        ledger.record_batch(&batch).expect("batch");
        assert_eq!(ledger.spend("k1").expect("spend").tokens, 30);
    }

    #[test]
    fn rejects_update_when_row_already_written() {
        let ledger = Ledger::open_in_memory().expect("open");
        ledger
            .record(&entry("k1", "openai", "gpt-4o", 10))
            .expect("record");
        let err = ledger
            .conn
            .execute("UPDATE usage SET cost_micros = 0", [])
            .expect_err("append-only");
        assert!(err.to_string().contains("append-only"));
    }

    #[test]
    fn rejects_delete_when_row_already_written() {
        let ledger = Ledger::open_in_memory().expect("open");
        ledger
            .record(&entry("k1", "openai", "gpt-4o", 10))
            .expect("record");
        let err = ledger
            .conn
            .execute("DELETE FROM usage", [])
            .expect_err("append-only");
        assert!(err.to_string().contains("append-only"));
    }

    #[test]
    fn reads_cap_back_when_installed() {
        let ledger = Ledger::open_in_memory().expect("open");
        let cap = Cap {
            key_id: "k1".into(),
            usd_micros: Some(5_000),
            tokens: None,
            refuse_unpriced: false,
        };
        ledger.set_cap(&cap).expect("cap");
        assert_eq!(ledger.cap("k1").expect("cap"), Some(cap));
    }

    #[test]
    fn returns_none_when_key_has_no_cap() {
        let ledger = Ledger::open_in_memory().expect("open");
        assert_eq!(ledger.cap("nope").expect("cap"), None);
    }

    #[test]
    fn sums_totals_across_keys_when_reported() {
        let ledger = Ledger::open_in_memory().expect("open");
        ledger
            .record(&entry("k1", "openai", "gpt-4o", 10))
            .expect("record");
        ledger
            .record(&entry("k2", "groq", "llama", 5))
            .expect("record");
        assert_eq!(ledger.report(10).expect("report").totals.tokens, 15);
    }

    #[test]
    fn renders_empty_state_when_no_rows() {
        assert_eq!(
            CostReport::default().toon(),
            "cost: 0 rows — no usage recorded\n"
        );
    }

    #[test]
    fn renders_header_and_count_when_rows_present() {
        let ledger = Ledger::open_in_memory().expect("open");
        ledger
            .record(&entry("k1", "openai", "gpt-4o", 10))
            .expect("record");
        let toon = ledger.report(10).expect("report").toon();
        assert!(toon.starts_with("cost[1]{key_id,provider,model,tokens,usd}\n"));
    }

    #[test]
    fn reads_back_what_the_same_connection_wrote() {
        let ledger = Ledger::open_in_memory().expect("open");
        ledger
            .record(&entry("k1", "openai", "gpt-4o", 10))
            .expect("record");
        let report = ledger.report(10).expect("report");
        assert_eq!(report.rows[0].key_id, "k1");
    }

    #[test]
    fn sqlite_connection_is_send_when_ledger_moves_to_a_worker() {
        fn assert_send<T: Send>() {}
        assert_send::<Ledger>();
    }
}
