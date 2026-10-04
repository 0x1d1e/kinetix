//! Anthropic Messages inbound frontend (FR-1.3).

use bytes::Bytes;
use serde_json::{json, Value};

use crate::frontends::{sse_frame, EncoderCtx};
use crate::types::{
    FinishReason, ImageData, InternalRequest, Message, Part, ProxyError, Role, StreamEvent,
    ThinkingLevel, ToolChoice, ToolDef,
};

/// Top-level Messages fields this decoder models into the canonical request.
/// Every entry needs a disposition in
/// `tests/fixtures/field-contract/anthropic-messages.json`; anything else lands
/// in `InternalRequest::extra`.
pub const DECODED_FIELDS: &[&str] = &[
    "model",
    "messages",
    "system",
    "stream",
    "temperature",
    "top_p",
    "top_k",
    "max_tokens",
    "stop_sequences",
    "output_config",
    "tools",
    "tool_choice",
    "thinking",
];

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
    match obj.get("system") {
        Some(Value::String(s)) => system.push(s.clone()),
        Some(Value::Array(blocks)) => {
            for b in blocks {
                if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                    if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                        system.push(t.to_string());
                    }
                }
            }
        }
        _ => {}
    }

    let mut out_messages = Vec::new();
    for m in messages {
        let role = match m.get("role").and_then(|r| r.as_str()) {
            Some("assistant") => Role::Assistant,
            _ => Role::User,
        };
        let parts = decode_content(m.get("content"));
        if !parts.is_empty() {
            out_messages.push(Message { role, parts });
        }
    }

    let tools = decode_tools(obj.get("tools"));
    let (tool_choice, tool_choice_name) = decode_tool_choice(obj.get("tool_choice"));

    let params = crate::types::SamplingParams {
        temperature: obj.get("temperature").and_then(|v| v.as_f64()),
        top_p: obj.get("top_p").and_then(|v| v.as_f64()),
        top_k: obj.get("top_k").and_then(|v| v.as_f64()),
        max_tokens: obj
            .get("max_tokens")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32),
        ..Default::default()
    };
    let mut params = params;
    if let Some(stop) = obj.get("stop_sequences").and_then(|s| s.as_array()) {
        for s in stop {
            if let Some(s) = s.as_str() {
                params.stop.push(s.to_string());
            }
        }
    }

    let effort = obj
        .get("output_config")
        .and_then(|config| config.get("effort"))
        .and_then(Value::as_str)
        .map(|effort| match effort {
            "low" => Ok(ThinkingLevel::Low),
            "medium" => Ok(ThinkingLevel::Medium),
            "high" => Ok(ThinkingLevel::High),
            "xhigh" => Ok(ThinkingLevel::XHigh),
            "max" => Ok(ThinkingLevel::Max),
            _ => Err(ProxyError::bad_request(format!(
                "unsupported Anthropic output_config.effort '{effort}'"
            ))),
        })
        .transpose()?;

    let thinking = match obj.get("thinking") {
        None => None,
        Some(t) => {
            let ttype = t.get("type").and_then(|x| x.as_str()).unwrap_or("");
            match ttype {
                "disabled" => Some(ThinkingLevel::Off),
                "adaptive" => Some(effort.unwrap_or(ThinkingLevel::Default)),
                "enabled" => {
                    let budget = t.get("budget_tokens").and_then(|b| b.as_u64()).unwrap_or(0);
                    Some(match budget {
                        0 => ThinkingLevel::Off,
                        b if b <= 2048 => ThinkingLevel::Low,
                        b if b <= 8192 => ThinkingLevel::Medium,
                        _ => ThinkingLevel::High,
                    })
                }
                _ => None,
            }
        }
    };

    let stream = obj.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);

    let mut extra = serde_json::Map::new();
    for (k, v) in obj {
        if !DECODED_FIELDS.contains(&k.as_str()) {
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
        include_usage: false,
        thinking,
        extra,
        raw_body: None,
    })
}

