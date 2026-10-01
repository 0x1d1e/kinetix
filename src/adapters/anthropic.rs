//! Anthropic Messages outbound adapter (wire format `anthropic`).
//!
//! Used for Anthropic upstreams and for cross-provider routes (FR-12.7).

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::adapters::{Adapter, DiscoveredModel, UpstreamContext};
use crate::types::{
    FailureKind, FinishReason, ImageData, InternalRequest, Part, ProxyError, Role, StreamEvent,
    TokenUsage, ToolChoice, UpstreamFailure,
};

pub struct AnthropicAdapter;

fn is_claude_code_oauth(ctx: &UpstreamContext<'_>) -> bool {
    ctx.credential.starts_with("sk-ant-oat")
        || ctx.provider.credential_plugin.contains("claude-code")
}

fn mark_last_cacheable_tool(tools: &mut Value) {
    let Some(tools) = tools.as_array_mut() else {
        return;
    };
    if let Some(tool) = tools
        .iter_mut()
        .rev()
        .find(|tool| tool.get("defer_loading").and_then(Value::as_bool) != Some(true))
    {
        if let Some(tool) = tool.as_object_mut() {
            tool.insert(
                "cache_control".to_string(),
                json!({ "type": "ephemeral", "ttl": "1h" }),
            );
        }
    }
}

fn insert_dotted(obj: &mut serde_json::Map<String, Value>, path: &str, value: Value) {
    let parts: Vec<&str> = path.split('.').collect();
    insert_dotted_rec(obj, &parts, value);
}

fn insert_dotted_rec(obj: &mut serde_json::Map<String, Value>, path: &[&str], value: Value) {
    if path.is_empty() {
        return;
    }
    if path.len() == 1 {
        obj.insert(path[0].to_string(), value);
        return;
    }
    let entry = obj.entry(path[0].to_string()).or_insert_with(|| json!({}));
    if !entry.is_object() {
        *entry = json!({});
    }
    if let Some(map) = entry.as_object_mut() {
        insert_dotted_rec(map, &path[1..], value);
    }
}

impl AnthropicAdapter {
    pub fn new() -> Self {
        AnthropicAdapter
    }

    fn content_blocks(parts: &[Part]) -> Vec<Value> {
        let mut out = Vec::new();
        for p in parts {
            match p {
                Part::Text(t) => out.push(json!({ "type": "text", "text": t })),
                Part::Thinking { text, signature } => {
                    if let Some(signature) = signature {
                        out.push(json!({
                            "type": "thinking",
                            "thinking": text,
                            "signature": signature
                        }));
                    }
                }
                Part::RedactedThinking { data } => out.push(json!({
                    "type": "redacted_thinking",
                    "data": data
                })),
                Part::Image(ImageData::Base64 { mime, data }) => out.push(json!({
                    "type": "image",
                    "source": { "type": "base64", "media_type": mime, "data": data }
                })),
                Part::Image(ImageData::Url(url)) => out.push(json!({
                    "type": "image",
                    "source": { "type": "url", "url": url }
                })),
                Part::ToolCall {
                    id,
                    name,
                    arguments,
                    ..
                } => {
                    let input: Value = serde_json::from_str(arguments).unwrap_or(json!({}));
                    out.push(json!({
                        "type": "tool_use",
                        "id": id.clone().unwrap_or_else(|| format!("toolu_{name}")),
                        "name": name,
                        "input": input
                    }));
                }
                Part::ToolResult {
                    tool_call_id,
                    content,
                    is_error,
                    ..
                } => {
                    out.push(json!({
                        "type": "tool_result",
                        "tool_use_id": tool_call_id,
                        "content": content,
                        "is_error": is_error
                    }));
                }
            }
        }
        out
    }

    fn build_messages(req: &InternalRequest) -> Vec<Value> {
        let mut msgs = Vec::new();
        for m in &req.messages {
            let role = match m.role {
                Role::Assistant => "assistant",
                Role::System => continue,
                _ => "user",
            };
            let blocks = Self::content_blocks(&m.parts);
            if blocks.is_empty() {
                continue;
            }
            msgs.push(json!({ "role": role, "content": blocks }));
        }
        msgs
    }

    fn build_tools(req: &InternalRequest) -> Option<Value> {
        if req.tools.is_empty() {
            return None;
        }
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                let mut tool = json!({
                    "name": t.name,
                    "description": t.description.clone().unwrap_or_default(),
                    "input_schema": if t.parameters.is_null() { json!({"type":"object","properties":{}}) } else { t.parameters.clone() }
                });
                if let Some(defer_loading) = t.defer_loading {
                    tool["defer_loading"] = json!(defer_loading);
                }
                tool
            })
            .collect();
        Some(Value::Array(tools))
    }

    fn apply_thinking_map(
        body: &mut serde_json::Map<String, Value>,
        ctx: &UpstreamContext<'_>,
        req: &InternalRequest,
    ) {
        let Some(level) = req.thinking else {
            return;
        };
        let tmap = ctx.model.thinking();
        let key = level.as_key();
        if tmap.is_adaptive() {
            if matches!(level, crate::types::ThinkingLevel::Off) {
                if let Some(v) = tmap.levels.get(key) {
                    if let Some(fields) = v.as_object() {
                        for (field, value) in fields {
                            insert_dotted(body, field, value.clone());
                        }
                    } else if let Some(field) = tmap.scalar_field() {
                        insert_dotted(body, field, v.clone());
                    }
                }
                if let Some(thinking) = body.get_mut("thinking").and_then(Value::as_object_mut) {
                    thinking.remove("budget_tokens");
                }
                return;
            }

            if !matches!(level, crate::types::ThinkingLevel::Default) {
                if let Some(v) = tmap.levels.get(key) {
                    if let Some(fields) = v.as_object() {
                        for (field, value) in fields {
                            insert_dotted(body, field, value.clone());
                        }
                    } else if let Some(field) = tmap.scalar_field() {
                        insert_dotted(body, field, v.clone());
                    }
                }
            }
            insert_dotted(body, "thinking.type", json!("adaptive"));
            if let Some(thinking) = body.get_mut("thinking").and_then(Value::as_object_mut) {
                thinking.remove("budget_tokens");
            }
            return;
        }

        let Some(v) = tmap.levels.get(key) else {
            return;
        };
        if v.is_object() {
            body.insert("thinking".to_string(), v.clone());
        } else if let Some(field) = tmap.scalar_field() {
            insert_dotted(body, field, v.clone());
            if field == "thinking.budget_tokens" {
                insert_dotted(body, "thinking.type", json!("enabled"));
            }
        }
    }

    fn build_tool_choice(req: &InternalRequest) -> Option<Value> {
        match req.tool_choice {
            Some(ToolChoice::None) => Some(json!({ "type": "none" })),
            Some(ToolChoice::Required) => Some(json!({ "type": "any" })),
            Some(ToolChoice::Specific) => Some(json!({
                "type": "tool",
                "name": req.tool_choice_name.clone().unwrap_or_default()
            })),
            Some(ToolChoice::Auto) | None => None,
        }
    }
}

