//! Upstream URL construction.
//!
//! Ports OmniRoute's `utils/urlSanitize.ts` join rule
//! (`normalizeBaseUrl` -> strip trailing slashes -> append path). Its exact
//! semantics, kept because they are load-bearing:
//!
//! * trailing slashes are stripped in a **loop**, so `https://h//` -> `https://h`;
//! * interior duplicate slashes are **never** collapsed;
//! * whitespace is trimmed off the base first;
//! * the appended path carries its own leading `/`, so a caller-supplied path
//!   missing one produces a malformed URL rather than being silently repaired.
//!
//! The last point is why [`chat_url`] appends the path itself instead of
//! accepting one: at P0 there is one path, so taking it as an argument would
//! only add a way to get this wrong.
//!
//! ## The join is idempotent on an endpoint
//!
//! `ar_registry`'s `base_url` is OmniRoute's `baseUrl` verbatim, and there
//! `baseUrl` is the **full endpoint**, not a stem: `providers/poe/index.ts`
//! assigns `baseUrl: POE_CHAT_COMPLETIONS_URL`, a value already ending in
//! `/chat/completions`. 188 of the 276 registry entries are shaped that way
//! (`nvidia` -> `.../v1/chat/completions`, `openrouter` -> `.../v1/chat/completions`),
//! and the other 88 are stems (`gemini` -> `.../v1beta/models`).
//!
//! So both conventions occur in one catalog and appending unconditionally
//! requests `.../v1/chat/completions/chat/completions`, which every one of those
//! 188 hosts answers `404`. Appending only when the stem does not already end in
//! the path is the one rule that is right for both.

/// Path appended to a provider's base URL for chat completions.
const CHAT_PATH: &str = "/chat/completions";

/// Builds the chat-completions URL for `base`.
///
/// `ar_registry::ProviderDef::base_url` documents itself as trailing-slash
/// free, but the value is parsed out of a user-authored `registry.json`, so the
/// trim is a trust-boundary check rather than a redundant one.
///
/// ```
/// use ar_exec::url::chat_url;
///
/// assert_eq!(chat_url("https://api.x.com/v1"), "https://api.x.com/v1/chat/completions");
/// assert_eq!(chat_url("https://api.x.com/v1//"), "https://api.x.com/v1/chat/completions");
/// // An entry that already names the endpoint is used as-is, not doubled.
/// assert_eq!(
///     chat_url("https://integrate.api.nvidia.com/v1/chat/completions"),
///     "https://integrate.api.nvidia.com/v1/chat/completions",
/// );
/// ```
pub fn chat_url(base: &str) -> String {
    endpoint_url(base, CHAT_PATH)
}

/// The URL a dispatch actually POSTs to, which is not always a chat-completions
/// URL.
///
/// `endpoint` overrides the wire's own path rule: an absolute path is joined
/// onto `base` under [`endpoint_url`]'s idempotent rule, and an absolute URL
/// replaces the whole thing. That is the one spelling a non-chat endpoint
/// needs — a decisions router answers at `/api/alpha/decisions`, and pointing
/// a provider's base URL there would still get `/chat/completions` appended.
/// `None` — every compiled-in entry — keeps the rule below.
///
/// Every wire but Gemini's addresses a collection of models at one fixed path, so
/// [`chat_url`] is right for all of them and for the 188 registry entries that
/// already spell that path out. Gemini is the exception and the reason this
/// function exists: its `base_url` is the *collection* (`.../v1beta/models`) and
/// each model is addressed as a **method on a member**,
/// `.../v1beta/models/{model}:generateContent`. Joining `/chat/completions` onto
/// that stem asks for a path the API does not have, which it answers `404` — a
/// model that is present and reachable, reported as missing.
///
/// A `stream` call takes `:streamGenerateContent?alt=sse` instead, so the answer
/// arrives as SSE frames rather than one JSON object.
///
/// An empty `model` falls back to [`chat_url`] rather than producing a URL ending
/// in a bare `:generateContent`: dispatch's own `upstream_model` doc allows an
/// empty value to mean "send the caller's spelling", and a nameless member path
/// cannot express that.
#[must_use]
pub fn dispatch_url(
    wire: crate::WireFormat,
    base: &str,
    model: &str,
    endpoint: Option<&str>,
    stream: bool,
) -> String {
    let stem = strip_trailing_slashes(base.trim());
    if let Some(endpoint) = endpoint {
        // An absolute endpoint is a whole URL: joining it onto anything would
        // double the scheme. A path carries its own leading `/`, matching
        // `endpoint_url`'s contract.
        if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
            return endpoint.to_owned();
        }
        return endpoint_url(base, endpoint);
    }
    match wire {
        crate::WireFormat::Gemini if !model.is_empty() => {
            // A streamed call takes Gemini's SSE method, whose frames the lift in
            // `ar-translate` reads; the whole-body method answers one JSON object.
            let method = if stream {
                ":streamGenerateContent?alt=sse"
            } else {
                ":generateContent"
            };
            let mut url = String::with_capacity(stem.len() + model.len() + method.len() + 1);
            url.push_str(stem);
            url.push('/');
            url.push_str(model);
            url.push_str(method);
            url
        }
        _ => chat_url(base),
    }
}

