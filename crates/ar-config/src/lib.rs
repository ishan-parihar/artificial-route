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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
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
    /// The filesystem watcher could not be installed.
    #[error("cannot watch {path}: {cause}")]
    Watch {
        /// Directory or file that could not be watched.
        path: PathBuf,
        /// Underlying watcher failure.
        cause: String,
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

/// A named chain of `provider/model` targets plus the strategy that walks it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Combo {
    /// Combo id; the `model` a client asks for resolves to this.
    pub id: String,
    /// How targets are selected.
    pub strategy: Strategy,
    /// `provider/model` targets, in strategy order.
    pub targets: Vec<String>,
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
    /// Routing combos.
    #[serde(default)]
    pub combos: Vec<Combo>,
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
                return Err(ConfigError::UnknownKey {
                    provider: p.id.clone(),
                    key: p.key.clone(),
                });
            }
        }
        Ok(())
    }

    /// Looks up a declared credential by name, borrowing the caller's name.
    pub fn key(&self, name: &str) -> Option<&Secret> {
        self.keys.get(name)
    }
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

    fn stub_lookup(name: &str) -> Result<Option<String>, String> {
        Ok(Some(format!("secret-for-{name}")))
    }

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
    fn redacts_secret_when_debug_formatted() {
        assert_eq!(format!("{:?}", Secret::new("sk-live-1234")), "Secret(***)");
    }

    /// Every name `ar-route::Strategy::parse` claims, so a rename there fails
    /// here instead of silently routing under [`Strategy::Deferred`].
    const ROUTE_STRATEGIES: [&str; 19] = [
        "priority", "round-robin", "cost-optimized", "lkgp", "weighted", "fill-first", "p2c",
        "least-used", "random", "strict-random", "headroom", "reset-window", "reset-aware",
        "quota-share-fair", "context-relay", "context-optimized", "cache-optimized", "fusion",
        "pipeline",
    ];

    #[test]
    fn parses_every_route_strategy_name() {
        for name in ROUTE_STRATEGIES {
            assert!(Strategy::parse(name).is_routable(), "{name}");
        }
        assert_eq!(ROUTE_STRATEGIES.len(), 19);
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
}
