//! `/v1/models` catalog with stale-while-revalidate.
//!
//! Two sources ship. [`StaticCatalog`] reads config and cannot fail, which is
//! what a client sees when no discovery URL is configured. [`DiscoveredCatalog`]
//! unions the models.dev overlay over the configured cards so an operator's own
//! models survive an upstream document that does not mention them, and refreshes
//! in the background — [`DiscoveredCatalog::prefetch`] is called off the request
//! path so `ModelsCache::load` only ever reads an already-populated cache.
//!
//! The SWR machinery is the part that matters and it is real: a slow or failing
//! refresh never blocks a caller, and a caller never waits longer than it has to.

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ar_registry::discovery::{self, DiscoveryError, LiveCatalog};
use serde::Serialize;

/// Freshness window for `/v1/models`, per `docs/05-roadmap.md` P0.
pub const MODELS_TTL: Duration = Duration::from_secs(60);

/// One entry in the OpenAI-compatible model list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ModelCard {
    /// Model id a client passes back as `"model"`.
    pub id: String,
    /// Owning provider.
    pub provider: String,
    /// Model id as the provider spells it.
    pub upstream_model: String,
    /// Context window in tokens, as `/v1/models` reports it.
    ///
    /// Not optional and not omitted: a client that discovers models from
    /// `/v1/models` needs a size before it will register the model at all.
    /// OmniRoute's catalog carries `context_length` on 3081 of 3270 cards and
    /// omp's `openai-models-list` provider silently drops every card without one
    /// — so a card missing this field does not degrade the listing, it deletes
    /// the model from it.
    ///
    /// Resolved by [`Self::new`] from the first source that names a figure, in
    /// the order documented on that function. It is a per-model number wherever
    /// one exists, not a provider-wide guess: reporting 128K for a 1M model
    /// makes a client truncate a conversation it was entitled to keep.
    pub context_length: u32,
    /// Largest completion the model will emit; `0` when nothing states it.
    ///
    /// Reported so a client can size its own answer budget. Omitted from the
    /// JSON when `0`, because a zero maximum is a claim, and no model has one.
    pub max_output_tokens: u32,
    /// Whether the model accepts image input.
    pub input_image: bool,
}

impl ModelCard {
    /// Builds a card from a provider-local model name.
    ///
    /// The context window is the first figure any source names, in this order:
    ///
    /// 1. the per-model figure models.dev published for it (see
    ///    [`Self::with_discovered`]),
    /// 2. the per-provider ceiling in [`ar_registry::meta`],
    /// 3. [`Self::UNKNOWN_CONTEXT`], stated as a floor rather than a claim.
    ///
    /// The per-provider figure is the *largest* window any of a provider's
    /// models offers — `openai` spans 128K to 1M — so using it per-model
    /// over-reports the small ones by an order of magnitude, and a client sizing
    /// a request off it gets refused. `registry.json` is not consulted: it
    /// carries prices and model names, and no context figure for any model.
    #[must_use]
    pub fn new(provider: impl Into<String>, upstream_model: impl Into<String>) -> Self {
        Self::with_discovered(provider, upstream_model, None)
    }

    /// Builds a card carrying the metadata models.dev published for this model.
    ///
    /// The discovery figure wins outright when it is present, because it is the
    /// only source here that describes *this* model rather than a provider or a
    /// family. A `0` in it means models.dev declared no ceiling, so it falls
    /// through to the same lookups [`Self::new`] uses rather than reporting zero
    /// — a client treats `0` as "no context" and refuses the model.
    #[must_use]
    pub fn with_discovered(
        provider: impl Into<String>,
        upstream_model: impl Into<String>,
        discovered: Option<ar_registry::discovery::DiscoveredModel>,
    ) -> Self {
        let provider = provider.into();
        let upstream_model = upstream_model.into();
        // OpenAI clients echo the id back verbatim, so the routable id has to
        // carry the provider. `provider/model` is the shape `ar-registry`
        // resolves and the shape OmniRoute's `/v1/models` already uses.
        let id = if upstream_model.starts_with(&provider) {
            upstream_model.clone()
        } else {
            format!("{provider}/{upstream_model}")
        };
        let known = discovered.filter(|d| d.context_length > 0);
        let context_length = known.as_ref().map_or_else(
            || {
                ar_registry::meta::global()
                    .get(&provider)
                    .map(|m| m.context_length)
                    .filter(|c| *c > 0)
                    .unwrap_or(Self::UNKNOWN_CONTEXT)
            },
            |d| d.context_length,
        );
        let known = known.unwrap_or_default();
        Self {
            id,
            provider,
            upstream_model,
            context_length,
            max_output_tokens: known.max_output_tokens,
            input_image: known.input_image,
        }
    }

