//! OpenAI Responses API inbound frontend (POST /v1/responses).
//!
//! Implements Kinetix's explicitly supported subset of the OpenAI Responses API.
//! Requests are validated and decoded for cross-format translation; the API
//! boundary retains the validated raw body so Responses targets can use native
//! same-format passthrough. Response-object storage/chaining remains unsupported.

use bytes::Bytes;
use serde_json::{json, Value};

use crate::frontends::{sse_frame, EncoderCtx};
use crate::types::{
    FinishReason, ImageData, InternalRequest, Message, Part, ProxyError, Role, StreamEvent,
    ThinkingLevel, ToolChoice, ToolDef,
};

// ---------------------------------------------------------------------------
// Request decoding
// ---------------------------------------------------------------------------

/// Top-level Responses fields Kinetix accepts. Every entry needs a disposition
/// in `tests/fixtures/field-contract/openai-responses.json`; any other field is
/// rejected during validation.
pub const DECODED_FIELDS: &[&str] = &[
    "model",
    "input",
    "instructions",
    "tools",
    "tool_choice",
    "temperature",
    "top_p",
    "top_k",
    "max_output_tokens",
    "max_tokens",
    "presence_penalty",
    "frequency_penalty",
    "stream",
    "reasoning",
    "reasoning_effort",
    "prompt_cache_key",
    "store",
    "background",
    "text",
    "stream_options",
    "truncation",
    "parallel_tool_calls",
    "metadata",
    "include",
];

pub fn decode_request(body: Value) -> Result<InternalRequest, ProxyError> {
    let obj = body
        .as_object()
        .ok_or_else(|| ProxyError::bad_request("request body must be a JSON object"))?;
    validate_supported_subset(obj)?;
    // Preserve the validated Responses body for same-format dispatch. The HTTP
    // API handler replaces this normalized JSON with the exact received bytes.
    let validated_raw_body = body.to_string();

    let model = obj
        .get("model")
        .and_then(|m| m.as_str())
        .ok_or_else(|| ProxyError::bad_request("missing required field 'model'"))?
        .to_string();

    let mut system = Vec::new();

    // 1. Optional instructions field (Responses API system-instruction convention).
    if let Some(inst) = obj.get("instructions").and_then(|v| v.as_str()) {
        if !inst.trim().is_empty() {
            system.push(inst.to_string());
        }
    }

    // 2. Input: can be a plain string, or an array of items (messages, function calls, function outputs).
    let mut out_messages = Vec::new();
    if let Some(input) = obj.get("input") {
        match input {
            Value::String(text) => {
                if !text.is_empty() {
                    out_messages.push(Message {
                        role: Role::User,
                        parts: vec![Part::Text(text.clone())],
                    });
                }
            }
            Value::Array(items) => {
                for item in items {
                    decode_input_item(item, &mut system, &mut out_messages)?;
                }
            }
            _ => {
                return Err(ProxyError::bad_request(
                    "'input' must be either a string or an array of items",
                ));
            }
        }
    }

    // 3. Tools
    let tools = decode_tools(obj.get("tools"));
    let (tool_choice, tool_choice_name) = decode_tool_choice(obj.get("tool_choice"));

    // 4. Sampling parameters
    let params = crate::types::SamplingParams {
        temperature: obj.get("temperature").and_then(|v| v.as_f64()),
        top_p: obj.get("top_p").and_then(|v| v.as_f64()),
        top_k: obj.get("top_k").and_then(|v| v.as_f64()),
        max_tokens: obj
            .get("max_output_tokens")
            .or_else(|| obj.get("max_tokens"))
            .and_then(|v| v.as_u64())
            .map(|v| v as u32),
        presence_penalty: obj.get("presence_penalty").and_then(|v| v.as_f64()),
        frequency_penalty: obj.get("frequency_penalty").and_then(|v| v.as_f64()),
        ..Default::default()
    };

    // 5. Reasoning / thinking effort
    let thinking = obj
        .get("reasoning")
        .and_then(|r| r.get("effort"))
        .and_then(|e| e.as_str())
        .or_else(|| obj.get("reasoning_effort").and_then(|v| v.as_str()))
        .and_then(map_reasoning_effort);

    let stream = obj.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);

    // Keep the canonical representation for cross-format translation. For a
    // Responses target, the validated raw body is retained for native
    // same-format passthrough; unsupported top-level semantics already failed
    // validation above.
    let mut extra = serde_json::Map::new();
    if let Some(prompt_cache_key) = obj.get("prompt_cache_key") {
        extra.insert("prompt_cache_key".to_string(), prompt_cache_key.clone());
    }
    // Same-format passthrough forwards `reasoning.summary`; no other wire
    // format can deliver reasoning summaries, so translation refuses it.
    if let Some(summary) = body.pointer("/reasoning/summary") {
        extra.insert(
            crate::frontends::REASONING_SUMMARY_KEY.to_string(),
            summary.clone(),
        );
    }
    let identity_issues = crate::frontends::resolve_tool_result_names(&mut out_messages);
    if let Some(issue) = identity_issues.first() {
        return Err(ProxyError::unsupported(format!(
            "Responses API tool-call state is not translatable: {issue}"
        )));
    }

    Ok(InternalRequest {
        requested_model: model,
        system,
        messages: out_messages,
        tools,
        tool_choice,
        tool_choice_name,
        params,
        stream,
        include_usage: false,
        thinking,
        extra,
        raw_body: Some(validated_raw_body),
    })
}

fn validate_supported_subset(obj: &serde_json::Map<String, Value>) -> Result<(), ProxyError> {
    for key in obj.keys() {
        if !DECODED_FIELDS.contains(&key.as_str()) {
            return Err(ProxyError::unsupported(format!(
                "Responses API field '{key}' is not supported by Kinetix's translated subset"
            )));
        }
    }

    if obj.get("stream").is_some_and(|value| !value.is_boolean()) {
        return Err(ProxyError::bad_request(
            "Responses API 'stream' must be boolean",
        ));
    }
    if obj
        .get("prompt_cache_key")
        .is_some_and(|value| !value.is_string())
    {
        return Err(ProxyError::bad_request(
            "Responses API 'prompt_cache_key' must be a string",
        ));
    }
    for key in ["store", "background"] {
        if obj.get(key).is_some_and(|value| !value.is_boolean()) {
            return Err(ProxyError::bad_request(format!(
                "Responses API '{key}' must be boolean"
            )));
        }
    }
    for key in [
        "temperature",
        "top_p",
        "top_k",
        "presence_penalty",
        "frequency_penalty",
    ] {
        if obj.get(key).is_some_and(|value| !value.is_number()) {
            return Err(ProxyError::bad_request(format!(
                "Responses API '{key}' must be numeric"
            )));
        }
    }
    for key in ["max_output_tokens", "max_tokens"] {
        if obj.get(key).is_some_and(|value| value.as_u64().is_none()) {
            return Err(ProxyError::bad_request(format!(
                "Responses API '{key}' must be a non-negative integer"
            )));
        }
    }

    if obj.get("store").and_then(Value::as_bool) == Some(true) {
        return Err(ProxyError::unsupported(
            "Responses API 'store: true' is unsupported; Kinetix does not persist response objects",
        ));
    }
    if obj.get("background").and_then(Value::as_bool) == Some(true) {
        return Err(ProxyError::unsupported(
            "Responses API background mode is unsupported",
        ));
    }
    if obj.get("parallel_tool_calls").is_some() {
        return Err(ProxyError::unsupported(
            "Responses API 'parallel_tool_calls' is not enforceable on translated upstreams",
        ));
    }
    if obj.get("metadata").is_some() {
        return Err(ProxyError::unsupported(
            "Responses API response metadata storage is unsupported",
        ));
    }
    if let Some(include) = obj.get("include") {
        let items = include
            .as_array()
            .ok_or_else(|| ProxyError::bad_request("Responses API 'include' must be an array"))?;
        if !items.is_empty() {
            return Err(ProxyError::unsupported(
                "Responses API 'include' expansions are unsupported",
            ));
        }
    }

    if let Some(truncation) = obj.get("truncation").and_then(Value::as_str) {
        if truncation != "disabled" {
            return Err(ProxyError::unsupported(
                "Responses API automatic truncation is unsupported; only 'disabled' is accepted",
            ));
        }
    }

    if let Some(text) = obj.get("text") {
        let text = text
            .as_object()
            .ok_or_else(|| ProxyError::bad_request("Responses API 'text' must be an object"))?;
        for key in text.keys() {
            if key != "format" {
                return Err(ProxyError::unsupported(format!(
                    "Responses API text.{key} is unsupported"
                )));
            }
        }
        if let Some(format) = text.get("format") {
            let format = format.as_object().ok_or_else(|| {
                ProxyError::bad_request("Responses API 'text.format' must be an object")
            })?;
            for key in format.keys() {
                if key != "type" {
                    return Err(ProxyError::unsupported(format!(
                        "Responses API text.format.{key} is unsupported"
                    )));
                }
            }
            let kind = format.get("type").and_then(Value::as_str).unwrap_or("text");
            if kind != "text" {
                return Err(ProxyError::unsupported(
                    "Responses API structured text.format is unsupported on translated upstreams",
                ));
            }
        }
    }

    if let Some(options) = obj.get("stream_options") {
        let options = options.as_object().ok_or_else(|| {
            ProxyError::bad_request("Responses API 'stream_options' must be an object")
        })?;
        for (key, value) in options {
            match key.as_str() {
                "include_obfuscation" if value.as_bool() == Some(false) => {}
                "include_obfuscation" => {
                    return Err(ProxyError::unsupported(
                        "Responses API stream obfuscation is unsupported",
                    ));
                }
                _ => {
                    return Err(ProxyError::unsupported(format!(
                        "Responses API stream_options.{key} is unsupported"
                    )));
                }
            }
        }
    }

    if let Some(reasoning) = obj.get("reasoning") {
        let reasoning = reasoning.as_object().ok_or_else(|| {
            ProxyError::bad_request("Responses API 'reasoning' must be an object")
        })?;
        for key in reasoning.keys() {
            if key != "effort" && key != "summary" {
                return Err(ProxyError::unsupported(format!(
                    "Responses API reasoning.{key} is unsupported; only reasoning.effort and reasoning.summary are accepted"
                )));
            }
        }
        if reasoning.get("summary").is_some_and(|v| !v.is_string()) {
            return Err(ProxyError::bad_request(
                "Responses API reasoning.summary must be a string",
            ));
        }
        if let Some(effort) = reasoning.get("effort") {
            let effort = effort.as_str().ok_or_else(|| {
                ProxyError::bad_request("Responses API reasoning.effort must be a string")
            })?;
            if map_reasoning_effort(effort).is_none() {
                return Err(ProxyError::unsupported(format!(
                    "Responses API reasoning effort '{effort}' is unsupported"
                )));
            }
        }
    }
    if let Some(effort) = obj.get("reasoning_effort") {
        let effort = effort.as_str().ok_or_else(|| {
            ProxyError::bad_request("Responses API reasoning_effort must be a string")
        })?;
        if map_reasoning_effort(effort).is_none() {
            return Err(ProxyError::unsupported(format!(
                "Responses API reasoning effort '{effort}' is unsupported"
            )));
        }
    }

    if let Some(choice) = obj.get("tool_choice") {
        match choice {
            Value::String(value) if matches!(value.as_str(), "auto" | "none" | "required") => {}
            Value::Object(value)
                if value.get("type").and_then(Value::as_str) == Some("function")
                    && value
                        .get("name")
                        .and_then(Value::as_str)
                        .is_some_and(|name| !name.is_empty())
                    && value
                        .keys()
                        .all(|key| matches!(key.as_str(), "type" | "name")) => {}
            Value::Null => {}
            _ => {
                return Err(ProxyError::unsupported(
                    "Responses API tool_choice supports only auto/none/required or a named function",
                ));
            }
        }
    }

    let nested = nested_translation_issues(obj);
    if let Some(issue) = nested.first() {
        return Err(ProxyError::unsupported(format!(
            "Responses API content is outside Kinetix's translated subset: {issue}"
        )));
    }

    Ok(())
}

