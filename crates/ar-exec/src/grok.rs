//! Grok Build's `cli-chat-proxy`, for the `grok-cli` family.
//!
//! The proxy speaks the OpenAI Responses wire, but it is stricter than OpenAI's
//! own endpoint: it refuses a few client identities, several body fields, and
//! some tool-output encodings a Codex or OpenAI-compatible client sends. This is
//! the port of OmniRoute's `grok-cli.ts` request path: the identity headers the
//! Grok Build CLI sends, and the body repairs its `transformRequest` applies, in
//! the same order.
//!
//! Not ported, because a plain chat does not reach them: foreign encrypted
//! reasoning stripping, root `anyOf` tool-schema normalization, Codex additional
//! tools, freeform and namespaced tool flattening, and the per-user `x-userid`
//! and `x-email` headers, which need account data this build does not carry.

use std::collections::BTreeMap;

use bytes::Bytes;
use serde_json::{Map, Value};

/// The Grok Build CLI release whose identity the proxy expects.
const CLIENT_VERSION: &str = "1.0.41";
const CLIENT_IDENTIFIER: &str = "grok-shell";
/// Sent as `X-XAI-Token-Auth`; the proxy checks it before it reads the bearer.
const TOKEN_AUTH: &str = "xai-grok-cli";
const REASONING_INCLUDE: &str = "reasoning.encrypted_content";
const DEFAULT_REASONING_EFFORT: &str = "high";
const SUPPORTED_REASONING_EFFORTS: [&str; 4] = ["low", "medium", "high", "xhigh"];
/// The one model that takes no `reasoning.effort` at all.
const NO_EFFORT_MODEL: &str = "grok-composer-2.5-fast";
/// The proxy refuses a request that carries more tools than this.
const MAX_TOOLS: usize = 200;
const UNSUPPORTED_PARAMS: [&str; 8] = [
    "presencePenalty",
    "frequencyPenalty",
    "logprobs",
    "topLogprobs",
    "presence_penalty",
    "frequency_penalty",
    "top_logprobs",
    "reasoning_effort",
];
/// `web_search` arguments the proxy rejects with `Argument not supported`. Codex
/// sends `external_web_access` on every turn.
const UNSUPPORTED_WEB_SEARCH_ARGS: [&str; 2] = ["external_web_access", "search_context_size"];

/// The identity headers the Grok Build CLI sends on every request.
///
/// `model` becomes `x-grok-model-override` when non-empty. `Content-Type`,
/// `Accept` and `Authorization` are the shared layer's job, not this one's.
pub(crate) fn client_headers(model: &str) -> BTreeMap<String, String> {
    // Node's `darwin`/`win32` and `arm64`/`x64` already match Rust's `macos`,
    // `windows`, `aarch64` and `x86_64`, so the platform names need no mapping.
    let mut headers = BTreeMap::from([
        (
            "x-grok-client-version".to_owned(),
            CLIENT_VERSION.to_owned(),
        ),
        (
            "x-grok-client-identifier".to_owned(),
            CLIENT_IDENTIFIER.to_owned(),
        ),
        ("x-grok-client-mode".to_owned(), "headless".to_owned()),
        (
            "User-Agent".to_owned(),
            format!(
                "{CLIENT_IDENTIFIER}/{CLIENT_VERSION} ({}; {})",
                std::env::consts::OS,
                std::env::consts::ARCH
            ),
        ),
        ("X-XAI-Token-Auth".to_owned(), TOKEN_AUTH.to_owned()),
        (
            "x-authenticateresponse".to_owned(),
            "authenticate-response".to_owned(),
        ),
    ]);
    if !model.is_empty() {
        headers.insert("x-grok-model-override".to_owned(), model.to_owned());
    }
    headers
}

/// Repairs a rendered Responses body into the shape the proxy accepts.
///
/// A body that is valid JSON but not an object is returned unchanged: the
/// renderer has already refused it.
///
/// # Errors
///
/// [`serde_json::Error`] when `body` is not JSON.
pub(crate) fn normalize_body(body: &[u8]) -> Result<Bytes, serde_json::Error> {
    let mut value: Value = serde_json::from_slice(body)?;
    let Some(request) = value.as_object_mut() else {
        return Ok(Bytes::copy_from_slice(body));
    };
    // The proxy's own Responses defaults: nothing is stored server-side.
    request.entry("store").or_insert(Value::Bool(false));
    let include = ensure_reasoning_include(request.remove("include"));
    request.insert("include".to_owned(), Value::Array(include));

    for param in UNSUPPORTED_PARAMS {
        request.remove(param);
    }

    let model = request
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    if let Some(reasoning) = normalize_reasoning(request.remove("reasoning"), &model) {
        request.insert("reasoning".to_owned(), Value::Object(reasoning));
    }

    if let Some(Value::Array(tools)) = request.get_mut("tools") {
        for tool in tools.iter_mut() {
            strip_web_search_args(tool);
        }
        tools.truncate(MAX_TOOLS);
    }

    if let Some(Value::Array(items)) = request.get_mut("input") {
        for item in items.iter_mut() {
            repair_input_item(item);
        }
    }

    Ok(Bytes::from(serde_json::to_vec(&value)?))
}

