# Audit notes — `classify_refresh` vs the reference taxonomy

Row-for-row cross-check of `ar_exec::oauth::classify_refresh` against
`../OmniRoute/open-sse/services/errorClassifier.ts` (+ `services/accountFallback.ts`
signal lists it imports) and `../OmniRoute/open-sse/executors/grok-cli.ts` refresh
mappings. Read-only on `../OmniRoute`; nothing here was changed.

## What was compared, and the two axes

Our classifier answers a *refresh-endpoint* question: given the (status, reason) a
token endpoint returned, is this account finished? The reference has **no single
equivalent** — it is two surfaces:

| Reference surface | Question it answers | Terminal? |
|---|---|---|
| `classifyProviderError` (`errorClassifier.ts:391`) | what does *this dispatch response* mean | `FORBIDDEN` / `ACCOUNT_DEACTIVATED` only |
| `refreshGrokBuildCredentialsOnce` (`grok-cli.ts:194`) | is *this refresh* retryable | `invalid_grant`, `invalid_client`, or 3rd failed attempt |

So a disagreement is reported against whichever surface names the same condition,
and each row records the reference line it was read from. Direction is stated as
**we-X / they-Y**, where Y is the reference's verdict on that same condition.

Our side of every comparison, as read at the end of this pass (line numbers are
against that tree, and the file was mid-flight while this was written):

- `TERMINAL_REFRESH_STATUS` — `crates/ar-exec/src/oauth.rs:455` (9 rows)
- `CARVE_OUT_TERMINAL_STATUS` — `oauth.rs:484` (1 row, `(401, "invalid_client")`),
  scanned by `reason_in` and the generated CHECK but *not* by the classifier's
  list membership test — a carve-out-only reason is terminal only where a
  `carve_out` arm names it
- `OAuthKind::carve_out` — `oauth.rs:348` (Cursor `expired`/`token_expired`, Claude
  `invalid_grant`, GrokCli `invalid_client`)
- `is_transient_status` — `oauth.rs:558` (408, 425, 429, 5xx)
- `classify_refresh` order — `oauth.rs:524` (carve-out → transient status → list →
  transient)
- `reason_in` — `oauth.rs:572`, lowercases the body and substring-matches our tokens
  across both lists

## Class A — we-terminal / they-transient

**Account-bricker candidates.** Each of these retires a session in `ar` where the
reference keeps it (or the account) alive. `ar`'s own rule is that the only way to
become terminal is to be named in the list — so every row here is a deliberate act,
and three of them sit on a body the reference explicitly reads as recoverable.

| # | (status, reason) | ours | reference | note |
|---|---|---|---|---|
| A1 | `(403, permission_denied)` | terminal | **transient / recoverable** — `errorClassifier.ts:540` puts `PERMISSION_DENIED` in `recoverableProject403` → `PROJECT_ROUTE_ERROR`, with the in-code reason that these are "NOT an account ban" and the connection must stay "active and recoverable once the project/API is fixed" | Strongest row. Detection agrees (we lowercase the body, so we match the same signal); only the **verdict** is opposite. Our `(403, permission_denied)` is the single row that can brick an account the reference would keep routing. |
| A2 | `(401, invalid_token)` | terminal | **transient / recoverable** — `OAUTH_INVALID_TOKEN` → `chatCore.ts:4055` only records `lastErrorType` and logs "token refresh available"; no cooldown ban, no `isActive:false` | Surface caveat: the reference reads this on a *dispatch* 401 and recovers by refreshing; we read it on the *refresh endpoint's* response. Different surfaces, same (401, invalid-token) condition, opposite direction. Flagged because our verdict is durable and theirs is one refresh away from fine. |
| A3 | `(401, token_revoked)` | terminal, immediately | **transient on first two attempts, terminal on the 3rd** — grok's set does not contain `token_revoked`, so `grok-cli.ts:211` only returns `null` (terminal) once `attempt === GROK_BUILD_REFRESH_MAX_ATTEMPTS` | Weaker than A1/A2: both sides eventually retire. We retire on the first sighting; the reference spends two round trips first. Disagreement is **timing**, not disposition. |
| A4 | `(401, token_expired)` | terminal for every kind except Cursor (which `carve_out` sends to transient, `oauth.rs:228`) | **transient** — no reference row names an expired token as terminal; `classifyProviderError`'s 401 branch yields `UNAUTHORIZED`, recorded at `chatCore.ts:4049` with no ban | The Cursor carve-out already agrees. Codex/Cline/Claude/Gemini-CLI all retire here. No reference evidence either way for `token_expired` specifically, so this is a divergence without a contradicting reference verdict — recorded for completeness, not as a confirmed brick. |

