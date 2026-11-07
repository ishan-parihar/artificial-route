# Codex namespace-tool compatibility for the Responses inbound

Date: 2026-11-06. Branch `feature/codex-namespace-tools` off `main` (`e335b31`).

## 1. What is broken

Codex CLI 0.166.0 posts to `POST /v1/responses` a `tools` array that contains
two kinds of entries this build refuses:

```
namespace   14   e.g. {"type":"namespace","name":"mcp__serena","tools":[…]}
web_search   1   {"type":"web_search","external_web_access":true}
function    11   e.g. {"type":"function","name":"exec_command",…}
```

`crates/ar-translate/src/tools.rs` refuses every `type` that is not `function`:

```rust
if kind != "function" {
    return Err(unsupported(kind));
}
```

so the request is rejected with HTTP 400 `unparsable_body`,
`unsupported tool type "namespace": this build does not run it`. A request with
only `developer` messages and function tools answers 200, so the `developer`
role was never the blocker — the earlier `router-ops` warning is stale.

Two things have to be true for Codex to work, and only the first is about
parsing:

1. **Ingress** — the namespace containers must be expanded into the flat
   function tools an upstream can actually run.
2. **Egress** — when the model calls one of those tools, the reply must carry
   the namespace back, because Codex resolves an incoming call by
   `ToolName::new(namespace, name).with_default_namespace()`
   (`codex-rs/core/src/tools/router.rs`). A flat name with no namespace defaults
   to the `functions` namespace, misses the MCP registry, and fails with
   `unsupported call: <name>`.

## 2. Codex's namespace semantics (verified against the source)

`codex-rs/protocol/src/tool_name.rs`:

```rust
pub const DEFAULT_FUNCTION_NAMESPACE: &str = "functions";
// with_default_namespace() fills in "functions" when namespace is None or "".
```

`codex-rs/core/src/tools/router.rs`, `build_tool_call`:

```rust
let tool_name = ToolName::new(namespace.clone(), name.clone()).with_default_namespace();
```

`codex-rs/protocol/src/models.rs` — the wire carries the namespace as a
**separate field** beside the name, not folded into it:

```rust
FunctionCall {
    name: String,
    namespace: Option<String>,
    arguments: String,
    call_id: String,
    …
}
```

and its own test pins the wire shape:

```json
{"type":"function_call","name":"mcp__codex_apps__gmail_get_recent_emails",
 "namespace":"mcp__codex_apps__gmail","arguments":"{\"top_k\":5}","call_id":"call-1"}
```

So the name in a `function_call` item is the **unqualified** name and the
namespace is its own field. The `namespace__name` join exists only in
Code Mode's own script-facing namespace (`codex-rs/tools/src/code_mode.rs`,
`code_mode_name_for_tool_name`), which is a different thing from the wire
field.

**Consequence for this build:** the flat name an upstream receives must encode
the namespace, because no upstream in the registry speaks the Responses
`namespace` container. The canonical form therefore has to carry the namespace
alongside the name and re-attach it on the way out — as the `namespace` field
of the `function_call` item, which is what Codex actually reads.

## 3. What Codex sends today (captured, not inferred)

Captured with a local capture proxy against Codex 0.166.0
(`codex exec -p capture`), 14 namespaces:

```
multi_agent_v1                              close_agent, resume_agent, send_input, …
mcp__browseros_neo                          act, await_human_help, diff, …
mcp__headroom                               headroom_compress, …
mcp__lsp                                   diagnostics, find_references, …
mcp__memory_wire                            memory_recall, …
mcp__serena                                 activate_project, …
mcp__sourcehound                            archive, catalog_list, …
mcp__token_savior                           add_field_to_model, …
mcp__codex_apps__codex_document_control     _execute_document_command, …
mcp__codex_apps__pets                       _adopt, …
mcp__codex_apps__plugin_management          _get_app_permissions, …
mcp__codex_apps__safety_settings            _get_family_info, …
mcp__codex_apps__search_service             _web_run
```

The inner entries are ordinary Responses function tools
(`{"type":"function","name":…,"description":…,"parameters":{…}}`), sometimes
with `strict`.

## 4. `web_search` is a separate problem, and it is client-side

The hosted `web_search` tool is refused by the same guard. Codex sends it
because `ProviderCapabilities::web_search` defaults to `true`
(`codex-rs/model-provider/src/capabilities.rs`) and the config's provider
capability override only reads `external_web_access` — `web_search` is not
overridable from `config.toml`.

`web_search = "disabled"` **does** remove the hosted tool: `create_web_search_tool`
returns `None` for `WebSearchMode::Disabled`
(`codex-rs/core/src/tools/hosted_spec.rs`). Verified by capture: with
`web_search="disabled"` the last request carries `namespace: 13, function: 11`
and no `web_search` entry.

