//! Inbound tool declarations, tool choice and tool calls, read from each wire.
//!
//! Every inbound request type keeps the fields it does not model in a `rest`
//! value, so the tool fields are read from there. The typed request structs
//! stay as they were, and a dialect that never sends tools pays nothing.
//!
//! A tool this build does not translate (a built-in such as web search) is
//! refused, not dropped. A dropped declaration is a request the model answers
//! as though the tool did not exist, which is the silent substitution this
//! crate exists to avoid.

use serde_json::{Value, json};

use crate::TranslateError;
use crate::canonical::{CanonicalTool, ToolCall, ToolChoice};

/// The schema an absent `parameters` means: a function that takes no arguments.
fn empty_schema() -> Value {
    json!({ "type": "object", "properties": {} })
}

fn invalid(reason: impl Into<String>) -> TranslateError {
    TranslateError::InvalidTool {
        reason: reason.into(),
    }
}

fn unsupported(kind: &str) -> TranslateError {
    TranslateError::UnsupportedTool {
        kind: kind.to_owned(),
    }
}

/// An optional string field.
fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// Builds a declaration from a name, an optional description and a schema read
/// under `schema_key` (`parameters` for OpenAI-style, `input_schema` for
/// Anthropic).
fn declared(decl: &Value, schema_key: &str) -> Result<CanonicalTool, TranslateError> {
    let name = str_field(decl, "name").ok_or_else(|| invalid("a tool has no `name`"))?;
    Ok(CanonicalTool {
        name: name.to_owned(),
        description: str_field(decl, "description").map(str::to_owned),
        parameters: decl
            .get(schema_key)
            .filter(|schema| !schema.is_null())
            .cloned()
            .unwrap_or_else(empty_schema),
    })
}

/// The `tools` array from `rest`: empty when absent or null.
fn tool_list(rest: &Value) -> Result<&[Value], TranslateError> {
    match rest.get("tools") {
        None | Some(Value::Null) => Ok(&[]),
        Some(Value::Array(items)) => Ok(items),
        Some(_) => Err(invalid("`tools` must be an array")),
    }
}

/// The non-null value at `key`, when there is one.
fn present<'a>(rest: &'a Value, key: &str) -> Option<&'a Value> {
    rest.get(key).filter(|value| !value.is_null())
}

/// OpenAI chat `tools`: `{"type":"function","function":{name,description,parameters}}`.
pub(crate) fn openai_tools(rest: &Value) -> Result<Vec<CanonicalTool>, TranslateError> {
    tool_list(rest)?
        .iter()
        .map(|tool| {
            let kind = str_field(tool, "type").unwrap_or_default();
            if kind != "function" {
                return Err(unsupported(kind));
            }
            let function = tool
                .get("function")
                .ok_or_else(|| invalid("a function tool has no `function` object"))?;
            declared(function, "parameters")
        })
        .collect()
}

/// OpenAI chat `tool_choice`: a keyword, or `{"type":"function","function":{name}}`.
pub(crate) fn openai_tool_choice(rest: &Value) -> Result<Option<ToolChoice>, TranslateError> {
    match present(rest, "tool_choice") {
        None => Ok(None),
        Some(Value::String(keyword)) => keyword_choice(keyword).map(Some),
        Some(object) => {
            let name = object
                .get("function")
                .and_then(|function| str_field(function, "name"))
                .ok_or_else(|| invalid("`tool_choice` names no function"))?;
            Ok(Some(ToolChoice::Named(name.to_owned())))
        }
    }
}

/// The Responses API's `tools`: the same declarations, flattened (no `function`
/// wrapper), with the same built-in refusal.
pub(crate) fn responses_tools(rest: &Value) -> Result<Vec<CanonicalTool>, TranslateError> {
    tool_list(rest)?
        .iter()
        .map(|tool| {
            let kind = str_field(tool, "type").unwrap_or_default();
            if kind != "function" {
                return Err(unsupported(kind));
            }
            declared(tool, "parameters")
        })
        .collect()
}

/// The Responses API's `tool_choice`: a keyword, or `{"type":"function","name"}`.
pub(crate) fn responses_tool_choice(rest: &Value) -> Result<Option<ToolChoice>, TranslateError> {
    match present(rest, "tool_choice") {
        None => Ok(None),
        Some(Value::String(keyword)) => keyword_choice(keyword).map(Some),
        Some(object) => {
            let name = str_field(object, "name")
                .ok_or_else(|| invalid("`tool_choice` names no function"))?;
            Ok(Some(ToolChoice::Named(name.to_owned())))
        }
    }
}

