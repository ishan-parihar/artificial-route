//! Server configuration: the combo table the router walks, and the two ways to
//! get one.
//!
//! Two sources, one shape:
//!
//! * [`ServerConfig::from_ar_config`] reads the File-mode `config.yaml` through
//!   `ar-config` and builds one [`RouteCombo`] per configured combo, so a
//!   `combos:` list with two providers per combo routes across both. This is
//!   what `ar serve` does. Credentials resolve `store -> $VAR -> error`; see
//!   [`ServerConfig::from_ar_config`].
//! * [`ServerConfig::from_provider`] builds a single unnamed combo from
//!   environment variables. Kept whole: a `File`-only config is the destination,
//!   but the env path is how a test, a one-off `curl`, and every CI job in this
//!   repo configure the server, and it is the only path that needs no file on
//!   disk.
//!
//! # The three fields beyond a combo table
//!
//! Each is a *deployment* fact rather than a routing one, so each is a field with
//! a builder and an env spelling rather than a `config.yaml` block. `ar serve`
//! builds its config through this file's constructors and has nowhere else to say
//! any of it, which is why [`ServerConfig::from_ar_config`] reads all three too:
//! a field only [`ServerConfig::from_env`] populated would leave the shipped serve
//! path with no way to set it.
//!
//! | field | env | default |
//! |---|---|---|
//! | [`ServerConfig::http_master_key`] | `AR_HTTP_MASTER_KEY` | `None` — no gate |
//! | [`ServerConfig::auth_mode`] | `AR_AUTH_MODE` | [`AuthMode::Required`], inert without a gate |
//! | [`ServerConfig::timeouts`] | `AR_STREAM_TIMEOUT_SECS` | empty — 120s everywhere |
//! | [`ServerConfig::model_discovery`] | `AR_MODEL_DISCOVERY` | `false` — config-only catalog |
//!
//! The gate defaults *closed on paper and open in practice*: there is no gate until
//! a master key names one, and no shipped command named one until
//! `AR_HTTP_MASTER_KEY` is exported. `AR_MASTER_KEY` deliberately does **not** arm
//! it — that variable owns the credential store's AEAD and a store master that
//! quietly became a network credential is a surprise in the wrong direction, so
//! the HTTP gate gets its own variable and its own 32 bytes.
//!
//! `ServerConfig` is the only place the gate can be armed from, which is why
//! [`crate::app::Components::into_state`] reads [`Self::http_master_key`]: `ar
//! serve` builds its `Components` with `with_exec` and never names a master key,
//! so a field nothing else populates would be a gate nothing can turn on.
//!
//! # Where the routing signals come from
//!
//! `ar_route::Candidate` has fifteen-odd optional fields with documented neutral
//! values, and three of them are populated here because real data exists:
//!
//! | field | source |
//! |---|---|
//! | `input_usd_per_mtok` | explicit config, else [`ar_tokens::PricingTable`], which reads `ar-registry`'s rows |
//! | `weight` | the target's own `weight:` in the config, else `1` |
//! | `quota` | explicit config |
//!
//! The rest stay neutral. An unpriced candidate is unpriced and
//! `Strategy::CostOptimized` sorts it last, which is the documented behaviour
//! rather than a guess: populating a price that does not exist would corrupt
//! every budget decision downstream, and half the registry carries no rows yet.
//!
//! The price source is one field, not a merge. [`ServerConfig::prices`] defaults
//! to [`ar_tokens::PricingTable::global`], which is `ar-registry`'s own rows —
//! a caller with richer data (a live `models.dev` sync, a private contract) hands
//! in a table and it *is* the source. Two tables merged here would be a second
//! place registry prices are translated, and the two would drift.

use std::collections::BTreeMap;
use std::time::Duration;

use ar_compress::Step;
use ar_config::Config;
use ar_exec::oauth::{OAuthKind, Session};
use ar_keys::{CredentialStore, Secret};
use ar_registry::WireFormat;
use ar_route::{Candidate, ProviderId, QuotaWindow, Strategy};
use ar_tokens::{NormalizedUsage, PricingTable};

use crate::app::REQUEST_TIMEOUT;
use crate::exec::{OAuthAuth, ProviderConfig};
use crate::models::ModelCard;

/// Default listen port, matching the OmniRoute-compatible `127.0.0.1:20128`.
pub const DEFAULT_PORT: u16 = 20128;

/// Environment variable naming the HTTP gate's master key.
///
/// Deliberately *not* [`ar_keys::MASTER_KEY_VAR`]. That one holds the credential
/// store's AEAD master; reusing it would turn "I want my API keys encrypted at
/// rest" into "my API keys are now a network credential", which is the opposite
/// of what exporting it says. A separate variable also lets an operator give the
/// two different values, which is what the blast radii deserve.
///
/// 32 bytes, as 64 hex characters or 32 raw bytes — the two shapes
/// [`ar_keys::Secret::from_slice`] can be handed without a decoder, and the only
/// two this crate will guess at. Base64 is not accepted: a silently different
/// encoding than the one `AR_MASTER_KEY` uses is how a key ends up wrong in a way
/// that only shows up as an unopenable store.
pub const HTTP_MASTER_KEY_VAR: &str = "AR_HTTP_MASTER_KEY";

/// Environment variable carrying per-model stream deadlines.
///
/// A bare number (`"600"`) is the deadline for every model; a `model=secs` list
/// (`"gpt-5.4=600,claude=300"`) is per model, with a bare number alongside it
/// acting as the fallback. Empty — the default — leaves every model on
/// [`REQUEST_TIMEOUT`], so nothing changes until an operator says so, and the
/// name says "stream" because a slow model is the case that needs one.
pub const STREAM_TIMEOUT_VAR: &str = "AR_STREAM_TIMEOUT_SECS";

/// Environment variable naming [`AuthMode`].
///
/// Spelled out rather than folded into a `require_api_key` boolean because the
/// three modes are not booleans: "degrade an invalid key" and "require a key" are
/// different decisions, and a boolean cannot say the first without lying about
/// the second.
pub const AUTH_MODE_VAR: &str = "AR_AUTH_MODE";

/// Environment variable arming the models.dev discovery overlay.
///
/// A switch, not a URL. models.dev is the only upstream this speaks and the URL
/// is [`ar_registry::discovery::MODELS_DEV_URL`]; an operator who needs a mirror
/// needs a fork of the fetch, and a half-honoured URL is worse than none.
pub const DISCOVERY_VAR: &str = "AR_MODEL_DISCOVERY";

/// The key [`STREAM_TIMEOUT_VAR`]'s bare-number form writes under.
///
/// A real model could be named `*`, so the fallback slot is not a model id — it is
/// a name no client sends, and [`ServerConfig::stream_deadline`] consults it
/// second. An operator who does name a model `*` gets what they asked for.
const ANY_MODEL: &str = "*";

/// How a configured HTTP gate treats a request that carries no usable credential.
///
/// The mode only matters once a gate exists ([`ServerConfig::http_master_key`] is
/// `Some`): with no gate there is nothing to enforce and every mode takes the same
/// request path. That is why the default below can be the strictest mode and the
/// server can still be open — see the module docs.
///
/// Spelled out rather than a boolean because the three are not two: degrading an
/// invalid key and requiring a key are different decisions, and a `required:
/// false` cannot say the first without also waiving the second.
/// A client that sends a token it no longer has a key for should still be served.
///
/// Recorded from the reference gateway's `REQUIRE_API_KEY=false` behaviour
/// (`clientApi.ts:83-96`), which exists because a stale CLI config — Codex
/// Desktop's auto-config, an agent harness — otherwise 401s every request
/// forever. The cost is that a wrong key is not an error, so an operator watching
/// only status codes cannot tell a working gate from a bypassed one; which is why
/// it is a mode and not the default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AuthMode {
    /// No credential in any recognised slot, or one that was presented and
    /// refused. Both are served as anonymous.
    ///
    /// A stale key is tolerated so a client keeps working; a *missing* one is
    /// served because a client that has no gate configured at all is the same
    /// shape from here, and refusing it would mean this mode only worked for
    /// clients that had once been configured.
    DegradeInvalidToAnon,
    /// No check at all, even with a gate configured.
    ///
    /// Exists for the one deployment that wants a gate *present* — so the
    /// credential store and the token minting path are wired and testable —
    /// without the request path enforcing it. Nothing in this crate prefers it,
    /// and it pairs with a `public: true` bind, which is the only deployment
    /// where not enforcing is defensible.
    Open,
    /// A request with no usable credential is a 401, and so is one carrying a
    /// credential the gate refuses.
    ///
    /// The default, and the only mode that is safe on a routable bind. It is also
    /// the default *for a server with no gate*, where it is inert: the
    /// enforcement decision is "is a gate configured", and the mode only says
    /// which way to answer once one is.
    #[default]
    Required,
}

impl AuthMode {
    /// The `config.yaml`/env spelling, which is also the one [`Self::parse`]
    /// reads.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DegradeInvalidToAnon => "degrade-invalid-to-anon",
            Self::Open => "open",
            Self::Required => "required",
        }
    }

    /// Parses [`Self::as_str`], case- and space-insensitively.
    ///
    /// An unrecognised value is [`Self::Required`], not a default: a typo in a
    /// mode name must not silently turn a gate off, and the strict mode is the
    /// one that fails safe. A silent default rather than a returned error, because
    /// [`crate::app::Components::into_state`] cannot fail — a `Result` here would
    /// give every caller a boot-failure path for a value with a safe reading.
    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "open" => Self::Open,
            "degrade-invalid-to-anon" | "degrade" => Self::DegradeInvalidToAnon,
            _ => Self::Required,
        }
    }
}

/// Combo id used when the config names none — the environment path, and any
/// config whose `combos:` list is empty.
///
/// A constant rather than a generated one so `x-ar-decision` and `/v1/models`
/// carry the same id for a default chain across restarts.
pub const DEFAULT_COMBO_ID: &str = "default";

/// One routable target inside a combo: a provider plus the model spelling that
/// provider uses.
///
/// `weight` defaults to `1` rather than to the combo's position, because
/// `weighted` over a combo that gives every entry the same number cancels in the
/// roulette wheel and cannot express "prefer this one" at all.
#[derive(Clone, Debug, PartialEq)]
pub struct ComboTarget {
    /// Provider that serves this model.
    pub provider: ProviderId,
    /// Provider-local model name.
    pub model: String,
    /// Explicit input price, when the operator declared one.
    pub input_usd_per_mtok: Option<f64>,
    /// Relative share for `Strategy::Weighted`.
    pub weight: u32,
    /// Current quota window, when one is known.
    pub quota: Option<QuotaWindow>,
}

impl ComboTarget {
    /// Builds a target with every optional signal neutral.
    #[must_use]
    pub fn new(provider: ProviderId, model: impl Into<String>) -> Self {
        Self {
            provider,
            model: model.into(),
            input_usd_per_mtok: None,
            weight: 1,
            quota: None,
        }
    }

    /// Sets the explicit input price in USD per 1M tokens.
    #[must_use]
    pub fn with_price(mut self, usd_per_mtok: f64) -> Self {
        self.input_usd_per_mtok = Some(usd_per_mtok);
        self
    }

    /// Sets the `Strategy::Weighted` share.
    #[must_use]
    pub fn with_weight(mut self, weight: u32) -> Self {
        self.weight = weight.max(1);
        self
    }

    /// Attaches a quota window.
    #[must_use]
    pub fn with_quota(mut self, quota: QuotaWindow) -> Self {
        self.quota = Some(quota);
        self
    }
}

/// A named chain of targets plus the strategy that walks it.
///
/// `id` is what a client's `model` field resolves against, and `strategy` is
/// per-combo rather than per-server: `config.yaml`'s `cheap` combo is
/// `cost-optimized` while `default` is `lkgp`, and collapsing them into one
/// server-wide strategy is the defect this type fixes.
///
/// `pool` is the bench — candidates reachable only by failing over. It is empty
/// on every config written before the field existed.
#[derive(Clone, Debug, PartialEq)]
pub struct RouteCombo {
    /// The combo id a client asks for as its `model`.
    pub id: String,
    /// How this combo's targets are ordered.
    pub strategy: Strategy,
    /// Targets, in strategy order for rank-based strategies.
    pub targets: Vec<ComboTarget>,
    /// The bench: candidates appended after `targets`, in declaration order.
    ///
    /// Never eligible for the winner — [`crate::routes::resolve`] picks over
    /// `targets` alone and only then extends the chain with these — so a pool
    /// entry cannot make a cheap provider win a healthy request. It exists to
    /// widen *failover*, which is audit F-HIGH-2: the live `free-stack` combo
    /// lists 2 targets against 7 candidates, and the 5 extras were only ever
    /// reachable by failing over.
    ///
    /// Empty on every config written before the field existed.
    pub pool: Vec<ComboTarget>,
    /// The engine that compresses this combo's prompts, at the level the config
    /// named. `None` means off, which is what a combo with no `compression:`
    /// block means.
    ///
    /// One `Step` rather than a pipeline because a combo block names one engine;
    /// a multi-engine pipeline is the header's and the panel's business, and
    /// `ar-compress` composes the two.
    pub compression: Option<Step>,
    /// The `fusion` combo's judge: a `provider/model` or a combo id whose
    /// provider synthesizes the panel.
    ///
    /// `None` is the reference's default and the honest one: with no judge
    /// configured, a fusion panel returns its first 2xx answer rather than
    /// spending a second dispatch synthesizing one, because synthesizing a
    /// single answer through a judge is a degradation, not fusion. Naming a
    /// judge turns on the synthesis half (`fusion.ts::handleFusionChat`).
    pub judge_model: Option<String>,
    /// The window this combo advertises, when the config named one.
    ///
    /// `None` leaves the reduction over this combo's own targets in charge — see
    /// [`RouteCombo::context_length`] for why that is the default and what it
    /// costs.
    pub context_length: Option<u32>,
}