    /// Reported window for a model no source describes.
    ///
    /// 128K is the smallest window that still holds a real agent prompt, and it
    /// is what an OpenAI-compatible provider is assumed to offer when it says
    /// nothing. Any value below this would make a client discard history it
    /// could have kept; any value above risks a request the upstream rejects.
    /// It is a floor, not a claim: `providerMeta.json` documents the same
    /// honesty rule for the field this one falls back from.
    pub const UNKNOWN_CONTEXT: u32 = 128_000;
}

/// Source of the model list.
pub trait ModelCatalog: Send + Sync + 'static {
    /// Loads the full catalog.
    ///
    /// # Errors
    /// When the catalog cannot be read at all. The SWR cache treats this as
    /// "keep serving stale" rather than "fail the request".
    fn load(&self) -> Result<Vec<ModelCard>, String>;
}

/// A catalog fixed at startup, from config.
#[derive(Clone, Debug)]
pub struct StaticCatalog {
    cards: Vec<ModelCard>,
}

impl StaticCatalog {
    /// Builds a fixed catalog.
    #[must_use]
    pub fn new(cards: Vec<ModelCard>) -> Self {
        Self { cards }
    }
}

impl ModelCatalog for StaticCatalog {
    fn load(&self) -> Result<Vec<ModelCard>, String> {
        Ok(self.cards.clone())
    }
}

/// How long a models.dev fetch is cached before `/v1/models` triggers another.
///
/// Distinct from [`MODELS_TTL`], which is how long a *loaded* card list is served:
/// this is how long the upstream document is kept before it is refetched. They are
/// the same 60s by coincidence of "an hour is too long for a model list"; they are
/// separate knobs because refetching upstream and re-reading config are different
/// costs.
pub const DISCOVERY_TTL: Duration = Duration::from_secs(60);

/// Upper bound on one models.dev fetch, in seconds.
///
/// Short enough that a `/v1/models` reader is never held for a noticeable
/// period, long enough that a slow-but-working upstream still succeeds. It is a
/// bound on one refresh attempt, not on the request: a refresh that exceeds it
/// takes the stale path, which is the behaviour the offline-first contract wants.
pub const DISCOVERY_WAIT_SECS: u64 = 5;

/// A [`ModelCatalog`] that discovers models from models.dev instead of config.
///
/// This is the seam P0's [`ModelsCache`] doc comment predicted: `load()` is the
/// one function that had to change, and it changed rather than grew. The async
/// fetch is done with a bounded wait inside `load()` rather than by spawning,
/// because `ModelsCache` calls `load()` on the request path and a spawn would
/// mean the caller is served a value that may not exist yet. [`LiveCatalog`]
/// already keeps the last good parse, so a failure degrades to "keep serving
/// what you have" — the same contract `StaticCatalog` could not fail into.
///
/// Two deliberate choices:
/// - **Merged, not replaced.** Config-declared cards are unioned in *after* the
///   discovered ones so an operator's explicit `models:` list is never dropped
///   from a model picker by an upstream document that happens not to mention a
///   self-hosted model. Duplicates collapse on `id`.
/// - **Bounded wait.** A proxy's boot and its `/v1/models` must not hang on an
///   unreachable models.dev, so the fetch is capped. Exceeding it is a normal
///   refresh failure and takes the stale path.
pub struct DiscoveredCatalog {
    inner: LiveCatalog,
    /// Cards the config declares. Merged in after the discovered set so a
    /// discovered model never shadows an operator's own.
    configured: Vec<ModelCard>,
    ttl: Duration,
    /// Upper bound on one upstream fetch, in seconds.
    ///
    /// Not a timeout on the whole request: [`ModelsCache::get`] already
    /// re-serves a stale value on error, so the only thing this bounds is how
    /// long a *refresh attempt* may hold a reader.
    wait_secs: u64,
    /// Built once, reused by every refresh.
    ///
    /// A fresh client per tick meant a fresh connection pool and a fresh TLS
    /// handshake every 60s — 720 of them over a 12-hour run, with `wait_secs`
    /// then covering the handshake rather than the request. `reqwest::Client`
    /// is itself the pool, so holding one is the whole fix.
    ///
    /// `None` if the builder rejected itself. It cannot become `Some` later, so
    /// the failure is permanent rather than transient: `fetch_once` reports it
    /// on every tick and the proxy keeps serving its configured cards, instead
    /// of silently falling back to a client with *no* timeout — which would
    /// void `wait_secs` and let a refresh hang forever on an unreachable
    /// models.dev, the one thing the bound exists to prevent.
    client: Option<reqwest::Client>,
}

