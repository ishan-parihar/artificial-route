//! Cache keys: `blake3` over a canonical JSON rendering.
//!
//! `docs/04` fixes the key as `blake3(tenant|model|canonical-msgs|params)`,
//! canonicalised through a `BTreeMap`. Two properties matter and both are load
//! bearing:
//!
//! * **Order independence.** `{"a":1,"b":2}` and `{"b":2,"a":1}` are the same
//!   request, so they must hash to the same key. `serde_json::Map` is *usually*
//!   a `BTreeMap` and therefore sorted — but a sibling crate enabling
//!   serde_json's `preserve_order` feature flips it to an `IndexMap` under
//!   Cargo's feature unification, silently and without a compile error. That is
//!   the "unsorted-nested" bug in `docs/04`, so the sorting here is explicit
//!   rather than inherited.
//! * **No raw prompts.** A key is a digest. Nothing in this module can log, hex
//!   -dump or otherwise leak request content, and there is no code path that
//!   keeps the rendered canonical form around after [`key_of`] returns.

use blake3::Hasher;
use serde::Serialize;
use serde_json::Value;

/// Digest length in bytes. A `blake3` output truncated to 32 bytes is the full
/// digest, so the constant is a promise rather than a saving.
pub const KEY_LEN: usize = 32;

/// A 32-byte request digest.
///
/// `Copy` is deliberately not derived: 32 bytes is above the 24-byte `Copy`
/// guideline (AGENTS.md §2), and a key that is accidentally cloned per call
/// site is a key that is accidentally compared by `==` somewhere it should be
/// compared by identity. Pass it as `&CacheKey`.
///
/// `Debug` renders the digest, never the request. There is no `Display`, so a
/// key cannot reach a log line by accident; `tracing` prints the `Debug` form,
/// which is the digest.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CacheKey([u8; KEY_LEN]);

impl CacheKey {
    /// Hashes `bytes` into a key. The only raw-bytes constructor.
    #[must_use]
    pub fn hash(bytes: &[u8]) -> Self {
        let mut hasher = Hasher::new();
        hasher.update(bytes);
        Self(*hasher.finalize().as_bytes())
    }

    /// Borrows the digest.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; KEY_LEN] {
        &self.0
    }

    /// Renders the digest as lowercase hex, for a `redb` key or a log field.
    ///
    /// Borrows rather than consumes: rendering a fingerprint is a read, and a
    /// `self`-taking `to_hex` on a non-`Copy` type forces a clone at every
    /// `&CacheKey` call site.
    #[must_use]
    pub fn to_hex(&self) -> String {
        let mut out = String::with_capacity(KEY_LEN * 2);
        for byte in self.0 {
            // `write!` into a `String` cannot fail; formatting into a
            // fixed-capacity buffer per byte avoids the `fmt::Write` import for
            // four lines of work.
            out.push(char::from_digit((byte >> 4).into(), 16).unwrap_or('0'));
            out.push(char::from_digit((byte & 0xf).into(), 16).unwrap_or('0'));
        }
        out
    }
}

impl std::fmt::Debug for CacheKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CacheKey({})", self.to_hex())
    }
}

/// Builds a key from an arbitrary set of named fields.
///
/// Fields are sorted by name and rendered as a JSON object, so call order does
/// not change the result. Duplicate names are the caller's bug and produce a
/// last-one-wins object; the alternative (a second `BTreeMap`) costs an
/// allocation to defend against a mistake this crate cannot detect anyway.
#[must_use]
pub fn key_of(fields: &[(&str, &Value)]) -> CacheKey {
    let mut sorted: Vec<&(&str, &Value)> = fields.iter().collect();
    sorted.sort_by_key(|(name, _)| *name);

    let mut buf = String::with_capacity(128);
    buf.push('{');
    for (idx, (name, value)) in sorted.iter().enumerate() {
        if idx > 0 {
            buf.push(',');
        }
        push_string(&mut buf, name);
        buf.push(':');
        write_value(value, &mut buf);
    }
    buf.push('}');
    CacheKey::hash(buf.as_bytes())
}