impl RouteCombo {
    /// The window to advertise for this combo: the declared figure when the
    /// config named one, else the smallest window among its targets' providers.
    ///
    /// The reduction is a bound, not a measurement: a request the combo accepts
    /// must fit whichever target is reached, so the smallest is what holds. It
    /// also under-reports a priority chain whose slowest target is a rare bench
    /// entry — which is why an operator can name the real figure instead, and
    /// why this never guesses upward on its own.
    #[must_use]
    pub fn context_length(&self) -> Option<u32> {
        self.context_length.filter(|w| *w > 0)
    }

    /// Builds a combo from ordered targets. `Strategy::Priority` order is the
    /// declaration order, which is why the vector is the order.
    #[must_use]
    pub fn new(id: impl Into<String>, strategy: Strategy, targets: Vec<ComboTarget>) -> Self {
        Self {
            id: id.into(),
            strategy,
            targets,
            pool: Vec::new(),
            compression: None,
            judge_model: None,
            context_length: None,
        }
    }

    /// Sets the window `/v1/models` advertises for this combo.
    ///
    /// Takes a count rather than a `Duration`-shaped type because it is a token
    /// budget, and a caller that has one should not have to convert.
    #[must_use]
    pub fn with_context_length(mut self, tokens: u32) -> Self {
        self.context_length = Some(tokens);
        self
    }

    /// Sets the bench that feeds failover after `targets` is exhausted.
    #[must_use]
    pub fn with_pool(mut self, pool: Vec<ComboTarget>) -> Self {
        self.pool = pool;
        self
    }

    /// Sets the engine that compresses this combo's prompts.
    #[must_use]
    pub fn with_compression(mut self, step: Step) -> Self {
        self.compression = Some(step);
        self
    }
}

/// Looks up the window models.dev published for one `provider`/`model`.
///
/// A callback rather than the catalog itself so the combo reduction does not
/// depend on `ar-registry`'s cache type, and `None` carries the whole point: a
/// source that knows nothing leaves the next rung to answer, and never supplies a
/// zero the reduction would treat as a figure.
pub type ModelWindow = dyn Fn(&str, &str) -> Option<u32>;

/// Resolved server configuration: the combo table plus the flat provider table
/// the executor dispatches against.
///
/// `Clone` is deliberately absent: `prices` holds an `ar_tokens::PricingTable`,
/// which is a `HashMap` and not `Clone`, and cloning a 32 MB price table to hand
/// a second config to a test would be the wrong trade. `Arc<ServerConfig>` is how
/// the state shares one anyway, and [`ServerConfig::single`] is the one spelling
/// of "empty but valid" — a `Default` impl would be a server with no port and no
/// providers, which is a config, not a default.
#[derive(Debug)]
pub struct ServerConfig {
    /// Listen port.
    pub port: u16,
    /// Strategy for the *default* chain, used when `combos` is empty.
    ///
    /// Per-combo strategy wins whenever `combos` is non-empty; this field is the
    /// environment path's strategy and the fallback for a combo with none.
    pub strategy: Strategy,
    /// Every provider this server can dispatch to, keyed by id in `by_id`.
    ///
    /// Wider than any single combo's targets: a combo is a *view* over this table,
    /// and the executor holds its own copy anyway.
    pub providers: Vec<ProviderConfig>,
    /// Routable combos. Empty means the flat provider list is the whole config,
    /// which is the environment path.
    pub combos: Vec<RouteCombo>,
    /// Prices for candidates that did not declare one.
    ///
    /// Defaults to [`ar_tokens::PricingTable::global`] — `ar-registry`'s own
    /// compiled-in rows. A caller with richer data (a live `models.dev` sync, a
    /// private deployment's contract) builds a table and hands it over, and that
    /// table *is* the source. The two are never merged, because a merge here is a
    /// second translation of the same rows and the two would drift.
    ///
    /// A field rather than an `Option`: `PricingTable` is not `Clone`, so an
    /// `Option<PricingTable>` would have to be rebuilt from the registry on every
    /// lookup to read it out of a `&self`.
    pub prices: PricingTable,
    /// Whether this server may be reached from off-host.
    ///
    /// `false` — the default — means loopback only. It is not a style preference:
    /// an LLM proxy with no credential check is a bill attached to a socket, so
    /// binding a routable address takes an explicit `server.public: true` and
    /// [`crate::app::bind_addr`] refuses otherwise.
    pub public: bool,
    /// How a configured gate treats a request with no usable credential.
    ///
    /// Defaults to [`AuthMode::Required`], which is inert until
    /// [`Self::http_master_key`] is `Some`. See [`AuthMode`].
    pub auth_mode: AuthMode,
    /// The HTTP gate's master key, when one is configured.
    ///
    /// `None` — the default, and what every in-repo test uses — means no gate
    /// and therefore no credential check. `ar_keys::Secret` rather than
    /// `Vec<u8>` so the redacting `Debug` is the one that runs: this value is the
    /// key that signs every accepted token, and a `Debug` that printed it would
    /// hand the whole gate to whatever logged the config.
    ///
    /// Read from [`HTTP_MASTER_KEY_VAR`] by both constructors.
    pub http_master_key: Option<Secret>,
    /// Per-model time-to-response-headers deadlines, by model or combo id.
    ///
    /// Empty by default, so every request keeps [`REQUEST_TIMEOUT`]. A reasoning
    /// model that takes six minutes to its first token cannot be served by a 120s
    /// blanket, and the blanket is the wrong place to fix it: the honest answer
    /// is per model, and only for a model an operator named. Read from
    /// [`STREAM_TIMEOUT_VAR`] by both constructors.
    ///
    /// [`Self::stream_deadline`] resolves a miss to the `*` entry and then to
    /// [`REQUEST_TIMEOUT`].
    pub timeouts: BTreeMap<String, Duration>,
    /// Whether `/v1/models` overlays the models.dev discovery catalog.
    ///
    /// Off by default. The overlay is a network call on a timer that a proxy with
    /// a complete compiled-in catalog does not need, and an operator who wants a
    /// model list that cannot change under them — an air-gapped or
    /// reproducibility-minded deployment — must be able to say so. When on, the
    /// discovered models are *unioned over* the configured ones, so turning it on
    /// can only add cards, never remove one a config named.
    ///
    /// Read from [`DISCOVERY_VAR`] by both constructors.
    pub model_discovery: bool,
}

impl ServerConfig {
    /// Builds a single-chain config: no combo table, so the flat provider list
    /// is the default chain.
    ///
    /// This is the shape the environment path and every in-repo test uses.
    #[must_use]
    pub fn single(port: u16, strategy: Strategy, providers: Vec<ProviderConfig>) -> Self {
        Self {
            port,
            strategy,
            providers,
            combos: Vec::new(),
            // `PricingTable::global` reads `ar-registry`'s `OnceLock`, so the
            // first lookup builds it and every later one is a hash probe.
            prices: PricingTable::global(),
            public: false,
            auth_mode: AuthMode::default(),
            http_master_key: None,
            timeouts: BTreeMap::new(),
            model_discovery: false,
        }
    }

    /// The time-to-response-headers budget for one model.
    ///
    /// An exact model or combo id wins; the `*` entry is the operator's blanket;
    /// [`REQUEST_TIMEOUT`] is the answer when neither exists. The last is the
    /// same number the layer used before this field existed, so an unconfigured
    /// server behaves identically.
    ///
    /// Keyed on the model's own spelling, which for a combo-table server is the
    /// combo id — the same string [`crate::routes::resolve`] matches — so a
    /// `timeouts:` entry cannot name a model the router would never route to.
    ///
    /// Ponytail: a client can still send any string, so an entry for a model
    /// nobody configured is simply never consulted. Refusing it would mean
    /// resolving the model here, which is the router's job and would duplicate its
    /// spelling rules.
    #[must_use]
    pub fn stream_deadline(&self, model: &str) -> Duration {
        self.timeouts
            .get(model)
            .or_else(|| self.timeouts.get(ANY_MODEL))
            .copied()
            .unwrap_or(REQUEST_TIMEOUT)
    }

    /// The longest deadline any model asked for, for the layer that has to
    /// cover all of them.
    ///
    /// The timeout layer is a per-router constant, so it has to be the *widest*
    /// deadline in the table: a per-request timeout narrower than this one would
    /// be cut by the layer before it could fire, which is the wrong order — the
    /// model-aware answer has to be the one the client sees. So this is
    /// [`Self::stream_deadline`] applied to each named model and the widest taken,
    /// which means the `*` fallback is honoured here too and the two functions
    /// cannot disagree about what any one model gets.
    ///
    /// Floored at [`REQUEST_TIMEOUT`], so a model asking for *less* narrows its
    /// own request without narrowing every other model's layer.
    #[must_use]
    pub fn max_deadline(&self) -> Duration {
        let widest = self
            .timeouts
            .keys()
            .map(|model| self.stream_deadline(model))
            .max()
            .unwrap_or(REQUEST_TIMEOUT);
        widest.max(REQUEST_TIMEOUT)
    }

    /// Arms the HTTP gate from [`HTTP_MASTER_KEY_VAR`], if it holds 32 bytes.
    ///
    /// A variable set to something else is reported on stderr and leaves the
    /// gate off, which is the direction that fails safe *for a loopback server*
    /// and unsafe for a public one. It is a warning rather than a boot failure
    /// because this function cannot return an error and a `public: true` server
    /// with a typo'd key should still answer `/healthz` so the operator can see
    /// the warning in the same place they will look for it.
    ///
    /// Ponytail: hex and raw only, no base64. `ar_keys` accepts three encodings
    /// through a crate-private decoder this crate cannot reach, and a second
    /// decoder that disagreed with it would be a key that works on one node and
    /// not another — worse than a rejected value, which is loud.
    #[must_use]
    pub fn with_http_master_key_from_env(mut self) -> Self {
        let Some(raw) = env(HTTP_MASTER_KEY_VAR) else {
            return self;
        };
        match decode_master(&raw) {
            Ok(secret) => self.http_master_key = Some(secret),
            // Names the variable and the shape, never the value: this line goes to
            // stderr on a server whose whole point is that it holds credentials.
            Err(reason) => eprintln!(
                "ar: {HTTP_MASTER_KEY_VAR} ignored — the HTTP auth gate stays OFF ({reason})"
            ),
        }
        self
    }

    /// Reads [`STREAM_TIMEOUT_VAR`] into [`Self::timeouts`].
    ///
    /// Accepts `600` or `gpt-5.4=600,claude=300` or both joined by a comma
    /// (`600,gpt-5.4=120`), where the bare number is the fallback every other
    /// model resolves to.
    ///
    /// A deadline that is not a positive number of seconds is dropped with a
    /// warning rather than refusing the config. A bad timeout is not a reason to
    /// refuse to route: the affected model falls back to [`REQUEST_TIMEOUT`],
    /// which is the documented answer for a model nobody successfully named, and
    /// refusing here would let one typo take down a server that was routing fine.
    #[must_use]
    pub fn with_timeouts_from_env(mut self) -> Self {
        let Some(raw) = env(STREAM_TIMEOUT_VAR) else {
            return self;
        };
        self.timeouts = parse_timeouts(&raw);
        self
    }

    /// Reads [`AUTH_MODE_VAR`] into [`Self::auth_mode`].
    ///
    /// Separate from the other two builders so a caller can set one without the
    /// others, which is what a single-field test needs.
    #[must_use]
    pub fn with_auth_mode_from_env(mut self) -> Self {
        if let Some(raw) = env(AUTH_MODE_VAR) {
            self.auth_mode = AuthMode::parse(&raw);
        }
        self
    }

    /// Reads [`DISCOVERY_VAR`] into [`Self::model_discovery`].
    ///
    /// A switch rather than a parsed mode, so the truthy spelling is the narrow
    /// one: only `1`/`true`/`on`/`yes` (case-insensitive) arm it. Anything else
    /// leaves it off. That direction is deliberate — this overlay makes a network
    /// call a proxy did not previously make, so an unrecognised value must not be
    /// read as consent to start dialling out.
    #[must_use]
    pub fn with_model_discovery_from_env(mut self) -> Self {
        if let Some(raw) = env(DISCOVERY_VAR) {
            self.model_discovery = matches!(
                raw.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "on" | "yes"
            );
        }
        self
    }