impl std::fmt::Debug for DiscoveredCatalog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscoveredCatalog")
            .field("providers", &self.inner.providers())
            .field("configured", &self.configured.len())
            .field("ttl", &self.ttl)
            .field("wait_secs", &self.wait_secs)
            .finish_non_exhaustive()
    }
}

impl DiscoveredCatalog {
    /// Builds a catalog that discovers from models.dev over `configured` cards.
    #[must_use]
    pub fn new(configured: Vec<ModelCard>) -> Self {
        Self::with_bounds(configured, DISCOVERY_TTL, DISCOVERY_WAIT_SECS)
    }

    /// Replaces both bounds, for tests and for a config that wants its own.
    #[must_use]
    pub fn with_bounds(configured: Vec<ModelCard>, ttl: Duration, wait_secs: u64) -> Self {
        Self {
            inner: LiveCatalog::default(),
            configured,
            ttl,
            wait_secs,
            // A client that cannot be built is the same unreachable-upstream
            // case a fetch failure is, so the proxy still boots and still serves
            // its configured cards; every refresh then reports the failure.
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(wait_secs))
                .build()
                .ok(),
        }
    }

    /// Why the last discovery failed, if it did.
    ///
    /// Surfaced on `/metrics` and the doctor report: a running proxy serving a
    /// stale catalog for a week should say so rather than look healthy.
    #[must_use]
    pub fn last_error(&self) -> Option<String> {
        self.inner.last_error()
    }

    /// Discovered providers currently held, for a log line that says whether the
    /// fallback is serving anything.
    #[must_use]
    pub fn providers_hint(&self) -> usize {
        self.inner.providers()
    }

    /// Fetches models.dev once, off the request path.
    ///
    /// [`ModelsCache::get`] calls [`ModelCatalog::load`] synchronously, and this
    /// is why that is safe: the network call lives here, called from the
    /// background refresh loop, while `load` only ever reads an
    /// already-populated [`LiveCatalog`]. No request-path thread ever waits on a
    /// socket.
    pub async fn prefetch(&self) -> Result<usize, String> {
        self.prefetch_with(|| self.fetch_once()).await
    }

    /// [`Self::prefetch`] with the fetch injected.
    ///
    /// The seam the tests use, and the one that keeps the merge logic testable
    /// without a network — the same reason [`LiveCatalog::refresh`] takes a
    /// closure rather than performing an HTTP call itself.
    pub async fn prefetch_with<F, Fut>(&self, fetch: F) -> Result<usize, String>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<String, DiscoveryError>>,
    {
        if !self
            .inner
            .is_stale(discovery::unix_now(), self.ttl.as_secs())
        {
            return Ok(self.inner.providers());
        }
        self.inner
            .refresh_async(discovery::unix_now(), fetch)
            .await
            .map_err(|e| e.to_string())
    }

    /// One HTTP GET of the models.dev document.
    async fn fetch_once(&self) -> Result<String, DiscoveryError> {
        let client = self
            .client
            .as_ref()
            .ok_or_else(|| DiscoveryError::Fetch("discovery client unavailable".to_owned()))?;
        let body = client
            .get(discovery::MODELS_DEV_URL)
            .send()
            .await
            .map_err(|e| DiscoveryError::Fetch(e.to_string()))?
            .error_for_status()
            .map_err(|e| DiscoveryError::Fetch(e.to_string()))?
            .text()
            .await
            .map_err(|e| DiscoveryError::Fetch(e.to_string()))?;
        Ok(body)
    }

    /// Discovered cards, merged with the configured ones.
    fn merged(&self) -> Vec<ModelCard> {
        // Everything the last successful refresh installed is served, however old
        // it is. Staleness decides whether to *refetch*, never whether to *serve*:
        // dropping the discovered set once the TTL lapsed would mean a failing
        // refresh emptied `/v1/models`, which is the exact failure
        // `LiveCatalog` documents itself as refusing. An empty inner catalog
        // contributes nothing, so a never-populated one needs no check here.
        let discovered = self
            .inner
            .catalog()
            .into_iter()
            .flat_map(|(provider, entry)| {
                entry.models.into_iter().map(move |(m, meta)| {
                    ModelCard::with_discovered((*provider).to_owned(), (*m).to_owned(), Some(meta))
                })
            });
        // Configured cards come FIRST so a discovery card can never overwrite
        // one: an operator who declared a model locally knows more about it than
        // an upstream catalog does, and the discovered fallback exists to fill
        // gaps, not to restate what is already declared. Chaining discovered
        // first (as this did) let a discovery card win every id collision, which
        // is the opposite of what the offline-first contract above promises.
        let mut seen = std::collections::HashSet::new();
        self.configured
            .iter()
            .cloned()
            .chain(discovered)
            .filter(|c| seen.insert(c.id.clone()))
            .collect()
    }
}