/// The `docs/04` key: `blake3(tenant | model | canonical request body)`.
///
/// `body` is the canonical request — [`ar_translate::CanonicalChat`] rendered to
/// a [`Value`], or the caller's own normalised object. Anything that changes
/// generation belongs inside it: a `temperature` that leaks out of the key is a
/// cache that serves a 0.7 answer to a 0.0 request.
#[must_use]
pub fn request_key(tenant: &str, model: &str, body: &Value) -> CacheKey {
    request_key_with(tenant, model, body, None)
}

/// [`request_key`], plus the caller-supplied key segment a request may carry in
/// its cache-control headers.
///
/// The reference folds its `x-omniroute-cache-key` into the same digest
/// (`semanticCacheManager.ts` reads it on both the lookup and the store side),
/// which makes it a *namespace* — two clients naming different segments never
/// see each other's entries, and an absent segment is the shared default. It is
/// sorted into the hashed fields (`"body" < "caller_key" < "model" <
/// "tenant"`) so the digest format stays one canonical string, and it is
/// hashed rather than concatenated so a segment cannot be crafted to collide
/// with another client's `(model, body)` pair.
#[must_use]
pub fn request_key_with(
    tenant: &str,
    model: &str,
    body: &Value,
    caller_key: Option<&str>,
) -> CacheKey {
    let mut buf = String::with_capacity(256);
    buf.push('{');
    // Sorted by name, matching `key_of`: "body" < "caller_key" < "model" <
    // "tenant".
    push_string(&mut buf, "body");
    buf.push(':');
    write_value(body, &mut buf);
    buf.push(',');
    if let Some(caller_key) = caller_key {
        push_string(&mut buf, "caller_key");
        buf.push(':');
        push_string(&mut buf, caller_key);
        buf.push(',');
    }
    push_string(&mut buf, "model");
    buf.push(':');
    push_string(&mut buf, model);
    buf.push(',');
    push_string(&mut buf, "tenant");
    buf.push(':');
    push_string(&mut buf, tenant);
    buf.push('}');
    CacheKey::hash(buf.as_bytes())
}

/// Convenience for callers holding a `serde`-serialisable request.
///
/// Serialises to a `Value` first so the canonical form is the same one
/// [`request_key`] hashes, whether the caller arrived by `json!` or by `derive`.
#[must_use]
pub fn request_key_of<T: Serialize + std::fmt::Debug>(tenant: &str, model: &str, body: &T) -> CacheKey {
    match serde_json::to_value(body) {
        Ok(value) => request_key(tenant, model, &value),
        // A `Serialize` impl that cannot produce a `Value` is a broken
        // serializer, not a bad request. Fall back to the model's own `Debug`
        // rendering: a degenerate but *stable* key, so the failure mode is a
        // permanently-missing entry rather than a panic on a live request.
        // The reference degrades to a literal `"nodigest"` marker here
        // (`chatCore/idempotency.ts`), which makes every unserialisable body
        // share one key -- a cache-poisoning collision. `Debug` at least
        // distinguishes them.
        Err(_) => request_key(tenant, model, &Value::String(format!("{body:?}"))),
    }
}