    /// Reads the File-mode `config.yaml` into a server config.
    ///
    /// The first combo is the default chain, so a listener that is given no
    /// `model` — `ar serve` binds before any request arrives — serves exactly
    /// what `config.yaml` lists first.
    ///
    /// `store` is the local encrypted credential table, consulted before the
    /// config's `$VAR`-expanded `keys:` map. `None` means "there is no store on
    /// this host", which is the pre-store configuration and still works: every
    /// credential then resolves from `keys:` alone. It is a parameter rather
    /// than an environment read because the resolution order is the security
    /// property, and a caller that cannot name the store cannot be trusted to
    /// have chosen the order.
    ///
    /// Every target resolves against the compiled-in catalog *plus* the config's
    /// `custom_providers:`, so a file-declared node routes on the same terms as a
    /// compiled-in one and needs no rebuild.
    ///
    /// # Errors
    ///
    /// - a combo target names a provider that is in neither, so there is no base
    ///   URL or wire format to dispatch to;
    /// - a combo target or pool entry names such a provider (same rule);
    /// - a combo declares no targets, which would make it unroutable;
    /// - a provider's credential is in neither the store nor `keys:`, or the
    ///   store holds it and would not decrypt it;
    /// - a custom provider's id collides with a compiled-in one.
    ///
    /// # Why the env-backed fields are read here too
    ///
    /// [`Self::with_http_master_key_from_env`], [`Self::with_auth_mode_from_env`]
    /// and [`Self::with_timeouts_from_env`] are applied to the result, so the one
    /// constructor `ar serve` and `ar run` both go through arms the gate. All
    /// three default to *off* / *required* / *120s*, so a config that names none
    /// of them produces exactly the config it produced before — which is the only
    /// reason a constructor can grow a side effect and stay honest.
    pub fn from_ar_config(
        cfg: &Config,
        port: Option<u16>,
        prices: Option<PricingTable>,
        public: bool,
        store: Option<&CredentialStore>,
    ) -> Result<Self, ComboError> {
        let mut providers: Vec<ProviderConfig> = Vec::new();
        let mut combos = Vec::with_capacity(cfg.combos.len());
        let table = prices.unwrap_or_else(PricingTable::global);
        let catalog = ar_registry::global().merge(&cfg.custom_providers)?;

        for combo in &cfg.combos {
            if combo.targets.is_empty() {
                return Err(ComboError::EmptyCombo {
                    id: combo.id.clone(),
                });
            }
            let mut targets = Vec::with_capacity(combo.targets.len());
            for (rank, target) in combo.targets.iter().enumerate() {
                targets.push(resolve_target(
                    target,
                    u32::try_from(rank).unwrap_or(u32::MAX),
                    combo.weight_of(target),
                    &catalog,
                    cfg,
                    store,
                    &mut providers,
                )?);
            }
            // The bench resolves on exactly the terms a target does, at load: a
            // pool entry that only failed at request time would cost a round trip
            // per occurrence instead of refusing the file. A pool entry is never
            // scored — it only ever fails over — so it carries the same default
            // share rather than a share of its own.
            let mut pool = Vec::with_capacity(combo.pool.len());
            for (offset, target) in combo.pool.iter().enumerate() {
                pool.push(resolve_target(
                    target,
                    u32::try_from(targets.len() + offset).unwrap_or(u32::MAX),
                    combo.weight_of(target),
                    &catalog,
                    cfg,
                    store,
                    &mut providers,
                )?);
            }
            let mut route = RouteCombo::new(
                combo.id.clone(),
                Strategy::parse(combo.strategy.as_str()),
                targets,
            )
            .with_pool(pool);
            // Carried verbatim: an operator naming the window their combo serves
            // is stating a fact about their own targets that no table here can
            // derive, and it is the only way a combo advertises above the
            // smallest window its chain happens to touch.
            route.context_length = combo.context_length;
            if let Some(compression) = combo.compression {
                route.compression = Some(compression.step());
            }
            // The judge's provider is resolved on the same terms a target is,
            // so a `judge_model:` naming an absent provider fails at load
            // rather than costing a round trip per fused request.
            if let Some(judge) = combo.judge_model.as_deref() {
                let (provider, model) =
                    judge
                        .split_once('/')
                        .ok_or_else(|| ComboError::MalformedJudgeModel {
                            id: combo.id.clone(),
                            judge: judge.to_owned(),
                        })?;
                resolve_target(
                    &format!("{provider}/{model}"),
                    u32::MAX,
                    1,
                    &catalog,
                    cfg,
                    store,
                    &mut providers,
                )?;
                route.judge_model = Some(judge.to_owned());
            }
            combos.push(route);
        }

        Ok(Self {
            port: port.unwrap_or(cfg.server.port),
            strategy: combos.first().map_or(Strategy::Priority, |c| c.strategy),
            providers,
            combos,
            prices: table,
            public,
            auth_mode: AuthMode::default(),
            http_master_key: None,
            timeouts: BTreeMap::new(),
            model_discovery: false,
        }
        .with_http_master_key_from_env()
        .with_auth_mode_from_env()
        .with_timeouts_from_env()
        .with_model_discovery_from_env())
    }

    /// The combo a client's `model` names.
    ///
    /// `None` when the request named nothing routable. With no combo table the
    /// flat provider list is the whole config, so a client that names no combo
    /// is routed over it — that is the environment path and the only way the
    /// pre-combo tests can work.
    #[must_use]
    pub fn combo(&self, id: &str) -> Option<&RouteCombo> {
        self.combos.iter().find(|c| c.id == id)
    }

    /// Every routable combo id, for the 400 that names them.
    #[must_use]
    pub fn combo_ids(&self) -> Vec<&str> {
        self.combos.iter().map(|c| c.id.as_str()).collect()
    }

    /// The default chain: the first combo, or the whole flat provider list when
    /// there is no combo table.
    ///
    /// First-not-best on purpose. `config.yaml` orders `combos:` deliberately —
    /// `default` first is a declaration that this is the fallback chain — and
    /// re-sorting by price would make the operator's stated intent a suggestion.
    #[must_use]
    pub fn default_combo(&self) -> DefaultChain<'_> {
        match self.combos.first() {
            Some(combo) => DefaultChain::Combo(combo),
            None => DefaultChain::Flat,
        }
    }

    /// The default chain's `compression:` step, for the `auto/*` path that
    /// resolves an alias over the default combo's candidates.
    ///
    /// An `auto/*` alias names no combo of its own, so the default chain's
    /// engine is the only one it could mean. `None` on the flat path: the
    /// environment provider list declares no compression at all.
    #[must_use]
    pub fn default_compression(&self) -> Option<Step> {
        match self.default_combo() {
            DefaultChain::Combo(combo) => combo.compression,
            DefaultChain::Flat => None,
        }
    }

    /// Routable candidates for `combo`, in strategy order.
    ///
    /// Rebuilt per request rather than cached: it is a `Vec` of ten-field structs
    /// over a handful of providers, and a cached copy would need invalidation the
    /// moment a config reload lands.
    ///
    /// Providers this build cannot dispatch to are **dropped**, with a warning.
    /// Leaving them in would spend one of three attempt slots on a request
    /// `ar-exec` refuses before it reaches the socket, and the refusal would
    /// read as a transport failure.
    #[must_use]
    pub fn candidates(&self, combo: Option<&RouteCombo>) -> Vec<Candidate> {
        match combo {
            Some(combo) => combo
                .targets
                .iter()
                .enumerate()
                .filter(|(_, t)| self.dispatchable(t))
                .map(|(i, t)| self.candidate(t, u32::try_from(i).unwrap_or(u32::MAX), None))
                .collect(),
            None => self
                .providers
                .iter()
                .enumerate()
                .filter(|(_, p)| p.is_dispatchable())
                .map(|(i, p)| {
                    let target = ComboTarget::new(p.id.clone(), &p.upstream_model);
                    let rank = p.rank.max(u32::try_from(i).unwrap_or(0));
                    self.candidate(&target, rank, p.input_usd_per_mtok)
                })
                .collect(),
        }
    }

    /// The combo's `pool:` entries, dispatchable, in declaration order.
    ///
    /// An iterator rather than a `Vec<Candidate>`: the chain in
    /// [`crate::routes::resolve`] already owns one buffer per request, and a
    /// second vector would be a second allocation the bench does not need. It is
    /// also never turned into candidates at all — a pool entry is not scored, so
    /// materialising a `Candidate` for one would be a struct built to be read
    /// once and thrown away.
    ///
    /// Providers this build cannot dispatch to are dropped for the same reason
    /// [`Self::candidates`] drops them: spending an attempt slot on a provider
    /// the executor refuses before the socket is a transport failure that never
    /// was one.
    pub fn pool<'c>(&'c self, combo: &'c RouteCombo) -> impl Iterator<Item = &'c ComboTarget> {
        combo.pool.iter().filter(move |t| self.dispatchable(t))
    }

    /// Model list derived from every combo target, plus every dispatchable
    /// provider's default model when there is no combo table.
    ///
    /// Ids are the *combo* id when one exists, because that is what a client
    /// passes back as `model`. A bare `provider/model` id would not route: with a
    /// combo table present, the router resolves `model` against combo ids and
    /// 400s anything else.
    ///
    /// A combo's window is reduced over the models it can actually serve — the
    /// smallest of them — unless the combo declares its own. It is not looked up
    /// as if the combo id were a provider: `providerMeta.json` is keyed by
    /// provider, so a combo id misses every lookup and falls to the unknown-model
    /// floor, which is how a 1M stack came to advertise 128K.
    ///
    /// A declared `context_length:` overrides the reduction, because the reduced
    /// figure is a bound over providers rather than a measurement of the models.
    #[must_use]
    pub fn model_cards(&self) -> Vec<ModelCard> {
        self.model_cards_with(None)
    }

    /// [`Self::model_cards`], with per-model windows from a discovered catalog.
    ///
    /// `discovered` maps `provider/model` to the window models.dev published for
    /// it. It is a fallback for the reduction, never an override: `providerMeta.json`
    /// is the operator-curated table and leads, while discovery fills the gaps —
    /// which is how a combo targeting `nvidia` (absent from `providerMeta.json`)
    /// resolves at all.
    #[must_use]
    pub fn model_cards_with(&self, discovered: Option<&ModelWindow>) -> Vec<ModelCard> {
        if self.combos.is_empty() {
            return self
                .providers
                .iter()
                .filter(|p| p.is_dispatchable() && !p.upstream_model.is_empty())
                .map(|p| {
                    // The published per-model window leads here too, for the same
                    // reason it leads in `ModelCard`: a provider-wide ceiling is
                    // the largest figure any of its models offers, and reporting
                    // that for one model invites a request it will refuse.
                    let known = discovered
                        .and_then(|f| f(p.id.as_ref(), &p.upstream_model))
                        .filter(|w| *w > 0)
                        .map(|w| ar_registry::discovery::DiscoveredModel {
                            context_length: w,
                            ..ar_registry::discovery::DiscoveredModel::default()
                        });
                    ModelCard::with_discovered(p.id.as_str(), &p.upstream_model, known)
                })
                .collect();
        }
        self.combos
            .iter()
            .map(|c| {
                let mut card = ModelCard::new(&c.id, &c.id);
                card.context_length = c
                    .context_length()
                    .or_else(|| self.combo_context(c, discovered))
                    .unwrap_or(card.context_length);
                card
            })
            .collect()
    }

    /// The window that bounds every request this combo can serve.
    ///
    /// All-or-nothing by construction. A `min` over the targets that happen to
    /// declare a window reports a bound the combo never established: in the live
    /// config 899 of 1833 combos name at least one target whose provider has no
    /// declared ceiling, and dropping those silently left 811 combos serving
    /// models larger than 128K advertising the floor. The same honesty rule
    /// `providerMeta.json` states for its own field applies here — an unresolved
    /// target means the reduction cannot bound the combo, so it derives nothing
    /// and the caller falls back to a declared `context_length:` or the
    /// unknown-model floor.
    ///
    /// The pool counts: a pool entry serves the request on failover, so a target
    /// larger than the bench does not raise what the combo can accept.
    ///
    /// `discovered` supplies a per-model window when `providerMeta.json` has no
    /// ceiling for the target's provider, which is the case for `nvidia` and
    /// `kilocode` — the two providers both stacks actually route through.
    fn combo_context(&self, combo: &RouteCombo, discovered: Option<&ModelWindow>) -> Option<u32> {
        let mut bound: Option<u32> = None;
        for t in combo.targets.iter().chain(combo.pool.iter()) {
            if !self.dispatchable(t) {
                // An undispatchable target cannot serve a request, so it bounds
                // nothing. Counting it would let a typo shrink every figure.
                continue;
            }
            // Per-model first, provider-wide second, for the same reason
            // `ModelCard` prefers them: the provider ceiling is the largest
            // window any of that provider's models offers, so using it for one
            // target over-reports it.
            let declared = discovered
                .and_then(|f| f(t.provider.as_ref(), &t.model))
                .or_else(|| {
                    ar_registry::meta::global()
                        .get(t.provider.as_ref())
                        .filter(|m| m.context_length > 0)
                        .map(|m| m.context_length)
                });
            let window = declared?;
            bound = Some(bound.map_or(window, |b: u32| b.min(window)));
        }
        bound
    }

    /// Whether any provider chain is configured at all.
    #[must_use]
    pub fn has_provider(&self) -> bool {
        self.providers.iter().any(|p| p.is_dispatchable())
            || self.combos.iter().any(|c| !c.targets.is_empty())
    }

    /// The wire dialect a combo's first dispatchable target speaks, for the
    /// diagnostics line. `None` when the combo is empty.
    #[must_use]
    pub fn wire_hint(&self, combo: &RouteCombo) -> Option<WireFormat> {
        combo
            .targets
            .iter()
            .find(|t| self.dispatchable(t))
            .and_then(|t| self.providers.iter().find(|p| p.id == t.provider))
            .map(|p| p.wire_format)
    }

    fn dispatchable(&self, target: &ComboTarget) -> bool {
        self.providers
            .iter()
            .any(|p| p.id == target.provider && p.is_dispatchable())
    }

    /// One candidate with every available signal attached.
    ///
    /// `explicit_price` is the flat table's declared price, which wins over both
    /// the target's own price and the optional pricing table: it is the operator
    /// stating a number for this specific deployment.
    fn candidate(&self, target: &ComboTarget, rank: u32, explicit_price: Option<f64>) -> Candidate {
        let mut c = Candidate::new(target.provider.clone(), &target.model)
            .with_rank(rank)
            .with_weight(target.weight);
        if let Some(quota) = target.quota {
            c = c.with_quota(quota);
        }
        c.input_usd_per_mtok = explicit_price
            .or(target.input_usd_per_mtok)
            .or_else(|| self.price_of(&target.provider, &target.model));
        c
    }

    /// Looks a price up in the optional table.
    ///
    /// Asked for the *input* price, so it probes with exactly one million prompt
    /// tokens and no completion: `ar_tokens::Cost` is defined over usage, not over
    /// a bare rate, and this is the honest way to read a rate out of it without
    /// reaching into the table's private rows. An absent row is
    /// [`ar_tokens::Cost::UNPRICED`], which becomes `None` — not zero.
    fn price_of(&self, provider: &ProviderId, model: &str) -> Option<f64> {
        let cost = self
            .prices
            .cost(provider.as_str(), model, NormalizedUsage::new(1_000_000, 0));
        cost.priced.then(|| cost.usd.as_f64())
    }

    /// Reads configuration from the process environment.
    ///
    /// `AR_UPSTREAM_URL` and `AR_UPSTREAM_MODEL` are the only required pair; the
    /// API key is optional because keyless providers exist. When the pair is
    /// missing the config comes back with an empty provider chain, and the server
    /// still boots — `/healthz` and `/metrics` answer, `/v1/chat/completions`
    /// returns 503. A proxy that refuses to start is harder to diagnose than one
    /// that says what is missing.
    ///
    /// Also reads the three deployment fields — the gate, its mode, and the
    /// per-model deadlines — so `ar run` and `ar serve` both get them, and both
    /// leave them at their defaults when nothing is set.
    #[must_use]
    pub fn from_env() -> Self {
        Self::from_provider(
            env("AR_PORT"),
            env("AR_UPSTREAM_URL"),
            env("AR_UPSTREAM_MODEL"),
            env("AR_API_KEY"),
            env("AR_PROVIDER"),
            env("AR_STRATEGY"),
            env("AR_INPUT_USD_PER_MTOK"),
        )
        .with_public(env_public())
        .with_http_master_key_from_env()
        .with_auth_mode_from_env()
        .with_timeouts_from_env()
        .with_model_discovery_from_env()
    }

    /// Builds a single-chain config from already-resolved values.
    ///
    /// The pure half of [`ServerConfig::from_env`], and the seam the File-mode
    /// reader replaced. Seven `Option<String>` rather than a config struct,
    /// because these seven are one flat env reader and a struct here would be a
    /// second source of truth for the same seven names.
    #[must_use]
    #[allow(
        clippy::too_many_arguments,
        reason = "one field per env var; a config struct here would be a second source of truth"
    )]
    pub fn from_provider(
        port: Option<String>,
        base_url: Option<String>,
        model: Option<String>,
        api_key: Option<String>,
        provider_id: Option<String>,
        strategy: Option<String>,
        price: Option<String>,
    ) -> Self {
        let port = port.and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_PORT);
        let strategy = Strategy::parse(&strategy.unwrap_or_else(|| "priority".to_owned()));

        let mut providers = Vec::new();
        if let Some(base_url) = base_url {
            let mut p = ProviderConfig::new(
                ProviderId::new(provider_id.unwrap_or_else(|| DEFAULT_COMBO_ID.to_owned())),
                base_url,
                api_key.unwrap_or_default(),
            )
            .with_model(model.unwrap_or_default());
            if let Some(price) = price.and_then(|v| v.parse().ok()) {
                p = p.with_price(price);
            }
            providers.push(p);
        }

        Self::single(port, strategy, providers)
    }

    /// Sets whether this server may be reached off-host.
    ///
    /// Deliberately a builder rather than a `from_ar_config` field: the `public`
    /// decision is a *deployment* choice (`ar serve --public`, `AR_PUBLIC`) and
    /// not a routing fact, so it does not belong in the combo reader.
    ///
    /// # TODO(#p1-config-public): `config.yaml`'s `server:` block should carry
    /// this so a file-mode install does not need the environment variable. The
    /// field belongs on `ar_config::Server`, which is another crate's file; until
    /// it lands, `AR_PUBLIC` is the only way to say it, and `ar-config` silently
    /// drops an unknown `public:` key because `Server` does not deny unknown
    /// fields. The same is true of `auth_mode`, `http_master_key` and `timeouts`.
    #[must_use]
    pub fn with_public(mut self, public: bool) -> Self {
        self.public = public;
        self
    }
}