No reference row names `token_revoked` or `token_expired` as a terminal *reason*;
both are `ar`'s own vocabulary (see Class C).

## Class B — they-terminal / we-transient

Safe direction: the reference bricks where `ar` keeps retrying. The cost is wasted
attempts bounded by the router's 3-attempt cap and the per-key cooldown, not a dead
account.

| # | (status, reason) | ours | reference | note |
|---|---|---|---|---|
| B1 | `(x, invalid_client)` for a non-`GrokCli` kind | transient (unrecognised-auth-failure) | **terminal** — `GROK_BUILD_TERMINAL_REFRESH_ERRORS` = `{invalid_grant, invalid_client}` (`grok-cli.ts:41`), checked at `grok-cli.ts:213` | **Closed on `grok-cli` itself, still open for the other five kinds.** `OAuthKind::GrokCli` is now a variant (`oauth.rs:253`) and `carve_out` returns `Unrecoverable` on `invalid_client` (`oauth.rs:348`), so the reference's own provider agrees — and because `reason_in` matches on the reason string alone, a 400 or 401 `invalid_client` both land on it. Codex/Cline/Claude/Gemini-CLI/Cursor still read it transient. Narrower than it looks: the reference's set is scoped to the grok executor, so no reference evidence says *their* `invalid_client` is terminal either. Separate defect, in our code: `CARVE_OUT_TERMINAL_STATUS` (`oauth.rs:484`) feeds `reason_in` and the generated CHECK but is **not** `TERMINAL_REFRESH_STATUS`, so `ar-cli`'s `terminal_reason` helper (`commands.rs`, reads only `TERMINAL_REFRESH_STATUS`) cannot resolve the row — the doctor cell that spells a terminal reason cannot name this one. |
| B2 | 3 failed refresh attempts, any reason | transient forever (bounded only by the router) | **terminal** — `grok-cli.ts:212` retires on the 3rd attempt regardless of status or code | We push the retry budget to the router; grok keeps its own counter in the refresher. Same eventual disposition, different owner of the count. |
| B3 | `(401, account_deactivated)` / `(403, account_deactivated)` prose body | transient | **terminal** — `ACCOUNT_DEACTIVATED` (`errorClassifier.ts:450`, `:484`) → `chatCore.ts:3875` | Detection gap, not a verdict gap. See Class C. |

## Class C — detection gaps (same condition, no match)

`reason_in` (`oauth.rs:432`) lowercases the body and substring-matches **our snake_case
tokens**. OmniRoute's terminal signals are **natural-language phrases**
(`accountFallback.ts:209`, `:272`). `"account has been disabled"` does not contain
`account_disabled`; `"invalid authentication credentials"` does not contain
`invalid_token`. So on a real deactivated-account body `ar` falls through to
`unrecognised-auth-failure` → transient, while the reference retires.

| Our token | Reference signal(s) that should match it | Match today? |
|---|---|---|
| `account_disabled` | `account_deactivated`, `account has been deactivated`, `account has been disabled`, `your account has been suspended`, `this account is deactivated`, `this service has been disabled in this account[ for violation]` | **no** — spaces, not underscores |
| `invalid_token` | `invalid authentication credentials`, `valid authentication credential`, `invalid credentials`, `oauth 2`, `login cookie`, `re-authenticate your cline account` | **no** — phrase forms |
| `permission_denied` | `PERMISSION_DENIED` (reference matches case-sensitively; we lowercase, so we match) | yes |
| `token_revoked`, `token_expired`, `unauthorized_client`, `no_refresh_token` | no reference equivalent | n/a |

This is a real defect in both directions at once: A1 above fires on a signal the
reference calls recoverable, and C here means the signal the reference calls terminal
never reaches the list. Both are one-line additions to the lists on each side and are
reported, not fixed, per the task boundary.

## Class D — agreements (recorded so the next reader knows they were checked)

