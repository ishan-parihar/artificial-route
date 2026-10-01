//! Per-provider metadata: how to authenticate, how big the window is, which
//! other protocols a provider speaks.
//!
//! Generated from `../OmniRoute/open-sse/config/providers/registry/*/index.ts` by
//! `ar import --from omniroute`, embedded here with `include_str!` and parsed
//! once, lazily. Every field is something a provider entry *declares* and this
//! crate does not interpret: what a dispatcher needs to send, and how big a
//! candidate's context window is.
//!
//! # Why this is a second file rather than fields on `ProviderDef`
//!
//! `ProviderDef` is the shape a dispatcher dispatches through: a base URL, a
//! wire format, an auth class, a model list, prices. It is also built
//! field-by-field in several places outside this crate, so adding a field to it
//! is a workspace-wide change for a value only a reader here needs.
//!
//! The table is keyed by provider id rather than folded into the registry, which
//! also keeps both documents single-shape: `registry.json` parses as a
//! `BTreeMap<Strng, ProviderDef>` and this one as a
//! `BTreeMap<Strng, ProviderMeta>`, with no wrapper branch in either loader.
//!
//! Every field carries `skip_serializing_if`, because the file is
//! `include_str!`-ed into every binary: an `authHeader: "bearer"` on the ~200
//! entries that only restate the default is `.rodata` for nothing.
//!
//! # Lookup, and the two questions this table answers
//!
//! `MetaCatalog::get` answers "what does this provider declare". The other
//! question is "is this name dispatchable and what does it cost", and it needs
//! both tables: an alias (`cc`) resolves to a provider *here*, and whether that
//! provider is flat-rate is a fact `registry.json` carries.
//! `MetaCatalog::is_flat_rate` takes both, because the composition is the fact
//! and neither half is.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use ar_core::Strng;
use serde::{Deserialize, Serialize};

use crate::Registry;

/// The generated metadata table, compact JSON for the same reason
/// `registry.json` is: `include_str!`-ed into every binary linking this crate,
/// so whitespace is `.rodata`.
/// A placeholder until `ar import` has run; the count assertion in the tests is
/// what turns an un-regenerated file into a build failure rather than a silently
/// empty table.
const PROVIDER_META_JSON: &str = include_str!("providerMeta.json");

/// The credential header a provider gets when the catalog names none.
/// /
/// `bearer` is the spelling upstream's entry builder defaults to
/// (`buildOpenAiCompatibleRegistryEntry`), so it is the right answer for the
/// ~200 entries that declare nothing.
pub const DEFAULT_AUTH_HEADER: &str = "bearer";

/// Everything a provider declares beyond its base URL, wire format and prices.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderMeta {
    /// OmniRoute's short id for this provider, when it has one.
    ///
    /// Not an alternative key: `Registry::get` resolves the canonical id only,
    /// because two providers may claim one alias. Recorded so a name that
    /// arrived through a config or an upstream list can be mapped back to the
    /// entry that owns it — `cc` is `claude`.
    #[serde(default, skip_serializing_if = "is_blank")]
    pub alias: Strng,
    /// The header the credential travels in, in `registry.json` spelling.
    ///
    /// `bearer` (the default) means `Authorization: Bearer …`; `x-api-key` and
    /// `x-goog-api-key` are the two non-bearer shapes upstream dispatches, and
    /// `kimi-coding` is the provider here that needs the first. `cookie` names a
    /// browser-session header rather than an API key, which is what the
    /// web-cookie providers need.
    ///
    /// Carried verbatim rather than resolved into an enum, for the same reason
    /// `ProviderDef::auth_kind` is: the set of shapes grows with the provider
    /// set, and a spelling this build cannot dispatch is still the spelling
    /// `ar doctor` should name.
    #[serde(default, skip_serializing_if = "is_blank")]
    pub auth_header: Strng,
    /// Origin override used only for the Responses-API shape.
    ///
    /// Providers that publish a `/v1/responses` endpoint on a different origin
    /// from their chat endpoint (`xai`, `github`, `cheaperinference`). Empty for
    /// every provider whose Responses requests go to the chat base URL.
    #[serde(default, skip_serializing_if = "is_blank")]
    pub responses_base_url: Strng,
    /// Every additional wire protocol this provider accepts, in
    /// `registry.json` spelling.
    ///
    /// Recorded in full rather than reduced to "also speaks claude", because the
    /// alternates are not interchangeable: each carries its own base URL, auth
    /// header and extra headers, and picking one is a per-connection decision
    /// (`providerSpecificData.targetFormat` upstream). The primary `WireFormat`
    /// stays the default and is *not* repeated here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alternate_formats: Vec<Strng>,
    /// A literal key sent as the bearer token when the request has no real
    /// credential, so a primarily-authenticated provider's free tier works
    /// anonymously.
    ///
    /// Two providers declare one: `kilocode` (`anonymous`) and `aihorde`
    /// (`0000000000`, AI Horde's documented anonymous key). Not a secret and not
    /// a bypass — it is a value the provider publishes, and the authenticated
    /// path is unaffected. Empty for every other entry.
    #[serde(default, skip_serializing_if = "is_blank")]
    pub anonymous_api_key: Strng,
    /// The largest context window any of this provider's models declares, in
    /// tokens; `0` when the catalog names none.
    ///
    /// A ceiling, not a per-model figure: `openai` spans 128K to 1M, so a single
    /// number cannot size a request and a routing decision must not read it as
    /// one. It is the *filter* a candidate context window is checked against —
    /// "can this provider hold a 900K prompt at all" — which is a per-provider
    /// question, and the largest declared value is the only honest answer.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub context_length: u32,
    /// The largest explicit maximum input-token budget any model declares, in
    /// tokens; `0` when none does.
    ///
    /// Distinct from `context_length`: this is the input ceiling when it is
    /// *smaller* than the window, because the backend reserves part of the
    /// window for output. Four providers declare one upstream.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub max_input_tokens: u32,
}