/// Reads `AR_PUBLIC`, treating anything but a clear yes as "no".
///
/// Default-deny on purpose: the cost of guessing wrong is a credentialed LLM
/// proxy published on a routable interface, and the cost of guessing the other
/// way is passing a flag.
fn env_public() -> bool {
    env("AR_PUBLIC").is_some_and(|v| {
        let v = v.trim().to_ascii_lowercase();
        matches!(v.as_str(), "1" | "true" | "yes" | "on")
    })
}

/// Parses [`STREAM_TIMEOUT_VAR`]'s value into the deadline table.
///
/// `600` is the deadline for every model; `gpt-5.4=600,claude=300` is per model;
/// both joined by a comma is per model with `600` as the fallback. An entry that
/// is not a positive number of seconds is dropped with a warning.
///
/// A free function rather than an inline loop so it is testable without the
/// process environment: the env is global, and a test that sets a variable while
/// another test clears it fails for reasons that have nothing to do with either.
fn parse_timeouts(raw: &str) -> BTreeMap<String, Duration> {
    let mut out = BTreeMap::new();
    for entry in raw.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        match entry.split_once('=') {
            Some((model, secs)) => match secs.trim().parse::<u64>() {
                Ok(secs) if secs > 0 => {
                    out.insert(model.trim().to_owned(), Duration::from_secs(secs));
                }
                // Names the model, never a value: this line sits next to the
                // master-key warning, and one convention about what may be
                // printed is easier to keep than two.
                _ => eprintln!(
                    "ar: {STREAM_TIMEOUT_VAR} ignored the seconds for {model:?}: not a positive number"
                ),
            },
            None => match entry.parse::<u64>() {
                Ok(secs) if secs > 0 => {
                    out.insert(ANY_MODEL.to_owned(), Duration::from_secs(secs));
                }
                _ => eprintln!(
                    "ar: {STREAM_TIMEOUT_VAR} ignored {entry:?}: not a positive number of seconds"
                ),
            },
        }
    }
    out
}

/// Decodes 64 hex characters or 32 raw bytes into a [`Secret`].
///
/// Two shapes and no more, because every shape added here is a shape a `Secret`
/// can be built from later by a different decoder — and a key that two decoders
/// read differently is a key that works on one node and not another. The reason
/// never echoes the value.
fn decode_master(raw: &str) -> Result<Secret, String> {
    const WRONG_SHAPE: &str = "expected 64 hex characters or 32 raw bytes";
    let trimmed = raw.trim();
    if trimmed.len() == 32 {
        return Secret::from_slice(trimmed.as_bytes()).map_err(|_| WRONG_SHAPE.to_owned());
    }
    if trimmed.len() == 64 && trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
        // Already known to be hex, so `from_str_radix` cannot fail; `unwrap_or(0)`
        // is unreachable rather than a guess.
        let bytes: Vec<u8> = (0..32)
            .map(|i| u8::from_str_radix(&trimmed[i * 2..i * 2 + 2], 16).unwrap_or(0))
            .collect();
        return Ok(Secret::new(bytes));
    }
    Err(WRONG_SHAPE.to_owned())
}

/// The chain a request uses when it named no routable combo.
///
/// `PartialEq` but not `Eq`: `ComboTarget` carries an `f64` price, and an `Eq`
/// promise a type cannot keep is worse than no promise at all.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DefaultChain<'a> {
    /// The first configured combo.
    Combo(&'a RouteCombo),
    /// No combo table: the flat provider list is the whole config.
    Flat,
}

/// Why a `config.yaml` combo could not become a routable chain.
#[derive(Clone, Debug, thiserror::Error)]
pub enum ComboError {
    /// A target named a provider neither the compiled-in catalog nor the
    /// config's `custom_providers:` carries.
    #[error("combo target {target:?} names provider {provider:?}, which is not in the registry")]
    UnknownProvider {
        /// The target string as written.
        target: String,
        /// The provider id parsed out of it.
        provider: String,
    },
    /// A combo declared no targets.
    #[error("combo {id:?} declares no targets, so nothing can route to it")]
    EmptyCombo {
        /// The combo id.
        id: String,
    },
    /// A `judge_model:` was written without the `provider/model` shape.
    #[error("combo {id:?} has judge_model {judge:?}, which must be provider/model")]
    MalformedJudgeModel {
        /// The combo id.
        id: String,
        /// The judge's target string as written.
        judge: String,
    },

    /// A provider's credential is in neither the credential store nor `keys:`.
    ///
    /// A miss, not an empty string. `ProviderConfig::api_key` documents empty as
    /// "keyless provider", and a provider that *has* an entry resolving to
    /// nothing is that — but a name with no entry at all is a hole in the
    /// config, and it used to become an unauthenticated upstream call.
    #[error(
        "provider {provider:?} references key {key:?}, which is in neither the credential store nor `keys:`"
    )]
    UnresolvedKey {
        /// The provider that holds the dangling reference.
        provider: String,
        /// The key name it referenced.
        key: String,
    },

    /// The store holds the key and would not hand it over: wrong
    /// `AR_MASTER_KEY`, a row written under another provider, or an unreadable
    /// database.
    ///
    /// Never falls through to `$VAR`. A silently downgraded credential is how a
    /// proxy ends up dispatching the wrong key and reporting it as a provider
    /// 401, so a store that cannot be read is an error the operator sees.
    #[error("provider {provider:?} cannot read key {key:?} from the credential store: {reason}")]
    UnreadableKey {
        /// The provider whose credential could not be read.
        provider: String,
        /// The key name it referenced.
        key: String,
        /// `ar-keys`' own reason, which is redacted by construction.
        reason: String,
    },

    /// A `custom_providers:` node could not be merged into the catalog.
    #[error(transparent)]
    CustomProvider(#[from] ar_registry::MergeError),
}

/// Resolves one `provider/model` string into a routable target, registering its
/// dispatch row on first sight of the provider id.
///
/// The single grammar for a `targets:` entry and a `pool:` entry. Shared on
/// purpose: a bench that resolved on different terms would be the one place in
/// the config where `doctor` says ok and dispatch cannot happen, which is the
/// failure `doctor` exists to prevent.
///
/// `providers` gains at most one row per provider id — two combos naming
/// `openai`, or a combo naming it as both target and pool, must not produce two
/// dispatch rows, or the executor's `by_id` lookup keeps whichever came last.
///
/// `rank` positions the entry in the combo's route-then-bench order and `weight`
/// is that entry's own `Strategy::Weighted` share, read from the config's own
/// `weight:` when it declared one. It used to be the combo's position in the
/// file, which made every entry in a combo share one number — a uniform share
/// cancels in the roulette wheel, so `weighted` over a combo could not express
/// "prefer this one" at all.
///
/// # Errors
///
/// [`ComboError::UnknownProvider`] when the provider half is in neither the
/// compiled-in catalog nor the config's `custom_providers:`, and the credential
/// errors from [`resolve_key`].
/// The bearer a free-tier gateway accepts from an unauthenticated caller.
///
/// Recorded from the provider registry rather than derived: `kilocode` publishes
/// this exact string, and a value inferred from a provider id would be an
/// invented wire format (AGENTS.md). A gateway that names its anonymous tier
/// differently is not served by this constant — it uses an ordinary API-key row.
const ANONYMOUS_API_KEY: &str = "anonymous";

/// The header a free-tier gateway requires alongside that bearer.
///
/// Also recorded, not inferred: the value is the *editor's* name, which is what
/// the gateway logs next to a free-tier request. `anonymous_editor` in the
/// `oauth:` block supplies it, because the name is the operator's to choose and a
/// wrong-but-present one is harder to notice than a missing one.
const ANONYMOUS_EDITOR_HEADER: &str = "X-KILOCODE-EDITORNAME";