impl Default for AnthropicAdapter {
    fn default() -> Self {
        Self::new()
    }
}

fn anthropic_retry_delay(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    if let Some(ms) = headers
        .get("retry-after-ms")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value >= 0.0)
    {
        return Some(((ms / 1000.0).ceil() as u64).max(1));
    }

    if let Some(value) = headers
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
    {
        if let Ok(seconds) = value.parse::<f64>() {
            if seconds.is_finite() && seconds >= 0.0 {
                return Some((seconds.ceil() as u64).max(1));
            }
        }
        if let Ok(when) = chrono::DateTime::parse_from_rfc2822(value) {
            let when = when.with_timezone(&chrono::Utc);
            let now = chrono::Utc::now();
            if when > now {
                return Some((when - now).num_seconds().max(1) as u64);
            }
        }
    }

    anthropic_reset_delay(headers)
}

/// Anthropic may omit Retry-After on 429s but provides RFC3339 reset
/// timestamps. Use the earliest future reset as the client/cooldown hint.
fn anthropic_reset_delay(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    const RESET_HEADERS: [&str; 5] = [
        "anthropic-ratelimit-unified-reset",
        "anthropic-ratelimit-requests-reset",
        "anthropic-ratelimit-tokens-reset",
        "anthropic-ratelimit-input-tokens-reset",
        "anthropic-ratelimit-output-tokens-reset",
    ];

    let now = chrono::Utc::now();
    RESET_HEADERS
        .iter()
        .filter_map(|name| headers.get(*name))
        .filter_map(|v| v.to_str().ok())
        .filter_map(|v| chrono::DateTime::parse_from_rfc3339(v.trim()).ok())
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .filter(|dt| *dt > now)
        .map(|dt| (dt - now).num_seconds().max(1) as u64)
        .min()
}

// Anthropic documents context-window exhaustion as a truncated response; the
// Responses API represents output/context-window token exhaustion with
// max_output_tokens:
// https://platform.claude.com/docs/en/build-with-claude/handling-stop-reasons
// https://developers.openai.com/api/docs/guides/reasoning
fn map_anthropic_stop_reason(reason: &str) -> FinishReason {
    match reason {
        "end_turn" | "stop_sequence" => FinishReason::Stop,
        "max_tokens" | "model_context_window_exceeded" => FinishReason::Length,
        "tool_use" => FinishReason::ToolCalls,
        other => FinishReason::Other(other.to_string()),
    }
}

fn parse_anthropic_usage(usage: &Value) -> TokenUsage {
    let ordinary = usage.get("input_tokens").and_then(Value::as_u64);
    let cached = usage.get("cache_read_input_tokens").and_then(Value::as_u64);
    let cache_write = usage
        .get("cache_creation_input_tokens")
        .and_then(Value::as_u64);
    let input = if ordinary.is_some() || cached.is_some() || cache_write.is_some() {
        Some(
            ordinary
                .unwrap_or(0)
                .saturating_add(cached.unwrap_or(0))
                .saturating_add(cache_write.unwrap_or(0)),
        )
    } else {
        None
    };

    TokenUsage {
        input,
        output: usage.get("output_tokens").and_then(Value::as_u64),
        cached,
        cache_write,
        thinking: None,
    }
}