| (status, reason) | ours | reference | line |
|---|---|---|---|
| `(400, invalid_grant)` | terminal (Claude excepted by carve-out) | terminal | `grok-cli.ts:41` |
| `(400, no_refresh_token)` | terminal | terminal — grok returns `null` before any HTTP when `!credentials.refreshToken` | `grok-cli.ts:293` |
| 200 with no `access_token` | not a `classify_refresh` input | terminal at 3rd attempt | `grok-cli.ts:219` |
| `(429, …)` | transient (`is_transient_status`) | `RATE_LIMITED` / `QUOTA_EXHAUSTED` cooldown, non-terminal | `errorClassifier.ts:417` |
| `(5xx, …)` | transient | `SERVER_ERROR`, non-terminal | `errorClassifier.ts:597` |
| `(402, …)` | transient (falls through) | `QUOTA_EXHAUSTED`, non-terminal | `errorClassifier.ts:455` |
| `(404, …)` | transient | `MODEL_NOT_FOUND` — model-scoped lockout, account untouched | `errorClassifier.ts:431` |
| `(422, gcp_project_required)` | transient | `GCP_PROJECT_REQUIRED` — rotates accounts, no ban | `errorClassifier.ts:604` |
| `(400, context overflow)` | transient | `CONTEXT_OVERFLOW`, non-terminal | `errorClassifier.ts:609` |
| 401 / 403 fingerprint rejection | transient | `FINGERPRINT_REJECTION`, explicitly "account state stays untouched" | `errorClassifier.ts:474` |
| 403 geo-block | transient | `GEO_BLOCKED`, non-terminal, cached exclusion | `errorClassifier.ts:466` |
| 403 Claude "request not allowed" | transient | `REQUEST_REJECTED`, non-terminal — a per-request refusal | `errorClassifier.ts:503` |
| 403 grok content refusal | transient | `REQUEST_REJECTED`, non-terminal | `errorClassifier.ts:513` |
| 403 account-verification prompt | transient | `PROJECT_ROUTE_ERROR`, recoverable — the 1-year ban was the wrong response | `errorClassifier.ts:487` |
| 408 / 425 | transient | unclassified (`null`) → no lockout | `errorClassifier.ts:622` |

## Summary

- 9 rows in `TERMINAL_REFRESH_STATUS`, 3 carve-outs (`Cursor expired`/`token_expired`,
  `Claude invalid_grant`, `GrokCli invalid_client`), 5 transient statuses
  (408/425/429/5xx), 1 carve-out-only row in `CARVE_OUT_TERMINAL_STATUS`.
- **4 we-terminal / they-transient rows (A1–A4)**, of which **A1
  `(403, permission_denied)` is a confirmed account-bricker** — the reference reads
  that exact signal as a recoverable project-config error and keeps the connection
  active. A2 is a surface-mismatch row, flagged as a bricker candidate. A3 differs
  only in timing. A4 has no contradicting reference row.
- **3 they-terminal / we-transient rows (B1–B3)** — safe direction. B1 is half-closed:
  `grok-cli` agrees now that it is an `OAuthKind`; the other five kinds still do not.
- **2 detection gaps (C)** — `account_disabled` and `invalid_token` cannot match the
  reference's phrase-form signals, so a genuinely deactivated account reads transient.
- **15 agreements (D)**, including the whole rate-limit / 5xx / quota / geo /
  fingerprint / content-refusal surface.

None of these were fixed here: no code outside docs is in scope for this pass. A1 and
C are the two that change routing behaviour for a real account.

## Named findings for the owning stream

Reported, not fixed. `file:line` against the tree as read at the end of this pass.

| id | file:line | finding |
|---|---|---|
| **A1** | `crates/ar-exec/src/oauth.rs:463` | `(403, permission_denied)` is terminal. `errorClassifier.ts:540` puts `PERMISSION_DENIED` in `recoverableProject403` → `PROJECT_ROUTE_ERROR`, explicitly "NOT an account ban". Detection already agrees (we lowercase); only the verdict is wrong. Highest-severity row in this report. |
| **C1** | `crates/ar-exec/src/oauth.rs:462` | `account_disabled` cannot match `ACCOUNT_DEACTIVATED_SIGNALS` (`accountFallback.ts:209`), which is phrase-form (`account has been disabled`, `account_deactivated`, …). A genuinely deactivated account therefore reads transient. A phrase-list alongside the tokens, or a `contains`-style alias set, is the fix. |
| **C2** | `crates/ar-exec/src/oauth.rs:459` | `invalid_token` cannot match `OAUTH_INVALID_TOKEN_SIGNALS` (`accountFallback.ts:272`), also phrase-form (`invalid credentials`, `invalid authentication credentials`, …). |
| **B1'** | `crates/ar-cli/src/commands.rs` (`terminal_reason`) | Reads only `TERMINAL_REFRESH_STATUS`, so it cannot resolve `(401, "invalid_client")` from `CARVE_OUT_TERMINAL_STATUS` even though the generated CHECK admits it. A doctor cell cannot spell a reason the store would accept. |
| **A2/A3/A4** | `oauth.rs:459–461` | The three 401 rows are terminal for every kind except Cursor's `token_expired`. The reference records 401 as refresh-available rather than retiring. Rows stay flagged pending a decision; they are the rows a wrong verdict bricks. |