fn resolve_target(
    target: &str,
    rank: u32,
    weight: u32,
    catalog: &ar_registry::Registry,
    cfg: &Config,
    store: Option<&CredentialStore>,
    providers: &mut Vec<ProviderConfig>,
) -> Result<ComboTarget, ComboError> {
    let (provider, model) = split_target_known(target, |id| catalog.get(id).is_some());
    let def = catalog
        .get(provider)
        .ok_or_else(|| ComboError::UnknownProvider {
            target: target.to_owned(),
            provider: provider.to_owned(),
        })?;

    if !providers.iter().any(|p| p.id.as_str() == provider) {
        let mut entry = match anonymous_entry(cfg, provider, def) {
            // The free tier brings its own credential, so nothing is resolved:
            // no store row is read, no `keys:` entry has to exist, and no OAuth
            // session is connected. That is the whole promise of the flag, and
            // resolving first would break it for the install it exists for —
            // one with no account and therefore nothing to put in `keys:`.
            Some(entry) => entry,
            None => {
                let key_name = cfg.key_name(provider).unwrap_or(provider);
                let key = resolve_key(cfg, provider, key_name, store)?;
                let mut entry = ProviderConfig::new(ProviderId::new(provider), &def.base_url, key)
                    .with_wire_format(def.wire_format)
                    .with_headers(def.headers.clone())
                    // The catalog's `authType`, carried so `is_dispatchable` can tell a
                    // provider that *needs* an OAuth executor from one that merely has a
                    // session configured. Without it, `oauth: None` would mean both
                    // "keyless" and "labelled oauth, and this build cannot do it".
                    .with_needs_oauth_executor(def.auth_kind.as_ref() == "oauth");
                if let Some(auth) = resolve_oauth(cfg, provider, key_name, store)? {
                    entry = entry.with_oauth(auth);
                }
                entry
            }
        };
        // The flat table's model is a display default only; a combo target always
        // carries its own spelling.
        entry.upstream_model = model.to_owned();
        entry.rank = rank;
        providers.push(entry);
    }

    Ok(ComboTarget::new(ProviderId::new(provider), model).with_weight(weight))
}

/// The dispatch row for an anonymous free-tier session, when the config asks for one.
///
/// Built whole rather than patched onto an existing row, because the ordering is
/// the contract: an anonymous session has no credential to resolve and no OAuth
/// connection to make, and a row that went through either step first would have
/// already demanded a store row the operator does not have.
///
/// `needs_oauth_executor` stays set even though the row dispatches without one,
/// because that flag is a statement about the *catalog* and `is_dispatchable`
/// consults [`ProviderConfig::anonymous`] first.
fn anonymous_entry(
    cfg: &Config,
    provider: &str,
    def: &ar_registry::ProviderDef,
) -> Option<ProviderConfig> {
    let declared = cfg.oauth_for(provider)?;
    if !declared.anonymous {
        return None;
    }
    let mut headers = def.headers.clone();
    if let Some(editor) = declared
        .anonymous_editor
        .as_deref()
        .filter(|e| !e.is_empty())
    {
        headers.insert(ANONYMOUS_EDITOR_HEADER.to_owned(), editor.to_owned());
    }
    Some(
        ProviderConfig::new(ProviderId::new(provider), &def.base_url, ANONYMOUS_API_KEY)
            .with_wire_format(def.wire_format)
            .with_headers(headers)
            .with_needs_oauth_executor(def.auth_kind.as_ref() == "oauth")
            .with_anonymous(true),
    )
}

/// Resolves a provider's OAuth session, when the config declares one.
///
/// The single place token material is read out of the credential store for OAuth
/// (F-CRIT-2's store, F-CRIT-1's executor). It reuses [`resolve_key`], so the
/// resolution *order* is literally the same function the API-key path uses — a
/// second order here would be a second security property, and this one governs a
/// rotating bearer.
///
/// `access_name` is the provider's own `keys:` entry: the access token needs no
/// second name. The refresh token is the second row, named by the session block.
///
/// # Errors
///
/// As [`resolve_key`]: a declared `refresh_key` that resolves in neither the store
/// nor `keys:` is a config hole, and a store that holds it and will not decrypt it
/// is an error the operator must see rather than a session that quietly cannot
/// renew.
///
/// `Ok(None)` when the config declares no session, or names a provider this build
/// has no executor for. The latter is deliberately *not* an error: red-team R1's
/// `kilocode` must keep failing loudly at `ar doctor` while still letting the
/// rest of a config serve, and `ProviderConfig::is_dispatchable` is what keeps it
/// out of the candidate list.
fn resolve_oauth(
    cfg: &Config,
    provider: &str,
    access_name: &str,
    store: Option<&CredentialStore>,
) -> Result<Option<OAuthAuth>, ComboError> {
    let Some(declared) = cfg.oauth_for(provider) else {
        return Ok(None);
    };
    let Some(kind) = OAuthKind::parse(provider) else {
        tracing::warn!(
            provider,
            "oauth session declared but this build has no executor for the provider; it will not route"
        );
        return Ok(None);
    };

    let access = resolve_key(cfg, provider, access_name, store)?;
    let refresh = match declared.refresh_key.as_deref() {
        Some(name) => resolve_key(cfg, provider, name, store)?,
        // No row named: the session is useable but not renewable, which is a
        // state the executor models and the doctor reports — not a load error.
        None => String::new(),
    };

    let mut session = Session::new(provider, kind);
    if let Some(url) = &declared.token_url {
        session = session.with_token_url(url);
    }
    if let Some(client_id) = &declared.client_id {
        session = session.with_client_id(client_id);
    }
    if let Some(scope) = &declared.scope {
        session = session.with_scope(scope);
    }

    Ok(Some(OAuthAuth::new(
        session,
        access,
        refresh,
        declared.expires_at,
    )))
}

/// Splits a `provider/model` target at the longest provider-looking prefix.
///
/// Every `/`-boundary is tried from longest provider to shortest, and the
/// first whose left half names a known provider wins. That covers both
/// shapes the ecosystem uses: nested provider paths
/// (`accounts/fireworks/models/mixtral`) and nested model paths
/// (`nvidia/moonshotai/kimi-k3`, `aihorde/aphrodite/TheDrummer/...`).
/// A target with no known prefix falls back to [`split_target`], so the
/// error names the longest guess rather than silently accepting it.
///
/// The one shared grammar: `ar serve` and `ar doctor` both resolve through
/// here, so a config one accepts is one the other accepts.
#[must_use]
pub fn split_target_known(target: &str, known: impl Fn(&str) -> bool) -> (&str, &str) {
    let mut end = target.len();
    while let Some(i) = target[..end].rfind('/') {
        let (head, tail) = (&target[..i], &target[i + 1..]);
        if !head.is_empty() && !tail.is_empty() && known(head) {
            return (head, tail);
        }
        end = i;
    }
    split_target(target)
}

/// Splits a `provider/model` target.
///
/// `rsplit_once` rather than `split_once`: a provider path can itself be
/// nested (`accounts/fireworks/models/mixtral`), and only the last segment is
/// the model. A target with no `/` is a bare model and routes through the flat
/// provider list, which is what `config.yaml` written by hand tends to contain.
#[must_use]
pub fn split_target(target: &str) -> (&str, &str) {
    match target.rsplit_once('/') {
        Some((provider, model)) if !provider.is_empty() && !model.is_empty() => (provider, model),
        _ => (DEFAULT_COMBO_ID, target),
    }
}