#[async_trait]
impl Adapter for AnthropicAdapter {
    fn wire_format(&self) -> &'static str {
        "anthropic"
    }

    fn build_url(&self, ctx: &UpstreamContext<'_>) -> Result<String, ProxyError> {
        let resolved = ctx.provider.resolved_base_url()?;
        let base = resolved.trim_end_matches('/');
        Ok(format!("{base}/messages"))
    }

    fn supports_count_tokens(&self) -> bool {
        true
    }

    fn count_tokens_url(&self, ctx: &UpstreamContext<'_>) -> Result<Option<String>, ProxyError> {
        let resolved = ctx.provider.resolved_base_url()?;
        let base = resolved.trim_end_matches('/');
        Ok(Some(format!("{base}/messages/count_tokens")))
    }

    fn apply_auth(
        &self,
        ctx: &UpstreamContext<'_>,
        mut req: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder, UpstreamFailure> {
        use crate::types::AuthScheme;
        if is_claude_code_oauth(ctx) {
            req = req.header("user-agent", "claude-cli/1.18.31 (external, cli)");
        }
        Ok(match ctx.provider.auth() {
            AuthScheme::None => req,
            AuthScheme::Bearer => req.bearer_auth(&ctx.credential),
            AuthScheme::CustomHeader => {
                let name = ctx
                    .provider
                    .custom_header_name
                    .clone()
                    .unwrap_or_else(|| "x-api-key".to_string());
                req.header(name, &ctx.credential)
            }
            AuthScheme::QueryParam => {
                let param = ctx
                    .provider
                    .custom_param_name
                    .clone()
                    .unwrap_or_else(|| "key".to_string());
                req.query(&[(param, &ctx.credential)])
            }
        })
    }

    fn build_body(
        &self,
        ctx: &UpstreamContext<'_>,
        req: &InternalRequest,
    ) -> Result<Value, UpstreamFailure> {
        let mut body = serde_json::Map::new();
        body.insert("model".to_string(), json!(ctx.model.upstream_id));
        body.insert("stream".to_string(), json!(true));

        let mut system = req.system.clone();
        for m in &req.messages {
            if m.role == Role::System {
                for p in &m.parts {
                    if let Part::Text(t) = p {
                        if !t.trim().is_empty() {
                            system.push(t.clone());
                        }
                    }
                }
            }
        }

        if is_claude_code_oauth(ctx) {
            const BILLING_HEADER: &str =
                "x-anthropic-billing-header: cc_version=1.18.31; cc_entrypoint=cli; cch=00000;";
            const SENTINEL: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
            if !system
                .iter()
                .any(|s| s.contains("x-anthropic-billing-header:"))
            {
                system.insert(0, BILLING_HEADER.to_string());
            }
            if !system.iter().any(|s| s.contains("You are Claude Code")) {
                let idx = if system.is_empty() { 0 } else { 1 };
                system.insert(idx, SENTINEL.to_string());
            }
        }

        if !system.is_empty() {
            if is_claude_code_oauth(ctx) {
                let last = system.len().saturating_sub(1);
                let blocks = system
                    .into_iter()
                    .enumerate()
                    .map(|(index, text)| {
                        if index == last {
                            json!({
                                "type": "text",
                                "text": text,
                                "cache_control": { "type": "ephemeral", "ttl": "1h" }
                            })
                        } else {
                            json!({ "type": "text", "text": text })
                        }
                    })
                    .collect::<Vec<_>>();
                body.insert("system".to_string(), Value::Array(blocks));
            } else {
                body.insert("system".to_string(), json!(system.join("\n\n")));
            }
        }
        body.insert("messages".to_string(), json!(Self::build_messages(req)));

        // max_tokens is required by Anthropic.
        let max = req
            .params
            .max_tokens
            .map(|m| m as i64)
            .or(ctx.model.max_output_tokens)
            .unwrap_or(4096);
        body.insert("max_tokens".to_string(), json!(max));

        let params = ctx.model.params();
        if let Some(t) = req.params.temperature {
            if params
                .get("temperature")
                .map(|s| s.supported)
                .unwrap_or(true)
            {
                body.insert("temperature".to_string(), json!(t));
            }
        }
        if let Some(p) = req.params.top_p {
            if params.get("top_p").map(|s| s.supported).unwrap_or(true) {
                body.insert("top_p".to_string(), json!(p));
            }
        }
        if let Some(k) = req.params.top_k {
            if params.get("top_k").map(|s| s.supported).unwrap_or(true) {
                body.insert("top_k".to_string(), json!(k));
            }
        }
        if !req.params.stop.is_empty() {
            body.insert("stop_sequences".to_string(), json!(req.params.stop));
        }

        if let Some(mut tools) = Self::build_tools(req) {
            if is_claude_code_oauth(ctx) {
                mark_last_cacheable_tool(&mut tools);
            }
            body.insert("tools".to_string(), tools);
        }
        if let Some(tc) = Self::build_tool_choice(req) {
            body.insert("tool_choice".to_string(), tc);
        }

        // Thinking map.
        Self::apply_thinking_map(&mut body, ctx, req);

        let extra = ctx.model.extra_request_value();
        if let Some(obj) = extra.as_object() {
            for (k, v) in obj {
                body.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }

        Ok(Value::Object(body))
    }

    fn normalize_passthrough_body(
        &self,
        ctx: &UpstreamContext<'_>,
        req: &InternalRequest,
        body: &mut Value,
    ) -> Result<(), UpstreamFailure> {
        let Some(body) = body.as_object_mut() else {
            return Ok(());
        };
        if let Some(max_tokens) = req.params.max_tokens {
            for alias in ["max_tokens", "max_completion_tokens", "max_output_tokens"] {
                body.remove(alias);
            }
            body.insert("max_tokens".into(), json!(max_tokens));
        }

        let tmap = ctx.model.thinking();
        if !tmap.is_adaptive() {
            return Ok(());
        }
        if matches!(req.thinking, Some(crate::types::ThinkingLevel::Off))
            && !tmap.anthropic_adaptive_off_is_executable()
        {
            return Err(UpstreamFailure {
                kind: FailureKind::BadRequest,
                status: None,
                retry_after_secs: None,
                message: format!(
                    "thinking level 'off' has no executable mapping for model '{}'",
                    ctx.model.display_name
                ),
                quota_reset_at: None,
            });
        }
        Self::apply_thinking_map(body, ctx, req);
        Ok(())
    }

    fn classify_error(
        &self,
        status: u16,
        body: &str,
        headers: &reqwest::header::HeaderMap,
    ) -> UpstreamFailure {
        let parsed: Value = serde_json::from_str(body).unwrap_or(Value::Null);
        let message = parsed
            .pointer("/error/message")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let etype = parsed
            .pointer("/error/type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_ascii_lowercase();

        let retry_after_secs = anthropic_retry_delay(headers);

        let lower = message.to_ascii_lowercase();
        let credential_error = status == 401
            || etype.contains("authentication")
            || lower.contains("invalid api key")
            || lower.contains("invalid x-api-key")
            || lower.contains("authentication token");

        let kind = match status {
            400 | 422 => FailureKind::BadRequest,
            401 => FailureKind::AuthError,
            403 if credential_error => FailureKind::AuthError,
            403 | 404 => FailureKind::TargetError,
            429 => {
                // Anthropic uses `rate_limit_error` for throttling; a quota /
                // credit exhaustion surfaces in the message. Distinguish them so
                // a quota problem benches the account until reset while a plain
                // rate limit only cools it down briefly (FR-12.7).
                if lower.contains("quota") || lower.contains("credit") {
                    FailureKind::QuotaExhausted
                } else {
                    FailureKind::RateLimit
                }
            }
            529 => FailureKind::ServerError,
            s if s >= 500 => FailureKind::ServerError,
            _ => FailureKind::ServerError,
        };

        UpstreamFailure {
            kind,
            status: Some(status),
            retry_after_secs,
            message: if message.is_empty() {
                format!("upstream returned HTTP {status}")
            } else {
                crate::crypto::redact(&message)
            },
            quota_reset_at: None,
        }
    }

    fn parse_stream_chunk(&self, data: &str) -> Result<Vec<StreamEvent>, UpstreamFailure> {
        let v: Value = serde_json::from_str(data).map_err(|e| UpstreamFailure {
            kind: FailureKind::MalformedUpstream,
            status: None,
            retry_after_secs: None,
            message: format!("invalid upstream chunk: {e}"),
            quota_reset_at: None,
        })?;
        let event_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let mut events = Vec::new();
        match event_type {
            "content_block_delta" => {
                let index = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as u32;
                let delta = v.get("delta").cloned().unwrap_or(Value::Null);
                match delta.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                    "text_delta" => {
                        if let Some(t) = delta.get("text").and_then(|x| x.as_str()) {
                            events.push(StreamEvent::TextDelta(t.to_string()));
                        }
                    }
                    "thinking_delta" => {
                        if let Some(t) = delta.get("thinking").and_then(|x| x.as_str()) {
                            events.push(StreamEvent::ThinkingDelta {
                                block_index: Some(index),
                                text: t.to_string(),
                                signature: None,
                            });
                        }
                    }
                    "signature_delta" => {
                        if let Some(s) = delta.get("signature").and_then(|x| x.as_str()) {
                            events.push(StreamEvent::ThinkingDelta {
                                block_index: Some(index),
                                text: String::new(),
                                signature: Some(s.to_string()),
                            });
                        }
                    }
                    "input_json_delta" => {
                        let index = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as u32;
                        if let Some(partial) = delta.get("partial_json").and_then(|x| x.as_str()) {
                            events.push(StreamEvent::ToolCallArgsDelta {
                                index,
                                args: partial.to_string(),
                            });
                        }
                    }
                    _ => {}
                }
            }
            "content_block_start" => {
                let index = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as u32;
                if let Some(block) = v.get("content_block") {
                    match block.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                        "tool_use" => {
                            let id = block.get("id").and_then(|i| i.as_str()).map(String::from);
                            let name = block
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("tool")
                                .to_string();
                            events.push(StreamEvent::ToolCallStart {
                                index,
                                id,
                                name,
                                signature: None,
                            });
                        }
                        "thinking" => events.push(StreamEvent::ThinkingBlockStart {
                            index,
                            thinking: block
                                .get("thinking")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            signature: block
                                .get("signature")
                                .and_then(Value::as_str)
                                .map(str::to_string),
                        }),
                        "redacted_thinking" => {
                            let data =
                                block.get("data").and_then(Value::as_str).ok_or_else(|| {
                                    UpstreamFailure {
                                        kind: FailureKind::ServerError,
                                        status: Some(502),
                                        retry_after_secs: None,
                                        message: "redacted_thinking block is missing string data"
                                            .into(),
                                        quota_reset_at: None,
                                    }
                                })?;
                            events.push(StreamEvent::RedactedThinking {
                                index,
                                data: data.to_string(),
                            });
                        }
                        _ => {}
                    }
                }
            }
            "content_block_stop" => {
                let index = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as u32;
                events.push(StreamEvent::ThinkingBlockStop { index });
            }
            "message_delta" => {
                if let Some(usage) = v.get("usage") {
                    events.push(StreamEvent::Usage(parse_anthropic_usage(usage)));
                }
                if let Some(reason) = v.pointer("/delta/stop_reason").and_then(|r| r.as_str()) {
                    events.push(StreamEvent::Finish(map_anthropic_stop_reason(reason)));
                }
            }
            "message_start" => {
                if let Some(usage) = v.pointer("/message/usage") {
                    events.push(StreamEvent::Usage(parse_anthropic_usage(usage)));
                }
            }
            _ => {}
        }
        Ok(events)
    }

    fn parse_full_response(&self, body: &Value) -> Result<Vec<StreamEvent>, UpstreamFailure> {
        let mut events = Vec::new();
        if let Some(blocks) = body.get("content").and_then(|c| c.as_array()) {
            for (idx, b) in blocks.iter().enumerate() {
                let idx = idx as u32;
                match b.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                    "text" => {
                        if let Some(t) = b.get("text").and_then(|x| x.as_str()) {
                            events.push(StreamEvent::TextDelta(t.to_string()));
                        }
                    }
                    "thinking" => {
                        events.push(StreamEvent::ThinkingBlockStart {
                            index: idx,
                            thinking: b
                                .get("thinking")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            signature: b
                                .get("signature")
                                .and_then(Value::as_str)
                                .map(str::to_string),
                        });
                        events.push(StreamEvent::ThinkingBlockStop { index: idx });
                    }
                    "redacted_thinking" => {
                        let data = b.get("data").and_then(Value::as_str).ok_or_else(|| {
                            UpstreamFailure {
                                kind: FailureKind::ServerError,
                                status: Some(502),
                                retry_after_secs: None,
                                message: "redacted_thinking block is missing string data".into(),
                                quota_reset_at: None,
                            }
                        })?;
                        events.push(StreamEvent::RedactedThinking {
                            index: idx,
                            data: data.to_string(),
                        });
                    }
                    "tool_use" => {
                        let id = b.get("id").and_then(|i| i.as_str()).map(String::from);
                        let name = b
                            .get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or("tool")
                            .to_string();
                        events.push(StreamEvent::ToolCallStart {
                            index: idx,
                            id,
                            name,
                            signature: None,
                        });
                        let input = b.get("input").cloned().unwrap_or(json!({}));
                        events.push(StreamEvent::ToolCallArgsDelta {
                            index: idx,
                            args: input.to_string(),
                        });
                    }
                    _ => {}
                }
            }
        }
        if let Some(usage) = body.get("usage") {
            events.push(StreamEvent::Usage(parse_anthropic_usage(usage)));
        }
        if let Some(reason) = body.get("stop_reason").and_then(|r| r.as_str()) {
            events.push(StreamEvent::Finish(map_anthropic_stop_reason(reason)));
        }
        Ok(events)
    }

    fn default_models_path(&self) -> &'static str {
        "/models"
    }

    fn parse_model_list(&self, body: &Value) -> Vec<DiscoveredModel> {
        let mut out = Vec::new();
        if let Some(data) = body.get("data").and_then(|d| d.as_array()) {
            for m in data {
                if let Some(id) = m.get("id").and_then(|i| i.as_str()) {
                    out.push(DiscoveredModel {
                        id: id.to_string(),
                        display_name: m
                            .get("display_name")
                            .and_then(|v| v.as_str())
                            .map(String::from),
                        context_window: None,
                        max_output_tokens: None,
                    });
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinking_and_redacted_blocks_retain_stream_boundaries_and_data() {
        let adapter = AnthropicAdapter::new();
        let start = adapter
            .parse_stream_chunk(
                r#"{"type":"content_block_start","index":4,"content_block":{"type":"thinking","thinking":""}}"#,
            )
            .unwrap();
        assert!(matches!(
            start.as_slice(),
            [StreamEvent::ThinkingBlockStart { index: 4, thinking, signature: None }] if thinking.is_empty()
        ));

        let thinking = adapter
            .parse_stream_chunk(
                r#"{"type":"content_block_delta","index":4,"delta":{"type":"thinking_delta","thinking":"foo"}}"#,
            )
            .unwrap();
        assert!(matches!(
            thinking.as_slice(),
            [StreamEvent::ThinkingDelta { block_index: Some(4), text, signature: None }] if text == "foo"
        ));
        let signature = adapter
            .parse_stream_chunk(
                r#"{"type":"content_block_delta","index":4,"delta":{"type":"signature_delta","signature":"sig"}}"#,
            )
            .unwrap();
        assert!(matches!(
            signature.as_slice(),
            [StreamEvent::ThinkingDelta { block_index: Some(4), text, signature: Some(sig) }]
                if text.is_empty() && sig == "sig"
        ));
        let stop = adapter
            .parse_stream_chunk(r#"{"type":"content_block_stop","index":4}"#)
            .unwrap();
        assert!(matches!(
            stop.as_slice(),
            [StreamEvent::ThinkingBlockStop { index: 4 }]
        ));
        let redacted = adapter
            .parse_stream_chunk(
                r#"{"type":"content_block_start","index":5,"content_block":{"type":"redacted_thinking","data":"opaque"}}"#,
            )
            .unwrap();
        assert!(matches!(
            redacted.as_slice(),
            [StreamEvent::RedactedThinking { index: 5, data }] if data == "opaque"
        ));
    }

    #[test]
    fn full_response_preserves_redacted_thinking_blocks() {
        let events = AnthropicAdapter::new()
            .parse_full_response(&serde_json::json!({
                "content": [
                    {"type": "thinking", "thinking": "reasoning", "signature": "sig"},
                    {"type": "redacted_thinking", "data": "opaque"}
                ],
                "stop_reason": "end_turn"
            }))
            .unwrap();
        assert!(matches!(
            events.as_slice(),
            [
                StreamEvent::ThinkingBlockStart { index: 0, thinking, signature: Some(sig) },
                StreamEvent::ThinkingBlockStop { index: 0 },
                StreamEvent::RedactedThinking { index: 1, data },
                StreamEvent::Finish(FinishReason::Stop),
                ..
            ] if thinking == "reasoning" && sig == "sig" && data == "opaque"
        ));
    }

    #[test]
    fn full_response_thinking_survives_non_streaming_openai_aggregation() {
        let events = AnthropicAdapter::new()
            .parse_full_response(&serde_json::json!({
                "content": [{
                    "type": "thinking",
                    "thinking": "private chain of thought",
                    "signature": "opaque-signature"
                }],
                "stop_reason": "end_turn"
            }))
            .unwrap();
        let response = crate::frontends::aggregate(
            crate::frontends::FrontendFormat::OpenAi,
            "test-model",
            "req-thinking-aggregate",
            events,
            &TokenUsage::default(),
        );

        assert_eq!(
            response["choices"][0]["message"]["reasoning_content"],
            "private chain of thought"
        );
    }

    #[test]
    fn model_context_window_exceeded_maps_to_length_in_full_and_streaming() {
        let adapter = AnthropicAdapter::new();
        for stop_reason in ["max_tokens", "model_context_window_exceeded"] {
            let full = adapter
                .parse_full_response(&serde_json::json!({
                    "content": [{"type": "text", "text": "partial"}],
                    "stop_reason": stop_reason
                }))
                .unwrap();
            assert!(matches!(
                full.last(),
                Some(StreamEvent::Finish(FinishReason::Length))
            ));

            let streaming = adapter
                .parse_stream_chunk(&format!(
                    r#"{{"type":"message_delta","delta":{{"stop_reason":"{stop_reason}"}}}}"#
                ))
                .unwrap();
            assert!(matches!(
                streaming.last(),
                Some(StreamEvent::Finish(FinishReason::Length))
            ));
        }
    }

    #[test]
    fn usage_normalizes_cache_read_and_creation_into_total_input() {
        let usage = serde_json::json!({
            "input_tokens": 100,
            "cache_creation_input_tokens": 30,
            "cache_read_input_tokens": 70,
            "output_tokens": 25
        });
        let normalized = parse_anthropic_usage(&usage);
        assert_eq!(normalized.input, Some(200));
        assert_eq!(normalized.cached, Some(70));
        assert_eq!(normalized.cache_write, Some(30));
        assert_eq!(normalized.output, Some(25));
    }

    #[test]
    fn cache_usage_regression_matches_provider_accounting() {
        let first = parse_anthropic_usage(&serde_json::json!({
            "input_tokens": 500,
            "cache_creation_input_tokens": 8000,
            "cache_read_input_tokens": 0
        }));
        assert_eq!(first.input, Some(8500));
        assert_eq!(first.cached, Some(0));
        assert_eq!(first.cache_write, Some(8000));

        let second = parse_anthropic_usage(&serde_json::json!({
            "input_tokens": 700,
            "cache_creation_input_tokens": 300,
            "cache_read_input_tokens": 8000
        }));
        assert_eq!(second.input, Some(9000));
        assert_eq!(second.cached, Some(8000));
        assert_eq!(second.cache_write, Some(300));
    }

    #[test]
    fn anthropic_retry_after_ms_takes_precedence() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("retry-after-ms", "1500".parse().unwrap());
        headers.insert("retry-after", "10".parse().unwrap());
        assert_eq!(anthropic_retry_delay(&headers), Some(2));
    }

    #[test]
    fn anthropic_retry_after_http_date_is_supported() {
        let mut headers = reqwest::header::HeaderMap::new();
        let when = (chrono::Utc::now() + chrono::Duration::seconds(30)).to_rfc2822();
        headers.insert("retry-after", when.parse().unwrap());
        let delay = anthropic_retry_delay(&headers).unwrap();
        assert!((1..=30).contains(&delay));
    }

    #[test]
    fn anthropic_reset_headers_supply_retry_delay() {
        let mut headers = reqwest::header::HeaderMap::new();
        let reset = (chrono::Utc::now() + chrono::Duration::seconds(60)).to_rfc3339();
        headers.insert("anthropic-ratelimit-requests-reset", reset.parse().unwrap());
        let delay = anthropic_reset_delay(&headers).unwrap();
        assert!((1..=60).contains(&delay));
    }
    use crate::db::{ModelRow, ProviderRow};
    use crate::types::{Message, ToolDef};

    fn provider() -> ProviderRow {
        ProviderRow {
            id: "prov_anthropic".into(),
            name: "Anthropic".into(),
            base_url: "https://api.anthropic.com/v1".into(),
            wire_format: "anthropic".into(),
            auth_scheme: "bearer".into(),
            custom_header_name: None,
            custom_param_name: None,
            extra_headers: "{}".into(),
            timeout_ms: 10000,
            capability_mode: "permissive".into(),
            models_path: None,
            rate_limit_rules: "{}".into(),
            enabled: 1,
            follow_redirects: 0,
            credential_hosts: String::new(),
            allow_insecure_tls: 0,
            created_at: "2026-01-01T00:00:00Z".into(),
            wire_plugin: String::new(),
            credential_plugin: String::new(),
            model_source_plugin: String::new(),
            credential_mode: "manual".into(),
            source_plugin_id: None,
            source_integration_id: None,
            pricing_scope: "direct_api".into(),
            integration_features: None,
            integration_protocols: None,
            connection_parameters: None,
            connection_parameters_attested: None,
        }
    }

    fn model() -> ModelRow {
        ModelRow {
            id: "m_sonnet".into(),
            provider_id: "prov_anthropic".into(),
            upstream_id: "claude-sonnet-5".into(),
            display_name: "Claude Sonnet 5".into(),
            enabled: 1,
            context_window: Some(200000),
            max_output_tokens: Some(8192),
            capabilities: "{}".into(),
            prices: "{}".into(),
            parameters: "{}".into(),
            thinking_map: "{}".into(),
            extra_request: "{}".into(),
            discovery: "{}".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            opaque_state_plugin: String::new(),
        }
    }

    fn base_request() -> InternalRequest {
        InternalRequest {
            requested_model: "claude-sonnet-5".into(),
            system: vec![],
            messages: vec![Message {
                role: Role::User,
                parts: vec![Part::Text("Hello".into())],
            }],
            tools: vec![],
            tool_choice: None,
            tool_choice_name: None,
            params: crate::types::SamplingParams::default(),
            stream: true,
            include_usage: false,
            thinking: None,
            extra: Default::default(),
            raw_body: None,
        }
    }

    #[test]
    fn body_replays_thinking_and_redacted_blocks_unchanged() {
        let p = provider();
        let m = model();
        let ctx = UpstreamContext {
            provider: &p,
            model: &m,
            account_id: None,
            session_context: None,
            credential: "k".into(),
        };
        let req = crate::frontends::anthropic::decode_request(json!({
            "model": "claude-sonnet-5",
            "messages": [{
                "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": "hidden reasoning", "signature": "signed-state"},
                    {"type": "redacted_thinking", "data": "opaque-redacted-state"}
                ]
            }]
        }))
        .unwrap();

        let body = AnthropicAdapter::new().build_body(&ctx, &req).unwrap();
        assert_eq!(
            body["messages"][0]["content"],
            json!([
                {"type":"thinking","thinking":"hidden reasoning","signature":"signed-state"},
                {"type":"redacted_thinking","data":"opaque-redacted-state"}
            ])
        );
    }

    fn rendered_system_text(body: &Value) -> String {
        match body.get("system") {
            Some(Value::String(system)) => system.clone(),
            Some(Value::Array(blocks)) => blocks
                .iter()
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n\n"),
            _ => panic!("system prompt present"),
        }
    }

    #[test]
    fn count_tokens_url_uses_messages_subresource() {
        let p = provider();
        let m = model();
        let ctx = UpstreamContext {
            provider: &p,
            model: &m,
            account_id: None,
            session_context: None,
            credential: "sk-ant-api03-regular-key".into(),
        };
        assert_eq!(
            AnthropicAdapter::new()
                .count_tokens_url(&ctx)
                .unwrap()
                .as_deref(),
            Some("https://api.anthropic.com/v1/messages/count_tokens")
        );
    }

    #[test]
    fn claude_code_translated_request_adds_stable_cache_anchors() {
        let mut p = provider();
        p.credential_plugin = "claude-code-oauth".into();
        let m = model();
        let mut req = base_request();
        req.system = vec!["stable project instructions".into()];
        req.tools = vec![
            ToolDef {
                name: "read".into(),
                description: Some("read a file".into()),
                parameters: json!({"type":"object","properties":{"path":{"type":"string"}}}),
                defer_loading: None,
            },
            ToolDef {
                name: "write".into(),
                description: Some("write a file".into()),
                parameters: json!({"type":"object","properties":{"path":{"type":"string"}}}),
                defer_loading: None,
            },
        ];
        let ctx = UpstreamContext {
            provider: &p,
            model: &m,
            account_id: None,
            session_context: None,
            credential: "oauth-token".into(),
        };

        let body = AnthropicAdapter::new().build_body(&ctx, &req).unwrap();
        let system = body["system"].as_array().expect("structured system");
        assert_eq!(
            system.last().unwrap()["cache_control"],
            json!({"type":"ephemeral","ttl":"1h"})
        );
        let tools = body["tools"].as_array().unwrap();
        assert!(tools[0].get("cache_control").is_none());
        assert_eq!(
            tools[1]["cache_control"],
            json!({"type":"ephemeral","ttl":"1h"})
        );

        let markers = system
            .iter()
            .filter(|block| block.get("cache_control").is_some())
            .count()
            + tools
                .iter()
                .filter(|tool| tool.get("cache_control").is_some())
                .count();
        assert_eq!(markers, 2);
    }

    #[test]
    fn claude_code_cache_prefix_is_stable_across_turns() {
        let mut p = provider();
        p.credential_plugin = "claude-code-oauth".into();
        let m = model();
        let mut first = base_request();
        first.system = vec!["stable project instructions".into()];
        first.tools = vec![ToolDef {
            name: "read".into(),
            description: Some("read a file".into()),
            parameters: json!({"type":"object"}),
            defer_loading: None,
        }];
        let mut second = first.clone();
        second.messages.push(Message {
            role: Role::User,
            parts: vec![Part::Text("next turn".into())],
        });
        let ctx = UpstreamContext {
            provider: &p,
            model: &m,
            account_id: None,
            session_context: None,
            credential: "oauth-token".into(),
        };

        let first_body = AnthropicAdapter::new().build_body(&ctx, &first).unwrap();
        let second_body = AnthropicAdapter::new().build_body(&ctx, &second).unwrap();
        assert_eq!(first_body["system"], second_body["system"]);
        assert_eq!(first_body["tools"], second_body["tools"]);
        assert_ne!(first_body["messages"], second_body["messages"]);
    }

    #[test]
    fn non_claude_translated_request_does_not_generate_cache_controls() {
        let p = provider();
        let m = model();
        let mut req = base_request();
        req.system = vec!["system".into()];
        req.tools = vec![ToolDef {
            name: "read".into(),
            description: None,
            parameters: json!({"type":"object"}),
            defer_loading: None,
        }];
        let ctx = UpstreamContext {
            provider: &p,
            model: &m,
            account_id: None,
            session_context: None,
            credential: "sk-ant-api03-regular-key".into(),
        };

        let body = AnthropicAdapter::new().build_body(&ctx, &req).unwrap();
        assert!(body["system"].is_string());
        assert!(body["tools"][0].get("cache_control").is_none());
    }

    // Anthropic's Create a Message API requires `max_tokens` as its output
    // limit field: https://platform.claude.com/docs/en/api/messages/create.
    #[test]
    fn passthrough_normalization_maps_route_token_aliases_to_messages_field() {
        let p = provider();
        let m = model();
        let ctx = UpstreamContext {
            provider: &p,
            model: &m,
            account_id: None,
            session_context: None,
            credential: "k".into(),
        };
        let mut req = base_request();
        req.params.max_tokens = Some(512);

        for alias in ["max_completion_tokens", "max_output_tokens"] {
            let mut body = json!({
                "max_tokens": 4096,
                "messages": [{"role":"user","content":"hello"}]
            });
            body[alias] = json!(512);

            AnthropicAdapter::new()
                .normalize_passthrough_body(&ctx, &req, &mut body)
                .unwrap();

            assert_eq!(body["max_tokens"], 512);
            assert!(body.get("max_completion_tokens").is_none());
            assert!(body.get("max_output_tokens").is_none());
        }
    }

    #[test]
    fn passthrough_normalization_preserves_client_cache_controls() {
        let p = provider();
        let m = model();
        let req = base_request();
        let ctx = UpstreamContext {
            provider: &p,
            model: &m,
            account_id: None,
            session_context: None,
            credential: "sk-ant-api03-regular-key".into(),
        };
        let mut body = json!({
            "model": "claude-sonnet-5",
            "stream": true,
            "system": [{
                "type": "text",
                "text": "stable",
                "cache_control": {"type":"ephemeral","ttl":"1h"}
            }],
            "messages": [{"role":"user","content":"hello"}],
            "max_tokens": 128
        });
        let before = body.clone();

        AnthropicAdapter::new()
            .normalize_passthrough_body(&ctx, &req, &mut body)
            .unwrap();

        assert_eq!(body, before);
    }

    #[test]
    fn anthropic_frontend_preserves_defer_loading_in_canonical_tools() {
        let req = crate::frontends::anthropic::decode_request(json!({
            "model": "claude-sonnet-5",
            "messages": [{"role":"user","content":"Hello"}],
            "tools": [{
                "name": "deferred",
                "description": "loaded on demand",
                "input_schema": {"type":"object","properties":{}},
                "defer_loading": true
            }]
        }))
        .unwrap();

        assert_eq!(req.tools[0].defer_loading, Some(true));
    }

    #[test]
    fn translated_request_preserves_deferred_tool_and_anchors_previous_tool() {
        let req = crate::frontends::openai::decode_request(json!({
            "model": "claude-sonnet-5",
            "messages": [{"role":"user","content":"Hello"}],
            "tools": [
                {
                    "type": "function",
                    "function": {
                        "name": "cacheable",
                        "description": "always loaded",
                        "parameters": {"type":"object","properties":{}}
                    }
                },
                {
                    "type": "function",
                    "defer_loading": true,
                    "function": {
                        "name": "deferred",
                        "description": "loaded on demand",
                        "parameters": {"type":"object","properties":{}}
                    }
                }
            ],
            "stream": true
        }))
        .unwrap();
        assert_eq!(req.tools[1].defer_loading, Some(true));

        let mut p = provider();
        p.credential_plugin = "claude-code-oauth".into();
        let m = model();
        let ctx = UpstreamContext {
            provider: &p,
            model: &m,
            account_id: None,
            session_context: None,
            credential: "oauth-token".into(),
        };

        let body = AnthropicAdapter::new().build_body(&ctx, &req).unwrap();
        let tools = body["tools"].as_array().unwrap();

        assert_eq!(tools[1]["defer_loading"], true);
        assert!(tools[1].get("cache_control").is_none());
        assert_eq!(
            tools[0]["cache_control"],
            json!({"type":"ephemeral","ttl":"1h"})
        );
    }

    #[test]
    fn claude_code_cache_prefix_is_stable_but_model_scoped() {
        let mut p = provider();
        p.credential_plugin = "claude-code-oauth".into();
        let sonnet = model();
        let mut opus = model();
        opus.upstream_id = "claude-opus-4.6".into();
        opus.display_name = "Claude Opus 4.6".into();
        let mut req = base_request();
        req.system = vec!["stable project instructions".into()];
        req.tools = vec![ToolDef {
            name: "read".into(),
            description: None,
            parameters: json!({"type":"object"}),
            defer_loading: None,
        }];

        let sonnet_ctx = UpstreamContext {
            provider: &p,
            model: &sonnet,
            account_id: None,
            session_context: None,
            credential: "oauth-token".into(),
        };
        let opus_ctx = UpstreamContext {
            provider: &p,
            model: &opus,
            account_id: None,
            session_context: None,
            credential: "oauth-token".into(),
        };
        let sonnet_body = AnthropicAdapter::new()
            .build_body(&sonnet_ctx, &req)
            .unwrap();
        let opus_body = AnthropicAdapter::new().build_body(&opus_ctx, &req).unwrap();

        assert_eq!(sonnet_body["system"], opus_body["system"]);
        assert_eq!(sonnet_body["tools"], opus_body["tools"]);
        assert_ne!(sonnet_body["model"], opus_body["model"]);
    }

    #[test]
    fn build_body_standard_key_does_not_inject_attribution() {
        let p = provider();
        let m = model();
        let req = base_request();
        let ctx = UpstreamContext {
            provider: &p,
            model: &m,
            account_id: None,
            session_context: None,
            credential: "sk-ant-api03-regular-key".into(),
        };

        let body = AnthropicAdapter::new().build_body(&ctx, &req).unwrap();
        assert!(body.get("system").is_none());
    }

    #[test]
    fn scalar_thinking_mapping_uses_budget_field() {
        let p = provider();
        let mut m = model();
        m.thinking_map = serde_json::json!({
            "levels": {"high": 4096},
            "budget_field": "thinking.budget_tokens"
        })
        .to_string();
        let mut req = base_request();
        req.thinking = Some(crate::types::ThinkingLevel::High);
        let ctx = UpstreamContext {
            provider: &p,
            model: &m,
            account_id: None,
            session_context: None,
            credential: "sk-ant-api03-regular-key".into(),
        };

        let body = AnthropicAdapter::new().build_body(&ctx, &req).unwrap();
        assert_eq!(
            body["thinking"],
            serde_json::json!({
                "type": "enabled",
                "budget_tokens": 4096
            })
        );
    }

    #[test]
    fn adaptive_thinking_mapping_uses_effort_without_budget() {
        let p = provider();
        let mut m = model();
        m.thinking_map = serde_json::json!({
            "mode": "adaptive",
            "levels": {"max": "max"},
            "level_field": "output_config.effort"
        })
        .to_string();
        let mut req = base_request();
        req.thinking = Some(crate::types::ThinkingLevel::Max);
        let ctx = UpstreamContext {
            provider: &p,
            model: &m,
            account_id: None,
            session_context: None,
            credential: "sk-ant-api03-regular-key".into(),
        };

        let body = AnthropicAdapter::new().build_body(&ctx, &req).unwrap();
        assert_eq!(body["thinking"], serde_json::json!({"type": "adaptive"}));
        assert_eq!(body["output_config"]["effort"], "max");
        assert!(body["thinking"].get("budget_tokens").is_none());
    }

    #[test]
    fn adaptive_thinking_default_preserves_omitted_effort() {
        let p = provider();
        let mut m = model();
        m.thinking_map = serde_json::json!({
            "mode": "adaptive",
            "levels": {"high": "high"},
            "level_field": "output_config.effort"
        })
        .to_string();
        let mut req = base_request();
        req.thinking = Some(crate::types::ThinkingLevel::Default);
        let ctx = UpstreamContext {
            provider: &p,
            model: &m,
            account_id: None,
            session_context: None,
            credential: "sk-ant-api03-regular-key".into(),
        };

        let body = AnthropicAdapter::new().build_body(&ctx, &req).unwrap();

        assert_eq!(body["thinking"], serde_json::json!({"type": "adaptive"}));
        assert!(body.get("output_config").is_none());
    }

    #[test]
    fn adaptive_thinking_off_stays_disabled_without_effort() {
        let p = provider();
        let mut m = model();
        m.thinking_map = serde_json::json!({
            "mode": "adaptive",
            "levels": {"off": {"thinking.type": "disabled"}},
            "level_field": "output_config.effort"
        })
        .to_string();
        let mut req = base_request();
        req.thinking = Some(crate::types::ThinkingLevel::Off);
        let ctx = UpstreamContext {
            provider: &p,
            model: &m,
            account_id: None,
            session_context: None,
            credential: "sk-ant-api03-regular-key".into(),
        };

        let body = AnthropicAdapter::new().build_body(&ctx, &req).unwrap();

        assert_eq!(body["thinking"], serde_json::json!({"type": "disabled"}));
        assert!(body["thinking"].get("budget_tokens").is_none());
        assert!(body.get("output_config").is_none());
    }

    #[test]
    fn build_body_oauth_token_injects_billing_header_and_sentinel() {
        let p = provider();
        let m = model();
        let req = base_request();
        let ctx = UpstreamContext {
            provider: &p,
            model: &m,
            account_id: None,
            session_context: None,
            credential: "sk-ant-oat01-test-oauth-token".into(),
        };

        let body = AnthropicAdapter::new().build_body(&ctx, &req).unwrap();
        let system = rendered_system_text(&body);
        assert!(system.contains("x-anthropic-billing-header: cc_version="));
        assert!(system.contains("You are Claude Code, Anthropic's official CLI for Claude."));
    }

    #[test]
    fn build_body_claude_code_plugin_injects_attribution() {
        let mut p = provider();
        p.credential_plugin = "plugin:dev.kinetix.claude-code-oauth/claude-code-oauth".into();
        let m = model();
        let req = base_request();
        let ctx = UpstreamContext {
            provider: &p,
            model: &m,
            account_id: None,
            session_context: None,
            credential: "some-opaque-or-exchanged-credential".into(),
        };

        let body = AnthropicAdapter::new().build_body(&ctx, &req).unwrap();
        let system = rendered_system_text(&body);
        assert!(system.contains("x-anthropic-billing-header: cc_version="));
        assert!(system.contains("You are Claude Code, Anthropic's official CLI for Claude."));
    }

    #[test]
    fn build_body_preserves_user_system_prompt() {
        let p = provider();
        let m = model();
        let mut req = base_request();
        req.system = vec!["You are a helpful coding assistant.".into()];
        let ctx = UpstreamContext {
            provider: &p,
            model: &m,
            account_id: None,
            session_context: None,
            credential: "sk-ant-oat01-test-oauth-token".into(),
        };

        let body = AnthropicAdapter::new().build_body(&ctx, &req).unwrap();
        let system = rendered_system_text(&body);
        assert!(system.starts_with("x-anthropic-billing-header: cc_version="));
        assert!(system.contains("You are Claude Code, Anthropic's official CLI for Claude."));
        assert!(system.ends_with("You are a helpful coding assistant."));
    }

    #[test]
    fn build_body_idempotent_if_billing_header_already_present() {
        let p = provider();
        let m = model();
        let mut req = base_request();
        req.system = vec![
            "x-anthropic-billing-header: cc_version=1.18.31; cc_entrypoint=cli; cch=00000;".into(),
            "You are Claude Code, Anthropic's official CLI for Claude.".into(),
        ];
        let ctx = UpstreamContext {
            provider: &p,
            model: &m,
            account_id: None,
            session_context: None,
            credential: "sk-ant-oat01-test-oauth-token".into(),
        };

        let body = AnthropicAdapter::new().build_body(&ctx, &req).unwrap();
        let system = rendered_system_text(&body);
        let count = system.matches("x-anthropic-billing-header:").count();
        assert_eq!(count, 1);
        let sentinel_count = system.matches("You are Claude Code").count();
        assert_eq!(sentinel_count, 1);
    }

    #[test]
    fn build_body_extracts_role_system_from_messages() {
        let p = provider();
        let m = model();
        let mut req = base_request();
        req.messages.insert(
            0,
            Message {
                role: Role::System,
                parts: vec![Part::Text("Be concise.".into())],
            },
        );
        let ctx = UpstreamContext {
            provider: &p,
            model: &m,
            account_id: None,
            session_context: None,
            credential: "sk-ant-oat01-test-oauth-token".into(),
        };

        let body = AnthropicAdapter::new().build_body(&ctx, &req).unwrap();
        let system = rendered_system_text(&body);
        assert!(system.contains("Be concise."));

        let messages = body
            .get("messages")
            .and_then(|v| v.as_array())
            .expect("messages array");
        // Role::System should not be mapped into the outbound user messages.
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0].get("role").and_then(|v| v.as_str()),
            Some("user")
        );
    }
}

#[cfg(test)]
mod error_scope_tests {
    use super::*;
    use crate::adapters::Adapter;

    #[test]
    fn forbidden_entitlement_does_not_poison_anthropic_credential() {
        let adapter = AnthropicAdapter::new();
        let forbidden = adapter.classify_error(
            403,
            r#"{"error":{"type":"permission_error","message":"model entitlement missing"}}"#,
            &reqwest::header::HeaderMap::new(),
        );
        assert_eq!(forbidden.kind, FailureKind::TargetError);

        let unauthorized = adapter.classify_error(
            401,
            r#"{"error":{"type":"authentication_error","message":"invalid x-api-key"}}"#,
            &reqwest::header::HeaderMap::new(),
        );
        assert_eq!(unauthorized.kind, FailureKind::AuthError);
    }
}