fn nested_translation_issues(obj: &serde_json::Map<String, Value>) -> Vec<String> {
    let mut issues = Vec::new();

    if obj
        .get("instructions")
        .is_some_and(|value| !value.is_string())
    {
        issues.push("instructions has a non-text structure that cannot be translated".to_string());
    }

    if let Some(Value::Array(items)) = obj.get("input") {
        for (item_index, item) in items.iter().enumerate() {
            let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
            match kind {
                "function_call" | "function_call_output" => {}
                "" | "message" => {
                    let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
                    if !matches!(role, "system" | "developer" | "user" | "assistant" | "tool") {
                        issues.push(format!("input[{item_index}] has unsupported role '{role}'"));
                    }
                    inspect_responses_content(
                        &format!("input[{item_index}].content"),
                        item.get("content"),
                        matches!(role, "system" | "developer" | "tool"),
                        &mut issues,
                    );
                }
                _ => issues.push(format!(
                    "input[{item_index}] type '{kind}' has no canonical cross-format representation"
                )),
            }
        }
    }

    if let Some(tools) = obj.get("tools").and_then(Value::as_array) {
        for (tool_index, tool) in tools.iter().enumerate() {
            let kind = tool
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("function");
            if kind != "function" {
                issues.push(format!(
                    "tools[{tool_index}] type '{kind}' has no canonical cross-format representation"
                ));
                continue;
            }
            if tool.get("function").is_some() {
                for key in tool
                    .as_object()
                    .into_iter()
                    .flat_map(|object| object.keys())
                {
                    if !matches!(key.as_str(), "type" | "function") {
                        issues.push(format!(
                            "tools[{tool_index}].{key} has unsupported function-tool semantics"
                        ));
                    }
                }
            }
            let function = tool.get("function").unwrap_or(tool);
            if function.get("strict").and_then(Value::as_bool) == Some(true) {
                issues.push(format!(
                    "tools[{tool_index}].strict=true cannot be enforced on translated upstreams"
                ));
            }
            for key in function
                .as_object()
                .into_iter()
                .flat_map(|object| object.keys())
            {
                if !matches!(
                    key.as_str(),
                    "type" | "name" | "description" | "parameters" | "strict" | "function"
                ) {
                    issues.push(format!(
                        "tools[{tool_index}].{key} has unsupported function-tool semantics"
                    ));
                }
            }
        }
    }

    issues
}

fn inspect_responses_content(
    path: &str,
    content: Option<&Value>,
    text_only: bool,
    issues: &mut Vec<String>,
) {
    let Some(Value::Array(parts)) = content else {
        return;
    };
    for (part_index, part) in parts.iter().enumerate() {
        let kind = part.get("type").and_then(Value::as_str).unwrap_or("");
        let part_path = format!("{path}[{part_index}]");
        let is_text = matches!(kind, "text" | "input_text" | "output_text");
        let is_image = matches!(kind, "image_url" | "input_image");
        if text_only && !is_text {
            issues.push(format!(
                "{part_path} type '{kind}' cannot be represented in this message role during translation"
            ));
            continue;
        }
        if !is_text && !is_image {
            issues.push(format!(
                "{part_path} type '{kind}' has no canonical cross-format representation"
            ));
            continue;
        }
        if is_image {
            let url = part
                .pointer("/image_url/url")
                .and_then(Value::as_str)
                .or_else(|| part.get("image_url").and_then(Value::as_str))
                .or_else(|| part.get("url").and_then(Value::as_str));
            if url.is_none() {
                issues.push(format!("{part_path} image is missing a translatable URL"));
            }
        }
    }
}

fn decode_input_item(
    item: &Value,
    system: &mut Vec<String>,
    out: &mut Vec<Message>,
) -> Result<(), ProxyError> {
    let item_type = item.get("type").and_then(|t| t.as_str()).unwrap_or("");

    match item_type {
        "function_call" => {
            let call_id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(|v| v.as_str())
                .filter(|value| !value.is_empty())
                .map(String::from)
                .ok_or_else(|| {
                    ProxyError::bad_request("Responses function_call requires a non-empty call_id")
                })?;
            let name = item
                .get("name")
                .and_then(|v| v.as_str())
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    ProxyError::bad_request("Responses function_call requires a non-empty name")
                })?
                .to_string();
            let args = item
                .get("arguments")
                .map(|a| match a {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .unwrap_or_else(|| "{}".to_string());
            out.push(Message {
                role: Role::Assistant,
                parts: vec![Part::ToolCall {
                    id: Some(call_id),
                    name,
                    arguments: args,
                    signature: None,
                }],
            });
            Ok(())
        }
        "function_call_output" => {
            let call_id = item
                .get("call_id")
                .and_then(|v| v.as_str())
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    ProxyError::bad_request(
                        "Responses function_call_output requires a non-empty call_id",
                    )
                })?
                .to_string();
            let content = match item.get("output") {
                Some(Value::String(s)) => s.clone(),
                Some(other) => other.to_string(),
                None => String::new(),
            };
            out.push(Message {
                role: Role::Tool,
                parts: vec![Part::ToolResult {
                    tool_call_id: call_id,
                    name: None,
                    content,
                    is_error: false,
                }],
            });
            Ok(())
        }
        _ => {
            // Can be typed as "message" or a bare role/content object
            let role = item.get("role").and_then(|r| r.as_str()).unwrap_or("user");
            match role {
                "system" | "developer" => {
                    if let Some(text) = content_as_text(item.get("content")) {
                        if !text.is_empty() {
                            system.push(text);
                        }
                    }
                }
                "user" => {
                    out.push(Message {
                        role: Role::User,
                        parts: decode_content_parts(item.get("content")),
                    });
                }
                "assistant" => {
                    let mut parts = decode_content_parts(item.get("content"));
                    if let Some(tcs) = item.get("tool_calls").and_then(|t| t.as_array()) {
                        for tc in tcs {
                            let id = tc.get("id").and_then(|i| i.as_str()).map(String::from);
                            let func = tc.get("function");
                            let name = func
                                .and_then(|f| f.get("name"))
                                .and_then(|n| n.as_str())
                                .unwrap_or("")
                                .to_string();
                            let args = func
                                .and_then(|f| f.get("arguments"))
                                .map(|a| match a {
                                    Value::String(s) => s.clone(),
                                    other => other.to_string(),
                                })
                                .unwrap_or_else(|| "{}".to_string());
                            if !name.is_empty() {
                                parts.push(Part::ToolCall {
                                    id,
                                    name,
                                    arguments: args,
                                    signature: None,
                                });
                            }
                        }
                    }
                    out.push(Message {
                        role: Role::Assistant,
                        parts,
                    });
                }
                "tool" => {
                    let tool_call_id = item
                        .get("tool_call_id")
                        .or_else(|| item.get("call_id"))
                        .and_then(|i| i.as_str())
                        .unwrap_or("")
                        .to_string();
                    let content = content_as_text(item.get("content")).unwrap_or_default();
                    out.push(Message {
                        role: Role::Tool,
                        parts: vec![Part::ToolResult {
                            tool_call_id,
                            name: None,
                            content,
                            is_error: false,
                        }],
                    });
                }
                _ => {}
            }
            Ok(())
        }
    }
}