This is a **Codex configuration** matter, not a proxy one. aroute cannot run a
hosted search, so the honest behaviour stays the current one: refuse the
declaration rather than drop it. What changes is that the operator has a
client-side switch, and the plan says so.

## 5. The plan

### 5.1 Canonical shape

`crates/ar-translate/src/canonical.rs`:

- `CanonicalTool` gains `namespace: Option<String>` — the namespace a
  declaration was declared under, `None` for a plain function.
- `ToolCall` gains `namespace: Option<String>` — the namespace a call was made
  under.

Both with `#[serde(default, skip_serializing_if = "Option::is_none")]`, so the
canonical JSON shape is unchanged for every existing client.

### 5.2 Ingress — namespace expansion

`crates/ar-translate/src/tools.rs`:

- `responses_tools()` learns a `namespace` container: `{"type":"namespace",
  "name":"mcp__serena","tools":[…]}`. Each inner function becomes one canonical
  tool whose `namespace` is the container's name and whose `name` is the
  **unqualified** inner name.
- The inner entry's `type` must be `function`. A `custom` (freeform) inner tool
  has no schema this build can route, so it is refused by name — the same
  fail-loud rule that already applies to every other unsupported tool type.
- A container with no `name`, or with a `name` that is not a string, is an
  `InvalidTool`.
- The plain function path is unchanged: `namespace` stays `None`.

The expansion is **not** a silent drop. A namespace whose inner tools are all
refused still fails the request, and a namespace that declares a tool whose
inner type is unknown fails it too.

### 5.3 Flattening for the upstream

Every upstream in the registry speaks OpenAI chat or Responses flat function
tools. Neither has a namespace container, so a namespaced tool must be
flattened to one flat function whose name carries the namespace.

The flat name is `namespace__name`, except that Codex's own Code Mode rule
(`code_mode_name_for_tool_name`) joins without the separator when the namespace
already ends with `_` or the name already starts with `_`:

```rust
Some(namespace) if namespace.ends_with('_') || tool_name.name.starts_with('_') =>
    format!("{namespace}{}", tool_name.name),
Some(namespace) => format!("{namespace}__{}", tool_name.name),
```

**This build uses the plain `namespace__name` join** and does not copy that
special case, because the join is only ever used to build a name an upstream
echoes back, and the round trip is what must be lossless. A name that already
starts with `_` (`_adopt`, `_web_run`) still round-trips through
`namespace__name` because the split is on the **first** `__` after a known
namespace, not on any `__` anywhere.

The split is therefore driven by the **declared** namespaces, not by parsing
the flat name. That is the load-bearing decision: the canonical request
carries the namespace, so the reply path knows which namespace a flat name
belongs to without guessing.

### 5.4 Egress — namespace restore

`crates/ar-translate/src/outbound.rs`:

- `render_openai_responses_body()` renders a namespaced declaration as a flat
  function tool named `namespace__name`.
- `responses_function_call()` (the `function_call` **item** a canonical call
  renders to) emits the `namespace` field beside the name, and the name
  unqualified — which is exactly what Codex reads.

`crates/ar-server/src/reframe.rs`:

- The `Reframer` accumulates a tool call's name from the upstream's flat name.
  It needs the namespace to put back, so the reframer is given the request's
  declared namespaces (the mapping flat name → namespace) when it is built.
- `open_tool()` and `close()` emit the `namespace` field on the
  `function_call` item.

`crates/ar-translate/src/outbound.rs`, `openai_reply_to_responses()` — the
non-streaming sibling — emits the same `namespace` field.

### 5.5 The namespace mapping has to reach the response path

The response path is in `ar-server`, and the request is in `ar-translate`.
Two options:

- (a) Carry the mapping in the canonical request (`CanonicalRequest`), so the
  server can hand it to the reframer.
- (b) Re-derive the mapping from the request body in the server.

(a) is the honest one: the router's contract says the body is opaque to the
router, but the **server** owns the translation, so the server can carry the
namespaces alongside the body. `CanonicalRequest` gains
`tool_namespaces: Vec<(String, String)>` — the flat name → namespace pairs,
empty for every client that sends no namespace.

### 5.6 Fields Codex needs that this build drops

Codex sends `reasoning`, `parallel_tool_calls`, `store`, `include` and
`client_metadata`. They are not a **blocker** — the request answers 200 without
them — but `reasoning` in particular changes what the model does.

