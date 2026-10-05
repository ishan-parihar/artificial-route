//! Runtime model discovery over models.dev, cached stale-while-revalidate.
//!
//! Ported from `../OmniRoute/src/lib/modelsDevSync.ts` + `modelDiscovery.ts` +
//! `reactiveModelSync.ts`, reduced to what this proxy can honour (docs/02,
//! `sync/`). Dropped on purpose: the SQLite write path, the settings UI, the
//! 24-hour scheduler, and `sync/bundle.ts` + `cloudSync.ts` — docs/02 marks
//! those DROP, and a bundle sync would mean carrying cloud auth into a proxy
//! whose whole credential model is "a key name in the environment".
//!
//! The compiled-in `include_str!` catalog stays the preferred path: a base URL
//! known at build time costs zero I/O. This module is the overlay that keeps a
//! running proxy's model list honest without a rebuild, and it is offline-first
//! — a failed refresh keeps the previous catalog rather than emptying it,
//! because a proxy that loses the network must keep routing.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use ar_core::Strng;
use serde::Deserialize;

/// The upstream catalog: one provider map with pricing, capabilities and model
/// ids each. The same document an OmniRoute export is built from.
pub const MODELS_DEV_URL: &str = "https://models.dev/api.json";

/// Seconds since the Unix epoch, saturating at 0 for a clock set before 1970.
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Ways a discovery refresh can fail.
#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    /// The request itself failed: offline, DNS, TLS, timeout, bad status.
    #[error("upstream fetch failed: {0}")]
    Fetch(String),
    /// A response arrived but is not a models.dev catalog.
    #[error("upstream payload is not a models.dev catalog: {0}")]
    Decode(#[from] serde_json::Error),
}

/// One model as models.dev describes it.
///
/// The three figures kept are the ones a client cannot infer and cannot recover
/// elsewhere: how large a prompt this model accepts, how much it can answer, and
/// whether it accepts images. `cost` is deliberately absent — `ar-route` sorts
/// unpriced last by design and prices come from `registry.json`, so a second
/// price table here would be a second answer to the same question.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct DiscoveredModel {
    /// Context window in tokens; `0` when the document names none.
    #[serde(default)]
    pub context_length: u32,
    /// Largest completion this model will emit, in tokens; `0` when unstated.
    #[serde(default)]
    pub max_output_tokens: u32,
    /// Whether the model accepts image input.
    #[serde(default)]
    pub input_image: bool,
}

impl DiscoveredModel {
    /// Reads the figures out of one raw models.dev model object.
    ///
    /// Every field is optional upstream and absent keys must not fail the whole
    /// catalog: models.dev omits `limit` for models that declare no ceiling, and
    /// one such model must not cost the other three thousand their metadata. So
    /// each figure falls back to `0`/`false` independently rather than failing.
    fn from_raw(raw: &serde_json::Value) -> Self {
        let limit = raw.get("limit");
        Self {
            context_length: limit
                .and_then(|l| l.get("context"))
                .and_then(serde_json::Value::as_u64)
                .and_then(|v| u32::try_from(v).ok())
                .unwrap_or(0),
            max_output_tokens: limit
                .and_then(|l| l.get("output"))
                .and_then(serde_json::Value::as_u64)
                .and_then(|v| u32::try_from(v).ok())
                .unwrap_or(0),
            input_image: raw
                .get("modalities")
                .and_then(|m| m.get("input"))
                .and_then(serde_json::Value::as_array)
                .is_some_and(|inputs| inputs.iter().any(|m| m.as_str() == Some("image"))),
        }
    }
}

/// One provider as models.dev describes it, reduced to what routing needs.
///
/// Prices are deliberately not carried: `ar-route` sorts unpriced last by design
/// (README) and reads no capability flag, so storing them here would be RAM
/// spent on nothing. Per-model metadata IS carried, because `/v1/models` is the
/// only place a client learns how large a prompt a model accepts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Discovered {
    /// Upstream API root, when the provider published one.
    #[serde(default)]
    pub base_url: Option<String>,
    /// First documented environment variable holding the credential.
    #[serde(default)]
    pub env_hint: Option<String>,
    /// This provider's models and their declared metadata, in document order.
    #[serde(default)]
    pub models: Vec<(Strng, DiscoveredModel)>,
}

