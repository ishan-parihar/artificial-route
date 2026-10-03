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
/// ```
pub fn chat_url(base: &str) -> String {
    endpoint_url(base, CHAT_PATH)
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
/// ```
#[must_use]
pub fn endpoint_url(base: &str, path: &str) -> String {
    let stem = strip_trailing_slashes(base.trim());
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
}