## Fix record

Appended after the survey above, which is left as written — that is the record of
what was found, and this is the record of what was changed about it. Verdicts moved
in `crates/ar-exec/src/oauth.rs`; reference lines cited below are OmniRoute's, read
at the same tree as everything above.

**Scope of the two halves.** Everything above is the *pre-fix* survey and keeps its
own numbers: the row count, the Class A and Class C counts, and every per-row verdict
there describe the tree as it stood when the survey was read, and are deliberately
not rewritten. The current state is this section. Where the two disagree, the
verdicts above are superseded as recorded below — `A1`/`A2`/`A4` are demoted, `A3` is
kept on purpose, `C1`/`C2` are closed, and the list is 6 rows plus 1 carve-out-only
row rather than the 9 rows the survey counted.

### End state

`TERMINAL_REFRESH_STATUS` is exactly **6 rows**:

| # | (status, reason) |
|---|---|
| 1 | `(400, "invalid_grant")` |
| 2 | `(400, "unauthorized_client")` |
| 3 | `(400, "no_refresh_token")` |
| 4 | `(401, "token_revoked")` |
| 5 | `(403, "account_disabled")` |
| 6 | `(410, "token_revoked")` |

plus **one carve-out-only row**: `(401, "invalid_client")` in
`CARVE_OUT_TERMINAL_STATUS`, terminal only for `GrokCli` because its `carve_out` arm
names it. 6 + 1 is the whole terminal surface — six rows in the shared list, one
reachable only through the carve-out, and nothing else can retire an account.

**Demoted to transient (3):** `(401, "invalid_token")`, `(401, "token_expired")`,
`(403, "permission_denied")`. The Cursor `token_expired` carve-out is **pruned as
dead**: it existed only to un-retire that row, and with the row gone there is nothing
left for it to override.

**`account_disabled` is reachable.** It now also matches the reference's phrase forms
verbatim, so a genuinely deactivated account reads terminal instead of falling through.