/// `skip_serializing_if` helper: keeps an unset name out of the JSON.
/// /
/// A `&str` method rather than `Strng::is_empty`, which resolves against
/// `ExactSizeIterator` on `Arc<str>` and does not compile.
fn is_blank(v: &Strng) -> bool {
    v.is_empty()
}

/// `skip_serializing_if` helper: keeps a `0` token ceiling out of the JSON.
fn is_zero(v: &u32) -> bool {
    *v == 0
}

/// The whole generated table, keyed by provider id.
#[derive(Debug, Clone, Default)]
pub struct MetaCatalog {
    by_id: BTreeMap<Strng, ProviderMeta>,
}

impl MetaCatalog {
    /// Number of providers the table describes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    /// Whether the table is empty. Never true for a valid build.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// What `id` declares.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&ProviderMeta> {
        self.by_id.get(id)
    }

    /// Iterates `(id, meta)` pairs in id order.
    pub fn iter(&self) -> impl Iterator<Item = (&Strng, &ProviderMeta)> {
        self.by_id.iter()
    }

    /// The provider whose entry declares `alias` as its short id.
    ///
    /// OmniRoute's flat-rate set names `cc`, the alias of `claude`, because a
    /// config may spell that provider either way. The catalog keys on the
    /// canonical id, so an alias-named entry has to be resolvable to the id that
    /// owns it — otherwise `cc` would be a name nothing in the binary
    /// recognises.
    ///
    /// Linear over the table, and that is the point: it is read once per config
    /// load, not on a request path.
    #[must_use]
    pub fn id_for_alias(&self, alias: &str) -> Option<&str> {
        self.by_id
            .iter()
            .find(|(_, m)| m.alias.as_ref() == alias)
            .map(|(id, _)| id.as_ref())
    }

    /// The credential header `id` dispatches with, and whether it is the bearer
    /// default.
    ///
    /// `(name, is_bearer)`, so the caller builds the value rather than this
    /// crate handing out a header map: prefixing is the dispatcher's job, and
    /// `x-api-key` takes the raw key where `bearer` takes `Bearer <key>`. An id
    /// the table does not carry answers with the default, so a config-declared
    /// node dispatches the same way a catalog one does.
    #[must_use]
    pub fn auth_header(&self, id: &str) -> (&str, bool) {
        match self.get(id).map(|m| m.auth_header.as_ref()) {
            Some(name) if !name.is_empty() => (name, name == DEFAULT_AUTH_HEADER),
            _ => (DEFAULT_AUTH_HEADER, true),
        }
    }

    /// The Responses-API origin for `id`, falling back to its chat base URL.
    ///
    /// `None` only for an id neither table carries, so a caller can tell "no
    /// such provider" from "same origin as chat".
    #[must_use]
    /// Two tables, so the borrow outlives both: the override lives in this one and
    /// the fallback in the other.
    pub fn responses_base_url<'a>(&'a self, id: &str, registry: &'a Registry) -> Option<&'a str> {
        let def = registry.get(id)?;
        let override_url = self
            .get(id)
            .map(|m| m.responses_base_url.as_ref())
            .unwrap_or_default();
        Some(if override_url.is_empty() {
            def.base_url.as_str()
        } else {
            override_url
        })
    }

    /// Whether `id` is billed flat, by canonical id or by alias.
    ///
    /// The composition of two tables and neither half is the answer: `cc` is a
    /// name this table resolves to a provider, and whether that provider is
    /// flat-rate is a fact `registry.json` carries.
    #[must_use]
    pub fn is_flat_rate(&self, registry: &Registry, id: &str) -> bool {
        let canonical = if registry.get(id).is_some() {
            Some(id)
        } else {
            self.id_for_alias(id)
        };
        canonical
            .and_then(|c| registry.get(c))
            .is_some_and(|d| d.flat_rate)
    }
}