fn nested_translation_issues(obj: &serde_json::Map<String, Value>) -> Vec<String> {
    let mut issues = Vec::new();

    if let Some(Value::Array(blocks)) = obj.get("system") {
        for (index, block) in blocks.iter().enumerate() {
            let kind = block.get("type").and_then(Value::as_str).unwrap_or("");
            if kind != "text" {
                issues.push(format!(
                    "system[{index}] type '{kind}' has no canonical cross-format representation"
                ));
            }
        }
    }

    if let Some(messages) = obj.get("messages").and_then(Value::as_array) {
        for (message_index, message) in messages.iter().enumerate() {
            let role = message
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("user");
            if !matches!(role, "user" | "assistant") {
                issues.push(format!(
                    "messages[{message_index}] has unsupported role '{role}'"
                ));
            }

            match message.get("content") {
                Some(Value::Array(blocks)) => {
                    for (block_index, block) in blocks.iter().enumerate() {
                        let kind = block.get("type").and_then(Value::as_str).unwrap_or("");
                        let path = format!("messages[{message_index}].content[{block_index}]");
                        match kind {
                            "text" | "tool_use" | "thinking" | "redacted_thinking" => {}
                            "image" => {
                                let source_type = block
                                    .get("source")
                                    .and_then(|source| source.get("type"))
                                    .and_then(Value::as_str)
                                    .unwrap_or("");
                                if !matches!(source_type, "base64" | "url") {
                                    issues.push(format!(
                                        "{path} image source type '{source_type}' is not translatable"
                                    ));
                                }
                            }
                            "tool_result" => {
                                if let Some(Value::Array(result_blocks)) = block.get("content") {
                                    for (result_index, result_block) in
                                        result_blocks.iter().enumerate()
                                    {
                                        let result_type = result_block
                                            .get("type")
                                            .and_then(Value::as_str)
                                            .unwrap_or("");
                                        if result_type != "text" {
                                            issues.push(format!(
                                                "{path}.content[{result_index}] type '{result_type}' would be dropped during translation"
                                            ));
                                        }
                                    }
                                }
                            }
                            _ => issues.push(format!(
                                "{path} type '{kind}' has no canonical cross-format representation"
                            )),
                        }
                    }
                }
                Some(Value::String(_)) | None => {}
                Some(_) => issues.push(format!(
                    "messages[{message_index}].content has unsupported nested structure"
                )),
            }
        }
    }

    if let Some(output_config) = obj.get("output_config") {
        match output_config.as_object() {
            Some(config) => {
                let adaptive = obj
                    .get("thinking")
                    .and_then(|thinking| thinking.get("type"))
                    .and_then(Value::as_str)
                    == Some("adaptive");
                for key in config.keys() {
                    if key != "effort" || !adaptive {
                        issues.push(format!(
                            "output_config.{key} has no canonical cross-format representation"
                        ));
                    }
                }
            }
            None => issues.push(
                "output_config has unsupported nested structure for cross-format translation"
                    .to_string(),
            ),
        }
    }

    if let Some(tools) = obj.get("tools").and_then(Value::as_array) {
        for (tool_index, tool) in tools.iter().enumerate() {
            if let Some(kind) = tool.get("type").and_then(Value::as_str) {
                if kind != "custom" {
                    issues.push(format!(
                        "tools[{tool_index}] server-tool type '{kind}' has no canonical cross-format representation"
                    ));
                }
            }
        }
    }

    issues
}