/// Renders `value` into `out` with every object's keys sorted.
///
/// This is the fix for the "unsorted-nested" defect: sorting only the top
/// level would still let `{"params":{"z":1,"a":2}}` and `{"params":{"a":2,"z":1}}`
//  hash differently.
fn write_value(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        // `Number`'s `Display` is its stored literal, so `1`, `1.0` and
        // `1e3` each round-trip as themselves -- stable across calls, which is
        // all a cache key needs.
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => push_string(out, s),
        Value::Array(items) => {
            out.push('[');
            for (idx, item) in items.iter().enumerate() {
                if idx > 0 {
                    out.push(',');
                }
                write_value(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable();
            out.push('{');
            for (idx, key) in keys.into_iter().enumerate() {
                if idx > 0 {
                    out.push(',');
                }
                push_string(out, key);
                out.push(':');
                // `map` is indexed by the key just collected, so this cannot
                // miss; `Value::Null` is only a placeholder for an impossible
                // case and would still be valid canonical JSON.
                write_value(map.get(key).unwrap_or(&Value::Null), out);
            }
            out.push('}');
        }
    }
}

/// Appends `s` as a canonical JSON string literal.
///
/// Hand-rolled rather than `serde_json::to_writer` for two reasons: the writer
/// returns a `Result` that is infallible for a `&str` into a `String` (and this
/// crate has no `unwrap` outside tests), and this escapes exactly what
/// `serde_json` escapes — `"`, `\`, and C0 controls — so a hand-built object and
/// a parsed one canonicalise to identical bytes. `serde_json` also leaves `/`
/// and DEL unescaped, and so does this.
fn push_string(out: &mut String, s: &str) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let n = c as u32;
                out.push_str("\\u00");
                out.push(char::from(HEX[((n >> 4) & 0xf) as usize]));
                out.push(char::from(HEX[(n & 0xf) as usize]));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{CacheKey, key_of, request_key, request_key_of};

    #[test]
    fn same_fields_hash_same_regardless_of_input_order() {
        let a = json!({"model": "m", "temperature": 0.2, "stream": false});
        let b = json!({"stream": false, "temperature": 0.2, "model": "m"});
        assert_eq!(request_key("t", "m", &a), request_key("t", "m", &b));
    }

    #[test]
    fn nested_objects_are_sorted_not_just_the_top_level() {
        let a = json!({"params": {"z": 1, "a": {"y": 2, "b": 3}}});
        let b = json!({"params": {"a": {"b": 3, "y": 2}, "z": 1}});
        assert_eq!(key_of(&[("params", &a)]), key_of(&[("params", &b)]));
    }

    #[test]
    fn distinct_tenants_do_not_share_an_entry() {
        let body = json!({"messages": []});
        assert_ne!(request_key("a", "m", &body), request_key("b", "m", &body));
    }

    #[test]
    fn distinct_models_do_not_share_an_entry() {
        let body = json!({"messages": []});
        assert_ne!(request_key("t", "a", &body), request_key("t", "b", &body));
    }

    #[test]
    fn field_name_order_in_key_of_does_not_matter() {
        let (x, y) = (json!(1), json!(2));
        assert_eq!(key_of(&[("a", &x), ("b", &y)]), key_of(&[("b", &y), ("a", &x)]));
    }

    #[test]
    fn different_field_names_with_equal_values_do_not_collide() {
        // The canonical object is name-tagged, so `{"a":1}` and `{"b":1}` differ.
        let one = json!(1);
        assert_ne!(key_of(&[("a", &one)]), key_of(&[("b", &one)]));
    }

    #[test]
    fn request_key_of_matches_request_key_on_the_same_body() {
        #[derive(serde::Serialize, Debug)]
        struct Body {
            model: String,
            messages: Vec<String>,
        }
        let typed = Body { model: "m".into(), messages: vec!["hi".into()] };
        let value = json!({"model": "m", "messages": ["hi"]});
        assert_eq!(request_key_of("t", "m", &typed), request_key("t", "m", &value));
    }

    #[test]
    fn escaped_control_characters_canonicalise_to_valid_json() {
        // The canonical rendering has to survive a parse, or the bytes we hash
        // describe a document no parser can read back.
        let raw = "a\u{1}\"\t\\b\u{7f}";
        let mut buf = String::new();
        super::push_string(&mut buf, raw);
        let text = format!("{{\"s\":{buf}}}");
        let parsed: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        assert_eq!(parsed, json!({ "s": raw }));
    }

    #[test]
    fn strings_differing_only_in_escaped_form_do_not_collide() {
        // `"a\u{1}b"` and `"a\\u0001b"` are different strings and must not
        // hash alike -- the failure this catches is a canonical form that
        // forgets to escape, making every control char a key collision.
        let a = key_of(&[("s", &json!("a\u{1}b"))]);
        let b = key_of(&[("s", &json!("a\\u0001b"))]);
        assert_ne!(a, b);
    }

    #[test]
    fn hex_round_trips_to_the_digest() {
        let key = CacheKey::hash(b"ar");
        let hex = key.to_hex();
        assert_eq!(hex.len(), 64);
        let mut bytes = [0u8; 32];
        for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
            let pair = std::str::from_utf8(chunk).map_or(0u8, |s| u8::from_str_radix(s, 16).unwrap_or(0));
            bytes[i] = pair;
        }
        assert_eq!(&bytes, key.as_bytes());
    }
}
