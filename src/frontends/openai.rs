//! OpenAI Chat Completions inbound frontend (FR-1.1).

use bytes::Bytes;
use serde_json::{json, Value};

use crate::frontends::{sse_frame, EncoderCtx};
use crate::types::{
    FinishReason, ImageData, InternalRequest, Message, Part, ProxyError, Role, StreamEvent,
    ThinkingLevel, ToolChoice, ToolDef,
};

pub fn decode_request(body: Value) -> Result<InternalRequest, ProxyError> {
    let obj = body
        .as_object()
        .ok_or_else(|| ProxyError::bad_request("request body must be a JSON object"))?;

    let model = obj
        .get("model")
        .and_then(|m| m.as_str())
        .ok_or_else(|| ProxyError::bad_request("missing required field 'model'"))?
        .to_string();

    let messages = obj
        .get("messages")
        .and_then(|m| m.as_array())
        .ok_or_else(|| ProxyError::bad_request("missing required field 'messages'"))?;

    let mut translation_issues = nested_translation_issues(obj);
    let mut system = Vec::new();
    let mut out_messages = Vec::new();

    for m in messages {
        let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("user");
        match role {
            "system" | "developer" => {
                if let Some(text) = content_as_text(m.get("content")) {
                    if !text.is_empty() {
                        system.push(text);
                    }
                }
            }
            "user" => {
                out_messages.push(Message {
                    role: Role::User,
                    parts: decode_content_parts(m.get("content")),
                });
            }
            "assistant" => {
                let mut parts = decode_content_parts(m.get("content"));
                if let Some(reasoning) = m
                    .get("reasoning_content")
                    .or_else(|| m.get("reasoning"))
                    .and_then(Value::as_str)
                {
                    if !reasoning.is_empty() {
                        parts.push(Part::Thinking {
                            text: reasoning.to_string(),
                            signature: m
                                .get("reasoning_signature")
                                .and_then(Value::as_str)
                                .map(String::from),
                        });
                    }
                }
                if let Some(tcs) = m.get("tool_calls").and_then(|t| t.as_array()) {
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
                out_messages.push(Message {
                    role: Role::Assistant,
                    parts,
                });
            }
            "tool" | "function" => {
                let tool_call_id = m
                    .get("tool_call_id")
                    .and_then(|i| i.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = m.get("name").and_then(|n| n.as_str()).map(String::from);
                let content = content_as_text(m.get("content")).unwrap_or_default();
                out_messages.push(Message {
                    role: Role::Tool,
                    parts: vec![Part::ToolResult {
                        tool_call_id,
                        name,
                        content,
                        is_error: false,
                    }],
                });
            }
            _ => {}
        }
    }

    let tools = decode_tools(obj.get("tools"));
    let (tool_choice, tool_choice_name) = decode_tool_choice(obj.get("tool_choice"));

    let mut params = crate::types::SamplingParams {
        temperature: obj.get("temperature").and_then(|v| v.as_f64()),
        top_p: obj.get("top_p").and_then(|v| v.as_f64()),
        top_k: obj.get("top_k").and_then(|v| v.as_f64()),
        max_tokens: obj
            .get("max_tokens")
            .or_else(|| obj.get("max_completion_tokens"))
            .and_then(|v| v.as_u64())
            .map(|v| v as u32),
        seed: obj.get("seed").and_then(|v| v.as_i64()),
        presence_penalty: obj.get("presence_penalty").and_then(|v| v.as_f64()),
        frequency_penalty: obj.get("frequency_penalty").and_then(|v| v.as_f64()),
        ..Default::default()
    };
    if let Some(stop) = obj.get("stop") {
        match stop {
            Value::String(s) => params.stop.push(s.clone()),
            Value::Array(a) => {
                for s in a {
                    if let Some(s) = s.as_str() {
                        params.stop.push(s.to_string());
                    }
                }
            }
            _ => {}
        }
    }

    let thinking = obj
        .get("reasoning_effort")
        .and_then(|v| v.as_str())
        .and_then(map_reasoning_effort);

    let stream = obj.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);
    let include_usage = obj
        .get("stream_options")
        .and_then(|value| value.get("include_usage"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // Fields we don't model: keep only genuinely unknown ones for optional forwarding.
    let mut extra = serde_json::Map::new();
    const KNOWN: [&str; 16] = [
        "model",
        "messages",
        "stream",
        "stream_options",
        "temperature",
        "top_p",
        "top_k",
        "max_tokens",
        "max_completion_tokens",
        "stop",
        "seed",
        "presence_penalty",
        "frequency_penalty",
        "tools",
        "tool_choice",
        "reasoning_effort",
    ];
    for (k, v) in obj {
        if !KNOWN.contains(&k.as_str()) {
            extra.insert(k.clone(), v.clone());
        }
    }
    translation_issues.extend(crate::frontends::resolve_tool_result_names(
        &mut out_messages,
    ));
    crate::frontends::attach_translation_issues(&mut extra, translation_issues);

    Ok(InternalRequest {
        requested_model: model,
        system,
        messages: out_messages,
        tools,
        tool_choice,
        tool_choice_name,
        params,
        stream,
        include_usage,
        thinking,
        extra,
        raw_body: None,
    })
}

fn nested_translation_issues(obj: &serde_json::Map<String, Value>) -> Vec<String> {
    let mut issues = Vec::new();

    if let Some(messages) = obj.get("messages").and_then(Value::as_array) {
        for (message_index, message) in messages.iter().enumerate() {
            let role = message
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("user");
            if !matches!(
                role,
                "system" | "developer" | "user" | "assistant" | "tool" | "function"
            ) {
                issues.push(format!(
                    "messages[{message_index}] has unsupported role '{role}'"
                ));
            }
            let text_only = matches!(role, "system" | "developer" | "tool" | "function");
            inspect_openai_content(
                &format!("messages[{message_index}].content"),
                message.get("content"),
                text_only,
                &mut issues,
            );

            for field in ["audio", "function_call", "refusal", "reasoning_details"] {
                if message.get(field).is_some_and(|value| !value.is_null()) {
                    issues.push(format!(
                        "messages[{message_index}].{field} has no canonical cross-format representation"
                    ));
                }
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
            }
        }
    }

    issues
}

fn inspect_openai_content(
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

fn content_as_text(content: Option<&Value>) -> Option<String> {
    match content? {
        Value::String(s) => Some(s.clone()),
        Value::Array(parts) => {
            let mut text = String::new();
            for p in parts {
                if p.get("type").and_then(|t| t.as_str()) == Some("text") {
                    if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
                        text.push_str(t);
                    }
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
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .map(String::from);
            (Some(ToolChoice::Specific), name)
        }
        _ => (Some(ToolChoice::Auto), None),
    }
}

// ---------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------

pub struct OpenAiEncoder {
    ctx: EncoderCtx,
    include_usage: bool,
    role_sent: bool,
    finish_sent: bool,
    usage: Option<crate::types::TokenUsage>,
    id: String,
    tool_started: std::collections::HashSet<u32>,
}

impl OpenAiEncoder {
    pub fn new(ctx: EncoderCtx) -> Self {
        let id = format!("chatcmpl-{}", ctx.request_id.replace(['-', '_'], ""));
        OpenAiEncoder {
            ctx,
            include_usage: true,
            role_sent: false,
            finish_sent: false,
            usage: None,
            id,
            tool_started: Default::default(),
        }
    }

    pub fn set_include_usage(&mut self, v: bool) {
        self.include_usage = v;
    }

    fn chunk(&self, delta: Value, finish: Option<&str>) -> Bytes {
        let frame = json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.ctx.created,
            "model": self.ctx.model_name,
            "choices": [{
                "index": 0,
                "delta": delta,
                "finish_reason": finish
            }]
        });
        sse_frame(None, &frame.to_string())
    }

    fn ensure_role(&mut self, out: &mut Vec<Bytes>) {
        if !self.role_sent {
            self.role_sent = true;
            out.push(self.chunk(json!({ "role": "assistant", "content": "" }), None));
        }
    }

    pub fn encode(&mut self, event: StreamEvent) -> Vec<Bytes> {
        let mut out = Vec::new();
        match event {
            StreamEvent::Start { .. } => {}
            StreamEvent::TextDelta(t) | StreamEvent::RefusalDelta(t) => {
                self.ensure_role(&mut out);
                out.push(self.chunk(json!({ "content": t }), None));
            }
            StreamEvent::ThinkingBlockStart { thinking, .. }
            | StreamEvent::ThinkingDelta { text: thinking, .. } => {
                self.ensure_role(&mut out);
                if !thinking.is_empty() {
                    out.push(self.chunk(json!({ "reasoning_content": thinking }), None));
                }
            }
            StreamEvent::ThinkingBlockStop { .. } | StreamEvent::RedactedThinking { .. } => {}
            StreamEvent::ToolCallStart {
                index, id, name, ..
            } => {
                self.ensure_role(&mut out);
                self.tool_started.insert(index);
                out.push(self.chunk(
                    json!({
                        "tool_calls": [{
                            "index": index,
                            "id": id.unwrap_or_else(|| format!("call_{}_{}", self.id, index)),
                            "type": "function",
                            "function": { "name": name, "arguments": "" }
                        }]
                    }),
                    None,
                ));
            }
            StreamEvent::ToolCallArgsDelta { index, args } => {
                self.ensure_role(&mut out);
                out.push(self.chunk(
                    json!({
                        "tool_calls": [{
                            "index": index,
                            "function": { "arguments": args }
                        }]
                    }),
                    None,
                ));
            }
            StreamEvent::Usage(u) => {
                self.usage = Some(u);
            }
            StreamEvent::Finish(f) => {
                self.finish_sent = true;
                out.push(self.chunk(json!({}), Some(openai_finish(&f))));
                if self.include_usage {
                    if let Some(u) = &self.usage {
                        out.push(self.usage_chunk(u));
                    }
                }
            }
        }
        out
    }

    fn usage_chunk(&self, u: &crate::types::TokenUsage) -> Bytes {
        let mut usage = json!({
            "prompt_tokens": u.input.unwrap_or(0),
            "completion_tokens": u.output.unwrap_or(0),
            "total_tokens": u.input.unwrap_or(0) + u.output.unwrap_or(0),
        });
        if let Some(cached) = u.cached {
            usage["prompt_tokens_details"] = json!({ "cached_tokens": cached });
        }
        if let Some(t) = u.thinking {
            usage["completion_tokens_details"] = json!({ "reasoning_tokens": t });
        }
        let frame = json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.ctx.created,
            "model": self.ctx.model_name,
            "choices": [],
            "usage": usage
        });
        sse_frame(None, &frame.to_string())
    }

    pub fn finalize(&mut self) -> Vec<Bytes> {
        let mut out = Vec::new();
        if !self.finish_sent {
            self.ensure_role(&mut out);
            out.push(self.chunk(json!({}), Some("stop")));
            if self.include_usage {
                if let Some(u) = &self.usage {
                    out.push(self.usage_chunk(u));
                }
            }
        }
        out.push(Bytes::from_static(b"data: [DONE]\n\n"));
        out
    }

    pub fn error_frame(&mut self, message: &str) -> Vec<Bytes> {
        let frame = json!({
            "error": {
                "message": message,
                "type": "upstream_error",
                "code": "stream_error"
            }
        });
        // Emit the error object, then an explicit error finish_reason so a
        // client cannot mistake a truncated stream for a clean completion
        // (FR-4.6, NFR-2.9: no silent splicing), then terminate the SSE stream.
        vec![
            sse_frame(None, &frame.to_string()),
            self.chunk(json!({}), Some("error")),
            Bytes::from_static(b"data: [DONE]\n\n"),
        ]
    }
}

fn openai_finish(f: &FinishReason) -> &str {
    match f {
        FinishReason::Stop => "stop",
        FinishReason::Length => "length",
        FinishReason::ToolCalls => "tool_calls",
        FinishReason::ContentFilter => "content_filter",
        FinishReason::Other(s) => s.as_str(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structured_response_formats_require_structured_output_capability() {
        for response_format in [
            json!({ "type": "json_object" }),
            json!({
                "type": "json_schema",
                "json_schema": { "name": "answer", "schema": { "type": "object" } }
            }),
        ] {
            let request = decode_request(json!({
                "model": "test-model",
                "messages": [{ "role": "user", "content": "answer" }],
                "response_format": response_format
            }))
            .unwrap();
            assert!(request.capability_needs().structured_output);
        }
    }

    #[test]
    fn streams_canonical_refusals_as_assistant_text() {
        let mut encoder = OpenAiEncoder::new(EncoderCtx {
            model_name: "example".into(),
            request_id: "request-id".into(),
            created: 1,
            responses: crate::frontends::ResponsesResponseFields::default(),
        });
        let frames = encoder.encode(StreamEvent::RefusalDelta("not allowed".into()));
        let wire = frames
            .iter()
            .map(|frame| String::from_utf8_lossy(frame).into_owned())
            .collect::<String>();
        assert!(wire.contains("not allowed"));
        assert!(wire.contains("\"content\""));
    }
}