/// The process-wide table, parsed on first call.
/// /
/// Lazy on the same reasoning as `crate::global`: the parse is not on the
/// `--version` fast path, and nothing on a dispatch path needs it.
/// /
/// # Panics
/// /
/// Panics if the embedded table is malformed. It is a generated source file, not
/// user input, so a panic at first use is the honest signal; it cannot be
/// triggered by a request.
#[must_use]
pub fn global() -> &'static MetaCatalog {
    static TABLE: OnceLock<MetaCatalog> = OnceLock::new();
    TABLE.get_or_init(|| {
        let by_id: BTreeMap<Strng, ProviderMeta> = serde_json::from_str(PROVIDER_META_JSON)
            .expect("embedded providerMeta.json is malformed");
        MetaCatalog { by_id }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A row with only the two fields the alias tests read.
    fn meta(alias: &str, auth_header: &str) -> ProviderMeta {
        ProviderMeta {
            alias: Strng::from(alias),
            auth_header: Strng::from(auth_header),
            ..ProviderMeta::default()
        }
    }

    /// A catalog from `(meta, id)` pairs, keyed by id.
    fn catalog(rows: Vec<(ProviderMeta, &str)>) -> MetaCatalog {
        MetaCatalog {
            by_id: rows
                .into_iter()
                .map(|(m, id)| (Strng::from(id), m))
                .collect(),
        }
    }

    #[test]
    fn reports_the_auth_header_a_provider_dispatches_with() {
        // The case the field exists for: `kimi-coding` sends its key as
        // `x-api-key`, so a bearer-only dispatcher would 401 against it.
        let (header, is_bearer) = global().auth_header("kimi-coding");
        assert_eq!(header, "x-api-key");
        assert!(
            !is_bearer,
            "x-api-key takes the raw key, not `Bearer <key>`"
        );
    }

    #[test]
    fn defaults_the_auth_header_to_bearer_when_the_catalog_names_none() {
        let (header, is_bearer) = global().auth_header("openai");
        assert_eq!(header, DEFAULT_AUTH_HEADER);
        assert!(is_bearer);
    }

    #[test]
    fn falls_back_to_bearer_for_an_id_the_table_lacks() {
        // A config-declared node dispatches the same way a catalog one does, so
        // a miss must not be a special case at the call site.
        let (header, is_bearer) = global().auth_header("no-such-provider");
        assert_eq!(header, DEFAULT_AUTH_HEADER);
        assert!(is_bearer);
    }

    #[test]
    fn reads_a_cookie_auth_header_for_a_web_session_provider() {
        // A browser-session provider does not take an API key at all, so the
        // header name is the fact a dispatcher needs and there is no bearer form.
        let (header, is_bearer) = global().auth_header("grok-web");
        assert_eq!(header, "cookie");
        assert!(!is_bearer);
    }

    #[test]
    fn resolves_the_cc_alias_to_the_claude_subscription() {
        // `cc` is in the upstream flat-rate list and names no provider; it is
        // `claude`'s short id. Without the alias, a config spelling it either way
        // would price the same plan two different ways.
        let meta = global();
        assert_eq!(meta.id_for_alias("cc"), Some("claude"));
        let registry = crate::global();
        assert!(meta.is_flat_rate(registry, "cc"));
        assert!(meta.is_flat_rate(registry, "claude"));
    }

    #[test]
    fn answers_no_flat_rate_for_an_alias_no_entry_declares() {
        let meta = catalog(vec![(meta("cc", "bearer"), "claude")]);
        let registry = crate::global();
        assert_eq!(meta.id_for_alias("nope"), None);
        assert!(!meta.is_flat_rate(registry, "nope"));
    }

    #[test]
    fn reads_the_anonymous_api_key_for_the_two_providers_that_publish_one() {
        // Not secrets and not bypasses: values the providers publish so a
        // primarily-authenticated gateway can serve its free tier with no
        // account at all.
        assert_eq!(
            global()
                .get("kilocode")
                .map(|m| m.anonymous_api_key.as_ref()),
            Some("anonymous")
        );
        assert_eq!(
            global()
                .get("aihorde")
                .map(|m| m.anonymous_api_key.as_ref()),
            Some("0000000000")
        );
    }

    #[test]
    fn prefers_a_responses_origin_override_over_the_chat_base_url() {
        // `xai` serves chat on one path and `/v1/responses` on another; a
        // dispatcher that reused the chat path would 404 the Responses shape.
        let registry = crate::global();
        assert_eq!(
            global().responses_base_url("xai", registry),
            Some("https://api.x.ai/v1/responses")
        );
        // A provider with no override answers on its own base URL.
        assert_eq!(
            global().responses_base_url("openai", registry),
            registry.get("openai").map(|d| d.base_url.as_str())
        );
        assert_eq!(
            global().responses_base_url("no-such-provider", registry),
            None
        );
    }

    #[test]
    fn records_every_declared_alternate_protocol() {
        // `deepseek` also speaks the Anthropic and Responses shapes, and each
        // alternate carries its own URL and auth header, so the set is recorded
        // in full rather than as a yes/no.
        let got: Vec<&str> = global()
            .get("deepseek")
            .expect("deepseek is in the table")
            .alternate_formats
            .iter()
            .map(|f| f.as_ref())
            .collect();
        assert_eq!(got, vec!["claude", "openai-responses"]);
    }

    #[test]
    fn carries_a_candidate_context_window_for_the_named_providers() {
        // The per-provider ceiling a candidate is filtered against. `openai` spans
        // 128K to 1M upstream, so the largest is the only honest single number.
        for (id, floor) in [
            ("openai", 1_000_000u32),
            ("anthropic", 200_000),
            ("kimi-coding", 1_048_576),
        ] {
            let ctx = global().get(id).map(|m| m.context_length).unwrap_or(0);
            assert!(ctx >= floor, "{id} context_length {ctx} below {floor}");
        }
    }

    #[test]
    fn carries_an_explicit_input_budget_separately_from_the_window() {
        // `codex` reserves part of its window for output, so its input ceiling is
        // smaller than the window and the two must not be conflated.
        let codex = global().get("codex").expect("codex is in the table");
        assert_eq!(codex.max_input_tokens, 272_000);
        assert!(
            codex.context_length > codex.max_input_tokens,
            "the window exceeds the input budget"
        );
    }

    #[test]
    fn ships_the_full_metadata_table() {
        let g = global();
        assert!(g.len() >= 250, "table holds {} providers", g.len());
        for id in [
            "openai",
            "kimi-coding",
            "aihorde",
            "kilocode",
            "xai",
            "deepseek",
        ] {
            assert!(g.get(id).is_some(), "{id} is in the table");
        }
    }

    #[test]
    fn names_an_alias_for_most_but_not_every_provider() {
        // 263 of 276 entries declare one. A table where every row had an alias
        // would mean the reader was inventing short ids rather than reading them.
        let with_alias = global().iter().filter(|(_, m)| !m.alias.is_empty()).count();
        assert!(
            (250..=270).contains(&with_alias),
            "{with_alias} entries carry an alias"
        );
    }

    #[test]
    fn round_trips_a_meta_through_json() {
        let json = serde_json::to_string(&meta("cc", "x-api-key")).unwrap();
        let back: ProviderMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(back.alias.as_ref(), "cc");
        assert_eq!(back.auth_header.as_ref(), "x-api-key");
    }

    #[test]
    fn omits_the_unset_fields_from_the_generated_json() {
        // The file is `include_str!`-ed into every binary, so a field meaning
        // "unchanged" written 253 times is `.rodata` for nothing.
        assert_eq!(
            serde_json::to_string(&ProviderMeta::default()).unwrap(),
            "{}"
        );
    }

    #[test]
    fn reads_an_empty_table_without_panicking() {
        let empty = MetaCatalog::default();
        assert!(empty.is_empty());
        assert_eq!(empty.get("openai"), None);
    }
}