`reasoning` **is carried**, as `CanonicalChat::reasoning_effort`:
`ReasoningEffort` is an enum (`Minimal` / `Low` / `Medium` / `High`), read
from `reasoning.effort` on both the Responses and the OpenAI chat wire and
rendered back on both. An effort this build does not know is **refused**
rather than mapped onto the nearest one: a request that asked for more
thinking than it gets should say so, not answer as if it had been asked for
less.

Only the effort is carried, not the `summary`. `summary: "auto"` asks the
upstream for a reasoning *trace* in the reply, and this build does not render
one back to a client (see the `reframe` module's note on `reasoning_content`),
so carrying it would promise something the reply path cannot deliver.

The effort has **two spellings**, on purpose. The canonical body
(`render_openai_body`) spells it `reasoning_effort`, because that body is read
back into a `CanonicalChat` by `ar-exec` before the upstream's wire is
rendered, and a body that spelled the field the way the *wire* does would lose
the effort at exactly that crossing. The upstream wire
(`render_openai_wire_body`, and the `ar-exec` OpenAI arm that forwards the
canonical body verbatim) spells it `reasoning: {"effort": …}`, which is what
an OpenAI provider reads. The two renderers differ in that one field and
nothing else, and a test asserts that.

`parallel_tool_calls`, `store`, `include` and `client_metadata` are recorded
as **not carried**, with the reason: `parallel_tool_calls` is a concurrency
hint no upstream in the registry reads, `store:false` and the `include` list
are OpenAI bookkeeping, and `client_metadata` is a Codex-internal routing
hint. Dropping them changes nothing the model sees.

### 5.7 Carrying the effort breaks the upstreams that reject it

Carrying the effort is only half the work. An OpenAI-compatible upstream that
does not read `reasoning` answers:

```
400 {"message":"Validation: Unsupported parameter(s): `reasoning`"}
```

and the whole request fails with `upstream_request_rejected`. In the
`large-stack` chain the two nvidia targets (`minimaxai/minimax-m3.1-flash` and
`stepfun-ai/step-5-preview`) reject it; pgsgrove accepts it.

The repo already has the right seam: `omit_optional_parameters` in
`provider_settings.<provider>.request_capabilities.<model>` names known OpenAI
chat fields an operator has verified may be omitted for that target.
`OpenAiOptionalParameter::Reasoning` joins that enum, and the operator
declares the omission per model:

```yaml
provider_settings:
  nvidia:
    request_capabilities:
      minimaxai/minimax-m3.1-flash:
        omit_optional_parameters: [reasoning]
      stepfun-ai/step-5-preview:
        omit_optional_parameters: [reasoning]
```

The omission is the target's own default for that parameter, which is the same
trade every other entry in that list makes. Declaring it per model rather than
per provider is deliberate: another nvidia model may well read `reasoning`,
and a provider-wide omission would take the effort away from it too.

## 6. Acceptance

1. `cargo test --release` passes.
2. `cargo clippy --all-targets --all-features --locked -- -D warnings` passes.
3. A Codex-shaped request with namespace containers answers **200** through the
   live aroute, with the namespaces flattened to function tools upstream.
4. A model that calls a namespaced tool gets a `function_call` item carrying
   the `namespace` field, and Codex dispatches the call to the right MCP
   server — verified with a **real MCP tool dispatch**, not a smoke reply.
5. A request that declares a genuinely unsupported hosted tool is still
   **refused**, not dropped.

### 6.1 What was verified, and how

Against the captured Codex request (24 tool entries: 13 `namespace`
containers, 11 flat `function` declarations):

| Check | Before | After |
|---|---|---|
| The captured Codex body, verbatim | 400 `unparsable_body`, `unsupported tool type "namespace"` | 200, SSE stream opens |
| A forced namespaced call, on the wire | — | `{"name":"memory_recall","namespace":"mcp__memory_wire"}` |
| A hosted `web_search` declaration | 400, named | 400, named (unchanged) |

A live `codex exec -p aroute` then dispatched a real MCP call:

```
mcp: memory-wire/memory_recall started
mcp: memory-wire/memory_recall (completed)
```

which is the acceptance criterion that matters: the namespace expansion and
the namespace restoration both hold, end to end, through the public tunnel.

Deployed to the local host (`127.0.0.1:20128`) and to the RackNerd host
behind `aroute.ishanparihar.com` (a static musl build, the same artifact
shape the release workflow publishes, because the VPS runs glibc 2.36 and
cannot run the host-linked binary).

## 7. Bound

The `web_search` hosted tool stays refused by aroute. The operator turns it off
in Codex (`web_search = "disabled"`), which is a client-side switch and the
only place it can be: the provider capability that would remove it is not
overridable from `config.toml` in Codex 0.166.0.