/// Reads one variable, treating an empty value as absent.
///
/// The one reader every `AR_*` variable goes through, so the gate and the
/// deadlines read the environment exactly the way the provider keys do — one
/// definition of "unset" for the whole config.
fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// Resolves one provider's credential: credential store, then `$VAR`, then an
/// error.
///
/// The store first because a name it holds is the encrypted at-rest value, and
/// `$VAR` is the fallback that predates the store — the order the audit's F-CRIT-2
/// fix asks for, and the one `ar doctor` reports per key so the two cannot
/// disagree.
///
/// An **empty** value is not a miss. `ProviderConfig::api_key` reads empty as
/// "keyless provider" and several registry entries are that, so `keys: {ollama:
/// ""}` is how an operator writes one and it must keep dispatching. A *missing*
/// name is the error: nothing declares it, and dispatching on an undeclared
/// credential is the unauthenticated upstream call this replaces.
fn resolve_key(
    cfg: &Config,
    provider: &str,
    name: &str,
    store: Option<&CredentialStore>,
) -> Result<String, ComboError> {
    if let Some(store) = store
        && let Some(text) = store
            .get_text(name)
            .map_err(|e| ComboError::UnreadableKey {
                provider: provider.to_owned(),
                key: name.to_owned(),
                reason: e.to_string(),
            })?
    {
        return Ok(text);
    }
    cfg.key(name).map_or_else(
        || {
            Err(ComboError::UnresolvedKey {
                provider: provider.to_owned(),
                key: name.to_owned(),
            })
        },
        |secret| Ok(secret.expose().to_owned()),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Duration;

    use ar_keys::{CredentialStore, Secret as StoreSecret};
    use ar_route::{QuotaWindow, Strategy};

    use ar_config::Config;

    /// Serialises the two tests that set process-global environment variables.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    use super::{
        AUTH_MODE_VAR, AuthMode, ComboError, ComboTarget, DISCOVERY_VAR, DefaultChain,
        HTTP_MASTER_KEY_VAR, RouteCombo, STREAM_TIMEOUT_VAR, ServerConfig, split_target,
    };
    use crate::exec::ProviderConfig;

    /// A free-tier kilocode block: no `keys:` row at all, because there is no
    /// account to put one in.
    const ANON_YAML: &str = r#"
keys: {}
providers:
  - id: kilocode
    key: kilocode
combos:
  - id: free
    strategy: priority
    targets:
      - kilocode/openrouter/free
oauth:
  - provider: kilocode
    anonymous: true
    anonymous_editor: artificial-route
"#;

    #[test]
    fn sends_the_constant_credential_and_the_editor_header_for_an_anonymous_free_tier() {
        // The two halves of the mechanism, and neither is derived: the credential
        // is the one the gateway publishes, the header value is the operator's.
        let cfg = ServerConfig::from_ar_config(
            &ar_config::Config::parse(ANON_YAML, |_| Ok(Some(String::new()))).expect("parses"),
            None,
            None,
            false,
            None,
        )
        .expect("the free tier builds");
        let row = cfg.providers.first().expect("one dispatch row");
        assert_eq!(row.api_key, "anonymous");
        assert_eq!(
            row.headers.get("X-KILOCODE-EDITORNAME").map(String::as_str),
            Some("artificial-route")
        );
    }

    #[test]
    fn admits_an_anonymous_free_tier_to_the_candidate_list_without_an_executor() {
        // R1's kilocode, end to end: the config a free-tier operator writes must
        // actually route, or the mechanism is decoration.
        let cfg = ServerConfig::from_ar_config(
            &ar_config::Config::parse(ANON_YAML, |_| Ok(Some(String::new()))).expect("parses"),
            None,
            None,
            false,
            None,
        )
        .expect("the free tier builds");
        let row = cfg.providers.first().expect("one dispatch row");
        assert!(row.is_dispatchable());
    }

    #[test]
    fn refuses_a_session_that_declares_both_anonymous_and_a_refresh_row() {
        let yaml = ANON_YAML.replace(
            "    anonymous_editor: artificial-route",
            "    anonymous_editor: artificial-route\n    refresh_key: kilocode_refresh",
        );
        let err = ar_config::Config::parse(&yaml, |_| Ok(Some(String::new())))
            .expect_err("contradictory");
        assert!(err.to_string().contains("holds no account"), "{err}");
    }

    const TWO_COMBO_YAML: &str = r#"
server:
  host: 127.0.0.1
  port: 20128
  public: true

keys:
  openai: k-openai
  anthropic: k-anthropic

providers:
  - id: openai
    key: openai
  - id: anthropic
    key: anthropic

combos:
  - id: default
    strategy: priority
    targets:
      - openai/gpt-5.4
      - anthropic/claude-sonnet-4-5
  - id: cheap
    strategy: cost-optimized
    targets:
      - openai/gpt-5.4
"#;

    fn parse(yaml: &str) -> Result<ar_config::Config, ar_config::ConfigError> {
        ar_config::Config::parse(yaml, |name| Ok(Some(format!("secret-{name}"))))
    }

    fn flat() -> ServerConfig {
        ServerConfig::single(
            20128,
            Strategy::Priority,
            vec![
                ProviderConfig::new(ar_route::ProviderId::new("openai"), "https://x/v1", "k")
                    .with_model("gpt-4o-mini")
                    .with_price(0.15),
                ProviderConfig::new(ar_route::ProviderId::new("groq"), "https://y/v1", "k")
                    .with_model("llama-3.3-70b")
                    .with_price(0.59),
            ],
        )
    }

    #[test]
    fn defaults_to_port_20128() {
        assert_eq!(super::DEFAULT_PORT, 20128);
    }

    #[test]
    fn derives_candidates_in_provider_order_when_no_combo_table() {
        let ids: Vec<String> = flat()
            .candidates(None)
            .iter()
            .map(|c| c.provider.as_str().to_owned())
            .collect();
        assert_eq!(ids, ["openai", "groq"]);
    }

    #[test]
    fn carries_price_into_candidates() {
        let c = &flat().candidates(None)[1];
        assert_eq!(c.input_usd_per_mtok, Some(0.59));
    }

    #[test]
    fn derives_model_cards() {
        let cards = flat().model_cards();
        assert_eq!(cards[0].id, "openai/gpt-4o-mini");
    }

    #[test]
    fn a_combo_advertises_the_smallest_window_it_can_be_dispatched_to() {
        // A combo id is not a provider id, so the registry cannot resolve one
        // and every combo used to fall to the unknown-model floor. This is the
        // reduction that replaced that: any request the combo accepts must fit
        // whichever target it reaches, so the smallest bounds it.
        let cfg = parse(
            "keys:\n  cline: k\n  nlpcloud: k\nproviders:\n  - id: cline\n    key: cline\n  - id: nlpcloud\n    key: nlpcloud\ncombos:\n  - id: stack\n    strategy: priority\n    targets: [cline/big, nlpcloud/small]\n",
        )
        .expect("config parses");
        let cards = ServerConfig::from_ar_config(&cfg, None, None, false, None)
            .expect("combos build")
            .model_cards();
        let stack = cards.iter().find(|c| c.id == "stack").expect("combo card");
        assert_eq!(
            stack.context_length,
            ar_registry::meta::global()
                .get("nlpcloud")
                .expect("provider b is in the registry")
                .context_length,
            "the reduction takes the minimum over the combo's targets"
        );
        assert_ne!(
            stack.context_length,
            crate::models::ModelCard::UNKNOWN_CONTEXT,
            "a combo whose targets declare windows must not read as unknown"
        );
    }

    #[test]
    fn a_declared_combo_window_overrides_the_reduction() {
        // The reduction is a bound over providers, not a measurement of the
        // models: a priority chain naming six 1M targets and one 128K bench entry
        // reduces to 128K. Naming the real figure is how the operator states it.
        let cfg = parse(
            "keys:\n  cline: k\n  nlpcloud: k\nproviders:\n  - id: cline\n    key: cline\n  - id: nlpcloud\n    key: nlpcloud\ncombos:\n  - id: stack\n    strategy: priority\n    targets: [cline/big, nlpcloud/small]\n    context_length: 1000000\n",
        )
        .expect("config parses");
        let stack = ServerConfig::from_ar_config(&cfg, None, None, false, None)
            .expect("combos build")
            .model_cards()
            .into_iter()
            .find(|c| c.id == "stack")
            .expect("combo card");
        assert_eq!(stack.context_length, 1_000_000);
    }

    #[test]
    fn a_combo_with_no_decidable_target_keeps_whatever_the_card_resolved() {
        // No target names a window, so the reduction yields nothing and the card
        // keeps its own answer rather than being overwritten with a guess.
        let cfg = parse(
            "keys:\n  adapta-web: k\nproviders:\n  - id: adapta-web\n    key: adapta-web\ncombos:\n  - id: stack\n    strategy: priority\n    targets: [adapta-web/m]\n",
        )
        .expect("config parses");
        let stack = ServerConfig::from_ar_config(&cfg, None, None, false, None)
            .expect("combos build")
            .model_cards()
            .into_iter()
            .find(|c| c.id == "stack")
            .expect("combo card");
        assert_eq!(
            stack.context_length,
            crate::models::ModelCard::UNKNOWN_CONTEXT
        );
    }

    #[test]
    fn one_unresolved_target_stops_the_reduction_rather_than_shrinking_it() {
        // The honesty bug, measured on the live config: 899 of 1833 combos name
        // at least one target whose provider declares no window, and reducing
        // over the rest reported a bound those combos never established — 811 of
        // them served models larger than the figure they advertised. An
        // unresolved target must yield no derived figure at all, so the caller
        // falls back to the declared value or the floor.
        //
        // `cline` declares 1050000 and `adapta-web` declares none: a minimum over
        // the resolved targets alone would be 1050000, a claim about a combo
        // that can also reach a provider with no ceiling.
        let cfg = parse(
            "keys:\n  cline: k\n  adapta-web: k\nproviders:\n  - id: cline\n    key: cline\n  - id: adapta-web\n    key: adapta-web\ncombos:\n  - id: stack\n    strategy: priority\n    targets: [cline/big, adapta-web/unknown]\n",
        )
        .expect("config parses");
        let stack = ServerConfig::from_ar_config(&cfg, None, None, false, None)
            .expect("combos build")
            .model_cards()
            .into_iter()
            .find(|c| c.id == "stack")
            .expect("combo card");
        assert_eq!(
            stack.context_length,
            crate::models::ModelCard::UNKNOWN_CONTEXT,
            "a partial reduction must not be reported as a bound"
        );
    }

    #[test]
    fn a_discovered_window_resolves_a_provider_the_registry_omits() {
        // The gap this closes, measured on the live config: `providerMeta.json`
        // declares no ceiling for `nvidia` or `kilocode` — the two providers both
        // stacks route through — so 811 combos serving larger models derived
        // nothing and advertised the floor. The models.dev overlay publishes a
        // per-model window for them, and that is the fallback the reduction reads
        // when the curated table is silent.
        let body = r#"{"nvidia":{"api":"https://integrate.api.nvidia.com/v1",
            "models":{"z-ai/glm-5.3":{"limit":{"context":1048576}}}}}"#;
        let view = std::sync::Arc::new(
            ar_registry::discovery::parse_catalog(body).expect("catalog parses"),
        );
        let cfg = parse(
            "keys:\n  nvidia: k\nproviders:\n  - id: nvidia\n    key: nvidia\ncombos:\n  - id: stack\n    strategy: priority\n    targets: [nvidia/z-ai/glm-5.3]\n",
        )
        .expect("config parses");
        let server =
            ServerConfig::from_ar_config(&cfg, None, None, false, None).expect("combos build");
        let lookup = move |provider: &str, model: &str| {
            ar_registry::discovery::published_window(&view, provider, model)
        };
        let stack = server
            .model_cards_with(Some(&lookup))
            .into_iter()
            .find(|c| c.id == "stack")
            .expect("combo card");
        assert_eq!(
            stack.context_length, 1_048_576,
            "a provider absent from providerMeta.json must still resolve"
        );
        // Without the fallback the same combo reads the floor, which is the
        // whole of the bug this closes.
        assert_ne!(
            stack.context_length,
            crate::models::ModelCard::UNKNOWN_CONTEXT
        );
    }

    #[test]
    fn a_combo_keeps_the_floor_when_neither_source_resolves() {
        // Both sources silent is a real answer and must not be papered over with a
        // partial figure: the reduction stays all-or-nothing.
        let cfg = parse(
            "keys:\n  adapta-web: k\nproviders:\n  - id: adapta-web\n    key: adapta-web\ncombos:\n  - id: stack\n    strategy: priority\n    targets: [adapta-web/m]\n",
        )
        .expect("config parses");
        let stack = ServerConfig::from_ar_config(&cfg, None, None, false, None)
            .expect("combos build")
            .model_cards_with(Some(&(|_: &str, _: &str| None)))
            .into_iter()
            .find(|c| c.id == "stack")
            .expect("combo card");
        assert_eq!(
            stack.context_length,
            crate::models::ModelCard::UNKNOWN_CONTEXT
        );
    }

    #[test]
    fn a_declared_window_survives_an_unresolvable_chain() {
        // The operator's answer is what the combo then reports, and it is the
        // only way a stack whose targets straddle unknown providers can state
        // the window it really serves.
        let cfg = parse(
            "keys:\n  cline: k\n  adapta-web: k\nproviders:\n  - id: cline\n    key: cline\n  - id: adapta-web\n    key: adapta-web\ncombos:\n  - id: stack\n    strategy: priority\n    targets: [cline/big, adapta-web/unknown]\n    context_length: 1000000\n",
        )
        .expect("config parses");
        let stack = ServerConfig::from_ar_config(&cfg, None, None, false, None)
            .expect("combos build")
            .model_cards()
            .into_iter()
            .find(|c| c.id == "stack")
            .expect("combo card");
        assert_eq!(stack.context_length, 1_000_000);
    }

    #[test]
    fn reports_no_provider_when_unconfigured() {
        let c = ServerConfig::single(1, Strategy::Priority, vec![]);
        assert!(!c.has_provider());
    }

    #[test]
    fn splits_a_provider_qualified_target() {
        assert_eq!(split_target("openai/gpt-5.4"), ("openai", "gpt-5.4"));
    }

    #[test]
    fn splits_the_last_segment_of_a_nested_provider_path() {
        assert_eq!(
            split_target("accounts/fireworks/models/mixtral"),
            ("accounts/fireworks/models", "mixtral")
        );
    }

    #[test]
    fn treats_a_bare_model_as_the_default_provider() {
        assert_eq!(split_target("gpt-4o"), ("default", "gpt-4o"));
    }

    #[test]
    fn builds_one_combo_per_configured_combo() {
        let cfg = parse(TWO_COMBO_YAML).expect("config parses");
        let server =
            ServerConfig::from_ar_config(&cfg, None, None, false, None).expect("combos build");
        assert_eq!(server.combo_ids(), ["default", "cheap"]);
    }

    #[test]
    fn reads_the_port_from_the_file_config() {
        let cfg = parse(TWO_COMBO_YAML).expect("config parses");
        let server =
            ServerConfig::from_ar_config(&cfg, None, None, false, None).expect("combos build");
        assert_eq!(server.port, 20128);
    }

    #[test]
    fn honours_a_port_override() {
        let cfg = parse(TWO_COMBO_YAML).expect("config parses");
        let server = ServerConfig::from_ar_config(&cfg, Some(9999), None, false, None)
            .expect("combos build");
        assert_eq!(server.port, 9999);
    }

    #[test]
    fn defaults_to_loopback_only_from_a_file_config() {
        // `ar_config::Server` has no `public` field yet (see the TODO on
        // `with_public`), so the file cannot grant it. The gate therefore
        // defaults to closed whatever the YAML says — which is the direction
        // that fails safe.
        let cfg = parse(TWO_COMBO_YAML).expect("config parses");
        let server =
            ServerConfig::from_ar_config(&cfg, None, None, false, None).expect("combos build");
        assert!(!server.public);
    }

    #[test]
    fn carries_an_explicit_public_flag() {
        let cfg = parse(TWO_COMBO_YAML).expect("config parses");
        let server =
            ServerConfig::from_ar_config(&cfg, None, None, true, None).expect("combos build");
        assert!(server.public);
    }

    #[test]
    fn builds_one_candidate_per_combo_target_that_can_be_dispatched() {
        // The live-path defect this fixes: a two-target combo used to collapse to
        // one candidate, so failover across it never happened. The registry's
        // `anthropic` entry carries the Anthropic wire, which this build cannot
        // send, so the honest answer here is one — see
        // `drops_an_undispatchable_target_from_the_candidate_list` and
        // `builds_a_multi_provider_chain_from_two_dispatchable_targets`.
        let cfg = parse(TWO_COMBO_YAML).expect("config parses");
        let server =
            ServerConfig::from_ar_config(&cfg, None, None, false, None).expect("combos build");
        let combo = server.combo("default").expect("default combo exists");
        assert_eq!(
            combo.targets.len(),
            2,
            "both targets are declared in the file"
        );
    }

    #[test]
    fn builds_a_multi_provider_chain_from_two_dispatchable_targets() {
        // The property the e2e depends on: a combo with two OpenAI-wire targets
        // yields two candidates, so the attempt loop has something to fail over
        // to. Built directly because the compiled-in registry carries one
        // OpenAI-wire provider.
        let mut server = flat();
        server.combos = vec![RouteCombo::new(
            "default",
            Strategy::Priority,
            vec![
                ComboTarget::new(ar_route::ProviderId::new("openai"), "gpt-4o-mini"),
                ComboTarget::new(ar_route::ProviderId::new("groq"), "llama-3.3-70b"),
            ],
        )];
        let combo = server.combo("default").expect("combo exists");
        let ids: Vec<String> = server
            .candidates(Some(combo))
            .iter()
            .map(|c| c.provider.as_str().to_owned())
            .collect();
        assert_eq!(ids, ["openai", "groq"]);
    }

    #[test]
    fn resolves_each_combo_to_its_own_targets() {
        let cfg = parse(TWO_COMBO_YAML).expect("config parses");
        let server =
            ServerConfig::from_ar_config(&cfg, None, None, false, None).expect("combos build");
        let cheap = server.combo("cheap").expect("cheap combo exists");
        assert_eq!(server.candidates(Some(cheap)).len(), 1);
    }

    #[test]
    fn carries_the_per_combo_strategy() {
        let cfg = parse(TWO_COMBO_YAML).expect("config parses");
        let server =
            ServerConfig::from_ar_config(&cfg, None, None, false, None).expect("combos build");
        assert_eq!(
            server.combo("cheap").map(|c| c.strategy),
            Some(Strategy::CostOptimized)
        );
    }

    #[test]
    fn takes_the_first_combo_as_the_default_chain() {
        let cfg = parse(TWO_COMBO_YAML).expect("config parses");
        let server =
            ServerConfig::from_ar_config(&cfg, None, None, false, None).expect("combos build");
        assert!(matches!(
            server.default_combo(),
            DefaultChain::Combo(c) if c.id == "default"
        ));
    }

    /// The config→server strategy bridge is `Strategy::parse(combo.as_str())`,
    /// so a name `ar-config` parses and `ar-route` does not would reach a client
    /// as `Deferred` with no error anywhere. `expiry-first` is the one that
    /// actually diverged — the variant shipped in `ar-route` before `ar-config`
    /// had an arm for it — so it is the one that pins the whole chain.
    #[test]
    fn carries_expiry_first_from_the_config_to_the_dispatcher() {
        let yaml = TWO_COMBO_YAML.replace("strategy: cost-optimized", "strategy: expiry-first");
        // A silent no-op here leaves the combo cost-optimized, so the assert
        // below would fail either way — but as a routing bug, not the fixture
        // drift it actually is. Naming the drift keeps the failure honest.
        assert_ne!(
            yaml, TWO_COMBO_YAML,
            "fixture no longer contains the strategy line being replaced"
        );
        let cfg = parse(&yaml).expect("config parses");
        let server =
            ServerConfig::from_ar_config(&cfg, None, None, false, None).expect("combos build");
        assert_eq!(
            server.combo("cheap").map(|c| c.strategy),
            Some(Strategy::ExpiryFirst),
            "a config naming expiry-first must reach the router as itself"
        );
    }

    #[test]
    fn resolves_a_provider_only_once_across_combos() {
        // `openai` appears in both combos; two dispatch rows would make the
        // executor's `by_id` keep whichever came last.
        let cfg = parse(TWO_COMBO_YAML).expect("config parses");
        let server =
            ServerConfig::from_ar_config(&cfg, None, None, false, None).expect("combos build");
        let openai = server
            .providers
            .iter()
            .filter(|p| p.id.as_str() == "openai")
            .count();
        assert_eq!(openai, 1);
    }

    #[test]
    fn rejects_a_target_whose_provider_is_not_in_the_registry() {
        let yaml = "keys:\n  a: k\nproviders:\n  - id: a\n    key: a\ncombos:\n  - id: c\n    strategy: priority\n    targets:\n      - nope/gpt-4o\n";
        let cfg = parse(yaml).expect("config parses");
        assert!(matches!(
            ServerConfig::from_ar_config(&cfg, None, None, false, None),
            Err(ComboError::UnknownProvider { .. })
        ));
    }

    #[test]
    fn splits_a_nested_model_path_at_a_known_provider() {
        // Live OmniRoute combos address nested model paths
        // (`nvidia/moonshotai/kimi-k3`); the rsplit half names no provider,
        // so the first slash wins when it names one.
        let yaml = "keys:\n  nvidia: k\nproviders:\n  - id: nvidia\n    key: nvidia\ncombos:\n  - id: free-stack\n    strategy: least-used\n    targets:\n      - nvidia/moonshotai/kimi-k3\n      - nvidia/z-ai/glm-5.3\n";
        let cfg = parse(yaml).expect("config parses");
        let server =
            ServerConfig::from_ar_config(&cfg, None, None, false, None).expect("combos build");
        let combo = server.combo("free-stack").expect("combo exists");
        let got: Vec<(String, String)> = server
            .candidates(Some(combo))
            .iter()
            .map(|c| (c.provider.as_str().to_owned(), c.model.as_ref().to_owned()))
            .collect();
        assert_eq!(
            got,
            [
                ("nvidia".to_owned(), "moonshotai/kimi-k3".to_owned()),
                ("nvidia".to_owned(), "z-ai/glm-5.3".to_owned())
            ]
        );
    }

    #[test]
    fn splits_a_multi_slash_model_path_at_the_registered_provider() {
        // `aihorde/aphrodite/TheDrummer/Cydonia-24B-v4.3`: neither the rsplit
        // half nor the first-slash half names a provider on its own; the
        // longest registered prefix wins.
        let yaml = "keys:\n  aihorde: k\nproviders:\n  - id: aihorde\n    key: aihorde\ncombos:\n  - id: c\n    strategy: priority\n    targets:\n      - aihorde/aphrodite/TheDrummer/Cydonia-24B-v4.3\n";
        let cfg = parse(yaml).expect("config parses");
        let server =
            ServerConfig::from_ar_config(&cfg, None, None, false, None).expect("combos build");
        let combo = server.combo("c").expect("combo exists");
        let got: Vec<(String, String)> = server
            .candidates(Some(combo))
            .iter()
            .map(|c| (c.provider.as_str().to_owned(), c.model.as_ref().to_owned()))
            .collect();
        assert_eq!(
            got,
            [(
                "aihorde".to_owned(),
                "aphrodite/TheDrummer/Cydonia-24B-v4.3".to_owned()
            )]
        );
    }

    #[test]
    fn rejects_a_combo_with_no_targets() {
        let yaml = "keys:\n  a: k\nproviders:\n  - id: a\n    key: a\ncombos:\n  - id: c\n    strategy: priority\n    targets: []\n";
        let cfg = parse(yaml).expect("config parses");
        assert!(matches!(
            ServerConfig::from_ar_config(&cfg, None, None, false, None),
            Err(ComboError::EmptyCombo { .. })
        ));
    }

    #[test]
    fn drops_an_undispatchable_target_from_the_candidate_list() {
        // `anthropic` is an anthropic-family wire, which this build dispatches,
        // so it keeps its slot alongside the OpenAI target.
        let cfg = parse(TWO_COMBO_YAML).expect("config parses");
        let server =
            ServerConfig::from_ar_config(&cfg, None, None, false, None).expect("combos build");
        let combo = server.combo("default").expect("default combo exists");
        let ids: Vec<String> = server
            .candidates(Some(combo))
            .iter()
            .map(|c| c.provider.as_str().to_owned())
            .collect();
        assert_eq!(ids, ["openai", "anthropic"]);
    }

    #[test]
    fn lists_combo_ids_as_the_routable_models() {
        let cfg = parse(TWO_COMBO_YAML).expect("config parses");
        let server =
            ServerConfig::from_ar_config(&cfg, None, None, false, None).expect("combos build");
        let ids: Vec<String> = server.model_cards().into_iter().map(|c| c.id).collect();
        assert_eq!(ids, ["default", "cheap"]);
    }

    #[test]
    fn resolves_weight_and_quota_from_the_combo_target() {
        let combo = RouteCombo::new(
            "c",
            Strategy::Weighted,
            vec![
                ComboTarget::new(ar_route::ProviderId::new("p"), "m").with_weight(4),
                ComboTarget::new(ar_route::ProviderId::new("q"), "m")
                    .with_quota(QuotaWindow::new(10, 3, 0)),
            ],
        );
        let mut server = flat();
        server.combos = vec![combo];
        server.providers = vec![
            ProviderConfig::new(ar_route::ProviderId::new("p"), "https://x/v1", "k"),
            ProviderConfig::new(ar_route::ProviderId::new("q"), "https://y/v1", "k"),
        ];
        let got = server.candidates(server.combo("c"));
        assert_eq!(got[0].weight, 4);
        assert_eq!(got[1].quota.map(|q| q.remaining()), Some(7));
    }

    const WEIGHTED_TARGETS_YAML: &str = r#"