fn decode_content(content: Option<&Value>) -> Vec<Part> {
    match content {
        Some(Value::String(s)) => vec![Part::Text(s.clone())],
        Some(Value::Array(blocks)) => {
            let mut out = Vec::new();
            for b in blocks {
                match b.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                    "text" => {
                        if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                            out.push(Part::Text(t.to_string()));
                        }
                    }
                    "image" => {
                        let source = b.get("source");
                        let stype = source
                            .and_then(|s| s.get("type"))
                            .and_then(|t| t.as_str())
                            .unwrap_or("");
                        match stype {
                            "base64" => {
                                let mime = source
                                    .and_then(|s| s.get("media_type"))
                                    .and_then(|m| m.as_str())
                                    .unwrap_or("image/png")
                                    .to_string();
                                let data = source
                                    .and_then(|s| s.get("data"))
                                    .and_then(|d| d.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                out.push(Part::Image(ImageData::Base64 { mime, data }));
                            }
                            "url" => {
                                if let Some(url) =
                                    source.and_then(|s| s.get("url")).and_then(|u| u.as_str())
                                {
                                    out.push(Part::Image(ImageData::Url(url.to_string())));
                                }
                            }
                            _ => {}
                        }
                    }
                    "tool_use" => {
                        let id = b.get("id").and_then(|i| i.as_str()).map(String::from);
                        let name = b
                            .get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or("")
                            .to_string();
                        let args = b
                            .get("input")
                            .map(|i| i.to_string())
                            .unwrap_or_else(|| "{}".to_string());
                        if !name.is_empty() {
                            out.push(Part::ToolCall {
                                id,
                                name,
                                arguments: args,
                                signature: None,
                            });
                        }
                    }
                    "tool_result" => {
                        let tool_call_id = b
                            .get("tool_use_id")
                            .and_then(|i| i.as_str())
                            .unwrap_or("")
                            .to_string();
                        let content = match b.get("content") {
                            Some(Value::String(s)) => s.clone(),
                            Some(Value::Array(arr)) => arr
                                .iter()
                                .filter_map(|x| x.get("text").and_then(|t| t.as_str()))
                                .collect::<Vec<_>>()
                                .join(""),
                            _ => String::new(),
                        };
                        let is_error = b.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false);
                        out.push(Part::ToolResult {
                            tool_call_id,
                            name: None,
                            content,
                            is_error,
                        });
                    }
                    "thinking" => {
                        let text = b
                            .get("thinking")
                            .and_then(|t| t.as_str())
                            .unwrap_or("")
                            .to_string();
                        let signature = b
                            .get("signature")
                            .and_then(|s| s.as_str())
                            .map(String::from);
                        out.push(Part::Thinking { text, signature });
                    }
                    "redacted_thinking" => {
                        let data = b
                            .get("data")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        out.push(Part::RedactedThinking { data });
                    }
                    _ => {}
                }
            }
            out
        }
        _ => vec![],
    }
}

fn decode_tools(tools: Option<&Value>) -> Vec<ToolDef> {
    let mut out = Vec::new();
    let Some(arr) = tools.and_then(|t| t.as_array()) else {
        return out;
    };
    for t in arr {
        let Some(name) = t.get("name").and_then(|n| n.as_str()) else {
            continue;
        };
        out.push(ToolDef {
            name: name.to_string(),
            description: t
                .get("description")
                .and_then(|d| d.as_str())
                .map(String::from),
            parameters: t
                .get("input_schema")
                .cloned()
                .unwrap_or(json!({ "type": "object", "properties": {} })),
            defer_loading: t.get("defer_loading").and_then(Value::as_bool),
        });
    }
    out
}

fn decode_tool_choice(tc: Option<&Value>) -> (Option<ToolChoice>, Option<String>) {
    match tc {
        None => (Some(ToolChoice::Auto), None),
        Some(Value::Object(o)) => match o.get("type").and_then(|t| t.as_str()) {
            Some("auto") => (Some(ToolChoice::Auto), None),
            Some("any") => (Some(ToolChoice::Required), None),
            Some("none") => (Some(ToolChoice::None), None),
            Some("tool") => (
                Some(ToolChoice::Specific),
                o.get("name").and_then(|n| n.as_str()).map(String::from),
            ),
            _ => (Some(ToolChoice::Auto), None),
        },
        _ => (Some(ToolChoice::Auto), None),
    }
}

// ---------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------

enum BlockState {
    Text,
    Thinking,
}

pub struct AnthropicEncoder {
    ctx: EncoderCtx,
    started: bool,
    next_index: u32,
    open_block: Option<(u32, BlockState)>,
    thinking_source_index: Option<u32>,
    /// internal tool index -> (anthropic block index, tool id, name)
    tool_blocks: std::collections::HashMap<u32, (u32, String, String)>,
    usage: crate::types::TokenUsage,
    finish: Option<FinishReason>,
    stop_reason_sent: bool,
}