/// Anthropic `tools`: `{name, description, input_schema}`. A `type` other than
/// `custom` marks a server-side tool this build does not run, so it is refused.
pub(crate) fn anthropic_tools(rest: &Value) -> Result<Vec<CanonicalTool>, TranslateError> {
    tool_list(rest)?
        .iter()
        .map(|tool| match str_field(tool, "type") {
            None | Some("custom") => declared(tool, "input_schema"),
            Some(kind) => Err(unsupported(kind)),
        })
        .collect()
}

/// Anthropic `tool_choice`: `{"type":"auto"|"any"|"none"|"tool"}`, where `any` is
/// "must call some tool" and `tool` names one.
pub(crate) fn anthropic_tool_choice(rest: &Value) -> Result<Option<ToolChoice>, TranslateError> {
    let Some(choice) = present(rest, "tool_choice") else {
        return Ok(None);
    };
    match str_field(choice, "type") {
        Some("auto") => Ok(Some(ToolChoice::Auto)),
        Some("any") => Ok(Some(ToolChoice::Required)),
        Some("none") => Ok(Some(ToolChoice::None)),
        Some("tool") => {
            let name = str_field(choice, "name")
                .ok_or_else(|| invalid("`tool_choice` of type `tool` names no tool"))?;
            Ok(Some(ToolChoice::Named(name.to_owned())))
        }
        other => Err(invalid(format!("unknown tool_choice type {other:?}"))),
    }
}

/// Gemini `tools`: `[{functionDeclarations: [{name, description, parameters}]}]`.
/// Every declaration in every group is flattened into one list. A group with no
/// `functionDeclarations` is a built-in (`googleSearch`, `codeExecution`), and
/// is refused by the name of its key.
pub(crate) fn gemini_tools(rest: &Value) -> Result<Vec<CanonicalTool>, TranslateError> {
    let mut out = Vec::new();
    for group in tool_list(rest)? {
        let Some(declarations) = group.get("functionDeclarations") else {
            let kind = group
                .as_object()
                .and_then(|fields| fields.keys().next())
                .map_or("", String::as_str);
            return Err(unsupported(kind));
        };
        let declarations = declarations
            .as_array()
            .ok_or_else(|| invalid("`functionDeclarations` must be an array"))?;
        for decl in declarations {
            out.push(declared(decl, "parameters")?);
        }
    }
    Ok(out)
}

/// Gemini `toolConfig.functionCallingConfig`: `mode` is `AUTO`, `ANY` (with one
/// allowed name, `Named`; otherwise `Required`) or `NONE`.
pub(crate) fn gemini_tool_choice(rest: &Value) -> Result<Option<ToolChoice>, TranslateError> {
    let Some(config) = present(rest, "toolConfig").and_then(|c| c.get("functionCallingConfig"))
    else {
        return Ok(None);
    };
    let allowed: Vec<&str> = config
        .get("allowedFunctionNames")
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    match str_field(config, "mode") {
        None | Some("MODE_UNSPECIFIED") => Ok(None),
        Some("AUTO" | "VALIDATED") => Ok(Some(ToolChoice::Auto)),
        Some("NONE") => Ok(Some(ToolChoice::None)),
        Some("ANY") => match allowed.as_slice() {
            [only] => Ok(Some(ToolChoice::Named((*only).to_owned()))),
            _ => Ok(Some(ToolChoice::Required)),
        },
        other => Err(invalid(format!(
            "unknown functionCallingConfig mode {other:?}"
        ))),
    }
}