fn content_as_text(content: Option<&Value>) -> Option<String> {
    match content? {
        Value::String(s) => Some(s.clone()),
        Value::Array(parts) => {
            let mut text = String::new();
            for p in parts {
                if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
                    text.push_str(t);
                }
            }
            Some(text)
        }
        _ => None,
    }
}

fn decode_content_parts(content: Option<&Value>) -> Vec<Part> {
    match content {
        Some(Value::String(s)) => {
            if s.is_empty() {
                vec![]
            } else {
                vec![Part::Text(s.clone())]
            }
        }
        Some(Value::Array(parts)) => {
            let mut out = Vec::new();
            for p in parts {
                match p.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                    "text" | "input_text" | "output_text" => {
                        if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
                            out.push(Part::Text(t.to_string()));
                        }
                    }
                    "image_url" | "input_image" => {
                        let url = p
                            .pointer("/image_url/url")
                            .and_then(|u| u.as_str())
                            .or_else(|| p.get("image_url").and_then(|u| u.as_str()))
                            .or_else(|| p.get("url").and_then(|u| u.as_str()));
                        if let Some(url) = url {
                            if let Some((mime, data)) = parse_data_url(url) {
                                out.push(Part::Image(ImageData::Base64 { mime, data }));
                            } else {
                                out.push(Part::Image(ImageData::Url(url.to_string())));
                            }
                        }
                    }
                    _ => {}
                }
            }
            out
        }
        _ => vec![],
    }
}

fn parse_data_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    let mime = meta.strip_suffix(";base64").unwrap_or(meta).to_string();
    Some((mime, data.to_string()))
}

fn decode_tools(tools: Option<&Value>) -> Vec<ToolDef> {
    let mut out = Vec::new();
    let Some(arr) = tools.and_then(|t| t.as_array()) else {
        return out;
    };
    for t in arr {
        // Can be flat {"type": "function", "name": ..., "parameters": ...}
        // or nested {"type": "function", "function": {"name": ...}}
        let func = t.get("function").unwrap_or(t);
        let Some(name) = func.get("name").and_then(|n| n.as_str()) else {
            continue;
        };
        out.push(ToolDef {
            name: name.to_string(),
            description: func
                .get("description")
                .and_then(|d| d.as_str())
                .map(String::from),
            parameters: func.get("parameters").cloned().unwrap_or(json!({})),
            defer_loading: t
                .get("defer_loading")
                .or_else(|| func.get("defer_loading"))
                .and_then(Value::as_bool),
        });
    }
    out
}

fn decode_tool_choice(tc: Option<&Value>) -> (Option<ToolChoice>, Option<String>) {
    match tc {
        None => (Some(ToolChoice::Auto), None),
        Some(Value::String(s)) => match s.as_str() {
            "none" => (Some(ToolChoice::None), None),
            "required" => (Some(ToolChoice::Required), None),
            "auto" => (Some(ToolChoice::Auto), None),
            _ => (Some(ToolChoice::Auto), None),
        },
        Some(Value::Object(o)) => {
            let name = o
                .get("name")
                .or_else(|| o.get("function").and_then(|f| f.get("name")))
                .and_then(|n| n.as_str())
                .map(String::from);
            (Some(ToolChoice::Specific), name)
        }
        _ => (Some(ToolChoice::Auto), None),
    }
}

pub fn response_fields_from_request(
    request: &InternalRequest,
) -> Result<crate::frontends::ResponsesResponseFields, ProxyError> {
    let tool_choice = match request.tool_choice.unwrap_or(ToolChoice::Auto) {
        ToolChoice::Auto => Value::String("auto".into()),
        ToolChoice::None => Value::String("none".into()),
        ToolChoice::Required => Value::String("required".into()),
        ToolChoice::Specific => {
            let name = request
                .tool_choice_name
                .as_deref()
                .filter(|name| !name.is_empty())
                .ok_or_else(|| {
                    ProxyError::bad_request(
                        "named Responses tool_choice is missing its function name",
                    )
                })?;
            json!({ "type": "function", "name": name })
        }
    };
    let tools = request
        .tools
        .iter()
        .map(|tool| {
            let mut value = json!({
                "type": "function",
                "name": tool.name,
                "parameters": tool.parameters,
            });
            if let Some(description) = &tool.description {
                value["description"] = json!(description);
            }
            value
        })
        .collect();
    Ok(crate::frontends::ResponsesResponseFields {
        parallel_tool_calls: true,
        tool_choice,
        tools,
    })
}

