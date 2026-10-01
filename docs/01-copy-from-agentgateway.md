# 01 — Copy directly from agentgateway

Base: `../agentgateway/`. License Apache-2.0. Pin to commit before vendoring.
Method: `path` dependencies first, fork into `artificial-route/crates/` when you edit.

## COPY WHOLE (no logic change, only rename)

| Source | Dest | Why |
|---|---|---|
| `crates/core/` | `crates/ar-core/` | `Strng` (8B ArcStr), `drain`, `readiness`, `nonblocking` log channel 128k, `AssertSize`, `ArcSwap`, zerocopy IO. Zero internal deps. |
| `crates/http/` | `crates/ar-http/` | `BufList<Bytes>` vectored zero-copy, `peekbody` (read model without consuming), `RecordedBody`, `BufferLimit`. |
| `crates/pool/` | `crates/ar-pool/` | Vendored hyper 1.x pool, `HostShards` 16 shards (~+20% tput), H1+H2. |
| `crates/llm/` | `crates/ar-llm/` | `RouteType`, `Provider` trait, `conversion/{completions,messages,responses,bedrock,vertex_gemini,openai_compat}`, `parse/{sse,aws_sse,transform}`, `types/*`, `tokenizer.rs` tiktoken. 32k LOC engine. |
| `crates/agentgateway-app/` | `crates/ar-cli-bootstrap/` | `main.rs` 2 lines, `lib.rs` clap + `#[global_allocator]` jemalloc `thp:never,background_thread:true,dirty_decay_ms:5000`, `read_config_contents` (`--config\|--file\|LOCAL_XDS_PATH\|/config/config.yaml\|$XDG_CONFIG_HOME`), `commands/run.rs` + `import|migrate|oneshot.rs`. |

## COPY + CARVE (keep file, delete modules inside)

| Source file | Keep | Delete |
|---|---|---|
| `crates/agentgateway/src/proxy/gateway.rs` | `Gateway::run`, per-bind tasks, SO_REUSEPORT, drain | xDS watch arms |
| `crates/agentgateway/src/proxy/httpproxy.rs` (5474 LOC) | `proxy/proxy_internal`, `apply_request_policies` order, `make_backend_call`, `build_service_call`, `apply_llm_request_policies` | MCP/A2A branches, ext_proc/ext_authz/oidc arms |
| `crates/agentgateway/src/store/policy.rs` | `RequestPolicyTrait/ResponsePolicyTrait/BackendPolicyTrait`, `RequestPolicy<T>=Empty\|Single\|Multiple`, `select()` first-match, `merge_with_inheritance` | CEL condition eval if dropping CEL |
| `crates/agentgateway/src/store/binds.rs` (4788 LOC) | `RoutePolicies/GatewayPolicies`, pre-merged snapshot, `policies_for_target_*` | xDS delta paths |
| `crates/agentgateway/src/llm/mod.rs` (3368 LOC) | `AIProvider`, `AIBackend::select_provider` p2c, `AmendOnDrop` accounting | Copilot/Custom if unneeded |
| `crates/agentgateway/src/llm/model_router.rs` (1916 LOC) | alias/wildcard, model extraction JSON+multipart+path, `/v1/models` listing, virtual-model resolve | — |
| `crates/agentgateway/src/llm/catalog/` | `ModelCatalog` ArcSwap + `model-catalog.json` pricing | refresh daemon P1 |
| `crates/agentgateway/src/types/agent.rs` (4138 LOC) | `Bind/Listener/Route/ModelRoute/Backend` IR | xDS-only variants |
| `crates/agentgateway/src/types/local.rs` (5835 LOC) | `LocalLLMConfig/Models/VirtualModel`, `NormalizedLocalConfig::from()` | `LocalMcpBackend`, UI fields |
| `crates/agentgateway/src/state_manager.rs` | `watch_config_file` notify-debouncer-full, `watch_resource_changes`, `PreviousState` diff | xDS client arm |
| `crates/agentgateway/src/import.rs` (1585 LOC) | `ConfigImporter -> ImportPlan{providers,models,routes}` trait shape | litellm source impl only as example |
| `crates/agentgateway/src/http/filters.rs` (513 LOC) | `HeaderModifier/UrlRewrite/DirectResponse/BackendRequestTimeout` | mirror, csrf, oidc arms |

## DELETE ENTIRELY (RAM + dep win)

* `crates/agentgateway/src/mcp/` 12.7k LOC + `rmcp` tree.
* `crates/hbone/`, `crates/xds/`, `crates/protos/`, `crates/htpasswd-verify-fork/`.
* `config_store.rs` (2519 LOC) + `database.rs` + `telemetry/log_store.rs` + `sqlx` — use `ConfigStoreMode::File`.
## DELETE ENTIRELY (RAM + dep win)

* `crates/xds/`, `crates/protos/`, `crates/hbone/`, `crates/htpasswd-verify-fork/`.
* `config_store.rs` (2519 LOC) + `database.rs` + `telemetry/log_store.rs` + `sqlx` — use `ConfigStoreMode::File`.
* `management/admin.rs` + `ui.rs` (1269 LOC) — keep `readiness + metrics` only. Build `UI=0`.
* `control/spiffe.rs`, `http/substrate/`, ~20 policy files: `ext_proc, ext_authz, oidc, oauth, csrf, cors, remoteratelimit, sessionaffinity`.

## FEATURE-GATE (default off, P1 on-demand)

* `crates/agentgateway/src/mcp/` 12.7k LOC + `rmcp` tree -> `ar-mcp` optional crate `--features mcp` (default off, +<25MB idle).
  P0 binary stays lean. P1 the 12-tool catalog only (see `06-axi-mcp.md`):
  `health, list_combos, switch_combo, check_quota, route_request, cost_report, list_models, explain_route`.
  Skip CCR, oneproxy, web_search, skills, gamification, obsidian/notion in P1.
  MCP wraps same `ar-route|ar-tokens|ar-obs` fns — no second implementation.
* `a2a/` 11KB — keep only if agent-passthrough wanted; cheap either way.
