//! OpenAI Chat Completions outbound adapter (wire format `openai`).
//!
//! Also used to translate for cross-provider routes (FR-12.7).

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::adapters::{Adapter, DiscoveredModel, UpstreamContext};
use crate::types::{
    FailureKind, FinishReason, ImageData, InternalRequest, Part, ProxyError, Role, StreamEvent,
    TokenUsage, ToolChoice, UpstreamFailure,
};

pub struct OpenAiAdapter;

impl OpenAiAdapter {
    pub fn new() -> Self {
        OpenAiAdapter
    }

    fn content_value(parts: &[Part]) -> Value {
        let has_non_text = parts.iter().any(|p| matches!(p, Part::Image(_)));
        if !has_non_text {
            // Plain string content.
            let text: String = parts
                .iter()
                .filter_map(|p| match p {
                    Part::Text(t) => Some(t.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            return json!(text);
        }
        let mut arr = Vec::new();
        for p in parts {
            match p {
                Part::Text(t) => arr.push(json!({ "type": "text", "text": t })),
                Part::Image(ImageData::Base64 { mime, data }) => arr.push(json!({
                    "type": "image_url",
                    "image_url": { "url": format!("data:{mime};base64,{data}") }
                })),
                Part::Image(ImageData::Url(url)) => {
                    arr.push(json!({ "type": "image_url", "image_url": { "url": url } }))
                }
                _ => {}
            }
        }
        Value::Array(arr)
    }

    fn build_messages(req: &InternalRequest) -> Vec<Value> {
        let mut msgs = Vec::new();
        for s in &req.system {
            msgs.push(json!({ "role": "system", "content": s }));
        }
        for m in &req.messages {
            match m.role {
                Role::Assistant => {
                    let text: String = m
                        .parts
                        .iter()
                        .filter_map(|p| match p {
                            Part::Text(t) => Some(t.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("");
                    let tool_calls: Vec<Value> = m
                        .parts
                        .iter()
                        .filter_map(|p| match p {
                            Part::ToolCall {
                                id,
                                name,
                                arguments,
                                ..
                            } => Some(json!({
                                "id": id.clone().unwrap_or_else(|| format!("call_{name}")),
                                "type": "function",
                                "function": { "name": name, "arguments": arguments }
                            })),
                            _ => None,
                        })
                        .collect();
                    let mut msg = json!({ "role": "assistant" });
                    if !text.is_empty() {
                        msg["content"] = json!(text);
                    } else {
                        msg["content"] = Value::Null;
                    }
                    if !tool_calls.is_empty() {
                        msg["tool_calls"] = Value::Array(tool_calls);
                    }
                    msgs.push(msg);
                }
                Role::Tool => {
                    for p in &m.parts {
                        if let Part::ToolResult {
                            tool_call_id,
                            content,
                            ..
                        } = p
                        {
                            msgs.push(json!({
                                "role": "tool",
                                "tool_call_id": tool_call_id,
                                "content": content
                            }));
                        }
                    }
                }
                Role::System | Role::User => {
                    msgs.push(json!({
                        "role": "user",
                        "content": Self::content_value(&m.parts)
                    }));
                }
            }
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
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description.clone().unwrap_or_default(),
                        "parameters": if t.parameters.is_null() { json!({"type":"object","properties":{}}) } else { t.parameters.clone() }
                    }
                })
            })
            .collect();
        Some(Value::Array(tools))
    }

    fn build_tool_choice(req: &InternalRequest) -> Option<Value> {
        match req.tool_choice {
            Some(ToolChoice::None) => Some(json!("none")),
            Some(ToolChoice::Required) => Some(json!("required")),
            Some(ToolChoice::Specific) => Some(json!({
                "type": "function",
                "function": { "name": req.tool_choice_name.clone().unwrap_or_default() }
            })),
            Some(ToolChoice::Auto) | None => None,
        }
    }

    fn apply_thinking_map(
        body: &mut serde_json::Map<String, Value>,
        ctx: &UpstreamContext<'_>,
        req: &InternalRequest,
    ) {
        let Some(level) = req.thinking else {
            return;
        };
        let key = level.as_key();
        let mapping = ctx.model.thinking();
        let Some(value) = mapping.levels.get(key) else {
            return;
        };

        if let Some(object) = value.as_object() {
            for (path, value) in object {
                insert_dotted(body, path, value.clone());
            }
        } else if let Some(field) = mapping.scalar_field() {
            insert_dotted(body, field, value.clone());
        }
    }
}

pub(super) fn insert_dotted(obj: &mut serde_json::Map<String, Value>, path: &str, value: Value) {
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

impl Default for OpenAiAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Adapter for OpenAiAdapter {
    fn wire_format(&self) -> &'static str {
        "openai"
    }

    fn build_url(&self, ctx: &UpstreamContext<'_>) -> Result<String, ProxyError> {
        let resolved = ctx.provider.resolved_base_url()?;
        let base = resolved.trim_end_matches('/');
        Ok(format!("{base}/chat/completions"))
    }

    fn apply_auth(
        &self,
        ctx: &UpstreamContext<'_>,
        req: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder, UpstreamFailure> {
        use crate::types::AuthScheme;
        Ok(match ctx.provider.auth() {
            AuthScheme::None => req,
            AuthScheme::Bearer => req.bearer_auth(&ctx.credential),
            AuthScheme::CustomHeader => {
                let name = ctx
                    .provider
                    .custom_header_name
                    .clone()
                    .unwrap_or_else(|| "authorization".to_string());
                if name.eq_ignore_ascii_case("authorization") {
                    req.header(name, format!("Bearer {}", ctx.credential))
                } else {
                    req.header(name, &ctx.credential)
                }
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
        if req
            .messages
            .iter()
            .flat_map(|message| &message.parts)
            .any(|part| matches!(part, Part::RedactedThinking { .. }))
        {
            return Err(UpstreamFailure {
                kind: FailureKind::BadRequest,
                status: None,
                retry_after_secs: None,
                message: "redacted Anthropic thinking state cannot be translated to OpenAI input"
                    .into(),
                quota_reset_at: None,
            });
        }
        let mut body = serde_json::Map::new();
        body.insert("model".to_string(), json!(ctx.model.upstream_id));
        body.insert("messages".to_string(), json!(Self::build_messages(req)));
        body.insert("stream".to_string(), json!(true));
        // Kinetix always asks OpenAI-compatible streaming upstreams for usage
        // so accounting remains correct. Client visibility is controlled
        // separately by the frontend encoder/passthrough filter.
        body.insert(
            "stream_options".to_string(),
            json!({ "include_usage": true }),
        );

        let params = ctx.model.params();
        let mut insert = |key: &str, v: Option<f64>| {
            let spec = params.get(key);
            if let Some(v) = v {
                if let Some(spec) = spec {
                    if !spec.supported {
                        return;
                    }
                    let mut val = v;
                    if let Some(min) = spec.min {
                        val = val.max(min);
                    }
                    if let Some(max) = spec.max {
                        val = val.min(max);
                    }
                    body.insert(key.to_string(), json!(val));
                    return;
                }
                body.insert(key.to_string(), json!(v));
            } else if let Some(spec) = spec {
                // FR-10.6: a configured default applies when the client omits
                // the field (only for a supported parameter).
                if spec.supported {
                    if let Some(d) = spec.default {
                        body.insert(key.to_string(), json!(d));
                    }
                }
            }
        };
        insert("temperature", req.params.temperature);
        insert("top_p", req.params.top_p);
        insert("top_k", req.params.top_k);
        if let Some(mt) = req.params.max_tokens {
            let mut v = mt as i64;
            if let Some(max) = ctx.model.max_output_tokens {
                v = v.min(max);
            }
            body.insert("max_tokens".to_string(), json!(v));
        }
        if !req.params.stop.is_empty() {
            body.insert("stop".to_string(), json!(req.params.stop));
        }
        if let Some(seed) = req.params.seed {
            body.insert("seed".to_string(), json!(seed));
        }
        if let Some(p) = req.params.presence_penalty {
            body.insert("presence_penalty".to_string(), json!(p));
        }
        if let Some(p) = req.params.frequency_penalty {
            body.insert("frequency_penalty".to_string(), json!(p));
        }
        // Provider prompt-cache hint (FR-7.1): on the OpenAI translation path
        // the hint is a request field, so preserve it rather than dropping it.
        // set-if-absent, so an admin model extra_request still wins.
        if let Some(key) = req.extra.get("prompt_cache_key").and_then(|v| v.as_str()) {
            body.entry("prompt_cache_key".to_string())
                .or_insert_with(|| json!(key));
        }
        // Non-canonical Chat fields admitted by the target field policy
        // (`frontends::apply_target_field_policy`) map 1:1 onto this wire.
        for key in [
            "user",
            "service_tier",
            "verbosity",
            "logit_bias",
            "web_search_options",
        ] {
            if let Some(value) = req.extra.get(key).filter(|v| !v.is_null()) {
                body.insert(key.to_string(), value.clone());
            }
        }
        // OpenAI only accepts `parallel_tool_calls` alongside `tools`.
        if !req.tools.is_empty() {
            if let Some(value) = req
                .extra
                .get("parallel_tool_calls")
                .filter(|v| v.is_boolean())
            {
                body.insert("parallel_tool_calls".to_string(), value.clone());
            }
        }
        if let Some(tools) = Self::build_tools(req) {
            body.insert("tools".to_string(), tools);
        }
        if let Some(tc) = Self::build_tool_choice(req) {
            body.insert("tool_choice".to_string(), tc);
        }
        Self::apply_thinking_map(&mut body, ctx, req);

        // Merge admin extra fields (FR-10.8).
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
            return Err(UpstreamFailure {
                kind: FailureKind::BadRequest,
                status: None,
                retry_after_secs: None,
                message: "OpenAI passthrough body must be a JSON object".into(),
                quota_reset_at: None,
            });
        };

        if let Some(max_tokens) = req.params.max_tokens {
            let requested = u64::from(max_tokens);
            // Preserve the caller's Chat token-limit spelling when it carries
            // the canonical value. Route overrides may add a conflicting alias;
            // in that case, prefer the field carrying the override, then fall
            // back to an existing Chat field before using this adapter's default.
            let field =
                if body.get("max_completion_tokens").and_then(Value::as_u64) == Some(requested) {
                    "max_completion_tokens"
                } else if body.get("max_tokens").and_then(Value::as_u64) == Some(requested) {
                    "max_tokens"
                } else if body.contains_key("max_completion_tokens") {
                    "max_completion_tokens"
                } else {
                    "max_tokens"
                };

            for alias in ["max_tokens", "max_completion_tokens", "max_output_tokens"] {
                body.remove(alias);
            }
            let mut max_tokens = max_tokens as i64;
            if let Some(max) = ctx.model.max_output_tokens {
                max_tokens = max_tokens.min(max);
            }
            body.insert(field.into(), json!(max_tokens));
        }
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
            .or_else(|| parsed.get("message").and_then(|v| v.as_str()))
            .unwrap_or("")
            .to_string();
        let code = parsed
            .pointer("/error/code")
            .or_else(|| parsed.get("code"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let retry_after_secs = headers
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok());

        let lower_message = message.to_ascii_lowercase();
        let lower_code = code.to_ascii_lowercase();
        let error_type = parsed
            .pointer("/error/type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let credential_error = status == 401
            || lower_code.contains("invalid_api_key")
            || lower_code.contains("invalid_auth")
            || error_type.contains("authentication")
            || lower_message.contains("invalid api key")
            || lower_message.contains("incorrect api key")
            || lower_message.contains("authentication token");

        let quota_exhausted = lower_code.contains("insufficient_quota")
            || lower_code.contains("insufficient_credit")
            || lower_code.contains("credit_insufficient")
            || lower_message.contains("insufficient balance")
            || lower_message.contains("credit insufficient")
            || lower_message.contains("insufficient credit")
            || (lower_message.contains("balance=") && lower_message.contains("required="));

        let kind = match status {
            400 | 422 if quota_exhausted => FailureKind::QuotaExhausted,
            400 | 422 => FailureKind::BadRequest,
            401 => FailureKind::AuthError,
            403 if credential_error => FailureKind::AuthError,
            403 | 404 => FailureKind::TargetError,
            429 => {
                if quota_exhausted || lower_message.contains("quota") {
                    FailureKind::QuotaExhausted
                } else {
                    FailureKind::RateLimit
                }
            }
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
        if data.trim() == "[DONE]" {
            return Ok(Vec::new());
        }
        let v: Value = serde_json::from_str(data).map_err(|e| UpstreamFailure {
            kind: FailureKind::MalformedUpstream,
            status: None,
            retry_after_secs: None,
            message: format!("invalid upstream chunk: {e}"),
            quota_reset_at: None,
        })?;
        Ok(openai_events(&v))
    }

    fn parse_full_response(&self, body: &Value) -> Result<Vec<StreamEvent>, UpstreamFailure> {
        Ok(openai_events(body))
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
                        display_name: None,
                        context_window: m.get("context_window").and_then(|v| v.as_i64()),
                        max_output_tokens: m.get("max_output_tokens").and_then(|v| v.as_i64()),
                    });
                }
            }
        }
        out
    }
}

fn openai_events(v: &Value) -> Vec<StreamEvent> {
    let mut events = Vec::new();
    if let Some(choices) = v.get("choices").and_then(|c| c.as_array()) {
        for choice in choices {
            // Streaming delta or non-streaming message.
            let delta = choice.get("delta").or_else(|| choice.get("message"));
            if let Some(delta) = delta {
                if let Some(content) = delta.get("content") {
                    if let Some(text) = content.as_str() {
                        if !text.is_empty() {
                            events.push(StreamEvent::TextDelta(text.to_string()));
                        }
                    }
                }
                if let Some(reasoning) = delta
                    .get("reasoning_content")
                    .or_else(|| delta.get("reasoning"))
                    .and_then(|r| r.as_str())
                {
                    if !reasoning.is_empty() {
                        events.push(StreamEvent::ThinkingDelta {
                            block_index: None,
                            text: reasoning.to_string(),
                            signature: None,
                        });
                    }
                }
                if let Some(tool_calls) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                    for tc in tool_calls {
                        let index = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as u32;
                        let id = tc.get("id").and_then(|i| i.as_str()).map(String::from);
                        let func = tc.get("function");
                        if let Some(name) =
                            func.and_then(|f| f.get("name")).and_then(|n| n.as_str())
                        {
                            if !name.is_empty() {
                                events.push(StreamEvent::ToolCallStart {
                                    index,
                                    id: id.clone(),
                                    name: name.to_string(),
                                    signature: None,
                                });
                            }
                        }
                        if let Some(args) = func
                            .and_then(|f| f.get("arguments"))
                            .and_then(|a| a.as_str())
                        {
                            if !args.is_empty() {
                                events.push(StreamEvent::ToolCallArgsDelta {
                                    index,
                                    args: args.to_string(),
                                });
                            }
                        }
                    }
                }
            }
            if let Some(reason) = choice.get("finish_reason").and_then(|r| r.as_str()) {
                events.push(StreamEvent::Finish(match reason {
                    "stop" => FinishReason::Stop,
                    "length" => FinishReason::Length,
                    "tool_calls" | "function_call" => FinishReason::ToolCalls,
                    "content_filter" => FinishReason::ContentFilter,
                    other => FinishReason::Other(other.to_string()),
                }));
            }
        }
    }
    if let Some(usage) = v.get("usage") {
        if !usage.is_null() {
            events.push(StreamEvent::Usage(TokenUsage {
                input: usage.get("prompt_tokens").and_then(|v| v.as_u64()),
                output: usage.get("completion_tokens").and_then(|v| v.as_u64()),
                cached: usage
                    .pointer("/prompt_tokens_details/cached_tokens")
                    .and_then(|v| v.as_u64()),
                cache_write: usage
                    .pointer("/prompt_tokens_details/cache_write_tokens")
                    .and_then(|v| v.as_u64()),
                thinking: usage
                    .pointer("/completion_tokens_details/reasoning_tokens")
                    .and_then(|v| v.as_u64()),
            }));
        }
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_keeps_reasoning_as_output_breakdown() {
        let events = openai_events(&serde_json::json!({
            "choices": [],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 80,
                "prompt_tokens_details": {
                    "cached_tokens": 40,
                    "cache_write_tokens": 10
                },
                "completion_tokens_details": {
                    "reasoning_tokens": 30
                }
            }
        }));
        let usage = events
            .into_iter()
            .find_map(|event| match event {
                StreamEvent::Usage(usage) => Some(usage),
                _ => None,
            })
            .expect("usage event");
        assert_eq!(usage.input, Some(100));
        assert_eq!(usage.output, Some(80));
        assert_eq!(usage.cached, Some(40));
        assert_eq!(usage.cache_write, Some(10));
        assert_eq!(usage.thinking, Some(30));
    }

    use crate::db::{ModelRow, ProviderRow};
    use crate::types::AuthScheme;

    fn provider() -> ProviderRow {
        ProviderRow {
            id: "prov".into(),
            name: "p".into(),
            base_url: "https://api.example.com/v1".into(),
            wire_format: "openai".into(),
            auth_scheme: "bearer".into(),
            custom_header_name: None,
            custom_param_name: None,
            extra_headers: "{}".into(),
            timeout_ms: 1000,
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
            id: "m".into(),
            provider_id: "prov".into(),
            upstream_id: "up".into(),
            display_name: "Up".into(),
            enabled: 1,
            context_window: None,
            max_output_tokens: None,
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
            requested_model: "up".into(),
            system: vec![],
            messages: vec![],
            tools: vec![],
            tool_choice: None,
            tool_choice_name: None,
            params: Default::default(),
            stream: true,
            include_usage: false,
            thinking: None,
            extra: Default::default(),
            raw_body: None,
        }
    }

    #[test]
    fn redacted_anthropic_thinking_is_not_silently_translated_to_openai() {
        let p = provider();
        let m = model();
        let ctx = UpstreamContext {
            provider: &p,
            model: &m,
            account_id: None,
            session_context: None,
            credential_metadata: None,
            credential: "k".into(),
        };
        let mut req = base_request();
        req.messages.push(crate::types::Message {
            role: crate::types::Role::Assistant,
            parts: vec![Part::RedactedThinking {
                data: "opaque".into(),
            }],
        });

        let failure = OpenAiAdapter.build_body(&ctx, &req).unwrap_err();
        assert_eq!(failure.kind, FailureKind::BadRequest);
        assert!(failure
            .message
            .contains("redacted Anthropic thinking state"));
    }

    #[test]
    fn chat_passthrough_preserves_token_limit_spelling_and_normalizes_route_aliases() {
        let p = provider();
        let m = model();
        let ctx = UpstreamContext {
            provider: &p,
            model: &m,
            account_id: None,
            session_context: None,
            credential_metadata: None,
            credential: "k".into(),
        };
        let adapter = OpenAiAdapter;
        let mut req = base_request();
        req.params.max_tokens = Some(512);

        // OpenAI documents max_completion_tokens as the Chat Completions
        // output-limit field; preserve it on native passthrough:
        // https://developers.openai.com/api/docs/guides/token-counting.
        let mut completion_tokens = serde_json::json!({"max_completion_tokens": 512});
        adapter
            .normalize_passthrough_body(&ctx, &req, &mut completion_tokens)
            .unwrap();
        assert_eq!(completion_tokens["max_completion_tokens"], 512);
        assert!(completion_tokens.get("max_tokens").is_none());
        assert!(completion_tokens.get("max_output_tokens").is_none());

        // A route-level Responses alias overrides the original Chat limit, then
        // is serialized using the Chat field already present in the request.
        let mut route_override = serde_json::json!({"max_tokens": 4096, "max_output_tokens": 512});
        adapter
            .normalize_passthrough_body(&ctx, &req, &mut route_override)
            .unwrap();
        assert_eq!(route_override["max_tokens"], 512);
        assert!(route_override.get("max_output_tokens").is_none());

        let mut capped_model = model();
        capped_model.max_output_tokens = Some(300);
        let capped_ctx = UpstreamContext {
            provider: &p,
            model: &capped_model,
            account_id: None,
            session_context: None,
            credential_metadata: None,
            credential: "k".into(),
        };
        let mut capped = serde_json::json!({"max_completion_tokens": 512});
        adapter
            .normalize_passthrough_body(&capped_ctx, &req, &mut capped)
            .unwrap();
        assert_eq!(capped["max_completion_tokens"], 300);
        assert!(capped.get("max_tokens").is_none());

        req.params.max_tokens = None;
        let mut untouched = serde_json::json!({"max_output_tokens": "unknown"});
        adapter
            .normalize_passthrough_body(&ctx, &req, &mut untouched)
            .unwrap();
        assert_eq!(untouched["max_output_tokens"], "unknown");
    }

    #[test]
    fn preserves_provider_prompt_cache_hint_on_translation_path() {
        let p = provider();
        let m = model();
        let ctx = UpstreamContext {
            provider: &p,
            model: &m,
            account_id: None,
            session_context: None,
            credential_metadata: None,
            credential: "k".into(),
        };
        let mut req = base_request();
        req.extra
            .insert("prompt_cache_key".into(), serde_json::json!("conv-123"));
        let adapter = OpenAiAdapter;
        let body = adapter.build_body(&ctx, &req).unwrap();
        assert_eq!(
            body.get("prompt_cache_key").and_then(|v| v.as_str()),
            Some("conv-123"),
            "the provider prompt-cache hint must survive translation (FR-7.1)"
        );
    }

    #[test]
    fn translated_body_preserves_top_k_mapped_thinking_and_internal_usage() {
        let p = provider();
        let mut m = model();
        m.parameters = serde_json::json!({
            "top_k": { "supported": true, "policy": "forward" }
        })
        .to_string();
        m.thinking_map = serde_json::json!({
            "levels": {
                "low": { "reasoning_effort": "low" },
                "medium": { "reasoning_effort": "medium" },
                "high": { "reasoning_effort": "high" }
            }
        })
        .to_string();
        let ctx = UpstreamContext {
            provider: &p,
            model: &m,
            account_id: None,
            session_context: None,
            credential_metadata: None,
            credential: "k".into(),
        };
        let mut req = base_request();
        req.params.top_k = Some(42.0);

        for (level, expected) in [
            (crate::types::ThinkingLevel::Low, "low"),
            (crate::types::ThinkingLevel::Medium, "medium"),
            (crate::types::ThinkingLevel::High, "high"),
        ] {
            req.thinking = Some(level);
            let body = OpenAiAdapter.build_body(&ctx, &req).unwrap();
            assert_eq!(body["top_k"], 42.0);
            assert_eq!(body["reasoning_effort"], expected);
            assert_eq!(body["stream_options"]["include_usage"], true);
        }
    }

    #[test]
    fn discovered_reasoning_aliases_emit_provider_tokens_through_build_body() {
        let metadata = serde_json::json!({
            "supportedThinkingEfforts": ["none", "low", "extra_high"]
        });
        let capability =
            crate::model_capabilities::normalize_reasoning_capability(&metadata).unwrap();
        let map = crate::model_capabilities::thinking_map_for_reasoning_with_wire(
            &capability,
            crate::types::WireFormat::Openai,
        )
        .unwrap();

        assert_eq!(map.levels.get("off"), Some(&serde_json::json!("none")));
        assert_eq!(
            map.levels.get("xhigh"),
            Some(&serde_json::json!("extra_high"))
        );

        let p = provider();
        let mut m = model();
        m.thinking_map = serde_json::to_string(&map).unwrap();
        let ctx = UpstreamContext {
            provider: &p,
            model: &m,
            account_id: None,
            session_context: None,
            credential_metadata: None,
            credential: "k".into(),
        };
        let adapter = OpenAiAdapter;
        let mut req = base_request();

        for (level, expected) in [
            (crate::types::ThinkingLevel::Off, "none"),
            (crate::types::ThinkingLevel::XHigh, "extra_high"),
        ] {
            req.thinking = Some(level);
            let body = adapter.build_body(&ctx, &req).unwrap();
            assert_eq!(body["reasoning_effort"], expected);
        }
    }

    #[test]
    fn nested_discovery_efforts_do_not_emit_responses_field_on_chat_completions() {
        let metadata = serde_json::json!({
            "reasoning": {
                "supported_efforts": ["minimal", "low", "high"]
            }
        });
        let capability =
            crate::model_capabilities::normalize_reasoning_capability(&metadata).unwrap();
        assert!(crate::model_capabilities::thinking_map_for_reasoning(&capability).is_none());

        let p = provider();
        let mut m = model();
        m.thinking_map = serde_json::json!({"levels": {}}).to_string();
        let ctx = UpstreamContext {
            provider: &p,
            model: &m,
            account_id: None,
            session_context: None,
            credential_metadata: None,
            credential: "k".into(),
        };
        let adapter = OpenAiAdapter;
        let mut req = base_request();
        req.thinking = Some(crate::types::ThinkingLevel::High);

        let body = adapter.build_body(&ctx, &req).unwrap();
        assert!(body.pointer("/reasoning/effort").is_none());
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn normalized_plugin_reasoning_emits_upstream_tokens_through_build_body() {
        let metadata = serde_json::json!({
            "reasoning": {
                "mode": "level",
                "levels": ["off", "xhigh"],
                "upstream_format": "openai_effort",
                "upstream_levels": {
                    "xhigh": "extra_high"
                }
            }
        });
        let capability =
            crate::model_capabilities::normalize_reasoning_capability(&metadata).unwrap();
        let map = crate::model_capabilities::thinking_map_for_reasoning(&capability).unwrap();

        assert_eq!(map.levels.get("off"), Some(&serde_json::json!("none")));
        assert_eq!(
            map.levels.get("xhigh"),
            Some(&serde_json::json!("extra_high"))
        );

        let p = provider();
        let mut m = model();
        m.thinking_map = serde_json::to_string(&map).unwrap();
        let ctx = UpstreamContext {
            provider: &p,
            model: &m,
            account_id: None,
            session_context: None,
            credential_metadata: None,
            credential: "k".into(),
        };
        let adapter = OpenAiAdapter;
        let mut req = base_request();

        for (level, expected) in [
            (crate::types::ThinkingLevel::Off, "none"),
            (crate::types::ThinkingLevel::XHigh, "extra_high"),
        ] {
            req.thinking = Some(level);
            let body = adapter.build_body(&ctx, &req).unwrap();
            assert_eq!(body["reasoning_effort"], expected);
        }
    }

    #[test]
    fn wire_format_is_openai() {
        assert_eq!(OpenAiAdapter.wire_format(), "openai");
        let _ = AuthScheme::Bearer;
    }
}

#[cfg(test)]
mod param_default_tests {
    use super::*;
    use crate::db::{ModelRow, ProviderRow};

    fn provider() -> ProviderRow {
        ProviderRow {
            id: "prov".into(),
            name: "p".into(),
            base_url: "https://api.example.com/v1".into(),
            wire_format: "openai".into(),
            auth_scheme: "bearer".into(),
            custom_header_name: None,
            custom_param_name: None,
            extra_headers: "{}".into(),
            timeout_ms: 1000,
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

    fn model_with_params(parameters: &str) -> ModelRow {
        ModelRow {
            id: "m".into(),
            provider_id: "prov".into(),
            upstream_id: "up".into(),
            display_name: "Up".into(),
            enabled: 1,
            context_window: None,
            max_output_tokens: None,
            capabilities: "{}".into(),
            prices: "{}".into(),
            parameters: parameters.into(),
            thinking_map: "{}".into(),
            extra_request: "{}".into(),
            discovery: "{}".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            opaque_state_plugin: String::new(),
        }
    }

    fn req_without_temperature() -> InternalRequest {
        InternalRequest {
            requested_model: "up".into(),
            system: vec![],
            messages: vec![],
            tools: vec![],
            tool_choice: None,
            tool_choice_name: None,
            params: Default::default(),
            stream: true,
            include_usage: false,
            thinking: None,
            extra: Default::default(),
            raw_body: None,
        }
    }

    #[test]
    fn configured_default_is_applied_when_client_omits_the_field() {
        let p = provider();
        let m = model_with_params(r#"{"temperature":{"supported":true,"default":0.3}}"#);
        let ctx = UpstreamContext {
            provider: &p,
            model: &m,
            account_id: None,
            session_context: None,
            credential_metadata: None,
            credential: "k".into(),
        };
        let adapter = OpenAiAdapter;
        let body = adapter
            .build_body(&ctx, &req_without_temperature())
            .unwrap();
        assert_eq!(
            body.get("temperature").and_then(|v| v.as_f64()),
            Some(0.3),
            "an unset field with a configured default must be filled (FR-10.6)"
        );
    }

    #[test]
    fn client_value_wins_over_the_default() {
        let p = provider();
        let m = model_with_params(r#"{"temperature":{"supported":true,"default":0.3}}"#);
        let ctx = UpstreamContext {
            provider: &p,
            model: &m,
            account_id: None,
            session_context: None,
            credential_metadata: None,
            credential: "k".into(),
        };
        let mut req = req_without_temperature();
        req.params.temperature = Some(0.9);
        let adapter = OpenAiAdapter;
        let body = adapter.build_body(&ctx, &req).unwrap();
        assert_eq!(body.get("temperature").and_then(|v| v.as_f64()), Some(0.9));
    }
}

#[cfg(test)]
mod error_scope_tests {
    use super::*;
    use crate::adapters::Adapter;

    #[test]
    fn forbidden_model_scope_does_not_poison_openai_credential() {
        let adapter = OpenAiAdapter::new();
        let forbidden = adapter.classify_error(
            403,
            r#"{"error":{"message":"model access denied","code":"model_not_allowed"}}"#,
            &reqwest::header::HeaderMap::new(),
        );
        assert_eq!(forbidden.kind, FailureKind::TargetError);

        let unauthorized = adapter.classify_error(
            401,
            r#"{"error":{"message":"invalid api key","code":"invalid_api_key"}}"#,
            &reqwest::header::HeaderMap::new(),
        );
        assert_eq!(unauthorized.kind, FailureKind::AuthError);
    }

    #[test]
    fn byok_credit_exhaustion_on_400_or_422_is_quota_exhausted() {
        let adapter = OpenAiAdapter::new();
        let cases = [
            (
                400,
                r#"{"message":"credit insufficient balance: balance=731 required=2612","type":"invalid_request_error","param":null,"code":"invalid_request_error"}"#,
            ),
            (
                422,
                r#"{"error":{"message":"insufficient balance for request","code":"invalid_request_error"}}"#,
            ),
            (
                400,
                r#"{"message":"billing limit reached","code":"insufficient_quota"}"#,
            ),
        ];

        for (status, body) in cases {
            let failure = adapter.classify_error(status, body, &reqwest::header::HeaderMap::new());
            assert_eq!(failure.kind, FailureKind::QuotaExhausted);
            assert!(failure.kind.is_account_scoped());
            assert!(failure.kind.is_retryable());
        }
    }

    #[test]
    fn ordinary_bad_request_remains_non_retryable() {
        let adapter = OpenAiAdapter::new();
        let failure = adapter.classify_error(
            400,
            r#"{"error":{"message":"invalid parameter: temperature","code":"invalid_request_error"}}"#,
            &reqwest::header::HeaderMap::new(),
        );

        assert_eq!(failure.kind, FailureKind::BadRequest);
        assert!(!failure.kind.is_account_scoped());
        assert!(!failure.kind.is_retryable());
    }
}