fn map_reasoning_effort(s: &str) -> Option<ThinkingLevel> {
    match s {
        "none" | "off" => Some(ThinkingLevel::Off),
        "minimal" => Some(ThinkingLevel::Minimal),
        "low" => Some(ThinkingLevel::Low),
        "medium" => Some(ThinkingLevel::Medium),
        "high" => Some(ThinkingLevel::High),
        "xhigh" => Some(ThinkingLevel::XHigh),
        "max" => Some(ThinkingLevel::Max),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Streaming SSE Encoder
// ---------------------------------------------------------------------------

pub struct ResponsesEncoder {
    ctx: EncoderCtx,
    response_id: String,
    seq: u64,
    created_sent: bool,
    active_text_item: bool,
    text_item_id: String,
    accumulated_text: String,
    accumulated_refusal: String,
    message_is_refusal: bool,
    active_tools: Vec<ActiveTool>,
    finish_sent: bool,
    usage: Option<crate::types::TokenUsage>,
}

struct ActiveTool {
    index: u32,
    call_id: String,
    name: String,
    arguments: String,
    output_index: usize,
}

impl ResponsesEncoder {
    pub fn new(ctx: EncoderCtx) -> Self {
        let response_id = format!("resp_{}", ctx.request_id.replace(['-', '_'], ""));
        let text_item_id = format!("msg_{}_0", ctx.request_id.replace(['-', '_'], ""));
        ResponsesEncoder {
            ctx,
            response_id,
            seq: 0,
            created_sent: false,
            active_text_item: false,
            text_item_id,
            accumulated_text: String::new(),
            accumulated_refusal: String::new(),
            message_is_refusal: false,
            active_tools: Vec::new(),
            finish_sent: false,
            usage: None,
        }
    }

    fn frame(&mut self, event_name: &str, mut data: Value) -> Bytes {
        let seq = self.seq;
        self.seq += 1;
        if let Some(obj) = data.as_object_mut() {
            obj.insert("type".to_string(), Value::String(event_name.to_string()));
            obj.insert("sequence_number".to_string(), json!(seq));
        }
        sse_frame(Some(event_name), &data.to_string())
    }

    fn ensure_created(&mut self, out: &mut Vec<Bytes>) {
        if !self.created_sent {
            self.created_sent = true;
            let resp_obj = response_object(
                &self.ctx.responses,
                &self.response_id,
                self.ctx.created,
                &self.ctx.model_name,
                "in_progress",
                None,
                None,
                json!([]),
                Value::Null,
            );
            out.push(self.frame("response.created", json!({ "response": resp_obj.clone() })));
            out.push(self.frame("response.in_progress", json!({ "response": resp_obj })));
        }
    }

    fn ensure_message_item(&mut self, out: &mut Vec<Bytes>, refusal: bool) {
        if !self.active_text_item {
            self.active_text_item = true;
            self.message_is_refusal = refusal;
            let item = json!({
                "id": self.text_item_id,
                "type": "message",
                "role": "assistant",
                "status": "in_progress",
                "content": []
            });
            out.push(self.frame(
                "response.output_item.added",
                json!({
                    "response_id": self.response_id,
                    "output_index": 0,
                    "item": item
                }),
            ));
            let part = if refusal {
                json!({ "type": "refusal", "refusal": "" })
            } else {
                json!({ "type": "output_text", "text": "", "annotations": [] })
            };
            out.push(self.frame(
                "response.content_part.added",
                json!({
                    "response_id": self.response_id,
                    "output_index": 0,
                    "content_index": 0,
                    "item_id": self.text_item_id,
                    "part": part
                }),
            ));
        }
    }

    fn close_text_item(&mut self, out: &mut Vec<Bytes>, status: &str) {
        if self.active_text_item {
            self.active_text_item = false;
            if self.message_is_refusal {
                out.push(self.frame(
                    "response.refusal.done",
                    json!({
                        "response_id": self.response_id,
                        "output_index": 0,
                        "content_index": 0,
                        "item_id": self.text_item_id,
                        "refusal": self.accumulated_refusal
                    }),
                ));
                out.push(self.frame(
                    "response.content_part.done",
                    json!({
                        "response_id": self.response_id,
                        "output_index": 0,
                        "content_index": 0,
                        "item_id": self.text_item_id,
                        "part": { "type": "refusal", "refusal": self.accumulated_refusal }
                    }),
                ));
                out.push(self.frame(
                    "response.output_item.done",
                    json!({
                        "response_id": self.response_id,
                        "output_index": 0,
                        "item": {
                            "id": self.text_item_id,
                            "type": "message",
                            "role": "assistant",
                            "status": status,
                            "content": [{ "type": "refusal", "refusal": self.accumulated_refusal }]
                        }
                    }),
                ));
            } else {
                out.push(self.frame(
                    "response.output_text.done",
                    json!({
                        "response_id": self.response_id,
                        "output_index": 0,
                        "content_index": 0,
                        "item_id": self.text_item_id,
                        "text": self.accumulated_text,
                        "logprobs": []
                    }),
                ));
                out.push(self.frame(
                    "response.content_part.done",
                    json!({
                        "response_id": self.response_id,
                        "output_index": 0,
                        "content_index": 0,
                        "item_id": self.text_item_id,
                        "part": {
                            "type": "output_text",
                            "text": self.accumulated_text,
                            "annotations": []
                        }
                    }),
                ));
                out.push(self.frame(
                    "response.output_item.done",
                    json!({
                        "response_id": self.response_id,
                        "output_index": 0,
                        "item": {
                            "id": self.text_item_id,
                            "type": "message",
                            "role": "assistant",
                            "status": status,
                            "content": [{
                                "type": "output_text",
                                "text": self.accumulated_text,
                                "annotations": []
                            }]
                        }
                    }),
                ));
            }
        }
    }

    pub fn encode(&mut self, event: StreamEvent) -> Vec<Bytes> {
        let mut out = Vec::new();
        match event {
            StreamEvent::Start { .. } => {
                self.ensure_created(&mut out);
            }
            StreamEvent::TextDelta(t) => {
                self.ensure_created(&mut out);
                self.ensure_message_item(&mut out, false);
                self.accumulated_text.push_str(&t);
                out.push(self.frame(
                    "response.output_text.delta",
                    json!({
                        "response_id": self.response_id,
                        "output_index": 0,
                        "content_index": 0,
                        "item_id": self.text_item_id,
                        "delta": t,
                        "logprobs": []
                    }),
                ));
            }
            StreamEvent::RefusalDelta(t) => {
                self.ensure_created(&mut out);
                self.ensure_message_item(&mut out, true);
                self.accumulated_refusal.push_str(&t);
                out.push(self.frame(
                    "response.refusal.delta",
                    json!({
                        "response_id": self.response_id,
                        "output_index": 0,
                        "content_index": 0,
                        "item_id": self.text_item_id,
                        "delta": t
                    }),
                ));
            }
            StreamEvent::ThinkingBlockStart { .. }
            | StreamEvent::ThinkingDelta { .. }
            | StreamEvent::ThinkingBlockStop { .. }
            | StreamEvent::RedactedThinking { .. } => {
                // Raw provider reasoning is not a Responses reasoning-summary
                // item. reasoning.effort is supported as an input control, but
                // reasoning output items/summaries are outside this subset.
            }
            StreamEvent::ToolCallStart {
                index, id, name, ..
            } => {
                self.ensure_created(&mut out);
                self.close_text_item(&mut out, "completed");
                let call_id = id.unwrap_or_else(|| format!("call_{}_{}", self.response_id, index));
                let output_index =
                    if self.accumulated_text.is_empty() && self.accumulated_refusal.is_empty() {
                        self.active_tools.len()
                    } else {
                        1 + self.active_tools.len()
                    };
                let tool = ActiveTool {
                    index,
                    call_id: call_id.clone(),
                    name: name.clone(),
                    arguments: String::new(),
                    output_index,
                };
                self.active_tools.push(tool);
                out.push(self.frame(
                    "response.output_item.added",
                    json!({
                        "response_id": self.response_id,
                        "output_index": output_index,
                        "item": {
                            "id": call_id,
                            "type": "function_call",
                            "call_id": call_id,
                            "name": name,
                            "arguments": "",
                            "status": "in_progress"
                        }
                    }),
                ));
            }
            StreamEvent::ToolCallArgsDelta { index, args } => {
                self.ensure_created(&mut out);
                if let Some(tool) = self.active_tools.iter_mut().find(|t| t.index == index) {
                    tool.arguments.push_str(&args);
                    let call_id = tool.call_id.clone();
                    let out_idx = tool.output_index;
                    out.push(self.frame(
                        "response.function_call_arguments.delta",
                        json!({
                            "response_id": self.response_id,
                            "output_index": out_idx,
                            "item_id": call_id,
                            "call_id": call_id,
                            "delta": args
                        }),
                    ));
                }
            }
            StreamEvent::Usage(u) => {
                self.usage = Some(u);
            }
            StreamEvent::Finish(finish) => {
                self.finish_sent = true;
                if let Some(failure) =
                    responses_stream_failure(&finish, !self.accumulated_refusal.is_empty())
                {
                    self.ensure_created(&mut out);
                    let response = failed_terminal_response(
                        &self.ctx.responses,
                        &self.response_id,
                        self.ctx.created,
                        &self.ctx.model_name,
                        failure,
                    );
                    out.push(self.frame("response.failed", json!({ "response": response })));
                    return out;
                }
                let incomplete_reason = responses_incomplete_reason(&finish);
                let response_status = if incomplete_reason.is_some() {
                    "incomplete"
                } else {
                    "completed"
                };
                self.close_text_item(&mut out, response_status);
                let finished_tools: Vec<(usize, String, String, String)> = self
                    .active_tools
                    .iter()
                    .map(|t| {
                        (
                            t.output_index,
                            t.call_id.clone(),
                            t.name.clone(),
                            t.arguments.clone(),
                        )
                    })
                    .collect();
                for (tool_output_index, tool_call_id, tool_name, tool_arguments) in finished_tools {
                    out.push(self.frame(
                        "response.function_call_arguments.done",
                        json!({
                            "response_id": self.response_id,
                            "output_index": tool_output_index,
                            "item_id": tool_call_id,
                            "call_id": tool_call_id,
                            "name": tool_name,
                            "arguments": tool_arguments
                        }),
                    ));
                    out.push(self.frame(
                        "response.output_item.done",
                        json!({
                            "response_id": self.response_id,
                            "output_index": tool_output_index,
                            "item": {
                                "id": tool_call_id,
                                "type": "function_call",
                                "call_id": tool_call_id,
                                "name": tool_name,
                                "arguments": tool_arguments,
                                "status": response_status
                            }
                        }),
                    ));
                }
                let final_resp = self.build_response_object(&finish);
                let terminal_event = if incomplete_reason.is_some() {
                    "response.incomplete"
                } else {
                    "response.completed"
                };
                out.push(self.frame(terminal_event, json!({ "response": final_resp })));
            }
        }
        out
    }

    fn build_response_object(&self, finish: &FinishReason) -> Value {
        if let Some(failure) =
            responses_stream_failure(finish, !self.accumulated_refusal.is_empty())
        {
            return failed_terminal_response(
                &self.ctx.responses,
                &self.response_id,
                self.ctx.created,
                &self.ctx.model_name,
                failure,
            );
        }
        let incomplete_reason = responses_incomplete_reason(finish);
        let response_status = if incomplete_reason.is_some() {
            "incomplete"
        } else {
            "completed"
        };
        let mut output = Vec::new();
        if !self.accumulated_text.is_empty()
            || !self.accumulated_refusal.is_empty()
            || self.active_tools.is_empty()
        {
            let mut content = Vec::new();
            if !self.accumulated_text.is_empty()
                || (self.accumulated_refusal.is_empty() && self.active_tools.is_empty())
            {
                content.push(json!({
                    "type": "output_text",
                    "text": self.accumulated_text,
                    "annotations": []
                }));
            }
            if !self.accumulated_refusal.is_empty() {
                content.push(json!({
                    "type": "refusal",
                    "refusal": self.accumulated_refusal
                }));
            }
            output.push(json!({
                "id": self.text_item_id,
                "type": "message",
                "role": "assistant",
                "status": response_status,
                "content": content
            }));
        }
        for tool in &self.active_tools {
            output.push(json!({
                "id": tool.call_id,
                "type": "function_call",
                "call_id": tool.call_id,
                "name": tool.name,
                "arguments": tool.arguments,
                "status": response_status
            }));
        }

        let usage_obj = responses_usage(self.usage.as_ref());

        response_object(
            &self.ctx.responses,
            &self.response_id,
            self.ctx.created,
            &self.ctx.model_name,
            response_status,
            None,
            incomplete_reason.map(|reason| json!({ "reason": reason })),
            json!(output),
            usage_obj,
        )
    }

    pub fn finalize(&mut self) -> Vec<Bytes> {
        let mut out = Vec::new();
        if !self.finish_sent {
            out.extend(self.encode(StreamEvent::Finish(FinishReason::Stop)));
        }
        out
    }

    pub fn error_frame(&mut self, message: &str) -> Vec<Bytes> {
        let response = failed_response(
            &self.ctx.responses,
            &self.response_id,
            self.ctx.created,
            &self.ctx.model_name,
            message,
        );
        vec![self.frame("response.failed", json!({ "response": response }))]
    }
}

// ---------------------------------------------------------------------------
// Non-streaming Aggregation
// ---------------------------------------------------------------------------

// Responses requires both usage detail objects; emit their documented counters as
// zero when upstream usage omitted them so every synthesized usage has one shape.
// https://github.com/openai/openai-python/blob/main/src/openai/types/responses/response_usage.py
fn responses_usage(usage: Option<&crate::types::TokenUsage>) -> Value {
    let input = usage.and_then(|usage| usage.input).unwrap_or(0);
    let output = usage.and_then(|usage| usage.output).unwrap_or(0);
    json!({
        "total_tokens": input.saturating_add(output),
        "input_tokens": input,
        "input_tokens_details": {
            "cached_tokens": usage.and_then(|usage| usage.cached).unwrap_or(0),
            "cache_write_tokens": usage.and_then(|usage| usage.cache_write).unwrap_or(0)
        },
        "output_tokens": output,
        "output_tokens_details": {
            "reasoning_tokens": usage.and_then(|usage| usage.thinking).unwrap_or(0)
        }
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResponsesTerminalFailure {
    UnsupportedProviderReason,
    UnstreamableRefusal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResponsesTerminal {
    Completed,
    Incomplete(&'static str),
    Refusal,
    Failed(ResponsesTerminalFailure),
}

fn responses_terminal(finish: &FinishReason) -> ResponsesTerminal {
    match finish {
        // OpenAI documents these incomplete reasons for Responses:
        // https://developers.openai.com/api/docs/guides/reasoning
        // https://developers.openai.com/api/docs/guides/structured-outputs
        FinishReason::Length => ResponsesTerminal::Incomplete("max_output_tokens"),
        FinishReason::ContentFilter => ResponsesTerminal::Incomplete("content_filter"),
        FinishReason::Other(reason) => match reason.as_str() {
            // Anthropic refusals are successful stops; Responses represents
            // them as completed messages with typed refusal content. Non-stream
            // aggregation can translate this terminal marker, but streaming
            // text may already have been committed.
            // https://platform.claude.com/docs/en/build-with-claude/handling-stop-reasons
            // https://developers.openai.com/api/docs/guides/structured-outputs
            "refusal" => ResponsesTerminal::Refusal,
            // OpenAI documents `steered` as a Responses incomplete reason:
            // https://developers.openai.com/api/reference/cli/resources/beta/subresources/responses
            "steered" => ResponsesTerminal::Incomplete("steered"),
            // Provider-specific or unknown values must not be copied into
            // incomplete_details.reason.
            _ => ResponsesTerminal::Failed(ResponsesTerminalFailure::UnsupportedProviderReason),
        },
        FinishReason::Stop | FinishReason::ToolCalls => ResponsesTerminal::Completed,
    }
}

fn responses_stream_failure(
    finish: &FinishReason,
    has_typed_refusal: bool,
) -> Option<ResponsesTerminalFailure> {
    match responses_terminal(finish) {
        ResponsesTerminal::Failed(failure) => Some(failure),
        ResponsesTerminal::Refusal if !has_typed_refusal => {
            Some(ResponsesTerminalFailure::UnstreamableRefusal)
        }
        ResponsesTerminal::Completed
        | ResponsesTerminal::Incomplete(_)
        | ResponsesTerminal::Refusal => None,
    }
}

fn responses_incomplete_reason(finish: &FinishReason) -> Option<&'static str> {
    match responses_terminal(finish) {
        ResponsesTerminal::Incomplete(reason) => Some(reason),
        ResponsesTerminal::Completed
        | ResponsesTerminal::Refusal
        | ResponsesTerminal::Failed(_) => None,
    }
}

// OpenAI's ResponseError.code is a closed enum; use server_error for Kinetix's
// internal terminal-mapping failures. Keep the Kinetix classification in the
// Rust enum rather than inventing a wire-level error code.
// https://github.com/openai/openai-python/blob/main/src/openai/types/responses/response_error.py
// OpenAI represents failed generations with status=failed and a response.failed
// terminal event, not incomplete_details.reason:
// https://developers.openai.com/api/reference/cli/resources/responses/methods/retrieve
// https://developers.openai.com/api/reference/cli/resources/beta/subresources/responses
fn response_object(
    fields: &crate::frontends::ResponsesResponseFields,
    response_id: &str,
    created_at: i64,
    model: &str,
    status: &str,
    error: Option<Value>,
    incomplete_details: Option<Value>,
    output: Value,
    usage: Value,
) -> Value {
    json!({
        "id": response_id,
        "object": "response",
        "created_at": created_at,
        "model": model,
        "status": status,
        "error": error,
        "incomplete_details": incomplete_details,
        "output": output,
        "usage": usage,
        "parallel_tool_calls": fields.parallel_tool_calls,
        "tool_choice": fields.tool_choice,
        "tools": fields.tools
    })
}

fn failed_response(
    fields: &crate::frontends::ResponsesResponseFields,
    response_id: &str,
    created_at: i64,
    model: &str,
    message: &str,
) -> Value {
    response_object(
        fields,
        response_id,
        created_at,
        model,
        "failed",
        Some(json!({ "message": message, "code": "server_error" })),
        None,
        json!([]),
        Value::Null,
    )
}

fn failed_terminal_response(
    fields: &crate::frontends::ResponsesResponseFields,
    response_id: &str,
    created_at: i64,
    model: &str,
    failure: ResponsesTerminalFailure,
) -> Value {
    let message = match failure {
        ResponsesTerminalFailure::UnsupportedProviderReason => {
            "The upstream response used a terminal reason that cannot be represented by the Responses API."
        }
        ResponsesTerminalFailure::UnstreamableRefusal => {
            "The upstream refusal cannot be represented as typed refusal content on this streaming path."
        }
    };
    failed_response(fields, response_id, created_at, model, message)
}

pub fn aggregate_responses(
    model_name: &str,
    request_id: &str,
    events: Vec<StreamEvent>,
    usage: &crate::types::TokenUsage,
) -> Value {
    aggregate_responses_with_fields(
        model_name,
        request_id,
        events,
        usage,
        &crate::frontends::ResponsesResponseFields::default(),
    )
}

pub fn aggregate_responses_with_fields(
    model_name: &str,
    request_id: &str,
    events: Vec<StreamEvent>,
    usage: &crate::types::TokenUsage,
    fields: &crate::frontends::ResponsesResponseFields,
) -> Value {
    let mut text = String::new();
    let mut refusal = String::new();
    let mut finish = FinishReason::Stop;
    let mut tool_calls: Vec<(u32, String, String, String)> = Vec::new();

    for ev in events {
        match ev {
            StreamEvent::TextDelta(t) => text.push_str(&t),
            StreamEvent::RefusalDelta(t) => refusal.push_str(&t),
            StreamEvent::Finish(reason) => finish = reason,
            StreamEvent::ToolCallStart {
                index, id, name, ..
            } => {
                let call_id = id.unwrap_or_else(|| format!("call_{}_{}", request_id, index));
                tool_calls.push((index, call_id, name, String::new()));
            }
            StreamEvent::ToolCallArgsDelta { index, args } => {
                if let Some(tc) = tool_calls.iter_mut().find(|t| t.0 == index) {
                    tc.3.push_str(&args);
                }
            }
            _ => {}
        }
    }

    let terminal = responses_terminal(&finish);
    if let ResponsesTerminal::Failed(failure) = terminal {
        let response_id = format!("resp_{}", request_id.replace(['-', '_'], ""));
        return failed_terminal_response(
            fields,
            &response_id,
            chrono::Utc::now().timestamp(),
            model_name,
            failure,
        );
    }
    if terminal == ResponsesTerminal::Refusal {
        refusal.push_str(&text);
        text.clear();
    }
    let incomplete_reason = responses_incomplete_reason(&finish);
    let response_status = if incomplete_reason.is_some() {
        "incomplete"
    } else {
        "completed"
    };
    let mut output = Vec::new();
    let text_item_id = format!("msg_{}_0", request_id.replace(['-', '_'], ""));

    if !text.is_empty() || !refusal.is_empty() || tool_calls.is_empty() {
        let mut content = Vec::new();
        if !text.is_empty() || refusal.is_empty() {
            content.push(json!({
                "type": "output_text",
                "text": text,
                "annotations": []
            }));
        }
        if !refusal.is_empty() {
            content.push(json!({ "type": "refusal", "refusal": refusal }));
        }
        output.push(json!({
            "id": text_item_id,
            "type": "message",
            "role": "assistant",
            "status": response_status,
            "content": content
        }));
    }

    for (_, call_id, name, args) in tool_calls {
        output.push(json!({
            "id": call_id,
            "type": "function_call",
            "call_id": call_id,
            "name": name,
            "arguments": if args.is_empty() { "{}".to_string() } else { args },
            "status": response_status
        }));
    }

    let usage_obj = responses_usage(Some(usage));

    response_object(
        fields,
        &format!("resp_{}", request_id.replace(['-', '_'], "")),
        chrono::Utc::now().timestamp(),
        model_name,
        response_status,
        None,
        incomplete_reason.map(|reason| json!({ "reason": reason })),
        json!(output),
        usage_obj,
    )
}

#[cfg(test)]
mod tests {
    #![allow(dead_code)]

    use super::*;

    // ResponseError.code from OpenAI's current generated Python API types:
    // https://github.com/openai/openai-python/blob/main/src/openai/types/responses/response_error.py
    const OPENAI_RESPONSE_ERROR_CODES: &[&str] = &[
        "server_error",
        "rate_limit_exceeded",
        "invalid_prompt",
        "data_residency_mismatch",
        "bio_policy",
        "misalignment_policy_violation",
        "vector_store_timeout",
        "invalid_image",
        "invalid_image_format",
        "invalid_base64_image",
        "invalid_image_url",
        "image_too_large",
        "image_too_small",
        "image_parse_error",
        "image_content_policy_violation",
        "invalid_image_mode",
        "image_file_too_large",
        "unsupported_image_media_type",
        "empty_image_file",
        "failed_to_download_image",
        "image_file_not_found",
    ];

    // Test-only mirrors of required fields in OpenAI's generated Responses types.
    // These catch schema drift in synthesized responses/events rather than only
    // asserting individual JSON keys.
    // https://github.com/openai/openai-python/blob/main/src/openai/types/responses/response.py
    // https://github.com/openai/openai-python/blob/main/src/openai/types/responses/response_usage.py
    // https://github.com/openai/openai-python/blob/main/src/openai/types/responses/response_error.py
    // https://github.com/openai/openai-python/blob/main/src/openai/types/responses/response_text_delta_event.py
    // https://github.com/openai/openai-python/blob/main/src/openai/types/responses/response_text_done_event.py
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum ResponseToolChoiceSchema {
        Option(ResponseToolChoiceOptionSchema),
        Function(ResponseFunctionToolChoiceSchema),
    }

    #[derive(serde::Deserialize)]
    enum ResponseToolChoiceOptionSchema {
        #[serde(rename = "auto")]
        Auto,
        #[serde(rename = "none")]
        NoneChoice,
        #[serde(rename = "required")]
        Required,
    }

    #[derive(serde::Deserialize)]
    struct ResponseFunctionToolChoiceSchema {
        #[serde(rename = "type")]
        kind: ResponseFunctionKindSchema,
        name: String,
    }

    #[derive(serde::Deserialize)]
    enum ResponseFunctionKindSchema {
        #[serde(rename = "function")]
        Function,
    }

    #[derive(serde::Deserialize)]
    #[serde(tag = "type")]
    enum ResponseToolSchema {
        #[serde(rename = "function")]
        Function {
            name: String,
            parameters: Option<Value>,
            description: Option<String>,
            defer_loading: Option<bool>,
        },
    }

    #[derive(serde::Deserialize)]
    struct ResponseUsageSchema {
        input_tokens: u64,
        input_tokens_details: InputTokenDetailsSchema,
        output_tokens: u64,
        output_tokens_details: OutputTokenDetailsSchema,
        total_tokens: u64,
    }

    #[derive(serde::Deserialize)]
    struct InputTokenDetailsSchema {
        cached_tokens: u64,
        cache_write_tokens: u64,
    }

    #[derive(serde::Deserialize)]
    struct OutputTokenDetailsSchema {
        reasoning_tokens: u64,
    }

    #[derive(serde::Deserialize)]
    struct ResponseErrorSchema {
        code: String,
        message: String,
    }

    #[derive(serde::Deserialize)]
    #[serde(tag = "type")]
    enum ResponseContentSchema {
        #[serde(rename = "output_text")]
        OutputText {
            annotations: Vec<Value>,
            text: String,
        },
        #[serde(rename = "refusal")]
        Refusal { refusal: String },
    }

    #[derive(serde::Deserialize)]
    #[serde(tag = "type")]
    enum ResponseOutputItemSchema {
        #[serde(rename = "message")]
        Message {
            id: String,
            role: String,
            status: String,
            content: Vec<ResponseContentSchema>,
        },
        #[serde(rename = "function_call")]
        FunctionCall {
            id: String,
            call_id: String,
            name: String,
            arguments: String,
            status: String,
        },
    }

    #[derive(serde::Deserialize)]
    enum ResponseObjectSchema {
        #[serde(rename = "response")]
        Response,
    }

    #[derive(serde::Deserialize)]
    enum ResponseStatusSchema {
        #[serde(rename = "completed")]
        Completed,
        #[serde(rename = "failed")]
        Failed,
        #[serde(rename = "in_progress")]
        InProgress,
        #[serde(rename = "cancelled")]
        Cancelled,
        #[serde(rename = "queued")]
        Queued,
        #[serde(rename = "incomplete")]
        Incomplete,
    }

    #[derive(serde::Deserialize)]
    struct ResponseSchema {
        id: String,
        object: ResponseObjectSchema,
        created_at: f64,
        model: String,
        status: ResponseStatusSchema,
        error: Option<ResponseErrorSchema>,
        incomplete_details: Option<Value>,
        output: Vec<ResponseOutputItemSchema>,
        usage: Option<ResponseUsageSchema>,
        parallel_tool_calls: bool,
        tool_choice: ResponseToolChoiceSchema,
        tools: Vec<ResponseToolSchema>,
    }

    #[derive(serde::Deserialize)]
    #[serde(tag = "type")]
    enum ResponseLifecycleEventSchema {
        #[serde(rename = "response.created")]
        Created {
            sequence_number: u64,
            response: ResponseSchema,
        },
        #[serde(rename = "response.in_progress")]
        InProgress {
            sequence_number: u64,
            response: ResponseSchema,
        },
        #[serde(rename = "response.completed")]
        Completed {
            sequence_number: u64,
            response: ResponseSchema,
        },
        #[serde(rename = "response.incomplete")]
        Incomplete {
            sequence_number: u64,
            response: ResponseSchema,
        },
        #[serde(rename = "response.failed")]
        Failed {
            sequence_number: u64,
            response: ResponseSchema,
        },
    }

    #[derive(serde::Deserialize)]
    #[serde(tag = "type")]
    enum ResponseOutputItemEventSchema {
        #[serde(rename = "response.output_item.added")]
        Added {
            sequence_number: u64,
            output_index: u64,
            item: ResponseOutputItemSchema,
        },
        #[serde(rename = "response.output_item.done")]
        Done {
            sequence_number: u64,
            output_index: u64,
            item: ResponseOutputItemSchema,
        },
    }

    #[derive(serde::Deserialize)]
    #[serde(tag = "type")]
    enum ResponseContentPartEventSchema {
        #[serde(rename = "response.content_part.added")]
        Added {
            sequence_number: u64,
            content_index: u64,
            item_id: String,
            output_index: u64,
            part: ResponseContentSchema,
        },
        #[serde(rename = "response.content_part.done")]
        Done {
            sequence_number: u64,
            content_index: u64,
            item_id: String,
            output_index: u64,
            part: ResponseContentSchema,
        },
    }

    #[derive(serde::Deserialize)]
    enum ResponseTextDeltaEventTypeSchema {
        #[serde(rename = "response.output_text.delta")]
        Delta,
    }

    #[derive(serde::Deserialize)]
    struct ResponseTextDeltaEventSchema {
        #[serde(rename = "type")]
        kind: ResponseTextDeltaEventTypeSchema,
        sequence_number: u64,
        content_index: u64,
        delta: String,
        item_id: String,
        logprobs: Vec<Value>,
        output_index: u64,
    }

    #[derive(serde::Deserialize)]
    enum ResponseTextDoneEventTypeSchema {
        #[serde(rename = "response.output_text.done")]
        Done,
    }

    #[derive(serde::Deserialize)]
    struct ResponseTextDoneEventSchema {
        #[serde(rename = "type")]
        kind: ResponseTextDoneEventTypeSchema,
        sequence_number: u64,
        content_index: u64,
        item_id: String,
        logprobs: Vec<Value>,
        output_index: u64,
        text: String,
    }

    fn validate_current_responses_event(event: Value) {
        let event_type = event["type"].as_str().expect("event type");
        match event_type {
            "response.created"
            | "response.in_progress"
            | "response.completed"
            | "response.incomplete"
            | "response.failed" => {
                let _: ResponseLifecycleEventSchema = serde_json::from_value(event).unwrap();
            }
            "response.output_item.added" | "response.output_item.done" => {
                let _: ResponseOutputItemEventSchema = serde_json::from_value(event).unwrap();
            }
            "response.content_part.added" | "response.content_part.done" => {
                let _: ResponseContentPartEventSchema = serde_json::from_value(event).unwrap();
            }
            "response.output_text.delta" => {
                let _: ResponseTextDeltaEventSchema = serde_json::from_value(event).unwrap();
            }
            "response.output_text.done" => {
                let _: ResponseTextDoneEventSchema = serde_json::from_value(event).unwrap();
            }
            unexpected => panic!("uncovered Responses event schema: {unexpected}"),
        }
    }

    fn validate_current_responses_response(response: Value) -> ResponseSchema {
        serde_json::from_value(response).unwrap()
    }

    #[test]
    fn synthesized_responses_stream_and_aggregation_match_current_wire_schemas() {
        let request = decode_request(json!({
            "model": "example",
            "input": "hello",
            "tool_choice": {"type": "function", "name": "weather"},
            "tools": [{
                "type": "function",
                "name": "weather",
                "description": "Get the weather",
                "parameters": {"type": "object"}
            }]
        }))
        .unwrap();
        let fields = response_fields_from_request(&request).unwrap();
        let usage = crate::types::TokenUsage {
            input: Some(3),
            output: Some(2),
            cached: None,
            cache_write: None,
            thinking: None,
        };
        let mut encoder = ResponsesEncoder::new(EncoderCtx {
            model_name: "example".into(),
            request_id: "schema-test".into(),
            created: 1,
            responses: fields.clone(),
        });
        let mut frames = Vec::new();
        for event in [
            StreamEvent::Start {
                upstream_request_id: None,
            },
            StreamEvent::TextDelta("hello".into()),
            StreamEvent::Usage(usage.clone()),
            StreamEvent::Finish(FinishReason::Stop),
        ] {
            frames.extend(encoder.encode(event));
        }

        let mut wire_events = Vec::new();
        for frame in frames {
            let frame = String::from_utf8(frame.to_vec()).unwrap();
            let payload = frame
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .expect("SSE data payload");
            let event: Value = serde_json::from_str(payload).unwrap();
            validate_current_responses_event(event.clone());
            wire_events.push(event);
        }
        for required_event in [
            "response.created",
            "response.in_progress",
            "response.output_item.added",
            "response.content_part.added",
            "response.output_text.delta",
            "response.output_text.done",
            "response.content_part.done",
            "response.output_item.done",
            "response.completed",
        ] {
            assert!(
                wire_events
                    .iter()
                    .any(|event| event["type"] == required_event),
                "missing schema-validated event {required_event}"
            );
        }
        let completed = wire_events
            .iter()
            .find(|event| event["type"] == "response.completed")
            .unwrap()["response"]
            .clone();
        let response = validate_current_responses_response(completed);
        assert!(matches!(response.object, ResponseObjectSchema::Response));
        assert!(matches!(response.status, ResponseStatusSchema::Completed));
        assert!(response.parallel_tool_calls);
        assert_eq!(response.tools.len(), 1);
        match response.tool_choice {
            ResponseToolChoiceSchema::Function(choice) => {
                assert!(matches!(choice.kind, ResponseFunctionKindSchema::Function));
                assert_eq!(choice.name, "weather");
            }
            ResponseToolChoiceSchema::Option(_) => panic!("function tool choice was not preserved"),
        }
        match response.tools.into_iter().next().unwrap() {
            ResponseToolSchema::Function { name, .. } => assert_eq!(name, "weather"),
        }
        let usage = response.usage.unwrap();
        assert_eq!(usage.input_tokens_details.cached_tokens, 0);
        assert_eq!(usage.input_tokens_details.cache_write_tokens, 0);
        assert_eq!(usage.output_tokens_details.reasoning_tokens, 0);
        assert_eq!(usage.total_tokens, usage.input_tokens + usage.output_tokens);

        let mut incomplete_encoder = ResponsesEncoder::new(EncoderCtx {
            model_name: "example".into(),
            request_id: "schema-test-incomplete".into(),
            created: 1,
            responses: fields.clone(),
        });
        let incomplete_frames = incomplete_encoder
            .encode(StreamEvent::TextDelta("partial".into()))
            .into_iter()
            .chain(incomplete_encoder.encode(StreamEvent::Finish(FinishReason::Length)));
        let mut incomplete_response = None;
        for frame in incomplete_frames {
            let frame = String::from_utf8(frame.to_vec()).unwrap();
            let payload = frame
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .expect("incomplete SSE data payload");
            let event: Value = serde_json::from_str(payload).unwrap();
            validate_current_responses_event(event.clone());
            if event["type"] == "response.incomplete" {
                incomplete_response = Some(event["response"].clone());
            }
        }
        let incomplete = validate_current_responses_response(
            incomplete_response.expect("incomplete terminal response"),
        );
        assert!(matches!(
            incomplete.status,
            ResponseStatusSchema::Incomplete
        ));
        assert_eq!(
            incomplete.incomplete_details.unwrap()["reason"],
            "max_output_tokens"
        );

        let nonstream = aggregate_responses_with_fields(
            "example",
            "schema-test-nonstream",
            vec![
                StreamEvent::TextDelta("hello".into()),
                StreamEvent::Finish(FinishReason::Stop),
            ],
            &usage_for_schema_test(),
            &fields,
        );
        let nonstream = validate_current_responses_response(nonstream);
        assert!(matches!(nonstream.status, ResponseStatusSchema::Completed));
        assert!(nonstream.parallel_tool_calls);
        assert_eq!(nonstream.tools.len(), 1);

        let failed_aggregate = aggregate_responses_with_fields(
            "example",
            "schema-test-terminal-failure",
            vec![StreamEvent::Finish(FinishReason::Other(
                "pause_turn".into(),
            ))],
            &usage_for_schema_test(),
            &fields,
        );
        let failed_aggregate = validate_current_responses_response(failed_aggregate);
        assert!(matches!(
            failed_aggregate.status,
            ResponseStatusSchema::Failed
        ));
        let failed_error = failed_aggregate.error.unwrap();
        assert_eq!(failed_error.code, "server_error");
        assert!(OPENAI_RESPONSE_ERROR_CODES.contains(&failed_error.code.as_str()));

        let mut failed_encoder = ResponsesEncoder::new(EncoderCtx {
            model_name: "example".into(),
            request_id: "schema-test-failure".into(),
            created: 1,
            responses: fields.clone(),
        });
        let failed_frames = failed_encoder.error_frame("upstream stream ended early");
        let failed_payload = String::from_utf8(failed_frames[0].to_vec()).unwrap();
        let failed_payload = failed_payload
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .expect("failed SSE data payload");
        let failed_event: Value = serde_json::from_str(failed_payload).unwrap();
        validate_current_responses_event(failed_event.clone());
        assert_eq!(failed_event["response"]["error"]["code"], "server_error");
        assert!(OPENAI_RESPONSE_ERROR_CODES
            .contains(&failed_event["response"]["error"]["code"].as_str().unwrap()));
    }

    fn usage_for_schema_test() -> crate::types::TokenUsage {
        crate::types::TokenUsage {
            input: Some(3),
            output: Some(2),
            cached: None,
            cache_write: None,
            thinking: None,
        }
    }

    #[test]
    fn response_fields_preserve_tool_choice_and_function_tools() {
        let request = decode_request(json!({
            "model": "example",
            "input": "hello",
            "tool_choice": {"type": "function", "name": "weather"},
            "tools": [{
                "type": "function",
                "name": "weather",
                "description": "Get the weather",
                "parameters": {"type": "object"}
            }]
        }))
        .unwrap();
        let fields = response_fields_from_request(&request).unwrap();

        assert!(fields.parallel_tool_calls);
        assert_eq!(
            fields.tool_choice,
            json!({"type": "function", "name": "weather"})
        );
        assert_eq!(
            fields.tools,
            vec![json!({
                "type": "function",
                "name": "weather",
                "description": "Get the weather",
                "parameters": {"type": "object"}
            })]
        );
    }

    #[test]
    fn response_fields_reject_a_named_tool_choice_without_its_name() {
        let mut request = decode_request(json!({
            "model": "example",
            "input": "hello",
            "tool_choice": {"type": "function", "name": "weather"}
        }))
        .unwrap();
        request.tool_choice_name = None;

        assert!(response_fields_from_request(&request).is_err());
    }

    #[test]
    fn validated_responses_body_is_retained_for_native_passthrough() {
        let body = json!({
            "model": "example",
            "input": "hello",
            "stream": true,
            "stream_options": {"include_obfuscation": false}
        });
        let expected = body.to_string();
        let request = decode_request(body).unwrap();
        assert_eq!(request.raw_body.as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn unsupported_responses_semantics_are_rejected_before_passthrough() {
        let error = decode_request(json!({
            "model": "example",
            "input": "hello",
            "metadata": {"private": "value"}
        }))
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("response metadata storage is unsupported"));
    }

    #[test]
    fn nonstream_aggregation_preserves_refusal_and_incomplete_semantics() {
        let usage = crate::types::TokenUsage::default();
        let refusal = aggregate_responses(
            "example",
            "request-id",
            vec![
                StreamEvent::RefusalDelta("not allowed".into()),
                StreamEvent::Finish(FinishReason::Stop),
            ],
            &usage,
        );
        assert_eq!(
            refusal.pointer("/output/0/content/0/type"),
            Some(&json!("refusal"))
        );
        assert_eq!(
            refusal.pointer("/output/0/content/0/refusal"),
            Some(&json!("not allowed"))
        );
        assert_eq!(refusal["status"], "completed");

        for (finish, reason) in [
            (FinishReason::Length, "max_output_tokens"),
            (FinishReason::ContentFilter, "content_filter"),
            (FinishReason::Other("steered".into()), "steered"),
        ] {
            let response = aggregate_responses(
                "example",
                "request-id",
                vec![
                    StreamEvent::TextDelta("partial".into()),
                    StreamEvent::Finish(finish),
                ],
                &usage,
            );
            assert_eq!(response["status"], "incomplete");
            assert_eq!(
                response.pointer("/incomplete_details/reason"),
                Some(&json!(reason))
            );
            assert_eq!(
                response.pointer("/output/0/status"),
                Some(&json!("incomplete"))
            );
        }

        for reason in ["pause_turn", "GEMINI_FUTURE_REASON", "provider_reason"] {
            let response = aggregate_responses(
                "example",
                "request-id",
                vec![
                    StreamEvent::TextDelta("partial".into()),
                    StreamEvent::Finish(FinishReason::Other(reason.into())),
                ],
                &usage,
            );
            assert_eq!(response["status"], "failed");
            let code = response
                .pointer("/error/code")
                .and_then(Value::as_str)
                .unwrap();
            assert!(
                OPENAI_RESPONSE_ERROR_CODES.contains(&code),
                "Responses error code {code:?} is outside the current OpenAI ResponseError enum"
            );
            assert_eq!(code, "server_error");
            assert_eq!(
                responses_terminal(&FinishReason::Other(reason.into())),
                ResponsesTerminal::Failed(ResponsesTerminalFailure::UnsupportedProviderReason)
            );
            assert_eq!(response["incomplete_details"], Value::Null);
            assert!(!response.to_string().contains(reason));
        }
    }

    #[test]
    fn anthropic_refusal_stop_reason_completes_without_invalid_incomplete_reason() {
        use crate::adapters::{anthropic::AnthropicAdapter, Adapter};

        let events = AnthropicAdapter::new()
            .parse_full_response(&json!({
                "content": [{"type": "text", "text": "I cannot help with that."}],
                "stop_reason": "refusal"
            }))
            .unwrap();
        let response = aggregate_responses(
            "example",
            "request-id",
            events,
            &crate::types::TokenUsage::default(),
        );

        assert_eq!(response["status"], "completed");
        assert_eq!(response["incomplete_details"], Value::Null);
        assert_eq!(
            response.pointer("/output/0/content/0/type"),
            Some(&json!("refusal"))
        );
        assert_eq!(
            response.pointer("/output/0/content/0/refusal"),
            Some(&json!("I cannot help with that."))
        );
    }

    #[test]
    fn streaming_encoder_emits_refusal_and_incomplete_terminal_events() {
        let mut encoder = ResponsesEncoder::new(EncoderCtx {
            model_name: "example".into(),
            request_id: "request-id".into(),
            created: 1,
            responses: crate::frontends::ResponsesResponseFields::default(),
        });
        let mut frames = encoder.encode(StreamEvent::RefusalDelta("not allowed".into()));
        frames.extend(encoder.encode(StreamEvent::Usage(crate::types::TokenUsage {
            input: Some(10),
            output: Some(5),
            cached: Some(2),
            cache_write: Some(3),
            thinking: Some(4),
        })));
        frames.extend(encoder.encode(StreamEvent::Finish(FinishReason::Length)));
        let wire = frames
            .iter()
            .map(|frame| String::from_utf8_lossy(frame).into_owned())
            .collect::<String>();
        assert!(wire.contains("response.refusal.delta"));
        assert!(wire.contains("response.refusal.done"));
        assert!(wire.contains("response.incomplete"));
        assert!(wire.contains("max_output_tokens"));
        assert!(wire.contains("\"status\":\"incomplete\""));
        assert!(wire.contains("\"input_tokens_details\""));
        assert!(wire.contains("\"cache_write_tokens\":3"));
        assert!(wire.contains("\"output_tokens_details\""));
        assert!(wire.contains("\"reasoning_tokens\":4"));
    }

    #[test]
    fn streaming_anthropic_refusal_fails_closed_after_text_was_streamed() {
        use crate::adapters::{anthropic::AnthropicAdapter, Adapter};

        let events = AnthropicAdapter::new()
            .parse_stream_chunk(r#"{"type":"message_delta","delta":{"stop_reason":"refusal"}}"#)
            .unwrap();
        let mut encoder = ResponsesEncoder::new(EncoderCtx {
            model_name: "example".into(),
            request_id: "request-id".into(),
            created: 1,
            responses: crate::frontends::ResponsesResponseFields::default(),
        });
        let mut frames = encoder.encode(StreamEvent::TextDelta("I cannot help with that.".into()));
        for event in events {
            frames.extend(encoder.encode(event));
        }
        let wire = frames
            .iter()
            .map(|frame| String::from_utf8_lossy(frame).into_owned())
            .collect::<String>();

        assert!(wire.contains("response.failed"));
        assert!(wire.contains("\"code\":\"server_error\""));
        assert!(!wire.contains("response.completed"));
        assert!(!wire.contains("response.incomplete"));
        assert!(!wire.contains("\"reason\":\"refusal\""));
    }

    #[test]
    fn streaming_anthropic_pause_turn_fails_without_echoing_provider_reason() {
        use crate::adapters::{anthropic::AnthropicAdapter, Adapter};

        let events = AnthropicAdapter::new()
            .parse_stream_chunk(r#"{"type":"message_delta","delta":{"stop_reason":"pause_turn"}}"#)
            .unwrap();
        let mut encoder = ResponsesEncoder::new(EncoderCtx {
            model_name: "example".into(),
            request_id: "request-id".into(),
            created: 1,
            responses: crate::frontends::ResponsesResponseFields::default(),
        });
        let mut frames = encoder.encode(StreamEvent::TextDelta("partial".into()));
        for event in events {
            frames.extend(encoder.encode(event));
        }
        let wire = frames
            .iter()
            .map(|frame| String::from_utf8_lossy(frame).into_owned())
            .collect::<String>();

        assert!(wire.contains("response.failed"));
        assert!(wire.contains("\"code\":\"server_error\""));
        assert!(!wire.contains("response.incomplete"));
        assert!(!wire.contains("pause_turn"));
        assert!(!wire.contains("\"reason\":"));
    }
}