/// The `include` list with the encrypted-reasoning entry present exactly once.
///
/// A non-array `include` is replaced, as the reference does: the proxy only
/// reads an array.
fn ensure_reasoning_include(include: Option<Value>) -> Vec<Value> {
    let mut items = match include {
        Some(Value::Array(items)) => items,
        _ => Vec::new(),
    };
    if !items
        .iter()
        .any(|item| item.as_str() == Some(REASONING_INCLUDE))
    {
        items.push(Value::String(REASONING_INCLUDE.to_owned()));
    }
    items
}

/// The reasoning object the proxy takes, or `None` when there is none to send.
///
/// The explicit-effort check runs before an invalid effort is dropped. An
/// explicit `none` or `off` is a request to switch reasoning off and must not be
/// promoted to the default, which is what the reference's #7358 fixed.
fn normalize_reasoning(value: Option<Value>, model: &str) -> Option<Map<String, Value>> {
    let mut reasoning = match value {
        Some(Value::Object(map)) => map,
        _ => Map::new(),
    };
    let explicit = reasoning.contains_key("effort");
    let supported = reasoning
        .get("effort")
        .and_then(Value::as_str)
        .is_some_and(|effort| SUPPORTED_REASONING_EFFORTS.contains(&effort));
    if !supported {
        reasoning.remove("effort");
    }
    if model == NO_EFFORT_MODEL {
        reasoning.remove("effort");
    } else if !explicit {
        reasoning.insert(
            "effort".to_owned(),
            Value::String(DEFAULT_REASONING_EFFORT.to_owned()),
        );
    }
    (!reasoning.is_empty()).then_some(reasoning)
}

/// Drops the `web_search` arguments the proxy refuses. Only the exact
/// `web_search` shape carries them, so every other tool passes untouched.
fn strip_web_search_args(tool: &mut Value) {
    let Some(rec) = tool.as_object_mut() else {
        return;
    };
    if rec.get("type").and_then(Value::as_str) != Some("web_search") {
        return;
    }
    for arg in UNSUPPORTED_WEB_SEARCH_ARGS {
        rec.remove(arg);
    }
}

/// Repairs one `input` item: a reasoning item's `content: null`, and a tool
/// result's `output`. Every other item passes through.
fn repair_input_item(item: &mut Value) {
    let Some(rec) = item.as_object_mut() else {
        return;
    };
    match rec.get("type").and_then(Value::as_str) {
        // Codex replays reasoning with `content: null`, and the proxy then fails
        // to decode the encrypted blob beside it. Omitting the key is accepted.
        Some("reasoning") if rec.get("content") == Some(&Value::Null) => {
            rec.remove("content");
        }
        Some("function_call_output") => {
            let output = sanitize_output(rec.get("output").unwrap_or(&Value::Null));
            rec.insert("output".to_owned(), Value::String(output));
        }
        _ => {}
    }
}

/// A tool result as the proxy's strict body parser accepts it, always a string.
///
/// Agent tool results arrive as truncated JSON or with broken `\u` escapes. The
/// proxy rejects both with `unexpected end of hex escape`, so they are repaired
/// before dispatch.
fn sanitize_output(output: &Value) -> String {
    match output {
        Value::Null => String::new(),
        Value::String(text) => sanitize_text(text),
        Value::Array(parts) => {
            let joined = parts
                .iter()
                .map(|part| match part {
                    Value::Object(rec) => match rec.get("text") {
                        Some(Value::String(text)) => text.clone(),
                        _ => part.to_string(),
                    },
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                })
                .collect::<Vec<_>>()
                .join("\n");
            sanitize_text(&joined)
        }
        other => other.to_string(),
    }
}

/// Compacts `text` when it is valid JSON, repairs and compacts it when it only
/// becomes valid after the broken escapes are dropped, and otherwise returns the
/// repaired text as-is.
fn sanitize_text(text: &str) -> String {
    if let Ok(value) = serde_json::from_str::<Value>(text) {
        return value.to_string();
    }
    let repaired = drop_broken_unicode_escapes(text);
    match serde_json::from_str::<Value>(&repaired) {
        Ok(value) => value.to_string(),
        Err(_) => repaired,
    }
}

