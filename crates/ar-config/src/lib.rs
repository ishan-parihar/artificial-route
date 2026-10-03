//! File-mode configuration.
//!
//! P0 is `File` mode only, mirroring `agentgateway`'s `ConfigStoreMode::File`:
//! one YAML file is the whole source of truth, and there is no UI-managed
//! overlay or control plane (docs/00 rules out xDS/HBONE/SPIFFE for P0).
//!
//! Load order matters and matches agentgateway: `$VAR` expansion happens on the
//! raw text *before* YAML parsing, so a secret can live in the environment and
//! never in the file.
//!
//! ```
//! use ar_config::Config;
//!
//! let yaml = "keys:\n  openai: sk-test\nproviders:\n  - id: openai\n    key: openai\n";
//! let cfg = Config::parse(yaml, |name| Ok(Some(format!("<{name}>")))).unwrap();
//! assert_eq!(cfg.providers.len(), 1);
//! ```

#![deny(missing_docs)]

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use ar_compress::{Engine, Intensity, Step};
use ar_registry::CustomProvider;
use notify_debouncer_full::notify::{self, RecommendedWatcher, RecursiveMode, Watcher};
use notify_debouncer_full::{DebouncedEvent, Debouncer, FileIdMap, new_debouncer};
use serde::{Deserialize, Serialize};
use tracing::{error, warn};

/// Editors and `sed -i` write a temp file then rename over the target, so the
/// config file's inode changes. A 500ms window collapses the write/rename/chmod
/// storm that produces into one reload.
const RELOAD_DEBOUNCE: Duration = Duration::from_millis(500);

/// Ways loading or reloading a config can fail.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file could not be read.
    #[error("cannot read {path}: {source}")]
    Read {
        /// Path that failed to read.
        path: PathBuf,
        /// Underlying I/O failure.
        source: std::io::Error,
    },
    /// A `$VAR` reference could not be resolved.
    #[error("cannot expand $VAR: {cause}")]
    Expand {
        /// Human-readable reason, taken from the expander.
        cause: String,
    },
    /// The expanded text is not valid YAML, or does not match the schema.
    #[error("invalid config YAML: {source}")]
    Yaml {
        /// Underlying parse failure, including line and column.
        source: serde_yaml::Error,
    },
    /// A provider referenced a key name that is not declared under `keys:`.
    #[error("provider {provider:?} references undeclared key {key:?}")]
    UnknownKey {
        /// The provider that holds the dangling reference.
        provider: String,
        /// The key name it referenced.
        key: String,
    },
    /// An OAuth placement URL is not a shape the login flow can use.
    #[error("oauth {provider:?} {field} must be {expectation}, got {url:?}")]
    BadOAuthUrl {
        /// Provider id whose session holds the offending URL.
        provider: String,
        /// Which of the two URL fields was rejected.
        field: &'static str,
        /// The shape that field has to have, spelled for the operator.
        expectation: &'static str,
        /// The value as written, verbatim.
        url: String,
    },
    /// The filesystem watcher could not be installed.
    #[error("cannot watch {path}: {cause}")]
    Watch {
        /// Directory or file that could not be watched.
        path: PathBuf,
        /// Underlying watcher failure.
        cause: String,
    },
    /// An OAuth session declares two things at once that cannot both be true.
    ///
    /// A load error rather than a runtime `if`, because each of these reads as a
    /// working config in YAML and behaves as something else on the wire: a
    /// session that both refreshes and dispatches anonymously sends a free-tier
    /// request while holding a refresh token it never uses.
    #[error("oauth {provider:?}: {reason}")]
    ContradictoryOAuth {
        /// Provider id whose session holds the contradiction.
        provider: String,
        /// The two fields that disagree, spelled for the operator.
        reason: &'static str,
    },
}

/// Ways a `compression:` block can be wrong.
///
/// Every arm is a *load* error rather than a silent fallback, because a combo
/// whose engine spelling is wrong would run nothing and report
/// `x-ar-compression: default;engines=-` — which reads on the client exactly
/// like "this combo is not configured to compress".
#[derive(Debug, thiserror::Error)]
pub enum CompressionError {
    /// The engine id is not in the catalog.
    #[error("compression.engine {id:?} is not one of lite, rtk, caveman")]
    UnknownEngine {
        /// The engine id as written.
        id: String,
    },
    /// The engine has one behaviour, so an intensity on it says nothing.
    #[error("compression.engine {engine} has no intensity dial; drop the `intensity` line")]
    NoDial {
        /// The engine that takes no level.
        engine: &'static str,
    },
    /// The engine has a dial, but not at this rung.
    #[error("compression.engine {engine} takes intensity {levels}, not {id:?}")]
    UnknownIntensity {
        /// The engine the level was paired with.
        engine: &'static str,
        /// The levels that engine does offer, weakest first.
        levels: String,
        /// The level as written.
        id: String,
    },
}

/// A credential, kept out of `Debug`/`Display` output.
///
/// A proxy holds live secrets; a stray `{:?}` in a log line or a TOON row would
/// leak them. `Display` is redacted too, so an accidental `{}` is safe as well.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    /// Wraps `raw` as a secret.
    pub fn new(raw: &str) -> Self {
        Self(raw.to_string())
    }

    /// Returns the underlying secret. Named loudly so every call site reads as
    /// a decision rather than an accident.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(***)")
    }
}

impl std::fmt::Display for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("***")
    }
}

/// Routing strategy for a combo.
///
/// Every name `ar-route` can name is nameable here, plus two escape hatches:
/// [`Strategy::Auto`] for the `auto/*` virtual factory and [`Strategy::Deferred`]
/// for a name this build does not know. Both carry the spelling, so a config
/// written against a newer build round-trips instead of failing to parse — a
/// renamed strategy must not make an otherwise-valid file unloadable.
///
/// This type is the *config* vocabulary; `ar-route::Strategy` is the
/// *routing* one and the two tables must agree. Adding a variant here is a
/// one-line change in [`Strategy::parse`] and one in [`Strategy::as_str`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum Strategy {
    // ---- lean-routing (P0) ----
    /// First-target ordered list; drain each before the next.
    Priority,
    /// Cycle through targets in order.
    RoundRobin,
    /// Minimize cost per request from the registry price table.
    CostOptimized,
    /// Pin to the last successful provider, then fall back to the rules.
    Lkgp,

    // ---- load-shaped (`targetSorters.ts`) ----
    /// Roulette-wheel draw over each target's weight.
    Weighted,
    /// Keep the configured order and drain the head.
    FillFirst,
    /// Power of two choices: draw two distinct, take the quieter.
    P2c,
    /// Fewest requests served wins, stable on ties.
    LeastUsed,
    /// Uniform draw.
    Random,
    /// Uniform draw over distinct providers.
    StrictRandom,

    // ---- quota-shaped (`quotaStrategies` / `headroomRanking`) ----
    /// Most free fraction of its window wins.
    Headroom,
    /// Soonest window rollover wins.
    ResetWindow,
    /// Most free fraction, discounted as a pool nears exhaustion.
    ResetAware,
    /// Reset-aware score, load-weighted by live in-flight.
    QuotaWeighted,
    /// Deficit-round-robin order by normalised weight.
    QuotaShareFair,

    // ---- context-shaped (`promptCacheAffinity` / `sortTargetsByContextSize`) ----
    /// Pure prefix pin: the HRW leader for this conversation.
    ContextRelay,
    /// Largest context window wins.
    ContextOptimized,
    /// Most already-cached prefix tokens, affinity breaking ties.
    CacheOptimized,

    // ---- panel-shaped (`dispatchPrelude`) ----
    /// Panel leader, among the targets the pre-dispatch expansion kept.
    Fusion,
    /// First stage of a chain, among the same kept targets.
    Pipeline,

    /// An `auto/*` virtual-factory spelling, carried verbatim.
    ///
    /// Sixteen weighted factors pick the live strategy per request
    /// (`autoCombo/scoring.ts` + `virtualFactory.ts`); `ar-route` resolves the
    /// name at request time, so the config only has to name the family.
    Auto(String),
    /// A strategy spelling this build does not recognise.
    ///
    /// A typo and an unimplemented feature land here alike, and both parse: a
    /// config naming one degrades to a `501` naming the strategy, never a load
    /// failure that takes the whole proxy down.
    Deferred(String),
}

