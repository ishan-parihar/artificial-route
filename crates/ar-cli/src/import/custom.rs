//! Custom-provider reproduction from OmniRoute's `provider_connections` table.
//!
//! Without this the import is not portable: a combo step names a provider by the
//! id `provider_connections.provider` carries, and every ad-hoc
//! `openai-compatible-*` node lives there and nowhere else. The provider tree
//! under `open-sse/config/providers` has no row for it, so an import that reads
//! only the tree emits a combo pointing at a provider the generated config does
//! not define — and the server refuses to start with "names provider ..., which
//! is not in the registry". That is the one failure mode that makes the whole
//! generated file unusable rather than merely incomplete.
//!
//! One `CustomProvider` per distinct provider id, built from that provider's
//! rows. Several connections to the same node (the common case: one base URL,
//! several keys) collapse to one entry, because `ar` addresses the credential
//! through `keys:` and the operator owns the choice of which key — a node with
//! five identical base URLs is one provider, not five.

use std::collections::BTreeMap;
use std::path::Path;

use ar_core::Strng;
use ar_registry::CustomProvider;

/// One row of `provider_connections`, narrowed to what a node is built from.
#[derive(Debug, serde::Deserialize)]
struct Connection {
    provider: String,
    provider_specific_data: Option<String>,
}

/// The `provider_specific_data` blob, for the handful of fields a node needs.
#[derive(Debug, Default, serde::Deserialize)]
struct ConnectionData {
    /// Spelled `baseUrl` in the store. Without this rename serde looks for
    /// `base_url`, matches nothing, and every node reads as base-URL-less —
    /// a silent failure that shows up only as an unroutable combo step.
    #[serde(default, rename = "baseUrl")]
    base_url: Option<String>,
    // `apiType` is deliberately NOT read: every node under the
    // `openai-compatible%` filter is a chat node, and `NODE_PROTOCOL` says so
    // without a field nothing consumes.
}

/// Reads every custom (non-registry) provider from an OmniRoute `storage.sqlite`.
///
/// Absent table, absent file, or an unreadable row is a note, never a failure:
/// the provider tree is the primary output and a store this build cannot read
/// must not make it unimportable. Same contract as `combos::read`.
pub(crate) fn read(path: &Path) -> Vec<CustomProvider> {
    let Ok(conn) = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    ) else {
        eprintln!(
            "note: cannot open {}; ad-hoc custom providers are not imported",
            path.display()
        );
        return Vec::new();
    };
    let query = concat!(
        "SELECT provider, provider_specific_data FROM provider_connections ",
        "WHERE is_active = 1 AND provider LIKE 'openai-compatible%'"
    );
    let Ok(mut stmt) = conn.prepare(query) else {
        eprintln!(
            "note: {} has no readable `provider_connections` table; ad-hoc custom providers are not imported",
            path.display()
        );
        return Vec::new();
    };
    let rows = stmt.query_map([], |r| {
        Ok(Connection {
            provider: r.get(0)?,
            provider_specific_data: r.get(1)?,
        })
    });
    let Ok(rows) = rows else {
        eprintln!(
            "note: cannot read `provider_connections` in {}",
            path.display()
        );
        return Vec::new();
    };

    // Several rows per provider: the first readable one that yields a usable
    // base URL wins, and the rest are the same endpoint with different keys.
    let mut by_provider: BTreeMap<Strng, CustomProvider> = BTreeMap::new();
    let mut skipped: Vec<Strng> = Vec::new();
    for row in rows.flatten() {
        let id = Strng::from(row.provider.trim());
        if id.is_empty() || by_provider.contains_key(&id) {
            continue;
        }
        let data = row
            .provider_specific_data
            .as_deref()
            .and_then(|raw| serde_json::from_str::<ConnectionData>(raw).ok())
            .unwrap_or_default();
        let Some(base_url) = data
            .base_url
            .as_deref()
            .map(str::trim)
            .filter(|u| !u.is_empty())
        else {
            skipped.push(id);
            continue;
        };
        by_provider.insert(
            id,
            CustomProvider {
                id: row.provider.trim().to_owned(),
                protocol: NODE_PROTOCOL,
                base_url: base_url.to_owned(),
                // The env var is a *reference* into `keys:`, and the importer
                // emits a key per provider id. The credential itself never
                // leaves sqlite: `render_yaml` writes `$NAME`, never the value.
                key_ref: row.provider.trim().to_owned(),
                headers: BTreeMap::new(),
            },
        );
    }
    for id in skipped {
        eprintln!(
            "note: custom provider {id:?} declares no baseUrl; a combo step naming it will not dispatch"
        );
    }
    let out: Vec<CustomProvider> = by_provider.into_values().collect();
    eprintln!(
        "note: {} ad-hoc custom provider(s) read from provider_connections",
        out.len()
    );
    out
}

