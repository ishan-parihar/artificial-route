//! `ar-server` — the axum 0.8 spine.
//!
//! Seven routes, three tower layers, one routing decision per request:
//!
//! | Route | Method | Behaviour |
//! |---|---|---|
//! | `/v1/chat/completions` | POST | OpenAI inbound, SSE pass-through, fallback chain |
//! | `/v1/messages` | POST | Anthropic Messages inbound, same pipeline |
//! | `/v1/responses` | POST | OpenAI Responses inbound, same pipeline |
//! | `/api/chat` | POST | Ollama chat inbound, same pipeline |
//! | `/v1/models` | GET | Catalog, stale-while-revalidate 60s |
//! | `/healthz` | GET | Liveness |
//! | `/metrics` | GET | Prometheus text exposition |
//!
//! Layers, outermost first: `trace_id` (mint/echo `x-ar-trace-id`),
//! `RequestBodyLimitLayer` (2MB), `TimeoutLayer` (120s to response headers).
//!
//! # What the pipeline does, in order
//!
//! auth → translate → guard → route → compress → cache → attempt loop.
//! Each stage is a sibling crate this one depends on, not a reimplementation:
//! `ar-keys`, `ar-translate`, `ar-guard`, `ar-route`, `ar-compress`, `ar-cache`,
//! `ar-exec`. See the module docs on [`routes`] for why the order is that order.
//!
//! # Two invariants that are not obvious from the code
//!
//! **A non-loopback bind requires an explicit public flag.** [`app::bind_addr`]
//! refuses `0.0.0.0` unless [`config::ServerConfig::public`] is set. There is no
//! bearer gate by default, so a routable bind would publish a credentialed LLM
//! proxy — and the flag exists so that decision is written down rather than
//! inferred from a `host:` line.
//!
//! **An unknown `model` is a 400, not a fallback.** With a combo table
//! configured, the request `model` selects a combo or the request is refused
//! with the ids that do exist. Routing it to the default chain instead would
//! answer a different question than the client asked.
//!
//! `Ponytail:` the executor is `ar-exec`'s `reqwest` client with no
//! connection-pool tuning beyond its own defaults, and `ar-keys`' `Admission`
//! lane control is not wired — see [`keys::AuthGate`] for why, and its TODO.

#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

pub mod app;
pub mod config;
pub mod exec;
pub mod keys;
pub mod metrics;
pub mod models;
pub mod routes;
pub mod text;
pub mod toon;
pub mod translate;

// The OAuth taxonomy and its single terminal-status list are re-exported here so
// `ar doctor` reads the same table the executor classifies against (F-MED-2):
// three consumers, one list, no second copy to drift.
pub use ar_exec::oauth::{
    OAuthKind, TERMINAL_REFRESH_STATUS, classify_refresh, terminal_check_constraint,
};

pub use app::{
    AppState, BindError, Components, MAX_BODY_BYTES, REQUEST_TIMEOUT, TRACE_HEADER, app, bind_addr,
    server,
};
pub use config::{ComboError, ComboTarget, RouteCombo, ServerConfig};
pub use exec::{HttpExec, OAuthAuth, ProviderConfig};
pub use keys::AuthGate;
pub use metrics::{Metrics, Outcome};
pub use models::{MODELS_TTL, ModelCard, ModelCatalog, ModelsCache, StaticCatalog};
pub use routes::{CACHE_HEADER, DECISION_HEADER, SESSION_HEADER, USAGE_HEADER};
