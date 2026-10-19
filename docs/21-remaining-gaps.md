# 21 — Remaining implementation gaps (register, 2026-10-17)

> **Superseded for state.** This doc owns the **history** of the 2026-10-17 register. The
> **state** of every open item is in [`docs/28-gap-ledger.md`](28-gap-ledger.md)
> (consolidated 2026-10-19).

Status: register, not plan. Each row names its authority (the doc that owns
the decision) so this file stays an index, and stale rows here lose to their
owner. Written after v0.1.16's branch landed /ui, per-model attribution and
the bounded drain.

## T1 — the 12-hour soak claim (P6's last open row)

Owner: `docs/05-roadmap.md` P6 accept line, `docs/02` soak row.

- Harness exists (`scripts/soak.sh`), 3-minute proof recorded, RSS flat ~26MB.
- Three 12h attempts failed for non-ar reasons: operator `pkill -f` at 54 min
  (the pattern matched the soak's own server — hence the kill-by-PID rule),
  host SIGBUS at 1h52m, and the current in-flight run's CSV is not on disk
  (no `/tmp/ar-soak*.csv`, no harness process found 2026-10-17).
- **Gap:** one unattended 12h run against a *released* musl binary, kill by
  PID, CSV preserved. Nothing else in P6 blocks.
- Cost: wall-clock only. Do not relaunch inside a session that will end
  first; launch detached (`systemd-run --user` or `nohup` with CSV outside
  `/tmp` so a reboot cannot eat it).

## T2 — stream-envelope unwrap + mid-stream error peek (MW2's P2 tail)

**Status 2026-10-18: open.** A probe of the API-key cline row found plain OpenAI SSE (44 frames, usage, `[DONE]`) and no `{success,data}` envelope. The unwrap waits for an OAuth cline session that streams the envelope. See docs/22 (T2, D7).

Owner: `docs/07-blockers.md` MW2 row ("Stream unwrap is P2 pending a gateway
probe").

- State: non-stream cline envelopes are stripped (MW2, shipped); the
  *streamed* cline reply still wraps frames in `{"success":…,"data":…}`, and a
  mid-stream error frame is relayed as data rather than failed over.
- **Gap:** the gateway probe (what does the cline SSE skin actually look like
  per frame?) then the `UsageTee`-side unwrap for that executor family,
  mirroring the non-stream arm.
- Cost: one probe + one unwrap site; risk is misclassifying a legitimate
  `data` payload as the envelope — needs the probe first, not guesswork.

## T3 — empty/degenerate 200 detection in the attempt loop (B2)

**Status 2026-10-18: closed for the non-streamed arm** (docs/22, D5, one classified-empty failover per request). The streamed arm stays open: the first-byte gate sees bytes, not content, and a stream cannot fail over after it commits.

Owner: `docs/07-blockers.md` B2 (kimi-k3 5/10 empty; "highest-value item
here").

- State: MW1 covers 200-with-`error`-object for non-stream; MW5 covers
  first-byte gates for stream. Neither covers *200 with content-shaped
  failure*: empty `content`, degenerate filler (`'<|close|>!'`), or
  reasoning-only output burning the whole token budget (glm-5.3 with
  `max_tokens<1500` — recorded in AROUTE_BUGS as a known non-bug upstream,
  but aroute still counts it a success and `least-used` keeps selecting the
  sick target).
- **Gap:** a content-shape classifier on the buffered non-stream body
  (empty/degenerate → failover with the same charge path as MW1), and the
  same verdict for the streamed arm before first commit (we already hold the
  first chunk there — MW5's re-wrap point is the natural site).
- Risk: false positives failing over good answers (code-heavy replies,
  whitespace-only content for some modalities). Needs a measured corpus
  before the threshold is set, not a heuristic.

## T4 — catalog hygiene: B3's retirement wall

Owner: `docs/07-blockers.md` B3.

- State: 12 of small-stack's 15 targets resolve through
  `stealth/space-bunny-alpha`, labelled "retires Oct 5" — i.e. already past
  date. Every probe this week still returned 200s, so the label is either
  stale or the retirement is rolling.
- **Gap:** the discovery tick watching for the successor id was the recorded
  step and has not been watched to conclusion; the config still carries the
  dependency. Decide: prune the 12 targets proactively, or wait for the
  catalog to say what replaced it.
- Cost: one config edit once the successor id is known; until then it is a
  monitoring task, not an implementation one.

**Status 2026-10-18: no successor id exists in any reachable catalog.** Live
cline model list (auth'd, 2026-10-18) carries no `bunny`/`space` id at all;
the compiled registry has none either. kilocode.org is unreachable from this
box (SSL handshake timeouts on every endpoint, curl included) so its live
catalog could not be read — its serving path still works through aroute
(small-stack 200s via laguna), which suggests the reachability failure is
client-side or regional, not an outage. Current small-stack (both boxes) no
longer references space-bunny targets — the config was pruned to two target
ids at some point after B3 was written, so the retirement wall no longer
bites. Re-open only if a future config edit reintroduces a `stealth/*` id.

## T5 — Gemini response translation (B4)

**Status 2026-10-18: closed for the non-streamed arm** (docs/22, D6), verified against a live `gemini-2.5-flash` reply. The streamed relay is unchanged by contract.

Owner: `docs/07-blockers.md` B4.

- State: gemini authenticates and answers, but in its own
  `{"candidates":[…]}` envelope; OpenAI-dialect clients see no choices. The
  streaming pass-through is a documented contract and stays.
- **Gap:** `ar-translate` gains a `gemini_to_canonical` for the non-stream
  arm, same shape as the Anthropic/Responses inbound translators.
- Cost: one translator + dispatch arm + golden tests; the envelope is small
  and stable.

## T6 — B1's guardrail-model answer rate

Owner: `docs/07-blockers.md` B1.

**Status 2026-10-18: closed, no change needed.** `nemotron-3.5-content-safety:free`
no longer appears in any combo on either box — local `~/.config/ar/config.yaml`
and the VPS config both carry a small-stack of exactly two target ids (laguna,
nemotron-super-vl) with no `content-safety` string anywhere in the file. The
17% figure predates that prune. The registry-vs-config question is moot for
one occurrence; if a second guardrail model appears, re-open with the
`moderation:` card-flag proposal.

## T7 — modelRoles routing after the free-stack trim (operator config, not code)

Owner: none in-repo; `~/.omp/agent/config.yml` is harness-side.

**Status 2026-10-18: closed.** Roles resolve against `models.yml` providers,
not against aroute directly — `aroute/free-stack` is provider `aroute` + model
`free-stack`, which routes. The user's own edits had already fixed
`advisor/task/smol`; the two remaining broken roles were repointed and
verified live (`web: aroute/small-stack`, `default: aroute/free-stack`;
checkpoint `pre-t7-modelroles`). small-stack answers 200 with content;
free-stack's transient 503s were nvidia-key cooldown state (direct upstream
probe returned 200), not routing — self-healing per the resilience design.

## T8 — discovery scheduler cadence (partial reversal, recorded)

Owner: `docs/02` deferred table, discovery row; `crates/ar-registry/src/discovery.rs:5`.

**Status 2026-10-18: closed with rationale.** The 24-hour sweep is a
hosted-service concern: there, one operator's catalog edit must reach every
tenants' cache on a schedule. Here the proxy and the config are the same
operator's machine — `DiscoveredCatalog`'s TTL tick keeps the union fresh
during a long-lived serve, and `aroute models --refresh` covers the explicit
path. A second, scheduled sweep would add a timer that can only race the
tick for the same effect. The `docs/02` row should read "closed by
decision" rather than "partially reversed"; the cadence difference is
intentional, not pending work.

## Not gaps (re-litigation bait, recorded so they stay closed)

- `/metrics` model labels — cardinality cap is deliberate (docs/20, kept).
- x-ar-model carrying the client spelling — deliberate, tested (routes.rs).
- Trace channel lossiness and redb's exclusive file lock — contract, not
  defect (docs/02 obs row).
- Mid-stream failover after content flowed — impossible without duplicating
  bytes; the in-band `stream_error` frame is the honest terminal (MW5).
- Pipeline/fusion semantics — shipped end to end including the judge;
  `docs/02` rows closed.