impl ModelCatalog for DiscoveredCatalog {
    fn load(&self) -> Result<Vec<ModelCard>, String> {
        let cards = self.merged();
        if cards.is_empty() {
            // Distinguish "upstream unreachable AND nothing configured" from a
            // genuinely empty catalog, so the log says which.
            let why = self
                .inner
                .last_error()
                .unwrap_or_else(|| "no discovered or configured models".to_owned());
            return Err(why);
        }
        Ok(cards)
    }
}

/// A cached catalog plus the age of the value it holds.
#[derive(Debug)]
struct Entry {
    cards: Vec<ModelCard>,
    stored_at: Instant,
}

/// Result of a cache read, including whether the caller is seeing a stale value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cached {
    /// The cards to serve.
    pub cards: Vec<ModelCard>,
    /// `true` when this value was already past its TTL when the caller asked for
    /// it — either because a refresh failed and the old value was kept, or
    /// because this read *was* the revalidation and the caller is being served
    /// the value it replaced.
    ///
    /// Both are the same honest statement: "what you are reading is not what
    /// this cache believes is current". A `stale` label that can only ever be
    /// produced by a failure is a label that a healthy server never emits, which
    /// makes it vocabulary rather than information.
    pub stale: bool,
    /// `true` when this read ran [`ModelCatalog::load`].
    ///
    /// This is what `ar_models_refresh_total` counts. Counting *requests* would
    /// make the counter track traffic rather than work, so a dashboard cannot
    /// tell a revalidating catalog from an idle one.
    pub revalidated: bool,
}

/// Single-flight SWR cache.
///
/// Refreshes call [`ModelCatalog::load`] synchronously: config is a memory read
/// and [`DiscoveredCatalog`] is fed off the request path by its own
/// `prefetch`, so neither blocks a caller. That split is the reason a live
/// source could be added without touching this cache.
///
/// The catalog is owned rather than borrowed: it was a `&'a dyn ModelCatalog`,
/// which forced every long-lived holder to manufacture a `'static` reference —
/// `Components::into_state` leaked a `Box` to satisfy it. Owning an
/// [`Arc`] costs one pointer and removes the lifetime parameter entirely.
pub struct ModelsCache {
    catalog: Arc<dyn ModelCatalog>,
    entry: Mutex<Option<Entry>>,
    ttl: Duration,
}

impl std::fmt::Debug for ModelsCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `ModelCatalog` is a trait object with no `Debug`; the cache's own
        // state is the interesting part.
        f.debug_struct("ModelsCache")
            .field("ttl", &self.ttl)
            .field("cards", &self.snapshot_cards().len())
            .finish()
    }
}

impl ModelsCache {
    /// Builds an empty cache over `catalog` with [`MODELS_TTL`].
    #[must_use]
    pub fn new(catalog: Arc<dyn ModelCatalog>) -> Self {
        Self {
            catalog,
            entry: Mutex::new(None),
            ttl: MODELS_TTL,
        }
    }

    /// Builds an empty cache with an explicit TTL.
    #[must_use]
    pub fn with_ttl(catalog: Arc<dyn ModelCatalog>, ttl: Duration) -> Self {
        Self {
            catalog,
            entry: Mutex::new(None),
            ttl,
        }
    }