impl Strategy {
    /// Parses the `config.yaml` spelling.
    ///
    /// Unknown names become [`Strategy::Deferred`] rather than an error, and
    /// anything starting with `auto` becomes [`Strategy::Auto`]. `Copy` is gone
    /// because those two carry the spelling.
    #[must_use]
    pub fn parse(name: &str) -> Self {
        match name {
            "priority" => Self::Priority,
            "round-robin" => Self::RoundRobin,
            "cost-optimized" => Self::CostOptimized,
            "lkgp" => Self::Lkgp,
            "weighted" => Self::Weighted,
            "fill-first" => Self::FillFirst,
            "p2c" => Self::P2c,
            "least-used" => Self::LeastUsed,
            "random" => Self::Random,
            "strict-random" => Self::StrictRandom,
            "headroom" => Self::Headroom,
            "reset-window" => Self::ResetWindow,
            "reset-aware" => Self::ResetAware,
            "quota-weighted" => Self::QuotaWeighted,
            "quota-share-fair" => Self::QuotaShareFair,
            "context-relay" => Self::ContextRelay,
            "context-optimized" => Self::ContextOptimized,
            "cache-optimized" => Self::CacheOptimized,
            "fusion" => Self::Fusion,
            "pipeline" => Self::Pipeline,
            // `auto` itself is the bare form the virtual factory accepts; the
            // explicit `auto/*` names are per-family selectors.
            other if other == "auto" || other.starts_with("auto/") => Self::Auto(other.to_owned()),
            other => Self::Deferred(other.to_owned()),
        }
    }

    /// The spelling used in `config.yaml` and in TOON output.
    ///
    /// Borrows rather than returning `&'static str`: the two escape-hatch
    /// variants own their spelling. Serialize/Deserialize go through
    /// [`Strategy::parse`] and this method, so a config cannot be written in one
    /// spelling and read back as another.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Priority => "priority",
            Self::RoundRobin => "round-robin",
            Self::CostOptimized => "cost-optimized",
            Self::Lkgp => "lkgp",
            Self::Weighted => "weighted",
            Self::FillFirst => "fill-first",
            Self::P2c => "p2c",
            Self::LeastUsed => "least-used",
            Self::Random => "random",
            Self::StrictRandom => "strict-random",
            Self::Headroom => "headroom",
            Self::ResetWindow => "reset-window",
            Self::ResetAware => "reset-aware",
            Self::QuotaWeighted => "quota-weighted",
            Self::QuotaShareFair => "quota-share-fair",
            Self::ContextRelay => "context-relay",
            Self::ContextOptimized => "context-optimized",
            Self::CacheOptimized => "cache-optimized",
            Self::Fusion => "fusion",
            Self::Pipeline => "pipeline",
            Self::Auto(name) | Self::Deferred(name) => name,
        }
    }

    /// Whether this build can route with the named strategy.
    ///
    /// `false` for both escape hatches: `Auto` resolves per request and
    /// `Deferred` does not resolve at all, so neither is a strategy a caller can
    /// predict a verdict from.
    #[must_use]
    pub fn is_routable(&self) -> bool {
        !matches!(self, Self::Auto(_) | Self::Deferred(_))
    }
}

impl From<String> for Strategy {
    fn from(name: String) -> Self {
        Self::parse(&name)
    }
}

impl From<Strategy> for String {
    fn from(strategy: Strategy) -> Self {
        strategy.as_str().to_owned()
    }
}

/// Listen address for the `serve` command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Server {
    /// Interface to bind. Loopback by default: this is a local proxy.
    pub host: String,
    /// TCP port to bind.
    pub port: u16,
}

impl Default for Server {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 20128,
        }
    }
}

/// One configured provider instance.
///
/// `key` is a *reference* into the top-level `keys:` map, not a literal. That
/// indirection is what lets several providers share one credential, and it
/// keeps the secret in the environment rather than in this list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCfg {
    /// Provider id; must exist in the `ar-registry` catalog.
    pub id: String,
    /// Name of the entry in `keys:` holding this provider's credential.
    pub key: String,
}

/// One `targets:` entry as written: a bare `provider/model` string, or the
/// same string with a `weight:`.
///
/// The string form is every config written before this field existed, so it has
/// to keep parsing byte for byte. The map form is the only way an operator can
/// say "this one gets more than its share" — the reference carries a `weight`
/// on every combo step (`src/lib/combos/steps.ts:13-56`) and this schema had no
/// way to spell it.
///
/// `untagged` rather than a two-variant enum the caller matches on: there is
/// nothing to match *on*, the two forms collapse into the same `TargetLoad`
/// either way, and an untagged enum is the only spelling serde_yaml accepts for
/// "either a string or this map".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
enum TargetEntry {
    /// `openai/gpt-5.4` — no opinion about share.
    Bare(String),
    /// `{ target: openai/gpt-5.4, weight: 3 }`.
    Weighted {
        /// The `provider/model` target string.
        target: String,
        /// `Strategy::Weighted` share for this target.
        weight: u32,
    },
}

/// A named chain of `provider/model` targets plus the strategy that walks it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Combo {
    /// Combo id; the `model` a client asks for resolves to this.
    pub id: String,
    /// How targets are selected.
    pub strategy: Strategy,
    /// `provider/model` targets, in strategy order.
    pub targets: Vec<String>,
    /// Explicit `weight:` per target string, from the map form of a
    /// `targets:` entry.
    ///
    /// Keyed by the target string rather than held in a parallel vector: a combo
    /// that names one `provider/model` twice is already a no-op duplicate, so
    /// the two entries could not meaningfully carry different shares, and a map
    /// cannot drift out of step with [`Self::targets`] the way a second vector
    /// would. Absent entry = the target said nothing, which is every config
    /// written before this field existed.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub weights: BTreeMap<String, u32>,
    /// `provider/model` candidates that feed failover but never win the pick.
    ///
    /// The bench, not a second routing table: entries are appended *after* the
    /// target chain, so a strategy still scores only `targets` and a pool entry
    /// cannot change which provider serves a healthy request. It widens what
    /// happens once the targets refuse, which is the whole of audit F-HIGH-2 —
    /// the live `free-stack` combo lists 2 targets against 7 candidates.
    ///
    /// Absent or empty means no bench, which is every config written before this
    /// field existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pool: Vec<String>,
    /// The engine that compresses this combo's prompts.
    ///
    /// Absent means off, which is what omission means everywhere else in this
    /// schema: no `compression:` block, no compression. A request's
    /// `x-ar-compression` header still wins over it.
    #[serde(default)]
    pub compression: Option<Compression>,
    /// The `fusion` judge: a `provider/model` whose provider synthesizes the
    /// panel into one answer (`fusion.ts::handleFusionChat`'s `judgeModel`).
    ///
    /// Absent means no synthesis, which is the reference's own default: the
    /// panel answers and the first 2xx is returned. Naming a judge turns the
    /// second dispatch on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judge_model: Option<String>,
}

impl<'de> Deserialize<'de> for Combo {
    /// Reads the string-or-map `targets:` form, then folds the map entries'
    /// weights into [`Combo::weights`].
    ///
    /// Hand-written because the weight lives in a *sibling* field, and serde has
    /// no way to let a field deserializer populate another one. The wire struct
    /// carries both spellings of the weight so a config round-trips: a bare
    /// target on the way out, a `weights:` map beside it, and a target written
    /// either way on the way back in.
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Wire {
            id: String,
            strategy: Strategy,
            targets: Vec<TargetEntry>,
            #[serde(default)]
            weights: BTreeMap<String, u32>,
            #[serde(default)]
            pool: Vec<String>,
            #[serde(default)]
            compression: Option<Compression>,
            #[serde(default)]
            judge_model: Option<String>,
        }

        let wire = Wire::deserialize(d)?;
        let mut weights = wire.weights;
        let targets = wire
            .targets
            .into_iter()
            .map(|entry| match entry {
                TargetEntry::Bare(target) => target,
                TargetEntry::Weighted { target, weight } => {
                    weights.entry(target.clone()).or_insert(weight);
                    target
                }
            })
            .collect();
        Ok(Self {
            id: wire.id,
            strategy: wire.strategy,
            targets,
            weights,
            pool: wire.pool,
            compression: wire.compression,
            judge_model: wire.judge_model,
        })
    }
}

impl Combo {
    /// The `Strategy::Weighted` share declared for `target`, or `1` when the
    /// config named none.
    ///
    /// `1` is what every target used to get, so an unweighted config draws
    /// exactly as it did before: a uniform share is uniform whatever constant it
    /// is, and `ar-server` used to synthesise a per-combo index for exactly this
    /// field, which cancelled in the roulette wheel and did nothing else.
    /// Clamped to `1` because `Candidate::with_weight` treats `0` as `1` anyway;
    /// doing it here means the number a caller reads back is the number that
    /// routes.
    #[must_use]
    pub fn weight_of(&self, target: &str) -> u32 {
        self.weights.get(target).copied().unwrap_or(1).max(1)
    }
}

