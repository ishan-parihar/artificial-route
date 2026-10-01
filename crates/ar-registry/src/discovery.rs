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

/// One provider as models.dev describes it, reduced to what routing needs.
///
/// Prices and capability flags are deliberately not carried: `ar-route` sorts
/// unpriced last by design (README) and reads no capability field, so storing
/// them here would be RAM spent on nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Discovered {
    /// Upstream API root, when the provider published one.
    #[serde(default)]
    pub base_url: Option<String>,
    /// First documented environment variable holding the credential.
    #[serde(default)]
    pub env_hint: Option<String>,
    /// Model ids this provider serves, in document order.
    #[serde(default)]
    pub models: Vec<Strng>,
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
            models: raw.models.into_keys().map(Strng::from).collect(),
        }
    }
}

/// A provider map keyed by provider id, sorted for deterministic output.
pub type Catalog = BTreeMap<Strng, Discovered>;

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
    catalog: Catalog,
    body: Option<String>,
    fetched_at: Option<u64>,
    last_error: Option<String>,
    refreshes: u64,
}

impl LiveCatalog {
    /// The catalog as of the last successful refresh, fresh or stale.
    pub fn catalog(&self) -> Catalog {
        self.lock().catalog.clone()
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
        let mut snap = self.lock();
        snap.refreshes += 1;
        match fetch().and_then(|body| parse_catalog(&body).map(|catalog| (catalog, body))) {
            Ok((catalog, body)) => {
                let providers = catalog.len();
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

    /// Two providers, three models between them.
    const UPSTREAM: &str = r#"{
      "openai": {"api":"https://api.openai.com/v1","env":["OPENAI_API_KEY"],
        "models":{"gpt-4o":{},"gpt-4o-mini":{}}},
      "anthropic": {"api":"https://api.anthropic.com/v1","env":["ANTHROPIC_API_KEY"],
        "models":{"claude-sonnet-4":{}}}
    }"#;

    #[test]
    fn discovers_when_upstream_lists() {
        let c = LiveCatalog::default();

        assert_eq!(c.refresh(100, || Ok(UPSTREAM.to_owned())).unwrap(), 2);
        assert_eq!(c.providers(), 2);
        assert_eq!(
            c.catalog()["openai"].models,
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
}