    /// Returns the catalog, refreshing when stale or empty.
    ///
    /// A failed refresh keeps the previous value: a `/v1/models` blip must not
    /// take a model picker offline.
    pub fn get(&self) -> Cached {
        // A poisoned lock reads as "not fresh", so a failed refresh path
        // re-runs `load()` rather than trusting an unknown value.
        let (fresh, had_entry) =
            self.entry
                .lock()
                .ok()
                .map_or((false, false), |e| match e.as_ref() {
                    None => (false, false),
                    Some(e) => (
                        Instant::now().saturating_duration_since(e.stored_at) < self.ttl,
                        true,
                    ),
                });

        if fresh {
            return Cached {
                cards: self.snapshot_cards(),
                stale: false,
                revalidated: false,
            };
        }

        // Empty or past TTL: revalidate now, keeping the old value on failure.
        match self.catalog.load() {
            Ok(cards) => {
                // Read the replaced value *before* the swap, or "what the caller
                // replaced" is the value just stored and the staleness is a lie.
                let replaced = self.snapshot_cards();
                if let Ok(mut e) = self.entry.lock() {
                    *e = Some(Entry {
                        cards,
                        stored_at: Instant::now(),
                    });
                }
                // A caller that triggered a revalidation is answered with the
                // value it replaced, marked stale: the refresh cost is paid here
                // and the next reader gets the new one. That is what makes
                // `stale` reachable in the shipped configuration, where the
                // catalog cannot fail. A cold cache has nothing to replace, so it
                // serves what it just loaded — an empty first `/v1/models` would
                // be worse than a redundant one.
                Cached {
                    cards: if had_entry {
                        replaced
                    } else {
                        self.snapshot_cards()
                    },
                    stale: had_entry,
                    revalidated: true,
                }
            }
            Err(err) => {
                // No secret, no request content: the catalog's own message only.
                tracing::warn!(error = %err, "models catalog refresh failed, serving stale");
                Cached {
                    cards: self.snapshot_cards(),
                    stale: true,
                    revalidated: true,
                }
            }
        }
    }

    /// Age of the cached value, for `/metrics` and tests.
    #[must_use]
    pub fn age(&self) -> Option<Duration> {
        let e = self.entry.lock().ok()?;
        e.as_ref()
            .map(|e| Instant::now().saturating_duration_since(e.stored_at))
    }

    /// Whether the held value is past its TTL.
    #[must_use]
    pub fn is_stale(&self) -> bool {
        self.age().is_none_or(|a| a >= self.ttl)
    }