/// Removes each `\u` followed by at most three hex digits and then no more hex.
///
/// That is the reference's `\\u([0-9A-Fa-f]{0,3})(?![0-9A-Fa-f])`, written as a
/// scan. A complete `\uXXXX` escape is left alone.
fn drop_broken_unicode_escapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("\\u") {
        out.push_str(&rest[..at]);
        let after = &rest[at + 2..];
        let run = after.bytes().take_while(u8::is_ascii_hexdigit).count();
        if run <= 3 {
            rest = &after[run..];
        } else {
            out.push_str("\\u");
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{client_headers, normalize_body};

    fn run(body: Value) -> Value {
        let bytes = serde_json::to_vec(&body).expect("encodes");
        serde_json::from_slice(&normalize_body(&bytes).expect("normalizes")).expect("json")
    }

    #[test]
    fn normalize_should_keep_the_encrypted_reasoning_include_when_absent() {
        let out = run(json!({"model": "grok-4.6", "input": "hi"}));

        assert_eq!(out["include"], json!(["reasoning.encrypted_content"]));
    }

    #[test]
    fn normalize_should_not_duplicate_the_reasoning_include_when_present() {
        let out = run(json!({"model": "grok-4.6", "include": ["reasoning.encrypted_content"]}));

        assert_eq!(out["include"], json!(["reasoning.encrypted_content"]));
    }

    #[test]
    fn normalize_should_default_store_to_false_when_absent() {
        let out = run(json!({"model": "grok-4.6"}));

        assert_eq!(out["store"], json!(false));
    }

    #[test]
    fn normalize_should_drop_the_sampling_params_the_proxy_refuses() {
        let out = run(json!({"model": "grok-4.6", "frequency_penalty": 0.5, "logprobs": true}));

        assert!(out.get("frequency_penalty").is_none() && out.get("logprobs").is_none());
    }

    #[test]
    fn normalize_should_default_reasoning_effort_to_high_when_none_is_given() {
        let out = run(json!({"model": "grok-4.6"}));

        assert_eq!(out["reasoning"], json!({"effort": "high"}));
    }

    #[test]
    fn normalize_should_keep_an_explicit_off_switch_when_effort_is_invalid() {
        let out = run(json!({"model": "grok-4.6", "reasoning": {"effort": "none"}}));

        assert!(out.get("reasoning").is_none());
    }

    #[test]
    fn normalize_should_send_no_effort_for_the_composer_model() {
        let out = run(json!({"model": "grok-composer-2.5-fast"}));

        assert!(out.get("reasoning").is_none());
    }

    #[test]
    fn normalize_should_strip_the_web_search_args_the_proxy_rejects() {
        let out = run(json!({
            "model": "grok-4.6",
            "tools": [{"type": "web_search", "external_web_access": true, "search_context_size": "low"}]
        }));

        assert_eq!(out["tools"], json!([{"type": "web_search"}]));
    }

    #[test]
    fn normalize_should_cap_tools_at_two_hundred() {
        let tools: Vec<Value> = (0..250)
            .map(|i| json!({"type": "function", "name": i}))
            .collect();
        let out = run(json!({"model": "grok-4.6", "tools": tools}));

        assert_eq!(out["tools"].as_array().map(Vec::len), Some(200));
    }

    #[test]
    fn normalize_should_drop_null_content_from_a_reasoning_item() {
        let out = run(json!({"model": "grok-4.6", "input": [
            {"type": "reasoning", "id": "r1", "content": null, "encrypted_content": "blob"}
        ]}));

        assert!(out["input"][0].get("content").is_none());
    }

    #[test]
    fn normalize_should_compact_a_valid_json_tool_output() {
        let out = run(json!({"model": "grok-4.6", "input": [
            {"type": "function_call_output", "call_id": "c1", "output": "{ \"a\" : 1 }"}
        ]}));

        assert_eq!(out["input"][0]["output"], json!("{\"a\":1}"));
    }

    #[test]
    fn normalize_should_drop_a_truncated_unicode_escape_from_a_tool_output() {
        let out = run(json!({"model": "grok-4.6", "input": [
            {"type": "function_call_output", "call_id": "c1", "output": r#"{"a":"x\u12""#}
        ]}));

        assert_eq!(out["input"][0]["output"], json!(r#"{"a":"x""#));
    }

    #[test]
    fn normalize_should_keep_a_complete_unicode_escape_in_a_tool_output() {
        let out = run(json!({"model": "grok-4.6", "input": [
            {"type": "function_call_output", "call_id": "c1", "output": r"A"}
        ]}));

        assert_eq!(out["input"][0]["output"], json!(r"A"));
    }

    #[test]
    fn client_headers_should_name_the_token_auth_the_proxy_checks() {
        let headers = client_headers("grok-4.6");

        assert_eq!(headers["X-XAI-Token-Auth"], "xai-grok-cli");
    }

    #[test]
    fn client_headers_should_omit_the_model_override_when_no_model_is_named() {
        let headers = client_headers("");

        assert!(!headers.contains_key("x-grok-model-override"));
    }
}