/// A combo's `compression:` block: which engine, and how hard.
///
/// The engine id and the level both resolve through `ar-compress`, which owns
/// the catalog, so this struct keeps no list of its own to drift — the defect
/// the audit finds in the reference, whose four engine catalogs disagree. The
/// parse is strict on purpose: a wrong spelling is a load error naming the fix,
/// never a silent "nothing ran", which reads on the client exactly like a combo
/// that was never configured to compress.
///
/// ```
/// use ar_config::Config;
/// use ar_compress::{Engine, Intensity};
///
/// let yaml = concat!(
///     "keys:\n  k: v\ncombos:\n  - id: c\n    strategy: priority\n",
///     "    targets: [openai/gpt-5.4]\n    compression: { engine: rtk, intensity: aggressive }\n",
/// );
/// let cfg = Config::parse(yaml, |_| Ok(Some("v".to_owned()))).unwrap();
/// let want = (Engine::Rtk, Some(Intensity::Aggressive));
/// assert_eq!(
///     cfg.combos[0].compression.map(|c| (c.engine, c.level)),
///     Some(want),
/// );
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawCompression", into = "RawCompression")]
pub struct Compression {
    /// The engine that runs.
    pub engine: Engine,
    /// How hard it works. `None` means the engine's own default.
    pub level: Option<Intensity>,
}

impl Compression {
    /// The plan step this setting resolves to.
    ///
    /// No level means the engine's own middle rung, so `engine: lite` alone
    /// compresses exactly as it did before the dial existed.
    #[must_use]
    pub fn step(self) -> Step {
        match self.level {
            Some(level) => Step::at(self.engine, level),
            None => Step::new(self.engine),
        }
    }
}

/// The YAML shape, kept as strings so the error can say *which* spelling is
/// wrong instead of serde reporting a missing enum variant.
#[derive(Deserialize, Serialize)]
struct RawCompression {
    engine: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    intensity: Option<String>,
}

impl TryFrom<RawCompression> for Compression {
    type Error = CompressionError;

    fn try_from(raw: RawCompression) -> Result<Self, Self::Error> {
        let engine = Engine::from_id(&raw.engine).ok_or_else(|| CompressionError::UnknownEngine {
            id: raw.engine.clone(),
        })?;
        let level = match raw.intensity {
            None => None,
            // A no-dial engine with a level is rejected rather than ignored: the
            // reference accepts `codex-responses`/`omniglyph` modes here and
            // drops them, which is how an operator ends up believing a dial is
            // engaged when nothing reads it.
            Some(_) if engine.levels().is_empty() => {
                return Err(CompressionError::NoDial {
                    engine: engine.as_str(),
                });
            }
            Some(id) => {
                let level = Intensity::from_id(&id).ok_or_else(|| {
                    CompressionError::UnknownIntensity {
                        engine: engine.as_str(),
                        levels: ladder(engine),
                        id: id.clone(),
                    }
                })?;
                if !engine.levels().contains(&level) {
                    return Err(CompressionError::UnknownIntensity {
                        engine: engine.as_str(),
                        levels: ladder(engine),
                        id,
                    });
                }
                Some(level)
            }
        };
        Ok(Self { engine, level })
    }
}

impl From<Compression> for RawCompression {
    fn from(c: Compression) -> Self {
        Self {
            engine: c.engine.as_str().to_owned(),
            intensity: c.level.map(|l| l.as_str().to_owned()),
        }
    }
}