/// The raw models.dev provider object. Three fields are read; the rest of the
/// document (costs, limits, modalities) is skipped by the deserializer, not
/// materialised — `serde` ignores unknown keys, so the parse stays cheap on a
/// 4,000-model document.
#[derive(Deserialize)]
struct RawProvider {
    #[serde(default)]
    api: Option<String>,
    #[serde(default)]
    env: Option<Vec<String>>,
    #[serde(default)]
    models: BTreeMap<String, serde_json::Value>,
}

impl From<RawProvider> for Discovered {
    fn from(raw: RawProvider) -> Self {
        Self {
            base_url: raw.api,
            env_hint: raw.env.and_then(|e| e.into_iter().next()),
            // The per-model bodies are already materialised as
            // `serde_json::Value`; reading the three figures out of them here is
            // what makes a 1M-context model advertise 1M instead of a guessed
            // default. Reducing to `into_keys()` would discard exactly the data
            // a client cannot recover anywhere else.
            models: raw
                .models
                .into_iter()
                .map(|(id, body)| (Strng::from(id.as_str()), DiscoveredModel::from_raw(&body)))
                .collect(),
        }
    }
}

/// A provider map keyed by provider id, sorted for deterministic output.
pub type Catalog = BTreeMap<Strng, Discovered>;

/// The most recent successfully parsed catalog, process-wide.
///
/// A combo's context window is reduced over its targets inside `ar-server`, which
/// is built before any `LiveCatalog` exists and holds no handle to one. This is
/// the seam that lets the reduction consult the overlay anyway: a window published
/// for one fetch is a fact about the provider that does not go stale with the next
/// request, so every later reader sees it without re-fetching.
///
/// A `RwLock` over an `Arc` rather than a `OnceLock` because a refresh replaces
/// the catalog and a `OnceLock` cannot: only the first fetch would ever be seen,
/// so a stale first answer would outlive every correct one.
static LATEST: std::sync::RwLock<Option<std::sync::Arc<Catalog>>> = std::sync::RwLock::new(None);

/// Publishes a freshly parsed catalog for [`latest`].
pub(crate) fn publish(catalog: std::sync::Arc<Catalog>) {
    if let Ok(mut slot) = LATEST.write() {
        *slot = Some(catalog);
    }
}

/// The last successfully parsed catalog, when this process has fetched one.
#[must_use]
pub fn latest() -> Option<std::sync::Arc<Catalog>> {
    LATEST.read().ok().and_then(|slot| slot.clone())
}

/// One model window as models.dev published it, for a `provider` and `model`.
///
/// `model` may be spelled bare (`glm-5.3`) or already qualified
/// (`nvidia/z-ai/glm-5.3`), because a config target and a catalog id spell the
/// same model differently and the caller should not have to know which it holds.
/// `None` when this fetch declared no window, so the caller can fall through to
/// the next source rather than report zero.
#[must_use]
pub fn published_window(view: &Catalog, provider: &str, model: &str) -> Option<u32> {
    let entry = view.get(provider)?;
    let (_, meta) = entry
        .models
        .iter()
        .find(|(id, _)| &**id == model || model.ends_with(&format!("/{id}")))?;
    (meta.context_length > 0).then_some(meta.context_length)
}

/// Parses a models.dev `api.json` document into a [`Catalog`].
pub fn parse_catalog(body: &str) -> Result<Catalog, DiscoveryError> {
    let raw: BTreeMap<String, RawProvider> = serde_json::from_str(body)?;
    Ok(raw
        .into_iter()
        .map(|(id, p)| (Strng::from(id.as_str()), p.into()))
        .collect())
}

/// Stale-while-revalidate cache over a [`Catalog`].
///
/// A read never fails. [`LiveCatalog::catalog`] answers from whatever the last
/// successful refresh left behind and [`LiveCatalog::last_error`] says why it
/// may be old, so boot never depends on the network. The upstream body is kept
/// alongside the parsed form so a cached read can be replayed without
/// re-serialising into a shape models.dev never emitted.
#[derive(Debug, Default)]
pub struct LiveCatalog {
    inner: Mutex<Snapshot>,
}

#[derive(Debug, Default)]
struct Snapshot {
    /// Shared so a reader can keep a snapshot past the lock guard that vouched
    /// for it: `install` replaces this whole value, so a borrowed reference would
    /// dangle the moment a refresh lands on another task.
    catalog: std::sync::Arc<Catalog>,
    body: Option<String>,
    fetched_at: Option<u64>,
    last_error: Option<String>,
    refreshes: u64,
}

