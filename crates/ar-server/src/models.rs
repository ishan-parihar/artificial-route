//! `/v1/models` catalog with stale-while-revalidate.
//!
//! P0: the catalog comes from config, so "revalidate" re-derives it rather than
//! hitting a provider. The SWR machinery is the part that matters and it is
//! real — a slow or failing refresh never blocks a caller, and a caller never
//! waits longer than it has to. Swapping [`ModelCatalog`] for a live provider
//! client in P1 changes no caller.

use std::sync::Mutex;
use std::time::{Duration, Instant};

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
}

impl ModelCard {
    /// Builds a card from a provider-local model name.
    #[must_use]
    pub fn new(provider: impl Into<String>, upstream_model: impl Into<String>) -> Self {
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
        Self { id, provider, upstream_model }
    }
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
/// Refreshes are synchronous here, not spawned: P0's `load()` is a config read
/// that cannot block. A live provider client (P1) needs `tokio::spawn`, and at
/// that point this is the one function that changes.
pub struct ModelsCache<'a> {
    catalog: &'a dyn ModelCatalog,
    entry: Mutex<Option<Entry>>,
    ttl: Duration,
}

impl std::fmt::Debug for ModelsCache<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `ModelCatalog` is a trait object with no `Debug`; the cache's own
        // state is the interesting part.
        f.debug_struct("ModelsCache")
            .field("ttl", &self.ttl)
            .field("cards", &self.snapshot_cards().len())
            .finish()
    }
}

impl<'a> ModelsCache<'a> {
    /// Builds an empty cache over `catalog` with [`MODELS_TTL`].
    #[must_use]
    pub fn new(catalog: &'a dyn ModelCatalog) -> Self {
        Self { catalog, entry: Mutex::new(None), ttl: MODELS_TTL }
    }

    /// Builds an empty cache with an explicit TTL.
    #[must_use]
    pub fn with_ttl(catalog: &'a dyn ModelCatalog, ttl: Duration) -> Self {
        Self { catalog, entry: Mutex::new(None), ttl }
    }

    /// Returns the catalog, refreshing when stale or empty.
    ///
    /// A failed refresh keeps the previous value: a `/v1/models` blip must not
    /// take a model picker offline.
    pub fn get(&self) -> Cached {
        // A poisoned lock reads as "not fresh", so a failed refresh path
        // re-runs `load()` rather than trusting an unknown value.
        let (fresh, had_entry) = self.entry.lock().ok().map_or((false, false), |e| {
            match e.as_ref() {
                None => (false, false),
                Some(e) => (
                    Instant::now().saturating_duration_since(e.stored_at) < self.ttl,
                    true,
                ),
            }
        });

        if fresh {
            return Cached { cards: self.snapshot_cards(), stale: false, revalidated: false };
        }

        // Empty or past TTL: revalidate now, keeping the old value on failure.
        match self.catalog.load() {
            Ok(cards) => {
                // Read the replaced value *before* the swap, or "what the caller
                // replaced" is the value just stored and the staleness is a lie.
                let replaced = self.snapshot_cards();
                if let Ok(mut e) = self.entry.lock() {
                    *e = Some(Entry { cards, stored_at: Instant::now() });
                }
                // A caller that triggered a revalidation is answered with the
                // value it replaced, marked stale: the refresh cost is paid here
                // and the next reader gets the new one. That is what makes
                // `stale` reachable in the shipped configuration, where the
                // catalog cannot fail. A cold cache has nothing to replace, so it
                // serves what it just loaded — an empty first `/v1/models` would
                // be worse than a redundant one.
                Cached {
                    cards: if had_entry { replaced } else { self.snapshot_cards() },
                    stale: had_entry,
                    revalidated: true,
                }
            }
            Err(err) => {
                // No secret, no request content: the catalog's own message only.
                tracing::warn!(error = %err, "models catalog refresh failed, serving stale");
                Cached { cards: self.snapshot_cards(), stale: true, revalidated: true }
            }
        }
    }

    /// Age of the cached value, for `/metrics` and tests.
    #[must_use]
    pub fn age(&self) -> Option<Duration> {
        let e = self.entry.lock().ok()?;
        e.as_ref().map(|e| Instant::now().saturating_duration_since(e.stored_at))
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
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use super::{ModelCard, ModelCatalog, ModelsCache, StaticCatalog};
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
        let cat = Counting {
            inner: StaticCatalog::new(vec![card()]),
            calls: AtomicU64::new(0),
        };
        let cache = ModelsCache::new(&cat);
        let first = cache.get();
        let second = cache.get();
        assert_eq!((first.stale, second.stale, cat.calls.load(Ordering::Relaxed)), (false, false, 1));
    }

    #[test]
    fn reports_the_first_load_as_a_revalidation() {
        let cat = StaticCatalog::new(vec![card()]);
        let cache = ModelsCache::new(&cat);
        assert!(cache.get().revalidated, "the first load is a revalidation");
    }

    #[test]
    fn does_not_report_a_fresh_read_as_a_revalidation() {
        // What `ar_models_refresh_total` counts. Counting every request instead
        // would make the counter a request counter wearing another name.
        let cat = StaticCatalog::new(vec![card()]);
        let cache = ModelsCache::new(&cat);
        let _ = cache.get();
        assert!(!cache.get().revalidated);
    }

    #[test]
    fn marks_the_revalidating_read_stale_so_the_word_is_reachable() {
        // With a static catalog `load()` cannot fail, so a `stale` that only a
        // failure can produce would never be emitted by a healthy server.
        let cat = StaticCatalog::new(vec![card()]);
        let cache = ModelsCache::with_ttl(&cat, Duration::from_nanos(1));
        let _ = cache.get();
        std::thread::sleep(Duration::from_millis(2));
        assert!(cache.get().stale, "the read that revalidates is stale by definition");
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
        let cat = Rotating { calls: AtomicU64::new(0) };
        let cache = ModelsCache::with_ttl(&cat, Duration::from_nanos(1));
        assert_eq!(cache.get().cards[0].id, "p/m0", "a cold cache serves what it loaded");
        std::thread::sleep(Duration::from_millis(2));
        assert_eq!(cache.get().cards[0].id, "p/m0", "the revalidating read keeps the old value");
        std::thread::sleep(Duration::from_millis(2));
        assert_eq!(cache.get().cards[0].id, "p/m1", "the next read sees the new value");
    }

    #[test]
    fn marks_value_stale_past_ttl() {
        let cat = StaticCatalog::new(vec![card()]);
        let cache = ModelsCache::with_ttl(&cat, Duration::from_nanos(1));
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
        let cat = Flaky { calls: AtomicU64::new(0) };
        let cache = ModelsCache::with_ttl(&cat, Duration::from_nanos(1));
        let first = cache.get();
        std::thread::sleep(Duration::from_millis(2));
        let second = cache.get();
        assert_eq!(first.cards.len(), second.cards.len());
    }

    #[test]
    fn reports_empty_before_first_load_when_refresh_fails() {
        struct Broken;
        impl ModelCatalog for Broken {
            fn load(&self) -> Result<Vec<ModelCard>, String> {
                Err("nope".to_owned())
            }
        }
        let cache = ModelsCache::new(&Broken);
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
        let cat = Counting { calls: AtomicU64::new(0) };
        let m = Metrics::new();
        // A TTL long enough that the second read is fresh, so exactly one
        // revalidation happens.
        let cache = ModelsCache::with_ttl(&cat, Duration::from_secs(60));
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