/// Joins `path` onto a provider's base URL under the same rule [`chat_url`] uses.
///
/// The media family adds four more endpoints, and duplicating the trim-and-join
/// for each would be four chances to spell the same rule differently. `path`
/// must carry its own leading `/`, matching [`chat_url`]: a caller that omits it
/// gets a malformed URL rather than a silently repaired one.
///
/// ```
/// use ar_exec::url::endpoint_url;
///
/// assert_eq!(endpoint_url("https://api.x.com/v1/", "/embeddings"), "https://api.x.com/v1/embeddings");
/// // Already an endpoint: returned unchanged.
/// assert_eq!(
///     endpoint_url("https://agentrouter.org/v1/messages", "/v1/messages"),
///     "https://agentrouter.org/v1/messages",
/// );
/// ```
#[must_use]
pub fn endpoint_url(base: &str, path: &str) -> String {
    let stem = strip_trailing_slashes(base.trim());
    // Both conventions live in one catalog (see the module docs), so the join is
    // a no-op on a stem that already names this endpoint. Compared after the
    // trailing-slash strip, so `.../chat/completions//` still matches.
    if stem.ends_with(path) {
        return stem.to_owned();
    }
    let mut url = String::with_capacity(stem.len() + path.len());
    url.push_str(stem);
    url.push_str(path);
    url
}