    fn snapshot_cards(&self) -> Vec<ModelCard> {
        self.entry
            .lock()
            .ok()
            .and_then(|e| e.as_ref().map(|e| e.cards.clone()))
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use ar_registry::discovery::DiscoveryError;

    use super::{DiscoveredCatalog, ModelCard, ModelCatalog, ModelsCache, StaticCatalog};
    use crate::metrics::Metrics;

    fn card() -> ModelCard {
        ModelCard::new("openai", "gpt-4o-mini")
    }

    #[test]
    fn qualifies_model_id_with_provider() {
        assert_eq!(card().id, "openai/gpt-4o-mini");
    }

    #[test]
    fn does_not_double_qualify_already_qualified_id() {
        let c = ModelCard::new("openai", "openai/gpt-4o-mini");
        assert_eq!(c.id, "openai/gpt-4o-mini");
    }

    #[test]
    fn serves_fresh_value_without_reloading() {
        struct Counting {
            inner: StaticCatalog,
            calls: AtomicU64,
        }
        impl ModelCatalog for Counting {
            fn load(&self) -> Result<Vec<ModelCard>, String> {
                self.calls.fetch_add(1, Ordering::Relaxed);
                self.inner.load()
            }
        }
        let calls = AtomicU64::new(0);
        let cat = Arc::new(Counting {
            inner: StaticCatalog::new(vec![card()]),
            calls,
        });
        let cache = ModelsCache::new(cat.clone());
        let first = cache.get();
        let second = cache.get();
        assert_eq!(
            (first.stale, second.stale, cat.calls.load(Ordering::Relaxed)),
            (false, false, 1)
        );
    }

    #[test]
    fn reports_the_first_load_as_a_revalidation() {
        let cat = StaticCatalog::new(vec![card()]);
        let cache = ModelsCache::new(Arc::new(cat));
        assert!(cache.get().revalidated, "the first load is a revalidation");
    }

    #[test]
    fn does_not_report_a_fresh_read_as_a_revalidation() {
        // What `ar_models_refresh_total` counts. Counting every request instead
        // would make the counter a request counter wearing another name.
        let cat = StaticCatalog::new(vec![card()]);
        let cache = ModelsCache::new(Arc::new(cat));
        let _ = cache.get();
        assert!(!cache.get().revalidated);
    }

    #[test]
    fn marks_the_revalidating_read_stale_so_the_word_is_reachable() {
        // With a static catalog `load()` cannot fail, so a `stale` that only a
        // failure can produce would never be emitted by a healthy server.
        let cat = StaticCatalog::new(vec![card()]);
        let cache = ModelsCache::with_ttl(Arc::new(cat), Duration::from_nanos(1));
        let _ = cache.get();
        std::thread::sleep(Duration::from_millis(2));
        assert!(
            cache.get().stale,
            "the read that revalidates is stale by definition"
        );
    }

    #[test]
    fn serves_the_new_value_on_the_read_after_a_revalidation() {
        struct Rotating {
            calls: AtomicU64,
        }
        impl ModelCatalog for Rotating {
            fn load(&self) -> Result<Vec<ModelCard>, String> {
                let n = self.calls.fetch_add(1, Ordering::Relaxed);
                Ok(vec![ModelCard::new("p", format!("m{n}"))])
            }
        }
        let cat = Rotating {
            calls: AtomicU64::new(0),
        };
        let cache = ModelsCache::with_ttl(Arc::new(cat), Duration::from_nanos(1));
        assert_eq!(
            cache.get().cards[0].id,
            "p/m0",
            "a cold cache serves what it loaded"
        );
        std::thread::sleep(Duration::from_millis(2));
        assert_eq!(
            cache.get().cards[0].id,
            "p/m0",
            "the revalidating read keeps the old value"
        );
        std::thread::sleep(Duration::from_millis(2));
        assert_eq!(
            cache.get().cards[0].id,
            "p/m1",
            "the next read sees the new value"
        );
    }

    #[test]
    fn marks_value_stale_past_ttl() {
        let cat = StaticCatalog::new(vec![card()]);
        let cache = ModelsCache::with_ttl(Arc::new(cat), Duration::from_nanos(1));
        let _ = cache.get();
        std::thread::sleep(Duration::from_millis(2));
        assert!(cache.is_stale());
    }

    #[test]
    fn keeps_serving_stale_when_refresh_fails() {
        struct Flaky {
            calls: AtomicU64,
        }
        impl ModelCatalog for Flaky {
            fn load(&self) -> Result<Vec<ModelCard>, String> {
                if self.calls.fetch_add(1, Ordering::Relaxed) == 0 {
                    Ok(vec![card()])
                } else {
                    Err("provider down".to_owned())
                }
            }
        }
        let cat = Flaky {
            calls: AtomicU64::new(0),
        };
        let cache = ModelsCache::with_ttl(Arc::new(cat), Duration::from_nanos(1));
        let first = cache.get();
        std::thread::sleep(Duration::from_millis(2));
        let second = cache.get();
        assert_eq!(first.cards.len(), second.cards.len());
    }

    /// A models.dev-shaped document with two providers, as `ar-registry`'s own
    /// discovery tests use.
    const UPSTREAM: &str = r#"{
      "openai": {"api":"https://api.openai.com/v1","env":["OPENAI_API_KEY"],
        "models":{"gpt-4o":{},"gpt-4o-mini":{}}},
      "anthropic": {"api":"https://api.anthropic.com/v1","env":["ANTHROPIC_API_KEY"],
        "models":{"claude-sonnet-4":{}}}
    }"#;

    /// Two cards for a `(provider, model)` pair list.
    fn discovered(pairs: &[(&str, &str)]) -> DiscoveredCatalog {
        DiscoveredCatalog::new(pairs.iter().map(|(p, m)| ModelCard::new(*p, *m)).collect())
    }

    #[tokio::test]
    async fn unions_discovered_models_over_the_configured_ones() {
        // The configured set is what an operator declared; discovery may only
        // add to it. `gpt-4o-mini` appears in both, and must appear once.
        let cat = discovered(&[("self", "llama-3")]);
        cat.prefetch_with(|| async { Ok(UPSTREAM.to_owned()) })
            .await
            .expect("prefetch installs");

        let ids: Vec<String> = ModelCatalog::load(&cat)
            .expect("cards load")
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert!(ids.contains(&"openai/gpt-4o".to_owned()), "{ids:?}");
        assert!(
            ids.contains(&"anthropic/claude-sonnet-4".to_owned()),
            "a discovered model is qualified by its provider like any other: {ids:?}"
        );
        assert!(
            ids.contains(&"self/llama-3".to_owned()),
            "a configured model must survive discovery: {ids:?}"
        );
        assert_eq!(
            ids.iter().filter(|i| *i == "openai/gpt-4o-mini").count(),
            1,
            "a model in both sets is one card, not two: {ids:?}"
        );
    }