/// The dialect a node speaks.
///
/// OmniRoute records it in `apiType` and repeats it in the id, and the id prefix
/// is `openai-compatible-chat-*` for every node this importer reads. There is
/// only one spelling to map, so this is the constant rather than a match that
/// would look like it were discriminating: an `AnthropicCompatible` node would
/// need its own `provider_connections` filter above, and inventing one now would
/// emit a claim the store does not make.
const NODE_PROTOCOL: ar_registry::Protocol = ar_registry::Protocol::OpenaiCompatible;

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway store with the two rows an operator actually has: several
    /// keys to one node, and one key to another.
    fn db(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("storage.sqlite");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE provider_connections (
                provider TEXT, auth_type TEXT, is_active INTEGER,
                provider_specific_data TEXT
            );
            INSERT INTO provider_connections VALUES
                ('openai-compatible-chat-abc', 'apikey', 1,
                 '{\"apiType\":\"chat\",\"baseUrl\":\"https://one.example/v1\"}'),
                ('openai-compatible-chat-abc', 'apikey', 1,
                 '{\"apiType\":\"chat\",\"baseUrl\":\"https://one.example/v1\"}'),
                ('openai-compatible-chat-xyz', 'apikey', 1,
                 '{\"apiType\":\"chat\",\"baseUrl\":\"https://two.example/v1\"}'),
                ('openai-compatible-chat-dead', 'apikey', 1, '{}'),
                ('openai-compatible-chat-off', 'apikey', 0,
                 '{\"apiType\":\"chat\",\"baseUrl\":\"https://off.example/v1\"}');",
        )
        .unwrap();
        path
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ar-import-custom-{tag}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn should_collapse_several_keys_on_one_node_into_one_provider() {
        let dir = scratch("collapse");
        let path = db(&dir);
        let out = read(&path);
        assert_eq!(out.len(), 2, "two nodes, five rows: {out:?}");
        assert_eq!(out[0].id, "openai-compatible-chat-abc");
        assert_eq!(out[0].base_url, "https://one.example/v1");
        assert_eq!(out[1].id, "openai-compatible-chat-xyz");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn should_skip_an_inactive_or_baseurl_less_row_and_say_so() {
        let dir = scratch("skip");
        let path = db(&dir);
        let out = read(&path);
        let ids: Vec<&str> = out.iter().map(|c| c.id.as_str()).collect();
        assert!(
            !ids.contains(&"openai-compatible-chat-off"),
            "an inactive connection is not reachable: {ids:?}"
        );
        assert!(
            !ids.contains(&"openai-compatible-chat-dead"),
            "a row with no baseUrl cannot be dispatched: {ids:?}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn should_read_the_camel_case_base_url_the_store_actually_spells() {
        // `baseUrl`, not `base_url`. The failure mode is silent — every node
        // reads as base-URL-less and only an unroutable combo step gives it away.
        let dir = scratch("camel");
        let path = db(&dir);
        let out = read(&path);
        assert!(
            out.iter().any(|c| c.base_url == "https://one.example/v1"),
            "{out:?}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn should_point_the_key_ref_at_the_provider_itself() {
        // `render_yaml` emits one `keys:` entry per provider, named after it, so
        // the ref must be the id — a mismatch is a config that fails to expand
        // `$AR_KEY_...` at load and refuses to start.
        let dir = scratch("keyref");
        let path = db(&dir);
        let out = read(&path);
        for c in &out {
            assert_eq!(c.key_ref, c.id, "key_ref must match the emitted key");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn should_render_a_config_the_loader_actually_accepts() {
        // The whole point of this module: a combo step names a provider that
        // only `provider_connections` defines, so the generated file must carry
        // both the node and a `keys:` entry for its `key_ref`. Missing either is
        // a file that refuses to start, which is worse than an incomplete one.
        let dir = scratch("roundtrip");
        let path = db(&dir);
        let out = read(&path);
        let yaml = crate::import::render_yaml(&BTreeMap::new(), &[], &out);
        assert!(yaml.contains("custom_providers:"), "no node block:\n{yaml}");
        assert!(
            yaml.contains("key_ref: openai-compatible-chat-abc"),
            "no key_ref:\n{yaml}"
        );
        assert!(
            yaml.contains("openai-compatible-chat-abc: $AR_KEY_OPENAI_COMPATIBLE_CHAT_ABC"),
            "the generated key must carry the AR_KEY_ prefix every other key uses:\n{yaml}"
        );
        let parsed: ar_config::Config =
            serde_yaml::from_str(&yaml).expect("the generated yaml parses as a config");
        assert_eq!(
            parsed.custom_providers.len(),
            2,
            "{}",
            parsed.custom_providers.len()
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