/// Strips every trailing `/` from `value`.
///
/// The loop (rather than a single-char replace) matters: OmniRoute's
/// `buildUrl` uses a one-shot `replace(/\/$/, "")` that leaves `https://h//`
/// as `https://h/`, which is a second defect this avoids.
fn strip_trailing_slashes(value: &str) -> &str {
    let mut end = value.len();
    while end > 0 && value.as_bytes()[end - 1] == b'/' {
        end -= 1;
    }
    &value[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_every_trailing_slash_when_joined() {
        assert_eq!(
            chat_url("https://api.x.com/v1///"),
            "https://api.x.com/v1/chat/completions"
        );
    }

    #[test]
    fn trims_whitespace_before_joining() {
        assert_eq!(
            chat_url("  https://api.x.com/v1/  "),
            "https://api.x.com/v1/chat/completions"
        );
    }

    #[test]
    fn keeps_interior_duplicate_slashes_when_joined() {
        assert_eq!(
            chat_url("https://api.x.com//v1"),
            "https://api.x.com//v1/chat/completions"
        );
    }

    #[test]
    fn reduces_slash_only_input_to_empty() {
        assert_eq!(strip_trailing_slashes("///"), "");
    }

    #[test]
    fn joins_endpoint_path_when_base_trailing_slashes_trimmed() {
        assert_eq!(
            endpoint_url("https://api.x.com/v1///", "/ocr"),
            "https://api.x.com/v1/ocr"
        );
    }

    #[test]
    fn leaves_a_base_that_already_names_the_endpoint_alone() {
        // The registry carries both conventions; joining unconditionally asks
        // nvidia for /v1/chat/completions/chat/completions, which it 404s.
        for (base, path) in [
            (
                "https://integrate.api.nvidia.com/v1/chat/completions",
                "/chat/completions",
            ),
            ("https://agentrouter.org/v1/messages", "/v1/messages"),
            ("https://api.z.ai/api/anthropic/v1/messages", "/v1/messages"),
        ] {
            assert_eq!(endpoint_url(base, path), base, "{base} was doubled");
        }
    }

    #[test]
    fn matches_an_endpoint_base_through_trailing_slashes() {
        assert_eq!(
            chat_url("https://integrate.api.nvidia.com/v1/chat/completions///"),
            "https://integrate.api.nvidia.com/v1/chat/completions"
        );
    }

    #[test]
    fn still_appends_when_the_stem_is_a_genuine_prefix() {
        // Not a prefix test: `.../chat/completions` must not swallow a real stem
        // that merely contains the path earlier on.
        assert_eq!(
            chat_url("https://api.x.com/v1"),
            "https://api.x.com/v1/chat/completions"
        );
        assert_eq!(
            endpoint_url("https://api.x.com/chat/completions/v1", "/chat/completions"),
            "https://api.x.com/chat/completions/v1/chat/completions"
        );
    }

    #[test]
    fn gemini_addresses_a_member_by_method_not_a_fixed_path() {
        use crate::WireFormat;
        assert_eq!(
            dispatch_url(
                WireFormat::Gemini,
                "https://generativelanguage.googleapis.com/v1beta/models",
                "gemini-3.8-flash",
                None,
                false,
            ),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-3.8-flash:generateContent"
        );
        // Trailing slashes are trimmed the same way every other join is.
        assert_eq!(
            dispatch_url(
                WireFormat::Gemini,
                "https://generativelanguage.googleapis.com/v1beta/models///",
                "gemini-3.8-flash",
                None,
                false,
            ),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-3.8-flash:generateContent"
        );
    }

    #[test]
    fn gemini_stream_should_address_the_sse_method() {
        use crate::WireFormat;
        assert_eq!(
            dispatch_url(
                WireFormat::Gemini,
                "https://generativelanguage.googleapis.com/v1beta/models",
                "gemini-3.8-flash",
                None,
                true,
            ),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-3.8-flash:streamGenerateContent?alt=sse"
        );
    }

    #[test]
    fn every_other_wire_still_uses_the_chat_path() {
        use crate::WireFormat;
        for wire in [
            WireFormat::Openai,
            WireFormat::Anthropic,
            WireFormat::OpenaiResponses,
            WireFormat::Antigravity,
            WireFormat::Cursor,
        ] {
            assert_eq!(
                dispatch_url(wire, "https://api.x.com/v1", "m-1", None, false),
                "https://api.x.com/v1/chat/completions",
                "{wire:?} must not take the Gemini member path"
            );
        }
    }

    #[test]
    fn an_endpoint_path_joins_onto_the_base() {
        use crate::WireFormat;
        // The decisions shape: a non-chat endpoint at a path the base does not
        // carry. Appending the wire's path here is a 404 the journal would
        // blame on the provider.
        assert_eq!(
            dispatch_url(
                WireFormat::Openai,
                "https://openrouter.ai",
                "respan/span-01-lite:free",
                Some("/api/alpha/decisions"),
                false
            ),
            "https://openrouter.ai/api/alpha/decisions"
        );
    }

    #[test]
    fn an_absolute_endpoint_url_replaces_the_base_entirely() {
        use crate::WireFormat;
        assert_eq!(
            dispatch_url(
                WireFormat::Openai,
                "https://ignored.example/v1",
                "m",
                Some("https://api.experientiallabs.ai/v1/systemone"),
                false
            ),
            "https://api.experientiallabs.ai/v1/systemone"
        );
    }

    #[test]
    fn an_endpoint_path_that_the_base_already_carries_is_not_doubled() {
        use crate::WireFormat;
        // Same idempotence rule as the chat path: an operator who writes the
        // full endpoint into both fields gets one endpoint, not two.
        assert_eq!(
            dispatch_url(
                WireFormat::Openai,
                "https://openrouter.ai/api/alpha/decisions",
                "m",
                Some("/api/alpha/decisions"),
                false
            ),
            "https://openrouter.ai/api/alpha/decisions"
        );
    }

    #[test]
    fn an_endpoint_overrides_even_the_gemini_member_path() {
        // The override outranks every wire rule, including the one exception
        // that predates it — an explicit endpoint is the operator saying the
        // wire's addressing is wrong for this provider.
        use crate::WireFormat;
        assert_eq!(
            dispatch_url(
                WireFormat::Gemini,
                "https://api.x.com",
                "gemini-3.8-flash",
                Some("/v1/systemone"),
                false
            ),
            "https://api.x.com/v1/systemone"
        );
    }

    #[test]
    fn gemini_without_a_model_falls_back_rather_than_emitting_a_bare_method() {
        use crate::WireFormat;
        assert_eq!(
            dispatch_url(
                WireFormat::Gemini,
                "https://generativelanguage.googleapis.com/v1beta/models",
                "",
                None,
                false
            ),
            "https://generativelanguage.googleapis.com/v1beta/models/chat/completions"
        );
    }
}