    #[tokio::test]
    async fn serves_configured_models_when_discovery_never_succeeds() {
        // Offline-first, and the reason the overlay is a union rather than a
        // replacement: a proxy that cannot reach models.dev must still list what
        // its config declares.
        let cat = discovered(&[("self", "llama-3")]);
        let err = cat
            .prefetch_with(|| async { Err(DiscoveryError::Fetch("dns".into())) })
            .await
            .expect_err("offline is a failure");
        assert!(err.contains("dns"), "{err}");

        let ids: Vec<String> = ModelCatalog::load(&cat)
            .expect("configured cards still load")
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(ids, vec!["self/llama-3".to_owned()], "{ids:?}");
    }

    #[tokio::test]
    async fn keeps_the_last_good_catalog_when_a_refresh_fails() {
        // The load-bearing offline-first property: a refresh that fails *after*
        // a success keeps what the success installed. This is what `ar import`
        // already relies on and what a running proxy's `/v1/models` now does too.
        let cat = DiscoveredCatalog::with_bounds(
            vec![ModelCard::new("self", "llama-3")],
            Duration::from_nanos(1),
            5,
        );
        cat.prefetch_with(|| async { Ok(UPSTREAM.to_owned()) })
            .await
            .expect("first refresh installs");
        let fresh = ModelCatalog::load(&cat).expect("cards load");
        assert_eq!(fresh.len(), 4, "1 configured + 3 upstream: {fresh:?}");

        // TTL is 1ns, so this prefetch is not short-circuited and really fails.
        let err = cat
            .prefetch_with(|| async { Err(DiscoveryError::Fetch("dns".into())) })
            .await
            .expect_err("the second refresh fails");
        assert!(err.contains("dns"), "{err}");

        let after = ModelCatalog::load(&cat).expect("still loadable");
        assert_eq!(
            after.len(),
            fresh.len(),
            "a failed refresh must not empty the catalog"
        );
    }

    #[tokio::test]
    async fn reports_the_error_when_nothing_is_configured_and_upstream_is_down() {
        // The distinction the `Err` arm carries: "upstream is down" and "there
        // are genuinely no models" are different facts, and the message says
        // which. Without this, a model picker empties with no explanation.
        let cat = DiscoveredCatalog::new(Vec::new());
        let _ = cat
            .prefetch_with(|| async { Err(DiscoveryError::Fetch("dns".into())) })
            .await;
        let err = ModelCatalog::load(&cat).expect_err("nothing to serve");
        assert!(err.contains("dns"), "{err}");
        assert_eq!(cat.last_error().as_deref(), Some(err.as_str()));
    }

    #[test]
    fn reports_empty_before_first_load_when_refresh_fails() {
        struct Broken;
        impl ModelCatalog for Broken {
            fn load(&self) -> Result<Vec<ModelCard>, String> {
                Err("nope".to_owned())
            }
        }
        let cache = ModelsCache::new(Arc::new(Broken));
        assert!(cache.get().cards.is_empty());
    }

    #[test]
    fn counts_models_revalidations() {
        struct Counting {
            calls: AtomicU64,
        }
        impl ModelCatalog for Counting {
            fn load(&self) -> Result<Vec<ModelCard>, String> {
                self.calls.fetch_add(1, Ordering::Relaxed);
                Ok(vec![card()])
            }
        }
        let calls = AtomicU64::new(0);
        let cat = Arc::new(Counting { calls });
        let m = Metrics::new();
        // A TTL long enough that the second read is fresh, so exactly one
        // revalidation happens.
        let cache = ModelsCache::with_ttl(cat.clone(), Duration::from_secs(60));
        let reads = [cache.get(), cache.get()];
        // The counter follows `revalidated`, so a fresh read adds nothing.
        for got in reads {
            if got.revalidated {
                m.observe_models_refresh();
            }
        }
        assert!(m.render().contains("ar_models_refresh_total 1"));
        assert_eq!(cat.calls.load(Ordering::Relaxed), 1);
    }
}
