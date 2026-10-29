# Bug report and upgrade plan: OpenAI request transmutation and 400 failover

**Status: source implementation and local gates complete; provider-specific live verification remains pending.**
Measured 2026-10-29 on the local OpenCode/OMP clients calling the public
Racknerd aroute endpoint. This is a source upgrade plan; no harness or router
configuration was changed for this investigation.

## Execution result

The source work implements the safe routing and translation fixes below. The
Racknerd service and local harnesses were not changed or restarted, and no live
provider replay was run because the original NVIDIA error body and rejected field
are unavailable. The remaining provider-specific action is to add a per-model
capability row only after a safe reproduction establishes the field and its
semantics.

| Phase | Result |
|---|---|
| 0 — safe failure provenance | Implemented: bounded attempted provider/model, attempt, upstream status, and stable rule id reach response metadata, SQLite failure rows, `/ui/api/log`, and the Request Log detail. No raw upstream body is persisted or returned. |
| 1 — provider-aware 400 | Implemented: recognized target-capability 400s and ambiguous 400s fail over; only explicit JSON-syntax codes are terminal. Malformed local JSON and a present non-array `messages` field fail before dispatch. |
| 2 — target capabilities and adapters | Implemented: validated `provider_settings.<provider>.request_capabilities.<model>` rows flow to the OpenAI-wire renderer. Only explicitly listed known optional parameters are omitted. Messages, tool schemas, roles, and unknown fields remain unchanged. No NVIDIA-specific row is configured without evidence. |
| 3 — developer role | Implemented: canonical chat has a distinct `Developer` role; OpenAI chat/Responses preserve it, and other supported renderers map it into their documented instruction channel. |
| 4 — mock regressions | Implemented: mock combo rejects the first model with a structured unsupported-parameter 400, then serves from the second; exact per-target bodies, provenance headers, sanitized response, malformed-shape refusal, and role preservation are asserted. |

Focused package tests and workspace Clippy passed before the final release gate.

## Final gates — 2026-10-29

- `cargo test --release --locked -q`: passed with exit code 0, including workspace
  unit, integration, and documentation tests. The `ar-route` suite ran 299 tests;
  `ar-server` ran 521 tests; `live_path` ran 25 tests.
- `cargo clippy --all-targets --all-features --locked -- -D warnings`: passed.
- Focused `attempt_diagnostic_should_bound_unusually_long_target_names`: passed.
- `rustfmt --check --edition 2024` over modified Rust files: passed. The workspace
  `cargo fmt --all -- --check` still reports pre-existing formatting in the
  untouched `crates/ar-cli/src/toon.rs` and `crates/ar-cli/tests/cli.rs`; those
  files are not part of this change.
- Isolated 180-second loopback soak: passed. RSS reached a 35,456 KiB startup
  plateau and ended at 35,916 KiB (+460 KiB, against the script's +65,536 KiB
  limit); all health checks returned 200 with zero failures. The soak CSV is at
  `/tmp/ar-routeops-soak-20261029.csv`.
- No service, Racknerd deployment, router configuration, or local harness was
  changed. A real NVIDIA replay remains out of scope until its rejected field
  can be identified from safe evidence; no provider-specific capability row is
  guessed or enabled.

## Bug report

### One-line cause

The public router sent `large-stack` to its first target,
`nvidia/minimaxai/minimax-m3.1-flash`. NVIDIA returned HTTP 400. ArRoute treated
that provider's rejection as a request that no provider could serve and stopped
the combo, returning the generic `upstream rejected the request shape` error
instead of trying TokenHarbor or StepFun. The OpenAI chat path has no provider-
specific body adapter: it preserves the inbound body and the executor only
rewrites the top-level model name.

The failure ledger and route config establish the provider and control flow.
The exact rejected field, NVIDIA's original error text, and the particular stop
phrase that caused the abort are **not recoverable** from the retained logs: the
stored error is deliberately sanitized. Do not attribute this to a specific
field such as `max_completion_tokens` without a new safe reproduction.

### Impact and evidence

The Racknerd `failures` ledger records HTTP 400 `upstream rejected the request
shape` against provider `nvidia` for direct `minimax-m3.1-flash` calls and for
`large-stack`. OpenCode's local error log has four matching `large-stack`
failures at 02:20, 03:31, 04:35 and 05:05 IST; the remote ledger has the
corresponding provider-tagged rows within seconds. The public config confirms
that `large-stack` starts with NVIDIA Minimax, followed by TokenHarbor
DeepSeek and NVIDIA StepFun. OMP recorded the same 400 on the direct Minimax
route.

The OMP captures show structurally large requests (276–357 messages, 4–104
tools and roughly 0.5–1.1 MB request files). OpenCode sessions also have large
histories. This makes a payload-dependent compatibility issue plausible, but
request size or tool count alone is not proven to be the trigger; a later
large-stack call succeeded with roughly 130K input tokens. The OpenCode inbound
route was not retained, so the verbatim chat-path evidence is directly confirmed
for OMP and describes the OpenAI-compatible rendering gap, not a proven
OpenCode endpoint path.

### Confirmed source path

1. On `/v1/chat/completions`, `OpenAiChatTranslate` reads `model` and `stream`,
   then copies the inbound bytes unchanged into `CanonicalRequest`
   (`crates/ar-server/src/translate.rs:78-104`). This path is confirmed in the
   OMP capture; OpenCode's route is not present in its retained error log.
2. For an OpenAI-wire target, `ArExec::render_request` uses `rewrite_model`,
   which only replaces the top-level `model` field
   (`crates/ar-exec/src/lib.rs:430-433, 713-741`). It does not adapt message
   roles, tool schemas or request parameters to that target's capabilities.