impl LiveCatalog {
    /// The catalog as of the last successful refresh, fresh or stale.
    pub fn catalog(&self) -> Catalog {
        self.lock().catalog.as_ref().clone()
    }

    /// Number of providers currently held.
    pub fn providers(&self) -> usize {
        self.lock().catalog.len()
    }

    /// Why the last refresh failed. Cleared by the next success.
    pub fn last_error(&self) -> Option<String> {
        self.lock().last_error.clone()
    }

    /// The upstream body behind the current catalog.
    pub fn body(&self) -> Option<String> {
        self.lock().body.clone()
    }

    /// The parsed catalog, as a shareable snapshot.
    ///
    /// Cloned out rather than borrowed: `install` replaces the whole value under
    /// the write lock, so handing a caller a reference would outlive the guard
    /// that vouches for it. An `Arc` snapshot is the one shape that is safe to
    /// keep — and cheap, because the catalog is replaced wholesale rather than
    /// edited in place.
    #[must_use]
    pub fn catalog_snapshot(&self) -> Option<std::sync::Arc<Catalog>> {
        // `None` means "never fetched", which is what an empty default `Arc` is
        // for. A fetch that returned no providers is a real answer, not an absent
        // one, and collapsing the two would make an empty upstream look like an
        // unwarmed proxy.
        let guard = self.lock();
        (!guard.catalog.is_empty()).then(|| std::sync::Arc::clone(&guard.catalog))
    }

    /// Whether the cache holds nothing, or holds something older than `ttl_secs`.
    ///
    /// An empty cache is always stale: there is nothing to serve, so there is
    /// nothing to gain by waiting.
    pub fn is_stale(&self, now: u64, ttl_secs: u64) -> bool {
        self.lock()
            .fetched_at
            .is_none_or(|at| now.saturating_sub(at) >= ttl_secs)
    }

    /// Fetches and installs a new catalog, returning the provider count.
    ///
    /// `fetch` is injected rather than performed here: the parse and the cache
    /// transition are then testable without a network, and an HTTP client stays
    /// out of a crate whose job is a lookup table. On failure the previous
    /// catalog *and* body are kept and the error recorded — losing the network
    /// must not empty a running proxy's model list.
    pub fn refresh<F>(&self, now: u64, fetch: F) -> Result<usize, DiscoveryError>
    where
        F: FnOnce() -> Result<String, DiscoveryError>,
    {
        self.install(now, fetch())
    }

    /// [`Self::refresh`] for a fetch that has to await.
    ///
    /// The parse-and-install transition is shared, so the offline-first
    /// guarantee — a failed refresh keeps the previous catalog and body and only
    /// records the error — cannot be implemented twice and drift. This arm exists
    /// because the server's refresh loop has a runtime and the importing CLI's
    /// synchronous path does not; both write the same cache.
    pub async fn refresh_async<F, Fut>(&self, now: u64, fetch: F) -> Result<usize, DiscoveryError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<String, DiscoveryError>>,
    {
        self.install(now, fetch().await)
    }

    /// Installs a fetched document, or keeps the old snapshot and records why.
    fn install(
        &self,
        now: u64,
        fetched: Result<String, DiscoveryError>,
    ) -> Result<usize, DiscoveryError> {
        let mut snap = self.lock();
        snap.refreshes += 1;
        match fetched.and_then(|body| parse_catalog(&body).map(|catalog| (catalog, body))) {
            Ok((catalog, body)) => {
                let providers = catalog.len();
                let catalog = std::sync::Arc::new(catalog);
                publish(std::sync::Arc::clone(&catalog));
                snap.catalog = catalog;
                snap.body = Some(body);
                snap.fetched_at = Some(now);
                snap.last_error = None;
                Ok(providers)
            }
            Err(e) => {
                snap.last_error = Some(e.to_string());
                Err(e)
            }
        }
    }