`ar-cli`'s `terminal_reason` now resolves the **union** of `TERMINAL_REFRESH_STATUS`
and `CARVE_OUT_TERMINAL_STATUS` (finding B1'), so the doctor cell can name the one
reason the generated CHECK admits and the classifier can return.

### Per finding

| id | disposition | on what evidence |
|---|---|---|
| **A1** `(403, permission_denied)` | **demoted to transient** | `errorClassifier.ts:536-545` puts `PERMISSION_DENIED` in `recoverableProject403` → `PROJECT_ROUTE_ERROR`, and `chatCore.ts:4064-4072` records that verdict and warns "not banning" — no cooldown, no `isActive:false`. The strongest row in the survey, and the only confirmed account-bricker; it is gone. Detection already agreed (we lowercase the body), so demotion moves the verdict without touching matching. |
| **A2** `(401, invalid_token)` | **demoted to transient** | The reference's 401 branch (`errorClassifier.ts:436-452`) gives `oauthInvalid` precedence — `:437-439` returns `OAUTH_INVALID_TOKEN` *before* the `accountDeactivated` test at `:450` — and `chatCore.ts:4055-4063` handles that verdict as record-only: `lastErrorType` / `lastError` / `errorCode`, then a warn that "token refresh available". No ban, no cooldown. Our side was durable where theirs is one refresh away from fine, and the surface caveat in the survey is now moot in the safe direction. |
| **A4** `(401, token_expired)` | **demoted to transient** | No reference row names an expired token as terminal — the survey recorded this as a divergence without a contradicting verdict. Nothing to keep it terminal on, and the Cursor `token_expired` carve-out that used to un-retire it is pruned with it. |
| **A3** `(401, token_revoked)` | **KEPT terminal, on first sighting** | The one row left on purpose, and the only remaining we-terminal/they-transient row. `grok-cli.ts:211-216` computes `isTerminal` as `attempt === GROK_BUILD_REFRESH_MAX_ATTEMPTS \|\| errorCode ∈ {invalid_grant, invalid_client}`; `token_revoked` is in neither, so grok returns `undefined` and retries until the 3rd attempt. Both sides retire the account — the disagreement is **timing only**, and we own it explicitly rather than pretending it away. Rationale for keeping it axiomatic: a revoked token is not a project-config or IdP hiccup, it is a grant the issuer has withdrawn, and it is the one row where retiring on first sighting costs nothing a later sighting would have saved. |
| **C1** `account_disabled` | **closed by phrase aliases** | `ACCOUNT_DEACTIVATED_SIGNALS` (`accountFallback.ts:209`) is natural-language, so `reason_in` now also matches those phrases verbatim — `account_deactivated`, `account has been deactivated`, `account has been disabled`, `your account has been suspended`, `this account is deactivated`, `this service has been disabled in this account for violation`, `this service has been disabled in this account` — alongside our snake_case token. The row stays terminal and a real deactivated account now reaches it. |
| **C2** `invalid_token` | **closed by demotion** | With `(401, "invalid_token")` demoted there is no terminal row left to reach, so the detection gap no longer has a consequence: `OAUTH_INVALID_TOKEN_SIGNALS` (`accountFallback.ts:272`) is phrase-form (`invalid credentials`, `invalid authentication credentials`, `valid authentication credential`, `oauth 2`, `login cookie`, `re-authenticate your cline account`), none of which our underscore token matched, and the literal `invalid_token` matches nothing on the reference dispatch surface either. Such bodies now fall through to transient, which is exactly the reference's recoverable verdict. |

`B1'`, the doctor helper, is closed by the union lookup described above.

### Accepted divergences

Three, recorded so the next reader does not mistake them for unfixed findings:

**(a) Case sensitivity on `PERMISSION_DENIED` — safe direction.** The reference
matches it case-sensitively (`bodyStr.includes("PERMISSION_DENIED")`,
`errorClassifier.ts:540`) while our `reason_in` lowercases the body before matching.
Our transient match is therefore a superset of the reference's: we also demote
`permission_denied` where the reference would have kept reading the body for other
403 signals. That is the direction to err — a wrong transient costs one retry, a
wrong terminal costs an account — so the divergence is accepted, not closed.

**(b) No-refresh-token retirement cannot be mirrored at the classifier.** The
reference retires immediately when there is no refresh token to spend, before any
HTTP (`grok-cli.ts:293-296` returns `null` when `!credentials.refreshToken`), and
carries that as a terminal outcome. `classify_refresh` sees only a `(status, reason)`
pair returned by the token endpoint — it never sees credential state — so there is
no signal at this layer to mirror it on. It is enforced one level up, where the
credential row is actually read, and `no_refresh_token` stays a row for the issuer's
own answer to the same condition.

**(c) Runtime custom ban signals vs a compile-time CHECK.** The reference merges
operator-supplied phrases at runtime — `getMergedBannedSignals()`
(`accountFallback.ts:229`, consulted at `:480`) folds DB-loaded custom signals into
the deactivated check. Our terminal surface is a compile-time list generating a
static SQL CHECK, so it cannot absorb runtime configuration. Accepted as a property
of the design rather than a gap: the store constraint is what stops a transient
`(status, reason)` being recorded as a retirement, and that only holds if the set is
closed at build time.

**(d) Hard budget clamp: eval-only, live dispatch forwards unclamped.** The
reference returns an over-budget request body to the upstream with a validation
warning when its preserve-guards make the target unreachable; our
`clamp_to_budget` (`crates/ar-compress/src/budget.rs`) instead guarantees
never-over-budget text by cutting. A silent clamp on the dispatch path would both
truncate user content and diverge further from the reference's warn-not-clamp
contract, so live dispatch never calls it — its only caller is the eval harness
(`crates/ar-compress/src/eval.rs`). Wiring it ever requires an explicit opt-in (a
config knob plus a request header, both default-off) and a new row here. Pinned by
`forwards_an_over_budget_body_to_the_executor_unclamped`
(`crates/ar-server/src/routes.rs` tests): it fails the moment any default-on
truncation enters the dispatch path.