/// The levels an engine offers, for an error message.
fn ladder(engine: Engine) -> String {
    engine
        .levels()
        .iter()
        .map(|l| l.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// One OAuth session's *placement*, never its secrets.
///
/// AUDIT-REPORT F-CRIT-1's dispatch half. This block says **where** an OAuth
/// session's tokens live and how to renew them; it holds no token, so a
/// committed `config.yaml` cannot leak a credential through it — the same
/// property `keys:` has, and the reason this is not a second `Secret` field.
///
/// * The **access** token is the provider's existing `keys:` entry, i.e.
///   `providers[].key`. No new name is invented for it.
/// * The **refresh** token is a second row, named by [`Self::refresh_key`]. Two
///   rows rather than one JSON blob because `ar_keys::CredentialStore` is
///   name-keyed with a `list_names()` an operator — and `ar doctor` — can read
///   without decrypting anything.
/// * `token_url` is operator-supplied. AGENTS.md forbids inventing provider wire
///   formats, and a refresh endpoint guessed from a provider id is exactly that
///   invention, so this build refuses to refresh rather than guess.
/// * `authorization_url` is the *other* half of the same rule for browser
///   login: without it there is no URL to open, so `ar` login reports
///   `NoAuthorizationUrl` rather than guessing an authorize endpoint. Still
///   operator-supplied, never a per-provider default.
/// * `redirect_uri` is only carried here. Absent means the login flow builds a
///   loopback default of its own (the CLI's listener picks a free port); a
///   plain-http *remote* callback is refused, because an authorization code
///   posted over cleartext to anything but localhost is a code leak.
/// * `client_secret_key` names a *third* credential row — a static
///   `client_secret`, for the providers that are confidential rather than
///   public. `None` is the common case and means PKCE-only. Like every other
///   credential reference here it is a name; the value lives in the store.
///
/// ```
/// use ar_config::Config;
///
/// let yaml = concat!(
///     "keys:\n  codex: $CODEX_ACCESS\nproviders:\n  - id: codex\n    key: codex\n",
///     "oauth:\n  - provider: codex\n    refresh_key: codex_refresh\n",
///     "    token_url: https://auth.example/token\n",
///     "    authorization_url: https://auth.example/authorize\n",
///     "    redirect_uri: http://127.0.0.1:1455/callback\n",
///     "    client_secret_key: codex_client_secret\n",
/// );
/// let cfg = Config::parse(yaml, |name| Ok(Some(format!("synthetic-{name}")))).unwrap();
/// let session = cfg.oauth_for("codex").expect("the session");
/// assert_eq!(session.refresh_key.as_deref(), Some("codex_refresh"));
/// assert_eq!(
///     session.client_secret_key.as_deref(),
///     Some("codex_client_secret"),
/// );
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthSession {
    /// Registry provider id this session authenticates. One session per id: a
    /// rotating multi-account setup is several ids, because `ar_route`'s router
    /// addresses a connection by provider id and an id-addressed list is the
    /// only shape it can already route to.
    pub provider: String,
    /// Credential row holding the refresh token.
    ///
    /// `None` (or a row that resolves to nothing) means the session can be
    /// *used* but not renewed, so its first upstream 401 is terminal. `ar
    /// doctor` reports that rather than letting the 401 discover it.
    #[serde(default)]
    pub refresh_key: Option<String>,
    /// Refresh endpoint. Without one the session cannot renew.
    #[serde(default)]
    pub token_url: Option<String>,
    /// Browser-facing authorization endpoint, for a login flow (RFC 6749 §3.1).
    ///
    /// Operator-supplied for the same reason as [`Self::token_url`]: an authorize
    /// endpoint guessed from a provider id is an invented wire format. A session
    /// with none still dispatches with a key it was given; it just cannot be
    /// logged into, and `ar auth login` names the missing field instead of
    /// inventing an endpoint.
    #[serde(default)]
    pub authorization_url: Option<String>,
    /// Redirect the provider lands the person on after they authorize.
    ///
    /// A loopback URI for a same-machine CLI login, and any URI the provider has
    /// registered for a remote one. Paired with [`Self::authorization_url`]: a
    /// login cannot start without both, because the provider refuses a redirect
    /// it does not recognise. Never carries a secret — a redirect is public by
    /// construction, which is what lets the URL travel to another device.
    #[serde(default)]
    pub redirect_uri: Option<String>,
    /// OAuth client id, when the refresh endpoint requires one (RFC 6749 §6).
    #[serde(default)]
    pub client_id: Option<String>,
    /// Credential row holding a static `client_secret`.
    ///
    /// A *third* row, after the access token and the refresh token, for the
    /// providers that are confidential clients rather than PKCE-only public
    /// ones. `None` — the common case — means public client, no secret. The
    /// secret itself is never in this block; only its name is.
    #[serde(default)]
    pub client_secret_key: Option<String>,
    /// Scope requested on refresh, when the provider scopes its tokens.
    #[serde(default)]
    pub scope: Option<String>,
    /// Unix seconds at which the access token expires, when the operator knows.
    ///
    /// An operator hint, not a claim: it enables *proactive* refresh, and its
    /// absence costs one round trip on the 401 path rather than the session.
    /// Nothing reads a machine's clock into this file.
    #[serde(default)]
    pub expires_at: Option<u64>,
    /// Endpoint that mints a device grant (RFC 8628 §3.1).
    ///
    /// Operator-supplied for the same reason as [`Self::token_url`]. Its
    /// presence — together with [`Self::device_poll_url`] — is what makes this a
    /// *device* session, so `ar auth login` picks its mechanism from the config
    /// rather than from a per-provider table.
    #[serde(default)]
    pub device_auth_url: Option<String>,
    /// Endpoint that exchanges a device grant for tokens (RFC 8628 §3.4).
    ///
    /// Paired with [`Self::device_auth_url`]; [`Config::validate`] refuses one
    /// without the other, since a poll URL with nothing to poll surfaces only as
    /// a login timeout.
    #[serde(default)]
    pub device_poll_url: Option<String>,
    /// Dispatch on the provider's anonymous free tier instead of an account.
    ///
    /// Some gateways serve a free model set to an unauthenticated caller against
    /// a constant credential they name themselves (`Bearer anonymous` for Kilo).
    /// With this set the session needs **no** store rows, because there is no
    /// account to hold one — which is the point, the free tier is the tier an
    /// operator reaches for *because* they have no account.
    ///
    /// Refused at load alongside [`Self::refresh_key`] or an authorization
    /// endpoint; requires [`Self::anonymous_editor`].
    #[serde(default)]
    pub anonymous: bool,
    /// Editor name sent on the gateway's editor header when [`Self::anonymous`].
    ///
    /// Not a secret — it is the product name the upstream logs next to a
    /// free-tier request. Still never printed: a value in a config file is no
    /// licence for a log line, and `ar doctor` names the field, not its contents.
    #[serde(default)]
    pub anonymous_editor: Option<String>,
}

/// The whole P0 configuration document.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// Listen address.
    #[serde(default)]
    pub server: Server,
    /// Declared credentials, keyed by name and `$VAR`-expanded.
    #[serde(default)]
    pub keys: HashMap<String, Secret>,
    /// Configured provider instances.
    #[serde(default)]
    pub providers: Vec<ProviderCfg>,
    /// Providers declared in the file rather than compiled in.
    ///
    /// Merged into the catalog at load, so a new endpoint costs a few lines of
    /// YAML and no rebuild. An `id` that collides with a compiled-in entry is
    /// refused by [`ar_registry::Registry::merge`], which `ar doctor` reports
    /// and `ar serve` refuses.
    #[serde(default)]
    pub custom_providers: Vec<CustomProvider>,
    /// Routing combos.
    #[serde(default)]
    pub combos: Vec<Combo>,
    /// OAuth sessions, matched to providers by id. Holds no secrets — see
    /// [`OAuthSession`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub oauth: Vec<OAuthSession>,
}

impl Config {
    /// Expands `$VAR` in `yaml` using `lookup`, then parses and validates it.
    ///
    /// `lookup` is injected rather than read from the environment directly so
    /// the expansion path is testable without mutating process-global state.
    /// Expansion fails when a referenced variable is unset: a silently empty
    /// credential would go upstream as an unauthenticated request.
    pub fn parse<F>(yaml: &str, mut lookup: F) -> Result<Self, ConfigError>
    where
        F: FnMut(&str) -> Result<Option<String>, String>,
    {
        let expanded = shellexpand::env_with_context(yaml, |name| lookup(name))
            .map_err(|e| ConfigError::Expand { cause: e.to_string() })?;
        let cfg: Self =
            serde_yaml::from_str(&expanded).map_err(|source| ConfigError::Yaml { source })?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// [`Config::parse`] with `$VAR` resolved from the process environment.
    pub fn from_env(yaml: &str) -> Result<Self, ConfigError> {
        Self::parse(yaml, |name| std::env::var(name).map(Some).map_err(|e| e.to_string()))
    }

    /// Rejects dangling key references, which would otherwise surface as an
    /// unauthenticated upstream call at request time.
    fn validate(&self) -> Result<(), ConfigError> {
        for p in &self.providers {
            if !self.keys.contains_key(&p.key) {
                // An anonymous free tier holds no credential, so demanding a name
                // for one would make a `keys:` entry mandatory for exactly the
                // install the mechanism exists to serve: the one with no account.
                if self.oauth.iter().any(|s| s.provider == p.id && s.anonymous) {
                    continue;
                }
                return Err(ConfigError::UnknownKey {
                    provider: p.id.clone(),
                    key: p.key.clone(),
                });
            }
        }
        for c in &self.custom_providers {
            if !self.keys.contains_key(&c.key_ref) {
                return Err(ConfigError::UnknownKey {
                    provider: c.id.clone(),
                    key: c.key_ref.clone(),
                });
            }
        }
        for s in &self.oauth {
            if let Some(url) = &s.authorization_url
                && !http_url_ok(url)
            {
                return Err(ConfigError::BadOAuthUrl {
                    provider: s.provider.clone(),
                    field: "authorization_url",
                    expectation: "an http(s) URL with no whitespace",
                    url: url.clone(),
                });
            }
            if let Some(url) = &s.redirect_uri
                && !redirect_uri_ok(url)
            {
                return Err(ConfigError::BadOAuthUrl {
                    provider: s.provider.clone(),
                    field: "redirect_uri",
                    expectation: "an https URL or a loopback http one",
                    url: url.clone(),
                });
            }
            for (field, url, may_placeholder) in [
                ("device_auth_url", &s.device_auth_url, false),
                ("device_poll_url", &s.device_poll_url, true),
            ] {
                if let Some(url) = url
                    && (!http_url_ok(url) || !device_code_placeholder_ok(url, may_placeholder))
                {
                    return Err(ConfigError::BadOAuthUrl {
                        provider: s.provider.clone(),
                        field,
                        expectation: if may_placeholder {
                            "an http(s) URL with no whitespace, whose only brace group is {code}"
                        } else {
                            "an http(s) URL with no whitespace"
                        },
                        url: url.clone(),
                    });
                }
            }
            // Half a device flow is not a slower device flow: one endpoint with no
            // other is a hole the operator only discovers as a login that waits
            // out its whole timeout for a poll that was never going to happen.
            if s.device_auth_url.is_some() != s.device_poll_url.is_some() {
                return Err(ConfigError::ContradictoryOAuth {
                    provider: s.provider.clone(),
                    reason: "a device login needs both device_auth_url and device_poll_url, or neither",
                });
            }
            if s.anonymous {
                if s.anonymous_editor.as_deref().is_none_or(str::is_empty) {
                    return Err(ConfigError::ContradictoryOAuth {
                        provider: s.provider.clone(),
                        reason: "anonymous: true needs an anonymous_editor name; the gateway rejects the request without that header",
                    });
                }
                // A free-tier request has no account behind it, so every account
                // affordance in the same block is dead weight that reads as live.
                if s.refresh_key.is_some() || s.client_secret_key.is_some() {
                    return Err(ConfigError::ContradictoryOAuth {
                        provider: s.provider.clone(),
                        reason: "anonymous: true dispatches on the free tier and holds no account, so refresh_key/client_secret_key cannot be declared",
                    });
                }
                if s.authorization_url.is_some() {
                    return Err(ConfigError::ContradictoryOAuth {
                        provider: s.provider.clone(),
                        reason: "anonymous: true asks no one for consent, so authorization_url cannot be declared",
                    });
                }
            }
        }
        Ok(())
    }

    /// Looks up a declared credential by name, borrowing the caller's name.
    pub fn key(&self, name: &str) -> Option<&Secret> {
        self.keys.get(name)
    }

    /// The OAuth session declared for `provider`, if any.
    ///
    /// Borrowed, so a caller that only wants to *name* the session in a
    /// diagnostic never touches a token — and there is no token here to touch.
    /// First declaration wins: a config listing one id twice has made a choice a
    /// caller cannot predict, and the earlier row is the one the file reads
    /// top-down.
    pub fn oauth_for(&self, provider: &str) -> Option<&OAuthSession> {
        self.oauth.iter().find(|s| s.provider == provider)
    }

    /// The credential *name* a provider id resolves to, from either provider list.
    ///
    /// A compiled-in provider names its key through `providers:`, a custom node
    /// through `custom_providers:`. One lookup for both is what lets a combo
    /// target resolve its key without caring which list declared the id. The
    /// name, not the [`Secret`]: the encrypted store is keyed by it too, so the
    /// caller resolves it against both sources itself.
    #[must_use]
    pub fn key_name(&self, id: &str) -> Option<&str> {
        self.providers
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.key.as_str())
            .or_else(|| self.custom_providers.iter().find(|c| c.id == id).map(|c| c.key_ref.as_str()))
    }

    /// Whether `id` names a provider this config declares, from either list.
    #[must_use]
    pub fn declares(&self, id: &str) -> bool {
        self.providers.iter().any(|p| p.id == id) || self.custom_providers.iter().any(|c| c.id == id)
    }
}

/// The device-code placeholder a `device_poll_url` may carry: `{code}`.
const DEVICE_POLL_CODE_PLACEHOLDER: &str = "{code}";

/// Whether `url`'s only brace group is [`DEVICE_POLL_CODE_PLACEHOLDER`], or it has none.
///
/// Some providers address a device grant by path (`/poll/{code}`) rather than by
/// body parameter, and which of the two a given provider does is not derivable
/// from its provider id without inventing a wire format — so the substitution is
/// declared here, in the file an operator already edits, and performed by the
/// executor. That makes the typo worth catching at load: a misspelled group would
/// otherwise be polled verbatim for the whole timeout, and the login would report
/// "expired" rather than naming what was wrong with the config.
///
/// `may_placeholder` is false for `device_auth_url`, which is asked *before* any
/// device code exists — there is nothing there to substitute, so a `{code}` there
/// is a configuration mistake rather than a template.
fn device_code_placeholder_ok(url: &str, may_placeholder: bool) -> bool {
    let mut rest = url;
    let mut seen = false;
    while let Some(open) = rest.find('{') {
        let Some(close) = rest[open..].find('}') else {
            return false;
        };
        if &rest[open..open + close + 1] != DEVICE_POLL_CODE_PLACEHOLDER {
            return false;
        }
        seen = true;
        rest = &rest[open + close + 1..];
    }
    !rest.contains('}') && (may_placeholder || !seen)
}

/// Whether `url` is an `http`/`https` URL with a non-empty authority and no
/// embedded whitespace — the same shape [`ar_registry::CustomProvider`]
/// requires of a base URL, restated because that predicate is a method on a
/// type this crate does not own.
fn http_url_ok(url: &str) -> bool {
    let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
    else {
        return false;
    };
    !rest.is_empty() && !rest.starts_with('/') && !url.chars().any(char::is_whitespace)
}

/// Whether `url` is safe to receive an authorization code on.
///
/// `https` anywhere, `http` only on loopback: a cleartext callback to a remote
/// host hands the code to anyone on the path.
fn redirect_uri_ok(url: &str) -> bool {
    if !http_url_ok(url) {
        return false;
    }
    let Some(rest) = url.strip_prefix("http://") else {
        return true;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = if host.starts_with('[') {
        host.split_once(']').map_or(host, |(h, _)| h)
    } else {
        host.split_once(':').map_or(host, |(h, _)| h)
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// A loaded config plus the ability to swap in a newer one.
///
/// Readers call [`ConfigHandle::snapshot`], a single atomic load that allocates
/// nothing: a request in flight keeps the `Arc` it started with even if a
/// reload lands mid-request.
pub struct ConfigHandle {
    path: PathBuf,
    current: ArcSwap<Config>,
}

impl std::fmt::Debug for ConfigHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigHandle")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl ConfigHandle {
    /// Reads and parses `path`.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref().to_path_buf();
        let cfg = read_and_parse(&path)?;
        Ok(Self {
            path,
            current: ArcSwap::from_pointee(cfg),
        })
    }

    /// The current configuration. Cheap enough to call per request.
    pub fn snapshot(&self) -> Arc<Config> {
        self.current.load_full()
    }

    /// The file this handle was loaded from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Re-reads and swaps in the file.
    ///
    /// On failure the previous snapshot stays in place and the error is
    /// returned: a half-saved config must not take a running proxy down.
    pub fn reload(&self) -> Result<(), ConfigError> {
        let cfg = read_and_parse(&self.path)?;
        self.current.store(Arc::new(cfg));
        Ok(())
    }

    /// Watches the config file's directory and reloads on change.
    ///
    /// Watches the *directory* rather than the file: editors replace the file
    /// via rename, which silently invalidates an inode-bound watch. Returns a
    /// guard whose drop stops the watcher thread.
    pub fn watch(self: &Arc<Self>) -> Result<WatcherGuard, ConfigError> {
        let dir = self.path.parent().unwrap_or_else(|| Path::new(".")).to_path_buf();
        let watched = self.path.clone();

        let handle = Arc::clone(self);
        let mut debouncer = new_debouncer(
            RELOAD_DEBOUNCE,
            None,
            move |result: Result<Vec<DebouncedEvent>, Vec<notify::Error>>| {
                let Ok(events) = result else {
                    warn!("config watcher error; keeping previous snapshot");
                    return;
                };
                if !events.iter().any(|e| e.paths.contains(&watched)) {
                    return;
                }
                match handle.reload() {
                    Ok(()) => warn!(path = %handle.path.display(), "config reloaded"),
                    Err(e) => error!(
                        path = %handle.path.display(), error = %e,
                        "config reload failed; keeping previous snapshot"
                    ),
                }
            },
        )
        .map_err(|e| ConfigError::Watch {
            path: dir.clone(),
            cause: e.to_string(),
        })?;

        debouncer
            .watcher()
            .watch(&dir, RecursiveMode::NonRecursive)
            .map_err(|e| ConfigError::Watch {
                path: dir,
                cause: e.to_string(),
            })?;

        Ok(WatcherGuard { _debouncer: debouncer })
    }
}

/// Keeps a [`ConfigHandle::watch`] watcher alive; dropping it stops the thread.
pub struct WatcherGuard {
    _debouncer: Debouncer<RecommendedWatcher, FileIdMap>,
}

impl std::fmt::Debug for WatcherGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WatcherGuard")
    }
}