keys:
  openai: k-openai
  groq: k-groq

providers:
  - id: openai
    key: openai
  - id: groq
    key: groq

combos:
  - id: spread
    strategy: weighted
    targets:
      - openai/gpt-5.4
      - { target: groq/llama-3.3-70b, weight: 7 }
"#;

    #[test]
    fn threads_a_declared_per_target_weight_onto_the_candidate() {
        // The load the weight exists to carry. `weighted` used to hand every
        // entry in a combo one number — the combo's own position in the file —
        // so a uniform share cancelled in the roulette wheel and the operator
        // had no way to say "this one gets more". The declared number has to
        // reach the `Candidate`, which is the only thing `by_weight` reads.
        let cfg = Config::parse(WEIGHTED_TARGETS_YAML, |name| Ok(Some(format!("k-{name}"))))
            .expect("the map form of a target parses");
        let server =
            ServerConfig::from_ar_config(&cfg, None, None, false, None).expect("combos build");
        let combo = server.combo("spread").expect("the combo");
        let got = server.candidates(Some(combo));
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].weight, 1, "the bare target declared no share");
        assert_eq!(got[1].weight, 7, "the declared share reaches the candidate");
    }

    #[test]
    fn gives_every_target_the_same_default_share_when_none_is_declared() {
        // Order-equivalence with what the config reader used to synthesise: a
        // uniform share draws the same whichever constant it is, so an
        // unweighted config must keep behaving exactly as it did.
        let cfg = Config::parse(WEIGHTED_TARGETS_YAML, |name| Ok(Some(format!("k-{name}"))))
            .expect("parses");
        let server =
            ServerConfig::from_ar_config(&cfg, None, None, false, None).expect("combos build");
        let got = server.candidates(server.combo("spread"));
        assert_eq!(got[1].weight, 7);
        assert_eq!(
            got[0].weight, 1,
            "the unweighted target takes the uniform share"
        );
    }

    #[test]
    fn leaves_a_candidate_unpriced_when_the_table_has_no_row() {
        // `ar-tokens` ships no rows yet, so "no price" must stay "no price"
        // rather than becoming 0.0, which would route to it as if it were free.
        let c = &flat().candidates(None)[0];
        assert_eq!(c.input_usd_per_mtok, Some(0.15));
    }

    #[test]
    fn reads_a_price_out_of_an_explicit_table() {
        let mut table = ar_tokens::PricingTable::default();
        table.set(
            "groq",
            "llama-3.3-70b",
            ar_tokens::Prices {
                input_micros_per_mtok: 590_000,
                output_micros_per_mtok: 790_000,
            },
        );
        let mut server = flat();
        server.prices = table;
        server.providers[0].input_usd_per_mtok = None;
        assert_eq!(server.candidates(None)[1].input_usd_per_mtok, Some(0.59));
    }

    #[test]
    fn env_path_still_builds_a_single_provider() {
        let c = ServerConfig::from_provider(
            None,
            Some("https://api.x/v1".to_owned()),
            Some("m".to_owned()),
            Some("k".to_owned()),
            Some("p1".to_owned()),
            Some("priority".to_owned()),
            None,
        );
        assert!(c.combos.is_empty(), "the env path has no combo table");
        assert_eq!(c.providers.len(), 1);
        assert!(c.has_provider());
    }

    #[test]
    fn env_path_defaults_to_loopback_only() {
        let c = ServerConfig::from_env();
        assert!(!c.public, "an env-configured server must not be public");
    }

    fn store_with(entries: &[(&str, &str, &str)]) -> CredentialStore {
        let store = CredentialStore::open_in_memory(&StoreSecret::generate()).expect("store opens");
        for (provider, name, value) in entries {
            store
                .insert(provider, name, &StoreSecret::new(value.as_bytes().to_vec()))
                .expect("insert");
        }
        store
    }

    /// The key `TWO_COMBO_YAML`'s `openai` provider resolves to.
    fn openai_key(server: &ServerConfig) -> &str {
        server
            .providers
            .iter()
            .find(|p| p.id.as_str() == "openai")
            .expect("openai row")
            .api_key
            .as_str()
    }

    #[test]
    fn prefers_the_store_when_a_name_is_in_both_places() {
        // The order F-CRIT-2 asks for. A store that lost to `$VAR` would make the
        // encrypted copy decorative.
        let cfg = parse(TWO_COMBO_YAML).expect("config parses");
        let store = store_with(&[("openai", "openai", "sk-from-store")]);
        let server = ServerConfig::from_ar_config(&cfg, None, None, false, Some(&store))
            .expect("combos build");
        assert_eq!(openai_key(&server), "sk-from-store");
    }

    #[test]
    fn falls_back_to_the_var_when_the_store_has_no_row() {
        let cfg = parse(TWO_COMBO_YAML).expect("config parses");
        let store = store_with(&[("groq", "groq", "sk-unrelated")]);
        let server = ServerConfig::from_ar_config(&cfg, None, None, false, Some(&store))
            .expect("combos build");
        // `TWO_COMBO_YAML` declares literals, so the fallback value is the
        // literal, not an expanded `$VAR`.
        assert_eq!(openai_key(&server), "k-openai");
    }

    #[test]
    fn keeps_a_keyless_provider_dispatchable_when_the_key_is_declared_empty() {
        // Empty is the documented way to write a keyless provider, so an empty
        // entry must still reach the executor rather than become a resolve error.
        let yaml = "keys:\n  openai: \"\"\nproviders:\n  - id: openai\n    key: openai\ncombos:\n  - id: c\n    strategy: priority\n    targets:\n      - openai/gpt-5.4\n";
        let cfg = parse(yaml).expect("config parses");
        let server =
            ServerConfig::from_ar_config(&cfg, None, None, false, None).expect("combos build");
        assert!(openai_key(&server).is_empty());
    }

    #[test]
    fn refuses_a_provider_when_its_credential_resolves_nowhere() {
        // A combo target naming a provider the file never declared. It used to
        // become an unauthenticated upstream call.
        let yaml = "keys:\n  openai: k\nproviders:\n  - id: openai\n    key: openai\ncombos:\n  - id: c\n    strategy: priority\n    targets:\n      - openai/gpt-5.4\n      - groq/llama-3.3-70b\n";
        let cfg = parse(yaml).expect("config parses");
        assert!(matches!(
            ServerConfig::from_ar_config(&cfg, None, None, false, None),
            Err(ComboError::UnresolvedKey { .. })
        ));
    }

    #[test]
    fn refuses_to_fall_back_to_the_var_when_the_store_cannot_decrypt() {
        // A store that is present, holds the name, and will not hand it over.
        // Falling through to `$VAR` here would dispatch a *different* credential
        // and report the mismatch as a provider 401.
        let path =
            std::env::temp_dir().join(format!("ar-server-unreadable-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let writer =
                CredentialStore::open_with_material(&path, &StoreSecret::generate()).expect("open");
            writer
                .insert("openai", "openai", &StoreSecret::new(b"sk-stored".to_vec()))
                .expect("insert");
        }
        // Same file, another install's master: the row is there and unreadable.
        let store =
            CredentialStore::open_with_material(&path, &StoreSecret::generate()).expect("reopen");
        let cfg = parse(TWO_COMBO_YAML).expect("config parses");
        assert!(matches!(
            ServerConfig::from_ar_config(&cfg, None, None, false, Some(&store)),
            Err(ComboError::UnreadableKey { .. })
        ));
        let _ = std::fs::remove_file(&path);
    }

    /// A file-declared node, routed on the same terms as a compiled-in one.
    const CUSTOM_YAML: &str = r#"
keys:
  local: k-local

custom_providers:
  - id: local-gateway
    protocol: openai-compatible
    base_url: https://api.example.invalid/v1
    key_ref: local
    headers:
      x-api-key: hdr

combos:
  - id: default
    strategy: priority
    targets:
      - local-gateway/some-model
"#;

    #[test]
    fn routes_a_custom_provider_target_when_the_file_declares_it() {
        // The no-rebuild path end to end: an id that is in no compiled-in
        // catalog resolves, dispatches to, and is a candidate.
        let cfg = parse(CUSTOM_YAML).expect("config parses");
        let server =
            ServerConfig::from_ar_config(&cfg, None, None, false, None).expect("the node merges");
        assert_eq!(
            server.combo("default").expect("the combo").targets[0]
                .provider
                .as_str(),
            "local-gateway"
        );
    }

    #[test]
    fn carries_a_custom_providers_base_url_when_it_dispatches() {
        let cfg = parse(CUSTOM_YAML).expect("config parses");
        let server =
            ServerConfig::from_ar_config(&cfg, None, None, false, None).expect("the node merges");
        assert_eq!(
            server.providers[0].base_url,
            "https://api.example.invalid/v1"
        );
    }

    #[test]
    fn carries_a_custom_providers_headers_when_it_dispatches() {
        let cfg = parse(CUSTOM_YAML).expect("config parses");
        let server =
            ServerConfig::from_ar_config(&cfg, None, None, false, None).expect("the node merges");
        assert_eq!(
            server.providers[0]
                .headers
                .get("x-api-key")
                .map(String::as_str),
            Some("hdr")
        );
    }

    #[test]
    fn resolves_a_custom_providers_credential_through_its_key_ref() {
        let cfg = parse(CUSTOM_YAML).expect("config parses");
        let server =
            ServerConfig::from_ar_config(&cfg, None, None, false, None).expect("the node merges");
        assert_eq!(server.providers[0].api_key, "k-local");
    }

    #[test]
    fn refuses_a_custom_provider_when_its_id_collides_with_the_catalog() {
        let yaml = CUSTOM_YAML.replace("id: local-gateway", "id: openai");
        let cfg = parse(&yaml).expect("config parses");
        assert!(matches!(
            ServerConfig::from_ar_config(&cfg, None, None, false, None),
            Err(ComboError::CustomProvider(
                ar_registry::MergeError::Collides { .. }
            ))
        ));
    }

    #[test]
    fn drops_a_custom_provider_from_candidates_when_it_is_not_openai_compatible() {
        // An anthropic-compatible custom provider is a wire this build
        // dispatches, so it is listed as a candidate like the OpenAI spelling.
        let yaml = CUSTOM_YAML.replace("openai-compatible", "anthropic-compatible");
        let cfg = parse(&yaml).expect("config parses");
        let server =
            ServerConfig::from_ar_config(&cfg, None, None, false, None).expect("the node merges");
        let ids: Vec<String> = server
            .candidates(None)
            .iter()
            .map(|c| c.provider.as_str().to_owned())
            .collect();
        assert_eq!(ids, ["local-gateway"]);
    }

    // --- AuthMode ------------------------------------------------------

    #[test]
    fn the_strict_mode_is_the_default() {
        // A typo in a mode name must not silently turn a gate off, so the
        // parser's fallback and the default are the same answer.
        assert_eq!(AuthMode::default(), AuthMode::Required);
        assert_eq!(AuthMode::parse("nonsense"), AuthMode::Required);
    }

    #[test]
    fn every_mode_round_trips_through_its_spelling() {
        for mode in [
            AuthMode::Open,
            AuthMode::Required,
            AuthMode::DegradeInvalidToAnon,
        ] {
            assert_eq!(
                AuthMode::parse(mode.as_str()),
                mode,
                "{mode:?} did not round-trip"
            );
        }
    }

    #[test]
    fn a_modes_spelling_is_matched_case_and_space_insensitively() {
        assert_eq!(AuthMode::parse("  OPEN "), AuthMode::Open);
        assert_eq!(
            AuthMode::parse("Degrade-Invalid-To-Anon"),
            AuthMode::DegradeInvalidToAnon
        );
        // The short spelling is a convenience, not a second name for the mode.
        assert_eq!(AuthMode::parse("degrade"), AuthMode::DegradeInvalidToAnon);
    }

    // --- the env-backed fields ----------------------------------------

    #[test]
    fn a_server_with_no_master_key_has_no_gate() {
        // The property the default relies on: `Required` is inert until a key
        // names a gate, so an operator who exports nothing gets an open server.
        let config = flat();
        assert!(config.http_master_key.is_none());
    }

    // The one place the process environment is touched. Both readers are proved
    // here, in one test, because the env is global: two tests each setting one
    // variable clear each other's mid-test and the failure looks like the reader
    // is broken. One test cannot race itself.
    #[test]
    fn the_env_backed_fields_are_read_by_both_config_readers() {
        let key = "ab".repeat(32);
        let (from_file, from_env) = with_env(
            &[
                (HTTP_MASTER_KEY_VAR, key.as_str()),
                (AUTH_MODE_VAR, "degrade-invalid-to-anon"),
                (STREAM_TIMEOUT_VAR, "gpt-5.4=600"),
            ],
            || {
                let cfg = parse(TWO_COMBO_YAML).expect("config parses");
                (
                    ServerConfig::from_ar_config(&cfg, None, None, false, None)
                        .expect("combos build"),
                    ServerConfig::from_env(),
                )
            },
        );
        for (name, config) in [("from_ar_config", &from_file), ("from_env", &from_env)] {
            assert!(
                config.http_master_key.is_some(),
                "{name} did not arm the gate"
            );
            assert_eq!(
                config.auth_mode,
                AuthMode::DegradeInvalidToAnon,
                "{name} ignored {AUTH_MODE_VAR}"
            );
            assert_eq!(
                config.stream_deadline("gpt-5.4"),
                Duration::from_secs(600),
                "{name} ignored {STREAM_TIMEOUT_VAR}"
            );
        }
    }

    #[test]
    fn a_config_that_names_nothing_produces_the_config_it_produced_before() {
        // The load-bearing claim about reading the environment inside
        // `from_ar_config`: with none of the three variables set, every field is
        // the value it was before this change, so no existing install changes
        // behaviour. Asserted rather than assumed, because "it defaults the same"
        // is exactly the kind of thing a future default change breaks silently.
        let cfg = parse(TWO_COMBO_YAML).expect("config parses");
        let server =
            ServerConfig::from_ar_config(&cfg, None, None, false, None).expect("combos build");
        assert_eq!(server.auth_mode, AuthMode::Required);
        assert!(server.http_master_key.is_none());
        assert!(server.timeouts.is_empty());
        assert_eq!(
            server.stream_deadline("gpt-5.4"),
            crate::app::REQUEST_TIMEOUT
        );
    }

    #[test]
    fn a_master_key_is_accepted_as_hex_or_as_raw_bytes() {
        // Both shapes, because an operator has one of them and the other is a
        // 32-character typo away from a gate that silently does not exist.
        for material in ["ab".repeat(32), "a".repeat(32)] {
            let decoded = super::decode_master(&material).expect("accepted");
            assert_eq!(decoded.as_bytes().len(), ar_keys::KEY_LEN);
        }
    }

    #[test]
    fn the_two_accepted_shapes_decode_to_the_same_bytes() {
        // The property that makes "hex or raw" one setting rather than two: an
        // operator must not be able to export a value this decodes differently
        // from how they wrote it.
        let raw = "0123456789abcdef0123456789abcdef";
        let hex = "3031323334353637383961626364656630313233343536373839616263646566";
        assert_eq!(
            super::decode_master(raw).expect("raw").as_bytes(),
            super::decode_master(hex).expect("hex").as_bytes()
        );
    }

    #[test]
    fn a_value_that_is_neither_shape_is_refused() {
        // Base64 is deliberately not accepted: a silently different encoding than
        // the one `AR_MASTER_KEY` uses is how a key ends up wrong in a way that
        // only shows up as a gate that never opens.
        assert!(
            super::decode_master(&"a".repeat(43)).is_err(),
            "base64 was accepted"
        );
        assert!(
            super::decode_master(&"z".repeat(64)).is_err(),
            "non-hex was accepted"
        );
    }

    #[test]
    fn the_gate_key_is_never_rendered_in_debug() {
        let mut config = flat();
        config.http_master_key = Some(ar_keys::Secret::new(vec![0xab; ar_keys::KEY_LEN]));
        let text = format!("{config:?}");
        assert!(
            !text.contains(&"ab".repeat(8)),
            "the gate key leaked: {text}"
        );
        assert!(text.contains("redacted"), "unexpected Debug: {text}");
    }

    // The deadline table is tested through the pure parser rather than through
    // the process environment: the env is global, so two tests each setting one
    // variable can clear each other's mid-test, and the failure looks like the
    // reader is broken when it is a race. One env test at the bottom proves the
    // reader calls the parser; these prove the parser.
    #[test]
    fn a_bare_number_is_the_deadline_for_every_model() {
        let table = super::parse_timeouts("600");
        assert_eq!(table.get("*"), Some(&Duration::from_secs(600)));
    }

    #[test]
    fn a_per_model_list_names_one_model_and_leaves_the_rest_alone() {
        let table = super::parse_timeouts("gpt-5.4=600,claude=300");
        assert_eq!(table.get("gpt-5.4"), Some(&Duration::from_secs(600)));
        assert_eq!(table.get("claude"), Some(&Duration::from_secs(300)));
        // A model nobody named gets no entry, so it falls through to the default
        // rather than inheriting another model's.
        assert!(
            !table.contains_key("other"),
            "an unnamed model got a deadline: {table:?}"
        );
    }

    #[test]
    fn a_bare_number_alongside_a_per_model_entry_is_the_fallback() {
        let table = super::parse_timeouts("120,gpt-5.4=600");
        assert_eq!(table.get("gpt-5.4"), Some(&Duration::from_secs(600)));
        assert_eq!(table.get("*"), Some(&Duration::from_secs(120)));
    }

    #[test]
    fn a_zero_is_not_a_deadline() {
        // Zero would mean "cut every request off immediately", which is not a
        // thing an operator asking for a timeout means.
        assert!(super::parse_timeouts("0").is_empty());
    }

    #[test]
    fn a_bad_deadline_is_dropped_not_fatal() {
        // A bad timeout is not a reason to refuse to route: the affected model
        // falls back to the default, which is the documented answer.
        let table = super::parse_timeouts("gpt-5.4=0,claude=soon,nano=60");
        assert!(
            !table.contains_key("gpt-5.4"),
            "a zero deadline was accepted: {table:?}"
        );
        assert!(
            !table.contains_key("claude"),
            "an unparseable one was accepted: {table:?}"
        );
        assert_eq!(table.get("nano"), Some(&Duration::from_secs(60)));
    }

    #[test]
    fn an_empty_value_is_an_empty_table() {
        // The shape the env builder never sees — it returns early on an unset
        // variable — so the "no deadlines" answer is stated once here rather than
        // assumed of the caller.
        assert!(super::parse_timeouts("").is_empty());
        assert!(super::parse_timeouts(" , ,").is_empty());
    }

    #[test]
    fn the_auth_mode_is_parsed_from_its_spelling() {
        assert_eq!(AuthMode::parse("degrade"), AuthMode::DegradeInvalidToAnon);
    }

    #[test]
    fn arms_model_discovery_only_on_an_explicit_yes() {
        // The narrow direction matters: this overlay makes a network call the
        // proxy did not previously make, so a typo must not read as consent.
        let base = || ServerConfig::from_env();
        let on = |v: &str| with_env(&[(DISCOVERY_VAR, v)], base).model_discovery;
        for v in ["1", "true", "on", "yes", "ON", " True "] {
            assert!(on(v), "{v:?} should arm discovery");
        }
        for v in ["0", "false", "off", "", "maybe", "2"] {
            assert!(!on(v), "{v:?} must not arm discovery");
        }
    }

    #[test]
    fn leaves_model_discovery_off_when_the_env_names_nothing() {
        // The default configuration must make no network call it did not
        // previously make, so an unset variable is the whole test.
        let cfg = ServerConfig::from_env();
        assert!(!cfg.model_discovery, "an unset variable arms nothing");
    }

    /// Sets every named variable, builds, then clears them all.
    ///
    /// Two tests share this. Cargo runs a crate's tests as threads in one
    /// process and the environment is process-global, so an overlapping
    /// set/clear pair would have one test's `remove_var` clearing the other's
    /// variable mid-build — a flake that looks like a parser bug. `ENV_LOCK`
    /// serialises the two callers; nothing outside this module touches the env.
    ///
    /// Takes a closure rather than a value because every builder here is
    /// `self -> Self`: a value would have to be constructed before the variables
    /// are set, which is the opposite of what the test is checking.
    fn with_env<T>(vars: &[(&str, &str)], build: impl FnOnce() -> T) -> T {
        let _held = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for (key, value) in vars {
            // SAFETY: the guard above holds this module's only claim on the
            // environment, and the variables are cleared before it is released.
            unsafe { std::env::set_var(key, value) };
        }
        let out = build();
        for (key, _) in vars {
            // SAFETY: as above — still holding the guard, so no other test can
            // observe a half-cleared environment.
            unsafe { std::env::remove_var(key) };
        }
        out
    }
}