impl AnthropicEncoder {
    pub fn new(ctx: EncoderCtx) -> Self {
        AnthropicEncoder {
            ctx,
            started: false,
            next_index: 0,
            open_block: None,
            thinking_source_index: None,
            tool_blocks: Default::default(),
            usage: Default::default(),
            finish: None,
            stop_reason_sent: false,
        }
    }

    fn message_id(&self) -> String {
        format!("msg_{}", self.ctx.request_id.replace(['-', '_'], ""))
    }

    fn emit_start(&mut self, out: &mut Vec<Bytes>) {
        if self.started {
            return;
        }
        self.started = true;
        let frame = json!({
            "type": "message_start",
            "message": {
                "id": self.message_id(),
                "type": "message",
                "role": "assistant",
                "model": self.ctx.model_name,
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": { "input_tokens": 0, "output_tokens": 0 }
            }
        });
        out.push(sse_frame(Some("message_start"), &frame.to_string()));
    }

    fn close_open_block(&mut self, out: &mut Vec<Bytes>) {
        self.thinking_source_index = None;
        if let Some((idx, _)) = self.open_block.take() {
            let frame = json!({ "type": "content_block_stop", "index": idx });
            out.push(sse_frame(Some("content_block_stop"), &frame.to_string()));
        }
    }

    fn open_text_block(&mut self, out: &mut Vec<Bytes>) {
        if matches!(self.open_block, Some((_, BlockState::Text))) {
            return;
        }
        self.close_open_block(out);
        let idx = self.next_index;
        self.next_index += 1;
        let frame = json!({
            "type": "content_block_start",
            "index": idx,
            "content_block": { "type": "text", "text": "" }
        });
        out.push(sse_frame(Some("content_block_start"), &frame.to_string()));
        self.open_block = Some((idx, BlockState::Text));
    }

    fn open_thinking_block(&mut self, out: &mut Vec<Bytes>) {
        if matches!(self.open_block, Some((_, BlockState::Thinking))) {
            return;
        }
        self.close_open_block(out);
        let idx = self.next_index;
        self.next_index += 1;
        let frame = json!({
            "type": "content_block_start",
            "index": idx,
            "content_block": { "type": "thinking", "thinking": "", "signature": "" }
        });
        out.push(sse_frame(Some("content_block_start"), &frame.to_string()));
        self.open_block = Some((idx, BlockState::Thinking));
    }

    fn emit_thinking_delta(&self, out: &mut Vec<Bytes>, text: String, signature: Option<String>) {
        let idx = self.open_block.as_ref().map(|(i, _)| *i).unwrap_or(0);
        if !text.is_empty() {
            let frame = json!({
                "type": "content_block_delta",
                "index": idx,
                "delta": { "type": "thinking_delta", "thinking": text }
            });
            out.push(sse_frame(Some("content_block_delta"), &frame.to_string()));
        }
        if let Some(signature) = signature.filter(|signature| !signature.is_empty()) {
            let frame = json!({
                "type": "content_block_delta",
                "index": idx,
                "delta": { "type": "signature_delta", "signature": signature }
            });
            out.push(sse_frame(Some("content_block_delta"), &frame.to_string()));
        }
    }

    fn open_tool_block(&mut self, out: &mut Vec<Bytes>, index: u32, id: &str, name: &str) {
        // Tool calls use their own blocks; close any open text/thinking block.
        self.close_open_block(out);
        let block_idx = self.next_index;
        self.next_index += 1;
        let frame = json!({
            "type": "content_block_start",
            "index": block_idx,
            "content_block": { "type": "tool_use", "id": id, "name": name, "input": {} }
        });
        out.push(sse_frame(Some("content_block_start"), &frame.to_string()));
        self.tool_blocks
            .insert(index, (block_idx, id.to_string(), name.to_string()));
        self.open_block = Some((block_idx, BlockState::Text));
    }

