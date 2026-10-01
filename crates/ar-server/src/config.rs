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
//! # Where the routing signals come from
//!
//! `ar_route::Candidate` has fifteen-odd optional fields with documented neutral
//! values, and three of them are populated here because real data exists:
//!
//! | field | source |
//! |---|---|
//! | `input_usd_per_mtok` | explicit config, else [`ar_tokens::PricingTable`], which reads `ar-registry`'s rows |
//! | `weight` | explicit config |
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

use ar_compress::Step;
use ar_config::Config;
use ar_exec::oauth::{OAuthKind, Session};
use ar_keys::CredentialStore;
use ar_registry::WireFormat;
use ar_route::{Candidate, ProviderId, QuotaWindow, Strategy};
use ar_tokens::{NormalizedUsage, PricingTable};

use crate::exec::{OAuthAuth, ProviderConfig};
use crate::models::ModelCard;

/// Default listen port, matching the OmniRoute-compatible `127.0.0.1:20128`.
pub const DEFAULT_PORT: u16 = 20128;

/// Combo id used when the config names none — the environment path, and any
/// config whose `combos:` list is empty.
///
/// A constant rather than a generated one so `x-ar-decision` and `/v1/models`
/// carry the same id for a default chain across restarts.
pub const DEFAULT_COMBO_ID: &str = "default";

/// One routable target inside a combo: a provider plus the model spelling that
/// provider uses.
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
}

impl RouteCombo {
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
        }
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

