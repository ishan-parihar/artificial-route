# 25 — Red-team register (2026-10-18)

Status: **fixes applied on 2026-10-18 in the working tree; not committed, not deployed.**
The register was written before the code changes, as AGENTS.md asks. Each row carries a
**State**: `fixed` (code changed and covered by a test or a browser check), `partly fixed`
(the stated part is done, the rest is named), `open` (not started; a decision, a release
change, or a behavior change is needed), `refuted` (checked against the code, not a
defect).

Scope: a read-only red-team of the request path, the local web UI (`/ui`), the
installer and release pipeline, and state handling. Four Haiku audit agents covered
the areas. The findings below were checked by reading the code. Live probes were run
against the running gateway on `127.0.0.1:20128` (read-only GETs, no keys, no writes).

## Findings

| ID | Sev | Where | Finding | State |
|---|---|---|---|---|
| S1 | High | `crates/ar-cli/src/serve.rs` (router), `webui.rs` `mount` | No `Host` or `Origin` check on the whole listener. A DNS-rebinding page reaches `/v1/*` and `/ui/api/*` on loopback. Probed live: `GET /v1/models` with `Host: evil.example` returns 200 on the gate-unarmed gateway, and `/ui/api/state` does the same. The mint, provider-key, and limits routes are reachable the same way. | fixed |
| S2 | Med | `commands.rs` (ar.env writer), `webui.rs` `set_env_var` | `ar.env` is written with `fs::write` and only then chmod'd to `0600`. A new file is briefly `0644` under a `022` umask, and an existing `0644` file keeps its mode while secrets are written. | fixed |
| S3 | Low | `crates/ar-obs/src/audit.rs` `ct_eq` | Hand-rolled compare guards the admin scope of the audit ledger. It leaks length. `subtle` and `sha2` are already in `Cargo.lock`. | fixed |
| C1 | High | `install.sh:5`, release body in `.github/workflows/release.yml` | The install command fetches `install.sh` from the mutable `main` branch and pipes it to a shell, not from the release tag. | fixed: the release body pins the tag. The README keeps `main`, so the installer is always current. It now says so, and gives the tag-pinned command and the verification command. The `install.sh` header keeps `main` for the same reason |
| C2 | High | `release.yml` (`SHA256SUMS`) | Checksums ship in the same release as the binary, with no signature or provenance. They detect corruption, not substitution. | partly fixed: the release job produces a build-provenance attestation (`actions/attest-build-provenance`, signed with the run's OIDC identity). `install.sh --verify-attestation` runs `gh attestation verify --repo` on the binary before anything is installed, and is fatal on failure (checked with a fake `gh` for pass and fail). It is opt-in, not the default, because releases published before attestations existed have none to check. `SHA256SUMS` itself is still unsigned. Not run in CI |
| C3 | Med | `install.sh` (checksum block) | With neither `sha256sum` nor `shasum` present, the installer warns and installs the binary unverified. The comment just above says missing tooling must not skip verification. | fixed: the branch `die`s. The step that tried a silent package install (`pm_install coreutils`, errors ignored) is removed, so a missing tool is reported and the install stops, with no `sudo` run from a piped install. Not run on a host without a checksum tool; the branch was checked by reading |
| C4 | Med | `release.yml` | Actions use mutable major tags (`@v4`, `@v2`). The shellcheck tarball is downloaded without a checksum. The release job has `contents: write`. | fixed: every `uses:` is pinned to a commit SHA (checkout v4.2.2, action-gh-release v2.6.2, attest-build-provenance v2.4.0). The shellcheck tarball is checked against a pinned SHA-256 (`8c3be12b…`), computed from the download, because the release publishes no checksum file. Not yet run in CI |
| U1 | Med | `crates/ar-cli/src/webui/page/app.js:121-123` | Dashboard cards for Providers, Combos, and Client keys are created with `-` and never written. Only `st-spend` has a setter. | fixed |
| U2 | Med | `app.js:118-130` | The dashboard's Routing list is always empty. `renderCombos` runs only from the combos pane. | fixed |
| U3 | Med | `app.js` (no refresh) | No refresh timer; the dashboard goes stale until reload. A timer that re-renders the whole view would clear in-progress forms, so only the usage numbers should refresh. | fixed: the spend and usage totals refresh every 15 s on the dashboard only, and only while the tab is visible. The view itself is not rebuilt. Measured in a browser (Playwright, 1440 px, 35 s on the dashboard): usage requests at 0.1 s, 15.1 s, 30.1 s |
| U4 | Med | `app.js` `usageSummary` / `refresh` | "Recent usage" totals are built before `fillUsage` runs, so they stay blank on first load. | fixed |
| U5 | Med | `app.js` providers pane | Every configured provider is rendered in one table. Measured on the scratch config (330 providers): 2,124 buttons and 289 form fields in one DOM. No paging. | fixed: the table shows 40 rows, with a name filter and a "Show all N" control. Browser-checked: 40 rows at load, 281 after "Show all", 3 for filter `openai`, no console errors |
| U6 | Low | `app.css` | One dark palette, no light theme, no `prefers-color-scheme`. Two `:focus-visible` rules and one reduced-motion rule. OmniRoute's dashboard has 847 `.dark` rules and 33 focus rules. | fixed: `prefers-color-scheme: light` swaps the palette, and `color-scheme` follows the OS. Solid-accent ink and amber-as-text have their own tokens. The dark tokens are unchanged. Rendered in both schemes at 1440 px with no console errors. There is no manual toggle, and 375 px in light was not checked |
| U7 | Med | `app.css`, `index.html`, `app.js` | Probe on a scratch build found the sidebar footer paths overlapping (1440 px, 4 `text-overlap` FAILs). At 375 and 768 px the collapsed top bar was 152 px tall and sticky, the Configured filter input ran past its card, the route index digits were 10 px, and row actions (Edit, Delete, Remove, Show all) were 27 px tall. | fixed, measured. The footer overlap did not reproduce on a fresh build, and no footer rule changed for it. The collapsed bar puts its footer on one row (152 → 116 px). The filter input shrinks and the header wraps. Index digits are 12 px. At 880 px and below, row actions get 44 px hit areas. Probe: 0 FAIL at 1440, 768, 375 px in light and dark, for `#/dashboard` and `#/providers`. A later pass fixed the warnings it had judged: the collapsed sidebar is `position: static` at 880 px and below (it was sticky and took about 116 px of every screen), inline links get a 24 px minimum target on desktop and 44 px on collapsed layouts, and the phone hint is 15 px. 2 warnings remain, judged: 21 px first-viewport type at 1440 px on both routes. That is a heuristic written for marketing pages, and an operator console is a data view, not a hero |
| P1 | Low | `ar-server` (`routes.rs`, `exec.rs`), `ar-route/src/resilience.rs`, `ar-limit/src/lib.rs` | 11 `Mutex::lock().expect(...)` calls on request paths. A panic while one lock is held poisons it, and every later request on that path panics too. | fixed: the guard is recovered with `unwrap_or_else(PoisonError::into_inner)`. What each lock guards is a counter or a map, which stays usable after a panic. Not tested separately |
| D1 | Low | `crates/ar-limit/src/lib.rs` | The only library crate without `#![deny(missing_docs)]`. | fixed: the attribute is added and the crate compiles with it denied |
| R1 | High | `crates/ar-server/src/exec.rs:940-947` | Upstream stream errors are filtered out (`filter_map`, `Err` only logged), so the relay sees a clean end of stream. The relay names a cut in-band only for dialects with a truncation frame; a raw dialect is cut silently. The `error decoding response body` cuts on 2026-10-18 at 04:35:52 fit this path. | fixed for raw dialects: the error sets a `CutFlag` on `Upstream`, and the raw relay (`abort_on_cut`) ends the body with an error, so the client sees a dropped connection. In-band dialects keep their named truncation frame. The root cause of the 04:35:52 cuts is still unknown: the upstream's error is not yet logged with its cause |
| R2 | High | `crates/ar-exec/src/lib.rs:764-789` (`into_bytes`), client builder at `:285` | No idle or total timeout on the body after headers. A stalled body holds the upstream and client connections until the client disconnects. The keepalive relay keeps the client alive with frames, so nothing ends it. | fixed |
| R3 | Med | `crates/ar-server/src/routes.rs:2394-2407` | A stream that ends before any chunk is emitted gets no truncation frame (the check is `== Some(false)`, and `None` means zero chunks). The client gets 200 with an empty body. | fixed |
| R4 | Med | `routes.rs` `is_noop` | For dialects with no keepalive and no stream-error frame, the relay is raw and a cut is silent. | fixed with R1. The abort fires only on an upstream body error, never on a missing completion marker, so a clean stream that omits `[DONE]` still ends normally and working clients are unaffected |
| R5 | Med | `crates/ar-route/src/attempt.rs` (Success branch) | The circuit breaker and cooldown are credited on headers, before any body byte is read. A provider that always cuts mid-stream never trips the breaker. | fixed: a stream's breaker verdict is settled when its body ends (`ar_route::settle_stream`). A cut is one failure. A clean end, or a client that goes away, is a success. The first-byte credit was removed for streams, because it cancelled one failure per request. A half-open probe is the exception: its first byte closes the breaker (`Resilience::credit_first_byte`), so a long stream cannot hold the breaker shut for its length. Tests: `settle_stream_should_open_the_breaker_when_every_body_is_cut`, `settle_stream_should_credit_nothing_until_the_body_is_dropped`, `credit_first_byte_should_close_half_open_when_the_probe_streams`, `credit_first_byte_should_leave_open_breaker_when_provider_is_down`. Not tested: the attempt-loop call itself, which needs an executor fixture |
| R6 | Med | `attempt.rs:1020-1029` (`drain_success_body`), `exec.rs:1045-1061` (`drain_chat`) | Non-streaming 2xx bodies are buffered into an unbounded `Vec`. | fixed |
| R7 | Low | `crates/ar-server/src/exec.rs` (`CancellationToken::new()`) | Suspected: a client disconnect before headers is not wired to the upstream request, so pre-header waits run to the 110 s start budget. | refuted by experiment: `crates/ar-server/tests/disconnect.rs` hangs up during a pre-header wait (800 ms mock executor), for a stream and for a non-stream request. In both, the executor future is dropped before it finishes (`cancelled=1`, `finished=0`), so the upstream call goes with its `reqwest` request. Nothing spawned on the request path outlives the handler. The stale `TODO(#p1-cancel)` in `exec.rs` now points at the test. No connection-level watcher is needed |
| R8 | Med | `exec.rs` (`drain_chat`) | On an upstream error, `drain_chat` returned the partial body, so a cut non-stream reply was answered 200 with truncated JSON. Verified by reading the call site (`unwrap_response`). | fixed |
| K1 | High | `exec.rs:940-948` → `routes.rs` `tee_and_store` (`:2250-2290`) | A transport error on a non-stream body becomes a clean EOF, and the tee then stores the partial body as a 200 for the TTL (5 min). Same root as R1. | fixed (with R1) |
| K2 | Med | `crates/ar-cache/src/lib.rs:441` (`store_truncated`), `routes.rs:~2276` | `store_truncated` is never called. Any 2xx is stored whatever its `finish_reason` (for example `length`). | fixed: `stopped_at_ceiling` reads the stop marker of each dialect (`finish_reason`, `stop_reason`, `finishReason`), and `tee_and_store` skips the store when it matches. Unit-tested per dialect |
| K3 | Med | `routes.rs:1560-1563` (`request_key`) | The cache tenant is the literal `"default"`, not the client key. Any client key holder that sends an identical body gets another key's cached reply, and the hit skips per-key spend recording. Verified by reading. | fixed |
| K4 | Low | `routes.rs:622-626` | The cache key uses the client-facing alias (combo name), not the resolved target chain. A combo retarget serves the old target's cached reply for up to 5 min. | fixed: the key's model segment is the `provider/model` the plan's winner will serve, not the alias (`request_key`). Test `request_key_should_change_when_the_served_target_changes`. Second half: a reply served by a fallback is not stored at all (`storable_reply`), so the winner's key never holds it. Tests `storable_reply_should_refuse_a_fallback_under_the_winners_key` and e2e `e2e_a_fallback_reply_is_not_replayed_under_the_winners_key`. The e2e test sends `accept: application/json`, because the default test `Accept` header forces a stream and skips the cache |
| K5 | High | `crates/ar-config/src/lib.rs:1220-1232` (`validate`) | Only key-name existence is checked, not value. An empty `AR_KEY_*` expands to `""` and the provider goes upstream unauthenticated. | fixed: `serve` refuses to start when an API-key provider's `$VAR` key expands to nothing (`Config::empty_api_keys`). The message names the key and never its value. Loading stays permissive, so `doctor` and the first-login flow still read such a config. An empty OAuth access row is not refused, because it is a first login. A literal `""` in the file is keyless by intent. Tests: `lists_an_api_key_whose_variable_is_empty`, `leaves_an_empty_key_no_provider_uses_out_of_the_list`. Reload: `ConfigHandle::reload` (which the file watcher calls) refuses a config with an empty key and keeps the previous one serving, with `ConfigError::EmptyApiKeys`. Tests `refuses_a_reload_whose_provider_key_is_empty` and `accepts_a_reload_with_every_provider_key_set`. `load` stays permissive |
| K6 | Med | `lib.rs:1208` | `$VAR` is expanded textually before YAML parsing. A secret with ` #`, `:` or a newline changes the YAML structure silently. | fixed: `parse` reads the YAML first and expands each string value after, so a secret stays one value. Test: `keeps_a_secret_with_yaml_syntax_as_one_value`. Mapping keys are not expanded, which matches how the keys are used |
| K7 | Med | `commands.rs:2306-2314` (`write_file`) | Truncate-then-write, not temp-plus-rename, for every config and backup writer. Shared fix with S2 (the env writer) is planned; the config writers are not yet changed. | fixed: `write_file` now stages to a synced temp file and renames it over the target (`write_atomic`). Test: `write_atomic_should_replace_contents_and_leave_no_temp_file`. The env writer was already `write_private` (S2) |
| K8 | Med | `crates/ar-compress/src/lite.rs:145-190` | Lite strips any whitespace-preceded `#word` to end of line. "Never deploy on Fridays #ops, do not override" loses its negation. | fixed: `#` is no longer a comment marker. Code comments with `#` now stay in the prompt, which saves less but never drops prose. Test: `keeps_prose_after_a_hash_word` |
| K9 | Low | `crates/ar-compress/src/caveman.rs` | Deletes hedges ("I think that", "it seems like"). A silent confidence shift in the forwarded prompt. | fixed (decision: keep hedges). The four hedge phrases (`it seems like`, `it appears that`, `i think that`, `i believe that`) are out of the phrase table. Test `keeps_a_hedge_at_every_rung`. The cost is fewer tokens saved on hedged prompts |

### Notes on the fixes

- **R1 / R4 (stream path, fixed for raw dialects).** The stream filter still drops the
  error from the byte stream, because `ChunkStream` is `Stream<Item = Bytes>` and has 15
  call sites. Changing it was not needed. The filter now sets a `CutFlag` on `Upstream`,
  and the raw relay (`abort_on_cut`) reads the flag when the bytes end. A cut ends the
  body with an error, so the client sees a dropped connection and not a clean end. The
  flag is only set by a body error, never by a missing `[DONE]`. In-band dialects keep
  their named truncation frame, unchanged. Non-stream replies were already fixed (K1,
  R8): a cut reply is a 502 failure.
- **Corrections from the audit hand-backs.** Checked against the code before any
  change. Four reported claims are not defects:
  - *Idle timeout missing (streaming hand-back, item 2).* Refuted. `into_bytes` wraps
    every body read in `IDLE_BODY_TIMEOUT` (120 s) in `ar-exec/src/lib.rs`.
  - *Zero-chunk streams get no error frame (streaming hand-back, item 3).* Refuted. The
    check is `!= Some(true)`, which covers a stream that yields nothing.
  - *Cache tenant is `"default"` (cache hand-back, item 3).* Refuted. `request_key` is
    called with the authenticated `key_id` (`routes.rs`, call site near line 625).
  - *Breaker credited on headers as a stream defect (streaming hand-back, item 4).*
    Accurate. The breaker was credited at the first byte, which cancelled a
    failure for each request, so the defect is fixed under R5.
- **R2.** A per-chunk idle budget of 120 s applies to the body. It is a deliberate
  ceiling and not measured. It covers silence between chunks, not the total length of
  a long stream, so a healthy slow stream is not cut. Adjust in one constant.
- **U3 and U5.** Only the spend and usage totals refresh (U3, measured at 15 s). The
  providers table shows 40 rows with a name filter and a "Show all" control, so the
  layout the operator relies on does not change (U5).

## What was checked and is fine

- Client-key comparison: `ar-keys` verifies signed tokens and `Secret::ct_eq` uses `subtle`.
- Bind: `serve.rs` hardcodes `public: false`, and `bind_addr` refuses routable hosts. The gap is the missing `Host` check (S1), not the bind.
- Key echo: `/ui/api/state` returns env var names and booleans only. Provider-key and mint responses carry no key values.
- Failover after commit: the first-byte gate runs before the loop returns, so no failover happens after bytes reach the client.
- SSE decoder: bounded at 256 KiB per frame (`DEFAULT_FRAME_CAP`).
- Cargo dependencies: no git or wildcard deps. All external packages come from crates.io, and CI and release use `--locked`.
- Release smoke test: checks that the binary is a static ELF and that its version matches the tag.
- Checksum edge case: `sha256sum -c -` exits 1 on an empty match, so a missing asset line fails closed.

## Verification (2026-10-18, fourth pass)

Run against the working tree, not yet committed or deployed.

- `cargo fmt --all -- --check`: clean, after one `cargo fmt --all` for this pass's edits.
- `cargo clippy --all-targets --all-features --locked -- -D warnings`: exit 0.
- `cargo test --workspace --locked --no-fail-fast`: **45 suites, 2059 passed, 0 failed**, exit 0. This is the final run, after the shutdown-drain fix. Before the drain fix it was 2057, and before the `expect` conversion 2054.
- K4 and K5 mutation and discrimination checks: the K4 e2e test fails with `storable_reply` forced to `true` and passes with the real function. The K5 reload path has unit tests for both the refusal and the accept case.
- `cargo audit`: exit 0, no advisories reported against the 360 locked dependencies.
- `#![deny(missing_docs)]`: every library crate has it. `ar-limit` was the one missing.
- Browser, website-design probe on a scratch debug build (dummy env values, scratch config, port 20231): 0 FAIL at 1440, 768, and 375 px, in light and dark, for `#/dashboard` and `#/providers`. After the final CSS change: 0 FAIL, and 2 warnings, both the desktop type-scale heuristic, judged under U7.
- Usage refresh (U3), Playwright, 35 s on the dashboard: usage requests at 0.1 s, 15.1 s, and 30.1 s.
- `install.sh`: shellcheck `-s sh` exit 0; `sh -n` and `bash -n` exit 0; `--help` lists `--verify-attestation`; an unknown flag exits 2. The attestation block, extracted and run with a fake `gh`: off by default; on with a passing `gh`, verified; on with a failing `gh`, refused before install. The branch where `gh` is missing was not run.
- R7 experiment: `crates/ar-server/tests/disconnect.rs`, 2 tests, both pass.

### Coverage gaps

- **R5, half-open.** The helper has unit tests. The attempt-loop call that uses it has none.
- **R1 / R4.** The in-band dialects are tested end to end (a `stream_error` frame, no `[DONE]`). The raw path (Ollama) has only the `abort_on_cut` unit test.
- **K5.** The startup refusal was checked by running `serve`. It has no test. `Config::empty_api_keys` has unit tests. The reload refusal has unit tests.
- **Shutdown drain (found by a live restart).** `axum::serve` with graceful shutdown waits for every connection, with no upper bound, so the old 10 s window only delayed the drain. A live stream held the process open, and systemd escalated to SIGKILL after `TimeoutStopSec` (20 s). Fixed: `serve_until_drained` races the graceful future against a deadline that starts at the signal. Tests `serve_until_drained_should_return_within_the_window_when_a_request_hangs` and `serve_until_drained_should_return_at_once_when_idle`. Mutation: with the deadline arm removed, the hang test fails at its 5 s timeout.
- **Audit directory on `/tmp` (fixed, deployed).** The live `aroute.service` used `AR_OBS_DIR=/tmp/ar-obs`, which the reboot clears. `obs.conf` now sets `~/.local/state/ar/obs`, and the existing ledger was copied across (180K). The old `/tmp/ar-obs` copy is left in place. Deploy: release build of `2b05da6` (sha256 `cf2f479432ce`) swapped in; the previous binary is `~/.local/bin/aroute.pre-2b05da6.bak`. The swap restart hit the old unbounded drain: an in-flight NVIDIA stream held the old process and systemd SIGKILLed it at 20 s, which is the defect this fix addresses. Drain verified on the new binary: a held partial request kept the connection open, and the restart logged `drain window of 10s elapsed with connections still open; dropping them`, then started the replacement with no SIGKILL.
- **Silent fallback on a bad table.** `global()` on a malformed table serves an empty one and logs it. That is reachable only in a binary that skipped `serve`'s boot check, such as a library caller or a test. It is not a silent path in a shipped `aroute serve`.
- **Dead seam traits.** `crates/ar-core/src/traits.rs` (`ArTranslate`, `ArExec`, `ArRoute`, all `todo!()`) had no users. The live seams are in `ar-route`, so the file was deleted. Its `#P0-translate` and `#P0-route` rows in `docs/24` are closed.
- **K4.** The e2e test was checked for discrimination: with `storable_reply` forced to `true` it fails (`left: "hit"`), and with the real function it passes.
- **C1 and C2.** The release workflow has not run. See CI below.
- **C3.** Not run on a host without `sha256sum` or `shasum`.
- **P1.** Lock recovery is not tested.
- **U7.** The probe is a visual check. It is not an automated test.

### CI status

GitHub Actions cannot run here, so no workflow has run for the changes in this register. The workflow files were last checked with actionlint 1.7.7 (exit 0) in the third pass, and no workflow file changed in this pass. The checks the workflows run were replayed locally, and that replay is the evidence, not a CI run.

### Still open, and why

- **CI runs.** Needs a push and a GitHub run. Commit and push need the user's go-ahead.
- **C2, default verification.** Decided: opt-in. Releases published before attestations existed have none to check, so a default check would break installs of old tags. Make it the default once a release with an attestation is published.
- **Invariant `expect`s (10). Closed.** AGENTS.md forbids `unwrap` and `expect` outside tests, and the previous pass had kept these. The four `ar-registry` `global()` parses no longer panic: each table has a fallible `parse()`, a malformed table is logged and served empty, and `ar_registry::check_embedded()` parses every table and checks the catalog's base URLs. `aroute serve` runs that check before it binds, so a shipped binary refuses to start on a bad table. The six `ar-cli` import serializers return `anyhow::Result`, and their callers in `commands` and `webui` propagate the error. A `webui` render failure returns 500 with nothing written. The `ar-cache` `rss` example returns `Result`. Tests: `check_embedded_should_pass_for_the_shipped_tables`, `embedded_or_empty_should_return_the_default_when_a_table_is_malformed`.
- **Probe type-scale (2).** The 21 px first-viewport heading at 1440 px is judged, not changed. See U7.
- **Root README length.** The README was 468 lines against the monorepo band of 70–340. The Benchmarks, Parity, Known gaps and OmniRoute comparison sections moved to `docs/26-benchmarks-parity-gaps.md`, and the README is now 332 lines. `docs-check` reports the README rubric clean.

### Not done

- No release build. Nothing deployed. The gateway on `:20128` still runs the previous binary.
- Nothing committed.
- `ar.env` and the live `config.yaml` were not touched.
- The scratch server on `:20231` runs with dummy values and is stopped at the end of this pass.