    pub fn encode(&mut self, event: StreamEvent) -> Vec<Bytes> {
        let mut out = Vec::new();
        match event {
            StreamEvent::Start { .. } => {
                self.emit_start(&mut out);
            }
            StreamEvent::TextDelta(t) | StreamEvent::RefusalDelta(t) => {
                self.emit_start(&mut out);
                self.open_text_block(&mut out);
                let idx = self.open_block.as_ref().map(|(i, _)| *i).unwrap_or(0);
                let frame = json!({
                    "type": "content_block_delta",
                    "index": idx,
                    "delta": { "type": "text_delta", "text": t }
                });
                out.push(sse_frame(Some("content_block_delta"), &frame.to_string()));
            }
            StreamEvent::ThinkingBlockStart {
                index,
                thinking,
                signature,
            } => {
                self.emit_start(&mut out);
                if self.thinking_source_index != Some(index) {
                    self.close_open_block(&mut out);
                    self.open_thinking_block(&mut out);
                    self.thinking_source_index = Some(index);
                }
                self.emit_thinking_delta(&mut out, thinking, signature);
            }
            StreamEvent::ThinkingDelta {
                text, signature, ..
            } => {
                self.emit_start(&mut out);
                self.open_thinking_block(&mut out);
                self.emit_thinking_delta(&mut out, text, signature);
            }
            StreamEvent::ThinkingBlockStop { index } => {
                if self.thinking_source_index == Some(index) {
                    self.close_open_block(&mut out);
                }
            }
            StreamEvent::RedactedThinking { data, .. } => {
                self.emit_start(&mut out);
                self.close_open_block(&mut out);
                let idx = self.next_index;
                self.next_index += 1;
                let start = json!({
                    "type": "content_block_start",
                    "index": idx,
                    "content_block": { "type": "redacted_thinking", "data": data }
                });
                out.push(sse_frame(Some("content_block_start"), &start.to_string()));
                let stop = json!({ "type": "content_block_stop", "index": idx });
                out.push(sse_frame(Some("content_block_stop"), &stop.to_string()));
            }
            StreamEvent::ToolCallStart {
                index, id, name, ..
            } => {
                self.emit_start(&mut out);
                let id = id.unwrap_or_else(|| format!("toolu_{}_{}", self.ctx.request_id, index));
                self.open_tool_block(&mut out, index, &id, &name);
            }
            StreamEvent::ToolCallArgsDelta { index, args } => {
                let block_idx = self
                    .tool_blocks
                    .get(&index)
                    .map(|(i, _, _)| *i)
                    .or(self.open_block.as_ref().map(|(i, _)| *i))
                    .unwrap_or(0);
                let frame = json!({
                    "type": "content_block_delta",
                    "index": block_idx,
                    "delta": { "type": "input_json_delta", "partial_json": args }
                });
                out.push(sse_frame(Some("content_block_delta"), &frame.to_string()));
            }
            StreamEvent::Usage(u) => {
                self.usage.merge(&u);
            }
            StreamEvent::Finish(f) => {
                self.finish = Some(f);
            }
        }
        out
    }

    pub fn finalize(&mut self) -> Vec<Bytes> {
        let mut out = Vec::new();
        self.emit_start(&mut out);
        self.close_open_block(&mut out);
        if !self.stop_reason_sent {
            self.stop_reason_sent = true;
            let stop_reason = match &self.finish {
                Some(FinishReason::Length) => "max_tokens",
                Some(FinishReason::ToolCalls) => "tool_use",
                Some(FinishReason::Stop) | None => "end_turn",
                Some(FinishReason::ContentFilter) => "end_turn",
                Some(FinishReason::Other(_)) => "end_turn",
            };
            let delta = json!({
                "type": "message_delta",
                "delta": { "stop_reason": stop_reason, "stop_sequence": null },
                "usage": { "output_tokens": self.usage.output.unwrap_or(0) }
            });
            out.push(sse_frame(Some("message_delta"), &delta.to_string()));
        }
        out.push(sse_frame(
            Some("message_stop"),
            &json!({ "type": "message_stop" }).to_string(),
        ));
        out
    }

    pub fn error_frame(&mut self, message: &str) -> Vec<Bytes> {
        let frame = json!({
            "type": "error",
            "error": { "type": "api_error", "message": message }
        });
        vec![sse_frame(Some("error"), &frame.to_string())]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streams_canonical_refusals_as_assistant_text() {
        let mut encoder = AnthropicEncoder::new(EncoderCtx {
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
        assert!(wire.contains("text_delta"));
    }
}