    /// Locks the snapshot, taking the poison.
    ///
    /// A refresh that panicked left the previous catalog in place — the same
    /// guarantee the error path gives — so refusing every later read would be
    /// strictly worse than reading it.
    fn lock(&self) -> MutexGuard<'_, Snapshot> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two providers, three models between them. The model bodies carry the
    /// figures models.dev actually publishes, because the parse is required to
    /// keep them rather than reduce each object to its key.
    const UPSTREAM: &str = r#"{
      "openai": {"api":"https://api.openai.com/v1","env":["OPENAI_API_KEY"],
        "models":{
          "gpt-4o":{"limit":{"context":128000,"output":16384},"modalities":{"input":["text","image"]}},
          "gpt-4o-mini":{"limit":{"context":128000,"output":16384},"modalities":{"input":["text"]}}}},
      "anthropic": {"api":"https://api.anthropic.com/v1","env":["ANTHROPIC_API_KEY"],
        "models":{"claude-sonnet-4":{"limit":{"context":200000,"output":64000},"modalities":{"input":["text","image"]}}}}
    }"#;

    #[test]
    fn discovers_when_upstream_lists() {
        let c = LiveCatalog::default();

        assert_eq!(c.refresh(100, || Ok(UPSTREAM.to_owned())).unwrap(), 2);
        assert_eq!(c.providers(), 2);
        assert_eq!(
            c.catalog()["openai"]
                .models
                .iter()
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>(),
            vec![Strng::from("gpt-4o"), Strng::from("gpt-4o-mini")],
            "BTreeMap keys come out sorted, so the model list is deterministic"
        );
        assert_eq!(
            c.catalog()["openai"].base_url.as_deref(),
            Some("https://api.openai.com/v1")
        );
        assert!(!c.is_stale(100, 3600));
        assert!(
            c.is_stale(100 + 3600, 3600),
            "the ttl is a boundary, not a suggestion"
        );
    }

    #[test]
    fn stays_stale_when_offline() {
        let c = LiveCatalog::default();
        // Nothing fetched yet: no catalog, no error, and always due a fetch.
        assert!(c.is_stale(0, 3600));
        assert_eq!(c.catalog(), Catalog::new());

        c.refresh(100, || Ok(UPSTREAM.to_owned())).unwrap();
        let before = c.catalog();

        let err = c
            .refresh(200, || Err(DiscoveryError::Fetch("dns".into())))
            .unwrap_err();
        assert!(err.to_string().contains("dns"), "{err}");
        assert_eq!(
            c.catalog(),
            before,
            "a failed refresh must not empty the catalog"
        );
        assert!(
            c.body().is_some(),
            "the body that backs the catalog is kept too"
        );
        assert_eq!(c.lock().refreshes, 2);
        assert!(
            !c.is_stale(200, 3600),
            "100s old is inside a 1h ttl, so it is still served as current"
        );
        assert!(
            c.is_stale(100 + 3600, 3600),
            "and past the ttl it is served only as stale"
        );
        assert!(c.last_error().unwrap().contains("dns"));
    }

    #[test]
    fn keeps_the_per_model_figures_the_document_published() {
        // The bug this pins: the parse materialised every model body as a
        // `serde_json::Value` and then reduced the map to its keys, so a 1M model
        // and a 128K model were indistinguishable here — and `/v1/models`, whose
        // only per-model input is this, reported one number for both.
        let c = LiveCatalog::default();
        c.refresh(100, || Ok(UPSTREAM.to_owned())).unwrap();

        let models = &c.catalog()["openai"].models;
        let gpt4o = models
            .iter()
            .find(|(id, _)| &**id == "gpt-4o")
            .map(|(_, m)| m.clone())
            .expect("gpt-4o is in the document");
        assert_eq!(gpt4o.context_length, 128_000);
        assert_eq!(gpt4o.max_output_tokens, 16_384);
        assert!(gpt4o.input_image, "gpt-4o declares image input");

        let mini = models
            .iter()
            .find(|(id, _)| &**id == "gpt-4o-mini")
            .map(|(_, m)| m.clone())
            .expect("gpt-4o-mini is in the document");
        assert!(!mini.input_image, "gpt-4o-mini declares text only");

        assert_eq!(c.catalog()["anthropic"].models[0].1.context_length, 200_000);
    }

    #[test]
    fn a_model_without_declared_limits_reports_zero_rather_than_failing() {
        // models.dev omits `limit` for a model that declares no ceiling. One
        // such model must not cost the rest of the catalog its metadata, so the
        // figure defaults independently rather than the parse erroring.
        let body = r#"{"p":{"api":"https://x/v1","env":["K"],
            "models":{"z-with":{},"a-limit":{"limit":{"context":64000}}}}}"#;
        let cat = parse_catalog(body).unwrap();
        let models = &cat["p"].models;
        // Sorted, not document order: `a-limit` precedes `z-with`, which is what
        // makes each row's own value the thing under test.
        assert_eq!(models[0].1.context_length, 64_000);
        assert_eq!(models[1].1, DiscoveredModel::default());
    }
}