/// The `tool_calls` array on an OpenAI assistant turn. `arguments` arrives as a
/// JSON string; it is parsed here so every renderer receives an object. An empty
/// string is no arguments.
pub(crate) fn openai_tool_calls(rest: &Value) -> Result<Vec<ToolCall>, TranslateError> {
    let Some(calls) = present(rest, "tool_calls") else {
        return Ok(Vec::new());
    };
    let calls = calls
        .as_array()
        .ok_or_else(|| invalid("`tool_calls` must be an array"))?;
    calls
        .iter()
        .map(|call| {
            let id = str_field(call, "id").ok_or_else(|| invalid("a tool call has no `id`"))?;
            let function = call
                .get("function")
                .ok_or_else(|| invalid("a tool call has no `function`"))?;
            let name = str_field(function, "name")
                .ok_or_else(|| invalid("a tool call has no function `name`"))?;
            let raw = str_field(function, "arguments").unwrap_or_default();
            let arguments = if raw.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(raw).map_err(|err| {
                    invalid(format!(
                        "arguments of tool call {name:?} are not JSON: {err}"
                    ))
                })?
            };
            Ok(ToolCall {
                id: id.to_owned(),
                name: name.to_owned(),
                arguments,
            })
        })
        .collect()
}

/// The keyword spellings of `tool_choice` shared by OpenAI and Responses.
fn keyword_choice(keyword: &str) -> Result<ToolChoice, TranslateError> {
    match keyword {
        "auto" => Ok(ToolChoice::Auto),
        "none" => Ok(ToolChoice::None),
        "required" => Ok(ToolChoice::Required),
        other => Err(invalid(format!("unknown tool_choice {other:?}"))),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use crate::canonical::{ToolChoice, render_openai_body};
    use crate::outbound::{render_claude_body, render_gemini_body, render_openai_responses_body};
    use crate::{
        AnthropicMessages, GeminiChat, OpenAIChat, ResponsesApi, anthropic_to_canonical,
        gemini_to_canonical, responses_to_canonical, to_canonical,
    };

    fn openai(raw: &str) -> OpenAIChat {
        serde_json::from_str(raw).expect("fixture is valid OpenAI chat")
    }

    fn render(raw: &[u8]) -> Value {
        serde_json::from_slice(raw).expect("renderer emits JSON")
    }

    const WEATHER: &str = r#"{"type":"function","function":{"name":"get_weather",
        "description":"Current weather","parameters":{"type":"object",
        "properties":{"city":{"type":"string"}},"required":["city"]}}}"#;

    #[test]
    fn openai_tools_should_read_the_declared_name_when_a_function_is_declared() {
        let raw = format!(
            r#"{{"model":"m","messages":[{{"role":"user","content":"hi"}}],"tools":[{WEATHER}]}}"#
        );
        let chat = to_canonical(openai(&raw)).expect("valid request");
        assert_eq!(chat.tools[0].name, "get_weather");
    }

    #[test]
    fn openai_tools_should_refuse_a_builtin_when_one_is_declared() {
        let raw = r#"{"model":"m","messages":[{"role":"user","content":"hi"}],"tools":[{"type":"web_search"}]}"#;
        let err = to_canonical(openai(raw)).expect_err("a built-in is not run here");
        assert_eq!(
            err.to_string(),
            r#"unsupported tool type "web_search": this build does not run it"#
        );
    }

    #[test]
    fn openai_tool_choice_should_name_the_function_when_an_object_is_sent() {
        let raw = format!(
            r#"{{"model":"m","messages":[{{"role":"user","content":"hi"}}],"tools":[{WEATHER}],"tool_choice":{{"type":"function","function":{{"name":"get_weather"}}}}}}"#
        );
        let chat = to_canonical(openai(&raw)).expect("valid request");
        assert_eq!(
            chat.tool_choice,
            Some(ToolChoice::Named("get_weather".to_owned()))
        );
    }

    #[test]
    fn openai_tool_calls_should_parse_arguments_when_they_are_a_json_string() {
        let raw = r#"{"model":"m","messages":[{"role":"assistant","content":null,
            "tool_calls":[{"id":"c1","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"Oslo\"}"}}]}]}"#;
        let chat = to_canonical(openai(raw)).expect("valid request");
        assert_eq!(
            chat.messages[0].tool_calls[0].arguments,
            json!({"city":"Oslo"})
        );
    }

    #[test]
    fn openai_tool_calls_should_refuse_arguments_that_are_not_json() {
        let raw = r#"{"model":"m","messages":[{"role":"assistant",
            "tool_calls":[{"id":"c1","type":"function","function":{"name":"f","arguments":"{oops"}}]}]}"#;
        assert!(to_canonical(openai(raw)).is_err());
    }

    #[test]
    fn openai_round_trip_should_keep_the_call_pairing_when_a_tool_turn_follows() {
        let raw = format!(
            r#"{{"model":"m","tool_choice":"required","tools":[{WEATHER}],"messages":[
            {{"role":"user","content":"weather in Oslo?"}},
            {{"role":"assistant","content":null,"tool_calls":[{{"id":"c1","type":"function","function":{{"name":"get_weather","arguments":"{{\"city\":\"Oslo\"}}"}}}}]}},
            {{"role":"tool","tool_call_id":"c1","content":"4C"}}]}}"#
        );
        let body = render(&render_openai_body(
            &to_canonical(openai(&raw)).expect("valid"),
        ));
        assert_eq!(body["messages"][1]["tool_calls"][0]["id"], "c1");
        assert_eq!(body["messages"][2]["tool_call_id"], "c1");
    }

    #[test]
    fn openai_round_trip_should_omit_content_on_a_call_only_turn() {
        let raw = r#"{"model":"m","messages":[{"role":"assistant",
            "tool_calls":[{"id":"c1","type":"function","function":{"name":"f","arguments":"{}"}}]}]}"#;
        let body = render(&render_openai_body(
            &to_canonical(openai(raw)).expect("valid"),
        ));
        assert!(body["messages"][0].get("content").is_none());
    }

    #[test]
    fn anthropic_tools_should_render_input_schema_when_a_custom_tool_is_declared() {
        let raw = r#"{"model":"m","max_tokens":8,"messages":[{"role":"user","content":"hi"}],
            "tools":[{"name":"get_weather","description":"Current weather","input_schema":{"type":"object"}}],
            "tool_choice":{"type":"any"}}"#;
        let req: AnthropicMessages = serde_json::from_str(raw).expect("valid");
        let chat = anthropic_to_canonical(req).expect("valid request");
        let body = render(&render_claude_body(&chat));
        assert_eq!(body["tools"][0]["input_schema"], json!({"type":"object"}));
    }

    #[test]
    fn anthropic_tool_choice_should_render_any_when_required() {
        let raw = r#"{"model":"m","max_tokens":8,"messages":[{"role":"user","content":"hi"}],
            "tools":[{"name":"f","input_schema":{}}],"tool_choice":{"type":"any"}}"#;
        let req: AnthropicMessages = serde_json::from_str(raw).expect("valid");
        let chat = anthropic_to_canonical(req).expect("valid request");
        let body = render(&render_claude_body(&chat));
        assert_eq!(body["tool_choice"], json!({"type":"any"}));
    }

    #[test]
    fn gemini_tools_should_flatten_every_declaration_when_grouped() {
        let raw = r#"{"model":"m","contents":[{"role":"user","parts":[{"text":"hi"}]}],
            "tools":[{"functionDeclarations":[{"name":"a"},{"name":"b"}]}]}"#;
        let req: GeminiChat = serde_json::from_str(raw).expect("valid");
        let chat = gemini_to_canonical(req).expect("valid request");
        assert_eq!(chat.tools.len(), 2);
    }

    #[test]
    fn gemini_tool_config_should_allow_only_the_named_function_when_one_is_forced() {
        let raw = r#"{"model":"m","contents":[{"role":"user","parts":[{"text":"hi"}]}],
            "tools":[{"functionDeclarations":[{"name":"a"}]}],
            "toolConfig":{"functionCallingConfig":{"mode":"ANY","allowedFunctionNames":["a"]}}}"#;
        let req: GeminiChat = serde_json::from_str(raw).expect("valid");
        let chat = gemini_to_canonical(req).expect("valid request");
        let body = render(&render_gemini_body(&chat));
        assert_eq!(
            body["toolConfig"]["functionCallingConfig"]["allowedFunctionNames"],
            json!(["a"])
        );
    }

    #[test]
    fn responses_tools_should_render_flat_when_a_function_is_declared() {
        let raw = r#"{"model":"m","input":"hi",
            "tools":[{"type":"function","name":"get_weather","parameters":{"type":"object"}}],
            "tool_choice":"auto"}"#;
        let req: ResponsesApi = serde_json::from_str(raw).expect("valid");
        let chat = responses_to_canonical(req).expect("valid request");
        let body = render(&render_openai_responses_body(&chat));
        assert_eq!(body["tools"][0]["name"], "get_weather");
    }
}