fn read_and_parse(path: &Path) -> Result<Config, ConfigError> {
    let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    Config::from_env(&raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
server:
  host: 127.0.0.1
  port: 20128

keys:
  openai: $AR_TEST_OPENAI_KEY
  anthropic: $AR_TEST_ANTHROPIC_KEY

providers:
  - id: openai
    key: openai
  - id: anthropic
    key: anthropic

combos:
  - id: default
    strategy: lkgp
    targets:
      - openai/gpt-5.4
      - anthropic/claude-sonnet-4-5
"#;

    /// One combo carrying a `compression:` block, so a test names only the part
    /// it is about.
    fn combo_yaml(compression: &str) -> String {
        format!(
            "keys:\n  k: v\ncombos:\n  - id: c\n    strategy: priority\n    \
             targets: [openai/gpt-5.4]\n    {compression}\n"
        )
    }

    fn stub_lookup(name: &str) -> Result<Option<String>, String> {
        Ok(Some(format!("secret-for-{name}")))
    }

    /// A file-declared provider, resolved without any network or rebuild.
    const CUSTOM: &str = r#"
keys:
  local: $AR_TEST_LOCAL_KEY

custom_providers:
  - id: local-gateway
    protocol: openai-compatible
    base_url: https://api.example.invalid/v1
    key_ref: local

combos:
  - id: default
    strategy: priority
    targets:
      - local-gateway/some-model
"#;

    #[test]
    fn loads_sample_config_when_file_valid() {
        let cfg = Config::parse(SAMPLE, stub_lookup).unwrap();
        assert_eq!(cfg.combos.len(), 1);
    }

    #[test]
    fn expands_dollar_var_before_parsing() {
        let cfg = Config::parse(SAMPLE, stub_lookup).unwrap();
        assert_eq!(
            cfg.key("openai").map(Secret::expose),
            Some("secret-for-AR_TEST_OPENAI_KEY")
        );
    }

    #[test]
    fn rejects_provider_when_key_undeclared() {
        let yaml = "providers:\n  - id: openai\n    key: nope\n";
        assert!(matches!(
            Config::parse(yaml, stub_lookup),
            Err(ConfigError::UnknownKey { .. })
        ));
    }

    #[test]
    fn keeps_pool_as_a_separate_bench_when_declared() {
        // The whole F-HIGH-2 shape: 2 targets, more candidates. Targets and pool
        // are read as different lists, never merged.
        let yaml = "keys:\n  k: v\ncombos:\n  - id: c\n    strategy: priority\n    targets: [openai/gpt-5.4, openai/gpt-5.4-nano]\n    pool: [groq/llama-3.3-70b]\n";
        let cfg = Config::parse(yaml, stub_lookup).unwrap();
        assert_eq!(cfg.combos[0].pool, ["groq/llama-3.3-70b"]);
    }

    #[test]
    fn defaults_pool_to_empty_when_absent() {
        // Every config written before the field existed must load unchanged.
        assert!(Config::parse(SAMPLE, stub_lookup).unwrap().combos[0].pool.is_empty());
    }

    #[test]
    fn redacts_secret_when_debug_formatted() {
        assert_eq!(format!("{:?}", Secret::new("sk-live-1234")), "Secret(***)");
    }

    /// Every name `ar-route::Strategy::parse` claims, so a rename there fails
    /// here instead of silently routing under [`Strategy::Deferred`].
    const ROUTE_STRATEGIES: [&str; 20] = [
        "priority", "round-robin", "cost-optimized", "lkgp", "weighted", "fill-first", "p2c",
        "least-used", "random", "strict-random", "headroom", "reset-window", "reset-aware",
        "quota-weighted", "quota-share-fair", "context-relay", "context-optimized",
        "cache-optimized", "fusion", "pipeline",
    ];

    #[test]
    fn parses_every_route_strategy_name() {
        for name in ROUTE_STRATEGIES {
            assert!(Strategy::parse(name).is_routable(), "{name}");
        }
        assert_eq!(ROUTE_STRATEGIES.len(), 20);
    }

    #[test]
    fn parses_auto_family_as_auto() {
        assert_eq!(Strategy::parse("auto"), Strategy::Auto("auto".to_owned()));
        assert_eq!(Strategy::parse("auto/quality"), Strategy::Auto("auto/quality".to_owned()));
    }

    #[test]
    fn parses_unknown_name_as_deferred_rather_than_failing() {
        assert_eq!(Strategy::parse("totally-new"), Strategy::Deferred("totally-new".to_owned()));
        assert!(!Strategy::parse("totally-new").is_routable());
    }

    #[test]
    fn round_trips_original_four_names_byte_identically() {
        for name in ["priority", "round-robin", "cost-optimized", "lkgp"] {
            let yaml = format!(
                "keys:\n  k: v\ncombos:\n  - id: c\n    strategy: {name}\n    targets: [openai/gpt-5.4]\n"
            );
            let cfg = Config::parse(&yaml, |_| Ok(Some("v".to_owned()))).unwrap();
            let out = serde_yaml::to_string(&cfg).unwrap();
            assert!(out.contains(&format!("strategy: {name}")), "{out}");
        }
    }

    #[test]
    fn round_trips_carried_spelling_for_auto_and_deferred() {
        for name in ["auto/quality", "brand-new-strategy"] {
            let yaml = format!(
                "keys:\n  k: v\ncombos:\n  - id: c\n    strategy: {name}\n    targets: [openai/gpt-5.4]\n"
            );
            let cfg = Config::parse(&yaml, |_| Ok(Some("v".to_owned()))).unwrap();
            let out = serde_yaml::to_string(&cfg).unwrap();
            assert!(out.contains(&format!("strategy: {name}")), "{out}");
        }
    }

    #[test]
    fn loads_combo_when_strategy_is_new_name() {
        let yaml = "keys:\n  k: v\ncombos:\n  - id: c\n    strategy: quota-share-fair\n    targets: [openai/gpt-5.4]\n";
        let cfg = Config::parse(yaml, |_| Ok(Some("v".to_owned()))).unwrap();
        assert_eq!(cfg.combos[0].strategy, Strategy::QuotaShareFair);
    }

    #[test]
    fn loads_quota_weighted_as_its_own_strategy_rather_than_degrading() {
        // The regression this pins: `ar-route` has carried `quota-weighted` since
        // it landed and this table did not, so a config naming it parsed fine and
        // then answered every request with a 501 that said "deferred" — the
        // operator's own spelling treated as a typo.
        let yaml = "keys:\n  k: v\ncombos:\n  - id: c\n    strategy: quota-weighted\n    targets: [openai/gpt-5.4]\n";
        let cfg = Config::parse(yaml, stub_lookup).expect("quota-weighted is a strategy this build has");
        assert_eq!(cfg.combos[0].strategy, Strategy::QuotaWeighted);
        assert!(cfg.combos[0].strategy.is_routable());
        assert_eq!(cfg.combos[0].strategy.as_str(), "quota-weighted");
    }

    #[test]
    fn keeps_a_bare_string_target_list_exactly_as_written() {
        // Every config written before the map form existed. No weight is
        // invented for any of them, and the strings come back byte for byte.
        let yaml = "keys:\n  k: v\ncombos:\n  - id: c\n    strategy: weighted\n    targets: [openai/gpt-5.4, groq/llama-3.3-70b]\n";
        let cfg = Config::parse(yaml, stub_lookup).unwrap();
        let combo = &cfg.combos[0];
        assert_eq!(combo.targets, ["openai/gpt-5.4", "groq/llama-3.3-70b"]);
        assert!(combo.weights.is_empty(), "a bare list declares no weights");
        for target in &combo.targets {
            assert_eq!(combo.weight_of(target), 1, "{target} keeps the default share");
        }
    }

    #[test]
    fn reads_a_weight_off_the_map_form_of_a_target() {
        let yaml = concat!(
            "keys:\n  k: v\ncombos:\n  - id: c\n    strategy: weighted\n    targets:\n",
            "      - openai/gpt-5.4\n      - { target: groq/llama-3.3-70b, weight: 7 }\n",
        );
        let cfg = Config::parse(yaml, stub_lookup).unwrap();
        let combo = &cfg.combos[0];
        assert_eq!(combo.targets, ["openai/gpt-5.4", "groq/llama-3.3-70b"]);
        assert_eq!(combo.weight_of("groq/llama-3.3-70b"), 7);
        assert_eq!(combo.weight_of("openai/gpt-5.4"), 1, "the bare one declared nothing");
    }

    #[test]
    fn round_trips_a_declared_target_weight_through_serialisation() {
        // The map form is a schema, not a one-way import: serialising a combo
        // that named a weight and reading it back must not lose it.
        let yaml = concat!(
            "keys:\n  k: v\ncombos:\n  - id: c\n    strategy: weighted\n    targets:\n",
            "      - { target: groq/llama-3.3-70b, weight: 4 }\n",
        );
        let cfg = Config::parse(yaml, stub_lookup).unwrap();
        let out = serde_yaml::to_string(&cfg).expect("serialises");
        let back = Config::parse(&out, stub_lookup).expect("and reads back");
        assert_eq!(back.combos[0].weight_of("groq/llama-3.3-70b"), 4);
    }

    #[test]
    fn clamps_a_zero_weight_to_the_share_the_router_would_see() {
        // `Candidate::with_weight` already treats 0 as 1; doing it here means the
        // number an operator reads back is the number that routes.
        let yaml = concat!(
            "keys:\n  k: v\ncombos:\n  - id: c\n    strategy: weighted\n    targets:\n",
            "      - { target: groq/llama-3.3-70b, weight: 0 }\n",
        );
        let cfg = Config::parse(yaml, stub_lookup).unwrap();
        assert_eq!(cfg.combos[0].weight_of("groq/llama-3.3-70b"), 1);
    }

    const OAUTH_YAML: &str = concat!(
        "keys:\n  codex: $CODEX_ACCESS\nproviders:\n  - id: codex\n    key: codex\n",
        "oauth:\n  - provider: codex\n    refresh_key: codex_refresh\n",
        "    token_url: https://auth.example.invalid/token\n    expires_at: 1800000000\n",
    );

    #[test]
    fn reads_an_oauth_session_by_provider_id() {
        let cfg = Config::parse(OAUTH_YAML, |name| Ok(Some(format!("synthetic-{name}")))).unwrap();
        assert_eq!(cfg.oauth_for("codex").map(|s| s.provider.as_str()), Some("codex"));
    }

    #[test]
    fn reports_no_oauth_session_for_an_undeclared_provider() {
        let cfg = Config::parse(OAUTH_YAML, |name| Ok(Some(format!("synthetic-{name}")))).unwrap();
        assert!(cfg.oauth_for("cline").is_none());
    }

    #[test]
    fn reads_a_token_endpoint_and_an_expiry() {
        let cfg = Config::parse(OAUTH_YAML, |name| Ok(Some(format!("synthetic-{name}")))).unwrap();
        let session = cfg.oauth_for("codex").expect("the session");
        assert_eq!(session.token_url.as_deref(), Some("https://auth.example.invalid/token"));
        assert_eq!(session.expires_at, Some(1_800_000_000));
    }

    /// A browser-login session: every placement key present at once, so one
    /// parse proves the whole block is reachable and still secret-free.
    const BROWSER_LOGIN_YAML: &str = concat!(
        "keys:\n  codex: $CODEX_ACCESS\nproviders:\n  - id: codex\n    key: codex\n",
        "oauth:\n  - provider: codex\n    refresh_key: codex_refresh\n",
        "    token_url: https://auth.example.invalid/token\n",
        "    client_id: public-client\n",
        "    authorization_url: https://auth.example.invalid/authorize\n",
        "    redirect_uri: http://127.0.0.1:1455/callback\n",
        "    client_secret_key: codex_client_secret\n",
        "    scope: openid\n    expires_at: 1800000000\n",
    );

    /// A session with only the refresh half, as every config written before
    /// browser login existed looks.
    const OAUTH_REFRESH_ONLY_YAML: &str = concat!(
        "keys:\n  codex: $CODEX_ACCESS\nproviders:\n  - id: codex\n    key: codex\n",
        "oauth:\n  - provider: codex\n    refresh_key: codex_refresh\n",
        "    token_url: https://auth.example.invalid/token\n",
    );

    fn browser_login_cfg() -> Config {
        Config::parse(BROWSER_LOGIN_YAML, |name| Ok(Some(format!("synthetic-{name}"))))
            .expect("the browser-login block must load")
    }

    #[test]
    fn reads_the_browser_login_placement_keys_when_declared() {
        let session = browser_login_cfg().oauth_for("codex").expect("the session").clone();
        assert_eq!(
            session.authorization_url.as_deref(),
            Some("https://auth.example.invalid/authorize"),
        );
        assert_eq!(
            session.redirect_uri.as_deref(),
            Some("http://127.0.0.1:1455/callback"),
        );
        assert_eq!(
            session.client_secret_key.as_deref(),
            Some("codex_client_secret"),
        );
    }

    #[test]
    fn defaults_the_browser_login_keys_when_absent() {
        let cfg = Config::parse(OAUTH_REFRESH_ONLY_YAML, |name| {
            Ok(Some(format!("synthetic-{name}")))
        })
        .expect("a refresh-only block must still load");
        let session = cfg.oauth_for("codex").expect("the session");
        assert_eq!(session.authorization_url, None);
        assert_eq!(session.redirect_uri, None);
        assert_eq!(session.client_secret_key, None);
    }

    #[test]
    fn rejects_an_authorization_url_when_it_is_not_http() {
        let yaml = BROWSER_LOGIN_YAML.replace(
            "https://auth.example.invalid/authorize",
            "ftp://auth.example.invalid/authorize",
        );
        assert!(matches!(
            Config::parse(&yaml, |name| Ok(Some(format!("synthetic-{name}")))),
            Err(ConfigError::BadOAuthUrl { .. }),
        ));
    }

    #[test]
    fn rejects_an_authorization_url_when_it_carries_whitespace() {
        let yaml = BROWSER_LOGIN_YAML.replace(
            "https://auth.example.invalid/authorize",
            "https://auth.example.invalid/ authorize",
        );
        assert!(matches!(
            Config::parse(&yaml, |name| Ok(Some(format!("synthetic-{name}")))),
            Err(ConfigError::BadOAuthUrl { .. }),
        ));
    }

    #[test]
    fn rejects_a_remote_redirect_uri_when_it_is_plain_http() {
        let yaml = BROWSER_LOGIN_YAML
            .replace("http://127.0.0.1:1455/callback", "http://callbacks.example/callback");
        assert!(matches!(
            Config::parse(&yaml, |name| Ok(Some(format!("synthetic-{name}")))),
            Err(ConfigError::BadOAuthUrl { .. }),
        ));
    }

    #[test]
    fn accepts_a_remote_redirect_uri_when_it_is_https() {
        let yaml = BROWSER_LOGIN_YAML
            .replace("http://127.0.0.1:1455/callback", "https://ar.example/callback");
        assert!(
            Config::parse(&yaml, |name| Ok(Some(format!("synthetic-{name}")))).is_ok(),
            "https needs no loopback exemption"
        );
    }

    #[test]
    fn accepts_a_loopback_redirect_uri_when_it_is_plain_http() {
        let cfg = browser_login_cfg();
        assert_eq!(
            cfg.oauth_for("codex").and_then(|s| s.redirect_uri.as_deref()),
            Some("http://127.0.0.1:1455/callback"),
        );
    }

    #[test]
    fn keeps_the_browser_login_block_free_of_any_credential() {
        // Same property as `stores_no_token_in_the_oauth_block`, widened: a
        // client_secret is a *name* here too, or printing this block leaks.
        let cfg = browser_login_cfg();
        let rendered = serde_yaml::to_string(&cfg.oauth).expect("serialises");
        assert!(!rendered.contains("synthetic-"), "names only: {rendered}");
    }

    #[test]
    fn parses_a_config_with_no_oauth_block() {
        let cfg = Config::parse(SAMPLE, stub_lookup).unwrap();
        assert!(cfg.oauth.is_empty(), "omission is not-oauth, exactly like compression");
    }

    #[test]
    fn stores_no_token_in_the_oauth_block() {
        // The property the block exists for: a committed config.yaml cannot leak
        // a credential through it, because it has no field to put one in.
        let cfg = Config::parse(OAUTH_YAML, |name| Ok(Some(format!("synthetic-{name}")))).unwrap();
        let rendered = serde_yaml::to_string(&cfg.oauth).expect("serialises");
        assert!(!rendered.contains("synthetic-"), "names only: {rendered}");
    }

    #[test]
    fn loads_a_custom_provider_when_declared() {
        let cfg = Config::parse(CUSTOM, stub_lookup).unwrap();
        assert_eq!(cfg.custom_providers[0].id, "local-gateway");
    }

    #[test]
    fn resolves_a_custom_providers_credential_through_its_key_ref() {
        let cfg = Config::parse(CUSTOM, stub_lookup).unwrap();
        assert_eq!(cfg.key_name("local-gateway"), Some("local"));
        assert_eq!(
            cfg.key(cfg.key_name("local-gateway").unwrap_or_default()).map(Secret::expose),
            Some("secret-for-AR_TEST_LOCAL_KEY")
        );
    }

    #[test]
    fn rejects_a_custom_provider_when_its_key_ref_is_undeclared() {
        let yaml = "keys:\n  k: v\ncustom_providers:\n  - id: mine\n    protocol: openai-compatible\n    base_url: https://api.example.invalid/v1\n    key_ref: nope\n";
        assert!(matches!(
            Config::parse(yaml, stub_lookup),
            Err(ConfigError::UnknownKey { .. })
        ));
    }

    #[test]
    fn parses_anthropic_compatible_when_a_custom_provider_declares_it() {
        let yaml = "keys:\n  k: v\ncustom_providers:\n  - id: mine\n    protocol: anthropic-compatible\n    base_url: https://api.example.invalid\n    key_ref: k\n";
        let cfg = Config::parse(yaml, stub_lookup).unwrap();
        assert_eq!(
            cfg.custom_providers[0].protocol,
            ar_registry::Protocol::AnthropicCompatible
        );
    }

    #[test]
    fn parses_optional_headers_when_a_custom_provider_declares_them() {
        let yaml = "keys:\n  k: v\ncustom_providers:\n  - id: mine\n    protocol: openai-compatible\n    base_url: https://api.example.invalid\n    key_ref: k\n    headers:\n      x-api-key: v\n";
        let cfg = Config::parse(yaml, stub_lookup).unwrap();
        assert_eq!(
            cfg.custom_providers[0].headers.get("x-api-key").map(String::as_str),
            Some("v")
        );
    }

    #[test]
    fn round_trips_a_custom_provider_when_serialised_back() {
        let cfg = Config::parse(CUSTOM, stub_lookup).unwrap();
        let out = serde_yaml::to_string(&cfg).unwrap();
        assert!(out.contains("protocol: openai-compatible"), "{out}");
        assert!(out.contains("key_ref: local"), "{out}");
    }

    /// A combo with no `compression:` block is uncompressed, not an error — the
    /// omission has to keep meaning what it meant before the field existed.
    #[test]
    fn reads_a_combo_as_uncompressed_when_no_block_is_present() {
        let cfg = Config::parse(SAMPLE, stub_lookup).unwrap();
        assert_eq!(cfg.combos[0].compression, None);
    }

    #[test]
    fn defaults_the_level_to_the_engine_when_only_the_engine_is_named() {
        let cfg = Config::parse(&combo_yaml("compression: { engine: rtk }"), stub_lookup).unwrap();
        assert_eq!(cfg.combos[0].compression.map(Compression::step), Some(Step::new(Engine::Rtk)));
    }

    /// Every pair the catalog says is legal must load. This is the pin that keeps
    /// `Engine::levels` from naming a level the config parser then refuses.
    #[test]
    fn resolves_every_level_its_own_engine_offers() {
        for engine in [Engine::Lite, Engine::Rtk, Engine::Caveman] {
            for level in engine.levels() {
                let yaml = combo_yaml(&format!(
                    "compression: {{ engine: {}, intensity: {} }}",
                    engine.as_str(),
                    level.as_str()
                ));
                let cfg = Config::parse(&yaml, stub_lookup)
                    .unwrap_or_else(|e| panic!("{engine}@{level} must load: {e}"));
                assert_eq!(
                    cfg.combos[0].compression,
                    Some(Compression { engine, level: Some(*level) }),
                    "{engine}@{level}"
                );
            }
        }
    }

    #[test]
    fn refuses_a_combo_when_the_engine_is_not_in_the_catalog() {
        let err = Config::parse(&combo_yaml("compression: { engine: omniglyph }"), stub_lookup)
            .expect_err("an unwired engine id must not load");
        assert!(matches!(err, ConfigError::Yaml { .. }), "{err}");
    }

    /// The mispair the audit names: the reference accepts a combo override whose
    /// mode the engine does not have and ignores it, so the operator believes a
    /// dial is engaged. Here it is a load error instead.
    #[test]
    fn refuses_a_combo_when_an_intensity_crosses_engines() {
        let err = Config::parse(
            &combo_yaml("compression: { engine: rtk, intensity: ultra }"),
            stub_lookup,
        )
        .expect_err("a caveman level on rtk must not load");
        assert!(matches!(err, ConfigError::Yaml { .. }), "{err}");
    }

    #[test]
    fn refuses_a_combo_when_a_fixed_engine_is_given_an_intensity() {
        assert!(
            Config::parse(
                &combo_yaml("compression: { engine: lite, intensity: standard }"),
                stub_lookup
            )
            .is_err(),
            "lite has no dial, so an intensity on it is a mistake, not a no-op"
        );
    }

    #[test]
    fn round_trips_a_compression_block_byte_identically() {
        let cfg = Config::parse(
            &combo_yaml("compression: { engine: caveman, intensity: lite }"),
            stub_lookup,
        )
        .unwrap();
        let out = serde_yaml::to_string(&cfg).unwrap();
        assert!(out.contains("engine: caveman"), "{out}");
        assert!(out.contains("intensity: lite"), "{out}");
    }

    /// The mirror template is the file most likely to rot: it is copied, not
    /// compiled, so nothing else would notice it naming a level the schema
    /// stopped accepting. `include_str!` keeps the pin honest about the path.
    #[test]
    fn parses_the_omni_mirror_when_read_from_this_repo() {
        let cfg = Config::parse(include_str!("../../../config.omni-mirror.yaml"), stub_lookup)
            .expect("the mirror template must load");
        assert_eq!(cfg.combos.len(), 3);
    }

    /// All three live OmniRoute combos run `compressionMode: lite`. The mirror
    /// has to say so, or "mirrors the live combos 1:1" is not true.
    #[test]
    fn gives_every_mirrored_combo_the_live_lite_engine() {
        let cfg = Config::parse(include_str!("../../../config.omni-mirror.yaml"), stub_lookup)
            .expect("the mirror template must load");
        let engines: Vec<Option<Engine>> = cfg.combos.iter().map(|c| c.compression.map(|k| k.engine)).collect();
        assert_eq!(engines, [Some(Engine::Lite); 3]);
    }

    /// A free-tier block with no `keys:` row, which is the whole point: there is
    /// no account, so there is nothing to declare a name for.
    const ANON: &str = concat!(
        "keys: {}\n",
        "providers:\n  - id: kilocode\n    key: kilocode\n",
        "oauth:\n  - provider: kilocode\n    anonymous: true\n",
        "    anonymous_editor: artificial-route\n",
    );

    fn anon_cfg() -> Config {
        Config::parse(ANON, stub_lookup).expect("the anonymous fixture parses")
    }

    #[test]
    fn defaults_anonymous_to_false_when_the_block_omits_it() {
        let cfg = Config::parse(
            "keys:\n  grok: $G\nproviders:\n  - id: grok-cli\n    key: grok\noauth:\n  - provider: grok-cli\n",
            stub_lookup,
        )
        .expect("parses");
        assert!(!cfg.oauth_for("grok-cli").expect("declared").anonymous);
    }

    #[test]
    fn loads_an_anonymous_session_with_no_key_row_declared() {
        // The mechanism's whole promise: an install with no account needs no
        // `keys:` entry, and requiring one would exclude exactly that install.
        assert!(!anon_cfg().oauth.iter().any(|s| s.refresh_key.is_some()));
    }

    #[test]
    fn refuses_an_anonymous_session_that_declares_a_refresh_row() {
        let yaml = ANON.replace("    anonymous_editor: artificial-route", "    anonymous_editor: artificial-route\n    refresh_key: kilocode_refresh");
        let err = Config::parse(&yaml, stub_lookup).expect_err("no account to refresh");
        assert!(err.to_string().contains("holds no account"), "{err}");
    }

    #[test]
    fn refuses_an_anonymous_session_with_no_editor_name() {
        let yaml = ANON.replace("    anonymous_editor: artificial-route\n", "");
        let err = Config::parse(&yaml, stub_lookup).expect_err("the gateway needs the header");
        assert!(err.to_string().contains("anonymous_editor"), "{err}");
    }

    #[test]
    fn refuses_an_anonymous_session_that_also_declares_an_authorize_endpoint() {
        let yaml = ANON.replace(
            "    anonymous: true\n",
            "    anonymous: true\n    authorization_url: https://auth.example.invalid/authorize\n",
        );
        let err = Config::parse(&yaml, stub_lookup).expect_err("no one consents on the free tier");
        assert!(err.to_string().contains("asks no one for consent"), "{err}");
    }

    #[test]
    fn refuses_a_device_login_that_declares_only_one_endpoint() {
        let yaml = concat!(
            "keys:\n  grok: $G\n",
            "providers:\n  - id: grok-cli\n    key: grok\n",
            "oauth:\n  - provider: grok-cli\n",
            "    device_auth_url: https://auth.example.invalid/device\n",
        );
        let err = Config::parse(yaml, stub_lookup).expect_err("nothing to poll");
        assert!(err.to_string().contains("device_auth_url and device_poll_url"), "{err}");
    }

    #[test]
    fn loads_a_device_login_when_both_endpoints_are_declared() {
        let yaml = concat!(
            "keys:\n  grok: $G\n",
            "providers:\n  - id: grok-cli\n    key: grok\n",
            "oauth:\n  - provider: grok-cli\n",
            "    device_auth_url: https://auth.example.invalid/device\n",
            "    device_poll_url: https://auth.example.invalid/poll\n",
        );
        let cfg = Config::parse(yaml, stub_lookup).expect("both halves are declared");
        assert!(cfg.oauth_for("grok-cli").expect("declared").device_poll_url.is_some());
    }

    #[test]
    fn refuses_a_device_endpoint_that_is_not_an_http_url() {
        let yaml = concat!(
            "keys:\n  grok: $G\n",
            "providers:\n  - id: grok-cli\n    key: grok\n",
            "oauth:\n  - provider: grok-cli\n",
            "    device_auth_url: not-a-url\n",
            "    device_poll_url: https://auth.example.invalid/poll\n",
        );
        let err = Config::parse(yaml, stub_lookup).expect_err("an endpoint is never inferred");
        assert!(err.to_string().contains("device_auth_url"), "{err}");
    }

    /// A device block with `poll` as the poll URL, so a templating test varies one
    /// field rather than four.
    fn device_yaml(poll_url: &str) -> String {
        format!(
            concat!(
                "keys:\n  grok: $G\n",
                "providers:\n  - id: grok-cli\n    key: grok\n",
                "oauth:\n  - provider: grok-cli\n",
                "    device_auth_url: https://auth.example.invalid/codes\n",
                "    device_poll_url: {poll_url}\n",
            ),
            poll_url = poll_url
        )
    }

    #[test]
    fn loads_a_device_poll_url_carrying_the_code_placeholder() {
        // A provider that addresses the grant by path cannot be expressed by a flat
        // poll URL, so the placeholder has to be accepted rather than read as a
        // malformed URL — the request it builds is the operator's declaration.
        let cfg = Config::parse(&device_yaml("https://auth.example.invalid/poll/{code}"), stub_lookup)
            .expect("a templated poll URL is a declared endpoint");
        assert_eq!(
            cfg.oauth_for("grok-cli").expect("declared").device_poll_url.as_deref(),
            Some("https://auth.example.invalid/poll/{code}"),
            "the placeholder is carried through verbatim for the executor to substitute"
        );
    }

    #[test]
    fn refuses_a_poll_url_whose_brace_group_is_not_the_code_placeholder() {
        // A typo would otherwise be polled verbatim for the grant's whole lifetime,
        // and the login would report an expiry instead of naming the bad config.
        for bad in [
            "https://auth.example.invalid/poll/{}",
            "https://auth.example.invalid/poll/{codes}",
            "https://auth.example.invalid/poll/{code",
            "https://auth.example.invalid/{code}/poll}",
        ] {
            let err = Config::parse(&device_yaml(bad), stub_lookup)
                .expect_err("a placeholder the executor cannot substitute is refused at load");
            assert!(
                err.to_string().contains("device_poll_url"),
                "{bad} should name the offending field, got: {err}"
            );
        }
    }

    #[test]
    fn refuses_a_code_placeholder_in_the_device_auth_url() {
        // The initiate request is made before any device code exists, so there is
        // nothing to substitute there.
        let yaml = concat!(
            "keys:\n  grok: $G\n",
            "providers:\n  - id: grok-cli\n    key: grok\n",
            "oauth:\n  - provider: grok-cli\n",
            "    device_auth_url: https://auth.example.invalid/codes/{code}\n",
            "    device_poll_url: https://auth.example.invalid/poll\n",
        );
        let err = Config::parse(yaml, stub_lookup).expect_err("there is no device code yet to place there");
        assert!(err.to_string().contains("device_auth_url"), "{err}");
    }
}
