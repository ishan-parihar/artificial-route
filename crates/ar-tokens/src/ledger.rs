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
    created_at        INTEGER NOT NULL
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
";

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

const INSERT_USAGE: &str = "
INSERT INTO usage
    (key_id, provider, model, prompt_tokens, completion_tokens, total_tokens,
     cost_micros, priced, created_at)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
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
    /// What it cost in tokens.
    pub usage: NormalizedUsage,
    /// What it cost in dollars.
    pub cost: Cost,
    /// Unix seconds.
    pub created_at: i64,
}

/// What a key has spent so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Spend {
    /// Dollars recorded against the key.
    pub usd: Usd,
    /// Tokens recorded against the key.
    pub tokens: u64,
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
        if !has_column(&conn, "refuse_unpriced")? {
            conn.execute_batch(MIGRATE_REFUSE_UNPRICED)?;
        }
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
        key_id: &str,
        provider: &str,
        model: &str,
        upstream_usage: &serde_json::Value,
        created_at: i64,
        prices: &PricingTable,
    ) -> Result<ResponseMeta, TokenError> {
        let meta = ResponseMeta::from_upstream(prices, provider, model, upstream_usage);
        self.record(&Entry {
            key_id,
            provider,
            model,
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
        if cap.refuse_unpriced && !projected.priced {
            return Ok(Verdict::Deny(DenyReason::Unpriced));
        }
        let spend = self.spend(key_id)?;
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
fn has_column(conn: &Connection, column: &str) -> Result<bool, TokenError> {
    let mut stmt = conn.prepare("PRAGMA table_info(key_caps)")?;
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
    use super::{Cap, CostReport, DenyReason, Entry, Ledger, Verdict};
    use crate::pricing::{Cost, Prices, PricingTable, Usd};
    use crate::usage::NormalizedUsage;
    use rusqlite::Connection;
    use serde_json::json;

    fn entry<'a>(key_id: &'a str, provider: &'a str, model: &'a str, tokens: u32) -> Entry<'a> {
        Entry {
            key_id,
            provider,
            model,
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
    fn records_the_usage_a_response_reported() {
        let ledger = Ledger::open_in_memory().expect("open");
        ledger
            .record_response(
                "k1",
                "openai",
                "gpt-4o",
                &upstream_usage(),
                1_700_000_000,
                &priced_table(),
            )
            .expect("record");
        assert_eq!(ledger.spend("k1").expect("spend").tokens, 2_000_000);
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
                "k1",
                "anthropic",
                "claude-sonnet-4",
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
                "k1",
                "openai",
                "gpt-4o",
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
                "k1",
                "openai",
                "gpt-4o",
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
                "k1",
                "claude",
                "claude-sonnet-4",
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
                "k1",
                "openai",
                "no-such-model",
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
                    "k1",
                    "openai",
                    "gpt-4o",
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
