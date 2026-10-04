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
/// Every wire but Gemini's addresses a collection of models at one fixed path, so
/// [`chat_url`] is right for all of them and for the 188 registry entries that
/// already spell that path out. Gemini is the exception and the reason this
/// function exists: its `base_url` is the *collection* (`.../v1beta/models`) and
/// each model is addressed as a **method on a member**,
/// `.../v1beta/models/{model}:generateContent`. Joining `/chat/completions` onto
/// that stem asks for a path the API does not have, which it answers `404` — a
/// model that is present and reachable, reported as missing.
///
/// An empty `model` falls back to [`chat_url`] rather than producing a URL ending
/// in a bare `:generateContent`: dispatch's own `upstream_model` doc allows an
/// empty value to mean "send the caller's spelling", and a nameless member path
/// cannot express that.
#[must_use]
pub fn dispatch_url(wire: crate::WireFormat, base: &str, model: &str) -> String {
    let stem = strip_trailing_slashes(base.trim());
    match wire {
        crate::WireFormat::Gemini if !model.is_empty() => {
            let mut url = String::with_capacity(stem.len() + model.len() + 18);
            url.push_str(stem);
            url.push('/');
            url.push_str(model);
            url.push_str(":generateContent");
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
            ),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-3.8-flash:generateContent"
        );
        // Trailing slashes are trimmed the same way every other join is.
        assert_eq!(
            dispatch_url(
                WireFormat::Gemini,
                "https://generativelanguage.googleapis.com/v1beta/models///",
                "gemini-3.8-flash",
            ),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-3.8-flash:generateContent"
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
                dispatch_url(wire, "https://api.x.com/v1", "m-1"),
                "https://api.x.com/v1/chat/completions",
                "{wire:?} must not take the Gemini member path"
            );
        }
    }

    #[test]
    fn gemini_without_a_model_falls_back_rather_than_emitting_a_bare_method() {
        use crate::WireFormat;
        assert_eq!(
            dispatch_url(
                WireFormat::Gemini,
                "https://generativelanguage.googleapis.com/v1beta/models",
                ""
            ),
            "https://generativelanguage.googleapis.com/v1beta/models/chat/completions"
        );
    }
}