3. The attempt classifier reads at most 2 KiB of an upstream error body. A 400
   becomes terminal when the body contains any stop phrase
   (`invalid message format`, `malformed`, `context`, `prompt` or `token`);
   provider-capability phrases are checked first
   (`crates/ar-route/src/attempt.rs:185-212, 668-675`). The terminal outcome
   returns immediately, so later combo targets do not run (`:910-917`).
4. ArRoute returns status 400 with code `aborted` and its own generic message
   (`crates/ar-route/src/attempt.rs:1201-1207`,
   `crates/ar-server/src/routes.rs:3361-3369`). The failure ledger stores that
   sanitized message, not NVIDIA's original body.

Because this request ended as an `Abort`, some stop phrase matched and no
advance phrase took precedence. The retained data cannot identify which phrase.
The router does preserve provider attribution (`x-ar-decision` and
`x-ar-provider`); the missing diagnostic is the attempted upstream model and
the classifier rule/result, not the provider name.

### Separate, confirmed translation failure

The same Racknerd ledger has a distinct 400 for
`pgsgrove/deepseek-v4.1-flash-1017`: `unrecognised message role: "developer"`,
with provider `-`. This is not the NVIDIA upstream rejection. It matches the
canonical conversion path where `msg_to_canonical` calls `Role::from_wire` and
rejects roles it does not represent (`crates/ar-translate/src/translate.rs:940-943`;
`TranslateError::UnknownRole` at `:321-335`). The route path was not retained,
so the evidence proves a pre-dispatch role-conversion failure but not which
inbound endpoint produced it. Handle this as a separate regression case in the
same transmutation upgrade; do not merge its cause with the NVIDIA 400.

## Upgrade plan

Implement in dependency order. Keep raw prompts, credentials and unredacted
provider bodies out of logs and test fixtures.

### Phase 0 — preserve safe failure provenance

Extend the internal attempt/failure record with the attempted upstream model,
HTTP status, attempt number, and a stable classifier rule ID. For upstream JSON
errors, retain only an allowlisted code/type/parameter category or a bounded
hash of the normalized error class; never persist the raw body. Preserve the
existing router-authored client error. Make the attempted model distinguishable
from a successfully served model in telemetry and response metadata.

**Acceptance:** a synthetic 400 produces an operator-readable record naming
provider, upstream model, attempt number and classifier rule, with no prompt,
key or raw provider text in the ledger or response.

### Phase 1 — make 400 policy provider-aware

Replace broad substring decisions such as `context`, `prompt` and `token` with
structured, documented rejection categories. A globally malformed request may
stop once; a target-specific unsupported parameter, context limit or feature
must advance to another target. Unknown 400s must retain an explicit
`unclassified_400` outcome and a deliberate retry policy rather than silently
claiming that every provider would reject the request.

**Acceptance:** regression cases cover every existing stop/advance phrase,
JSON error `code`/`type` variants, and an NVIDIA-like capability 400. The
capability case advances through the combo; a proven global shape error stops
without dispatching identical requests to every provider.

### Phase 2 — add explicit target capabilities and selective request adapters

Represent provider/model request capabilities in a typed, validated source of
truth: parameter names, tool-call features, supported message roles, and any
provider-specific shape requirements. Add a target-aware renderer for OpenAI
chat requests. It should preserve fields by default and transform only a field
whose target incompatibility and semantics are documented; do not delete
unknown fields or invent provider wire formats. Keep the current passthrough
behavior when no targeted rule applies.

**Acceptance:** sanitized fixtures model the observed OpenCode tool/history
shape. A mock upstream verifies the exact rendered body per target and that a
rejected first target can be followed by a compatible fallback without losing
messages or tool-call correlation.

### Phase 3 — define `developer` role semantics for canonical conversions

The canonical model currently rejects `developer` in `msg_to_canonical`.
Define whether canonical chat needs a distinct developer role or whether each
outbound wire has a documented safe mapping. Preserve its instruction priority;
do not globally relabel it as `system` without checking the target protocol.
Apply the same rule to Responses and any other typed OpenAI inbound path that
uses `Role::from_wire`.

**Acceptance:** an inbound fixture with a `developer` message succeeds on each
supported target wire with the intended instruction semantics, while unknown
roles still fail before provider dispatch with a precise router-authored error.

### Phase 4 — verify the full combo and client-visible diagnostics

Use mock providers for deterministic fallback tests, then a minimal redacted
live request only after the offending NVIDIA rejection class is known. Verify
the public `large-stack` route advances on provider capability failures and
keeps terminal behavior for global malformed requests. Verify OpenCode and OMP
can distinguish a router-level shape rejection from a provider-specific
failure without receiving raw upstream text.

**Done means:** the source has regression coverage for the captured failure
class, `large-stack` either reaches an accepting fallback or returns a precise
global request error, the 1017 `developer` role case no longer fails by
accident, and telemetry identifies the exact attempted target and classifier
rule without storing user content.

## Evidence limits

- The provider attribution is measured from Racknerd's failure ledger and
  corroborated by the configured first target and OpenCode timestamps.
- Neither the public error response capture nor the retained failure ledger
  contains NVIDIA's original 400 body. Recovering the exact field requires
  another request with safe, structured diagnostics or a controlled fixture;
  replaying the full captured conversation is not required for this plan.
- The neighboring 1017 `developer` failure is a different, pre-dispatch
  translation error and must remain separately labeled in any incident report.