/// Resolved server configuration: the combo table plus the flat provider table
/// the executor dispatches against.
///
/// `Clone` is deliberately absent: `prices` holds an `ar_tokens::PricingTable`,
/// which is a `HashMap` and not `Clone`, and cloning a 32 MB price table to hand
/// a second config to a test would be the wrong trade. `Arc<ServerConfig>` is how
/// the state shares one anyway.
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
    /// Wider than any single combo's targets: a combo is a *view* over this
    /// table, and the executor holds its own copy anyway.
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
    /// `false` (the default) means loopback only. It is not a style preference:
    /// an LLM proxy with no credential check is a bill attached to a socket, so
    /// binding a routable address takes an explicit `server.public: true` and
    /// [`crate::app::bind_addr`] refuses otherwise.
    pub public: bool,
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
        }
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

        for (i, combo) in cfg.combos.iter().enumerate() {
            if combo.targets.is_empty() {
                return Err(ComboError::EmptyCombo { id: combo.id.clone() });
            }
            let weight = u32::try_from(i + 1).unwrap_or(u32::MAX);
            let mut targets = Vec::with_capacity(combo.targets.len());
            for (rank, target) in combo.targets.iter().enumerate() {
                targets.push(resolve_target(
                    target,
                    u32::try_from(rank).unwrap_or(u32::MAX),
                    weight,
                    &catalog,
                    cfg,
                    store,
                    &mut providers,
                )?);
            }
            // The bench resolves on exactly the terms a target does, at load: a
            // pool entry that only failed at request time would cost a round trip
            // per occurrence instead of refusing the file.
            let mut pool = Vec::with_capacity(combo.pool.len());
            for (offset, target) in combo.pool.iter().enumerate() {
                pool.push(resolve_target(
                    target,
                    u32::try_from(targets.len() + offset).unwrap_or(u32::MAX),
                    weight,
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
            if let Some(compression) = combo.compression {
                route.compression = Some(compression.step());
            }
            combos.push(route);
        }

        Ok(Self {
            port: port.unwrap_or(cfg.server.port),
            strategy: combos
                .first()
                .map_or(Strategy::Priority, |c| c.strategy),
            providers,
            combos,
            prices: table,
            public,
        })
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
    #[must_use]
    pub fn model_cards(&self) -> Vec<ModelCard> {
        if self.combos.is_empty() {
            return self
                .providers
                .iter()
                .filter(|p| p.is_dispatchable() && !p.upstream_model.is_empty())
                .map(|p| ModelCard::new(p.id.as_str(), &p.upstream_model))
                .collect();
        }
        self.combos
            .iter()
            .map(|c| ModelCard::new(&c.id, &c.id))
            .collect()
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
    /// Asked for the *input* price, so it probes with exactly one million
    /// prompt tokens and no completion: `ar_tokens::Cost` is defined over usage,
    /// not over a bare rate, and this is the honest way to read a rate out of it
    /// without reaching into the table's private rows. An absent row is
    /// [`ar_tokens::Cost::UNPRICED`], which becomes `None` — not zero.
    fn price_of(&self, provider: &ProviderId, model: &str) -> Option<f64> {
        let cost = self.prices.cost(
            provider.as_str(),
            model,
            NormalizedUsage::new(1_000_000, 0),
        );
        cost.priced.then(|| cost.usd.as_f64())
    }

    /// Reads configuration from the process environment.
    ///
    /// `AR_UPSTREAM_URL` and `AR_UPSTREAM_MODEL` are the only required pair; the
    /// API key is optional because keyless providers exist. When the pair is
    /// missing the config comes back with an empty provider chain, and the
    /// server still boots — `/healthz` and `/metrics` answer,
    /// `/v1/chat/completions` returns 503. A proxy that refuses to start is
    /// harder to diagnose than one that says what is missing.
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
    }

    /// Builds a single-chain config from already-resolved values. The pure half
    /// of [`ServerConfig::from_env`], and the seam the File-mode reader replaced.
    #[must_use]
    #[allow(clippy::too_many_arguments, reason = "one field per env var; a config struct here would be a second source of truth")]
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
    /// fields.
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

    /// A provider's credential is in neither the credential store nor `keys:`.
    ///
    /// A miss, not an empty string. `ProviderConfig::api_key` documents empty as
    /// "keyless provider", and a provider that *has* an entry resolving to
    /// nothing is that — but a name with no entry at all is a hole in the
    /// config, and it used to become an unauthenticated upstream call.
    #[error("provider {provider:?} references key {key:?}, which is in neither the credential store nor `keys:`")]
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
/// is the combo's `Strategy::Weighted` share, which is per-combo and therefore
/// identical for every entry in it.
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
    if let Some(editor) = declared.anonymous_editor.as_deref().filter(|e| !e.is_empty()) {
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

    Ok(Some(OAuthAuth::new(session, access, refresh, declared.expires_at)))
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
            Err(ComboError::UnresolvedKey { provider: provider.to_owned(), key: name.to_owned() })
        },
        |secret| Ok(secret.expose().to_owned()),
    )
}

#[cfg(test)]
mod tests {
    use ar_keys::{CredentialStore, Secret as StoreSecret};
    use ar_route::{QuotaWindow, Strategy};

    use super::{ComboError, ComboTarget, DefaultChain, RouteCombo, ServerConfig, split_target};
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
        let yaml = ANON_YAML.replace("    anonymous_editor: artificial-route", "    anonymous_editor: artificial-route\n    refresh_key: kilocode_refresh");
        let err = ar_config::Config::parse(&yaml, |_| Ok(Some(String::new()))).expect_err("contradictory");
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
        let server = ServerConfig::from_ar_config(&cfg, None, None, false, None).expect("combos build");
        assert_eq!(server.combo_ids(), ["default", "cheap"]);
    }

    #[test]
    fn reads_the_port_from_the_file_config() {
        let cfg = parse(TWO_COMBO_YAML).expect("config parses");
        let server = ServerConfig::from_ar_config(&cfg, None, None, false, None).expect("combos build");
        assert_eq!(server.port, 20128);
    }

    #[test]
    fn honours_a_port_override() {
        let cfg = parse(TWO_COMBO_YAML).expect("config parses");
        let server = ServerConfig::from_ar_config(&cfg, Some(9999), None, false, None).expect("combos build");
        assert_eq!(server.port, 9999);
    }

    #[test]
    fn defaults_to_loopback_only_from_a_file_config() {
        // `ar_config::Server` has no `public` field yet (see the TODO on
        // `with_public`), so the file cannot grant it. The gate therefore
        // defaults to closed whatever the YAML says — which is the direction
        // that fails safe.
        let cfg = parse(TWO_COMBO_YAML).expect("config parses");
        let server = ServerConfig::from_ar_config(&cfg, None, None, false, None).expect("combos build");
        assert!(!server.public);
    }

    #[test]
    fn carries_an_explicit_public_flag() {
        let cfg = parse(TWO_COMBO_YAML).expect("config parses");
        let server = ServerConfig::from_ar_config(&cfg, None, None, true, None).expect("combos build");
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
        let server = ServerConfig::from_ar_config(&cfg, None, None, false, None).expect("combos build");
        let combo = server.combo("default").expect("default combo exists");
        assert_eq!(combo.targets.len(), 2, "both targets are declared in the file");
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
        let server = ServerConfig::from_ar_config(&cfg, None, None, false, None).expect("combos build");
        let cheap = server.combo("cheap").expect("cheap combo exists");
        assert_eq!(server.candidates(Some(cheap)).len(), 1);
    }

    #[test]
    fn carries_the_per_combo_strategy() {
        let cfg = parse(TWO_COMBO_YAML).expect("config parses");
        let server = ServerConfig::from_ar_config(&cfg, None, None, false, None).expect("combos build");
        assert_eq!(server.combo("cheap").map(|c| c.strategy), Some(Strategy::CostOptimized));
    }

    #[test]
    fn takes_the_first_combo_as_the_default_chain() {
        let cfg = parse(TWO_COMBO_YAML).expect("config parses");
        let server = ServerConfig::from_ar_config(&cfg, None, None, false, None).expect("combos build");
        assert!(matches!(
            server.default_combo(),
            DefaultChain::Combo(c) if c.id == "default"
        ));
    }

    #[test]
    fn resolves_a_provider_only_once_across_combos() {
        // `openai` appears in both combos; two dispatch rows would make the
        // executor's `by_id` keep whichever came last.
        let cfg = parse(TWO_COMBO_YAML).expect("config parses");
        let server = ServerConfig::from_ar_config(&cfg, None, None, false, None).expect("combos build");
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
            [("aihorde".to_owned(), "aphrodite/TheDrummer/Cydonia-24B-v4.3".to_owned())]
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
        // `anthropic` is in the registry with a non-OpenAI wire, so this build
        // cannot POST to it. It must not occupy one of three attempt slots.
        let cfg = parse(TWO_COMBO_YAML).expect("config parses");
        let server = ServerConfig::from_ar_config(&cfg, None, None, false, None).expect("combos build");
        let combo = server.combo("default").expect("default combo exists");
        let ids: Vec<String> = server
            .candidates(Some(combo))
            .iter()
            .map(|c| c.provider.as_str().to_owned())
            .collect();
        assert_eq!(ids, ["openai"]);
    }

    #[test]
    fn lists_combo_ids_as_the_routable_models() {
        let cfg = parse(TWO_COMBO_YAML).expect("config parses");
        let server = ServerConfig::from_ar_config(&cfg, None, None, false, None).expect("combos build");
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
        assert_eq!(
            server.candidates(None)[1].input_usd_per_mtok,
            Some(0.59)
        );
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
            store.insert(provider, name, &StoreSecret::new(value.as_bytes().to_vec())).expect("insert");
        }
        store
    }

    /// The key `TWO_COMBO_YAML`'s `openai` provider resolves to.
    fn openai_key(server: &ServerConfig) -> &str {
        server.providers.iter().find(|p| p.id.as_str() == "openai").expect("openai row").api_key.as_str()
    }

    #[test]
    fn prefers_the_store_when_a_name_is_in_both_places() {
        // The order F-CRIT-2 asks for. A store that lost to `$VAR` would make the
        // encrypted copy decorative.
        let cfg = parse(TWO_COMBO_YAML).expect("config parses");
        let store = store_with(&[("openai", "openai", "sk-from-store")]);
        let server =
            ServerConfig::from_ar_config(&cfg, None, None, false, Some(&store)).expect("combos build");
        assert_eq!(openai_key(&server), "sk-from-store");
    }

    #[test]
    fn falls_back_to_the_var_when_the_store_has_no_row() {
        let cfg = parse(TWO_COMBO_YAML).expect("config parses");
        let store = store_with(&[("groq", "groq", "sk-unrelated")]);
        let server =
            ServerConfig::from_ar_config(&cfg, None, None, false, Some(&store)).expect("combos build");
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
        let server = ServerConfig::from_ar_config(&cfg, None, None, false, None).expect("combos build");
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
        let path = std::env::temp_dir().join(format!("ar-server-unreadable-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let writer =
                CredentialStore::open_with_material(&path, &StoreSecret::generate()).expect("open");
            writer.insert("openai", "openai", &StoreSecret::new(b"sk-stored".to_vec())).expect("insert");
        }
        // Same file, another install's master: the row is there and unreadable.
        let store = CredentialStore::open_with_material(&path, &StoreSecret::generate()).expect("reopen");
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
        assert_eq!(server.combo("default").expect("the combo").targets[0].provider.as_str(), "local-gateway");
    }

    #[test]
    fn carries_a_custom_providers_base_url_when_it_dispatches() {
        let cfg = parse(CUSTOM_YAML).expect("config parses");
        let server =
            ServerConfig::from_ar_config(&cfg, None, None, false, None).expect("the node merges");
        assert_eq!(server.providers[0].base_url, "https://api.example.invalid/v1");
    }

    #[test]
    fn carries_a_custom_providers_headers_when_it_dispatches() {
        let cfg = parse(CUSTOM_YAML).expect("config parses");
        let server =
            ServerConfig::from_ar_config(&cfg, None, None, false, None).expect("the node merges");
        assert_eq!(server.providers[0].headers.get("x-api-key").map(String::as_str), Some("hdr"));
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
            Err(ComboError::CustomProvider(ar_registry::MergeError::Collides { .. }))
        ));
    }

    #[test]
    fn drops_a_custom_provider_from_candidates_when_it_is_not_openai_compatible() {
        // `ar-exec` refuses an Anthropic body on the wire, so listing it as a
        // candidate would burn an attempt slot on a request guaranteed to fail.
        let yaml = CUSTOM_YAML.replace("openai-compatible", "anthropic-compatible");
        let cfg = parse(&yaml).expect("config parses");
        let server =
            ServerConfig::from_ar_config(&cfg, None, None, false, None).expect("the node merges");
        assert!(server.candidates(None).is_empty());
    }
}
