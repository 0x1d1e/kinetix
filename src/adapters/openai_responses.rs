//! Native OpenAI Responses API outbound adapter.
//!
//! This is a distinct execution transport from OpenAI Chat Completions. The
//! endpoint and request/response dialect are selected together by the resolved
//! target transport.

use async_trait::async_trait;
use serde_json::{json, Map, Value};

use crate::adapters::{Adapter, DiscoveredModel, UpstreamContext};
use crate::types::{
    FailureKind, FinishReason, ImageData, InternalRequest, Part, ProxyError, Role, StreamEvent,
    TokenUsage, ToolChoice, UpstreamFailure,
};

pub struct OpenAiResponsesAdapter;

impl OpenAiResponsesAdapter {
    pub fn new() -> Self {
        Self
    }

    fn failure(message: impl Into<String>) -> UpstreamFailure {
        UpstreamFailure {
            kind: FailureKind::ServerError,
            status: None,
            retry_after_secs: None,
            message: message.into(),
            quota_reset_at: None,
        }
    }

    fn bad_request(message: impl Into<String>) -> UpstreamFailure {
        UpstreamFailure {
            kind: FailureKind::BadRequest,
            status: None,
            retry_after_secs: None,
            message: message.into(),
            quota_reset_at: None,
        }
    }

    fn input_content(parts: &[Part], role: Role) -> Result<Vec<Value>, UpstreamFailure> {
        let mut content = Vec::new();
        for part in parts {
            match part {
                Part::Text(text) => content.push(json!({
                    "type": if role == Role::Assistant { "output_text" } else { "input_text" },
                    "text": text
                })),
                Part::Image(ImageData::Base64 { mime, data }) => content.push(json!({
                    "type": "input_image",
                    "image_url": format!("data:{mime};base64,{data}")
                })),
                Part::Image(ImageData::Url(url)) => content.push(json!({
                    "type": "input_image",
                    "image_url": url
                })),
                Part::ToolCall { .. } | Part::ToolResult { .. } => {
                    return Err(Self::bad_request(
                        "tool state must be serialized as Responses function-call input items",
                    ));
                }
                Part::Thinking { .. } => {
                    return Err(Self::bad_request(
                        "provider reasoning state is not supported as Responses input",
                    ));
                }
            }
        }
        Ok(content)
    }

    fn input_items(req: &InternalRequest) -> Result<Vec<Value>, UpstreamFailure> {
        let mut items = Vec::new();
        for message in &req.messages {
            match message.role {
                Role::System => {
                    let mut texts = Vec::new();
                    for part in &message.parts {
                        match part {
                            Part::Text(text) => texts.push(text.as_str()),
                            Part::Thinking { .. } => {
                                return Err(Self::bad_request(
                                    "provider reasoning state is not supported as Responses input",
                                ));
                            }
                            _ => {
                                return Err(Self::bad_request(
                                    "system messages only support text in Responses input",
                                ));
                            }
                        }
                    }
                    if !texts.is_empty() {
                        items.push(json!({
                            "role": "system",
                            "content": texts.join("")
                        }));
                    }
                }
                Role::User | Role::Assistant => {
                    let mut content = Vec::new();
                    for part in &message.parts {
                        match part {
                            Part::ToolCall {
                                id,
                                name,
                                arguments,
                                ..
                            } if message.role == Role::Assistant => {
                                let call_id =
                                    id.as_deref().filter(|id| !id.is_empty()).ok_or_else(|| {
                                        Self::bad_request(
                                            "Responses function calls require a non-empty call id",
                                        )
                                    })?;
                                items.push(json!({
                                    "type": "function_call",
                                    "call_id": call_id,
                                    "name": name,
                                    "arguments": arguments
                                }));
                            }
                            Part::ToolResult {
                                tool_call_id,
                                content: output,
                                ..
                            } if message.role == Role::User => {
                                if tool_call_id.is_empty() {
                                    return Err(Self::bad_request(
                                        "Responses function-call outputs require a non-empty call id",
                                    ));
                                }
                                items.push(json!({
                                    "type": "function_call_output",
                                    "call_id": tool_call_id,
                                    "output": output
                                }));
                            }
                            other => content.extend(Self::input_content(
                                std::slice::from_ref(other),
                                message.role,
                            )?),
                        }
                    }
                    if !content.is_empty() {
                        items.push(json!({
                            "role": if message.role == Role::Assistant { "assistant" } else { "user" },
                            "content": content
                        }));
                    }
                }
                Role::Tool => {
                    for part in &message.parts {
                        let Part::ToolResult {
                            tool_call_id,
                            content,
                            ..
                        } = part
                        else {
                            return Err(Self::bad_request(
                                "Responses tool messages require function-call output items",
                            ));
                        };
                        if tool_call_id.is_empty() {
                            return Err(Self::bad_request(
                                "Responses function-call outputs require a non-empty call id",
                            ));
                        }
                        items.push(json!({
                            "type": "function_call_output",
                            "call_id": tool_call_id,
                            "output": content
                        }));
                    }
                }
            }
        }
        Ok(items)
    }

    fn build_tools(req: &InternalRequest) -> Option<Value> {
        if req.tools.is_empty() {
            return None;
        }
        Some(Value::Array(
            req.tools
                .iter()
                .map(|tool| {
                    let mut value = json!({
                        "type": "function",
                        "name": tool.name,
                        "parameters": if tool.parameters.is_null() {
                            json!({"type":"object","properties":{}})
                        } else {
                            tool.parameters.clone()
                        }
                    });
                    if let Some(description) = &tool.description {
                        value["description"] = json!(description);
                    }
                    value
                })
                .collect(),
        ))
    }

    fn build_tool_choice(req: &InternalRequest) -> Result<Option<Value>, UpstreamFailure> {
        match req.tool_choice {
            Some(ToolChoice::None) => Ok(Some(json!("none"))),
            Some(ToolChoice::Required) => Ok(Some(json!("required"))),
            Some(ToolChoice::Specific) => {
                let name = req
                    .tool_choice_name
                    .as_deref()
                    .filter(|name| !name.is_empty())
                    .ok_or_else(|| {
                        Self::bad_request("named function choice requires a function name")
                    })?;
                Ok(Some(json!({
                    "type": "function",
                    "name": name
                })))
            }
            Some(ToolChoice::Auto) | None => Ok(None),
        }
    }

    fn apply_model_request_policy(
        ctx: &UpstreamContext<'_>,
        req: &InternalRequest,
        body: &mut Map<String, Value>,
    ) -> Result<(), UpstreamFailure> {
        let params = ctx.model.params();
        for (name, requested) in [
            ("top_k", req.params.top_k.is_some()),
            ("stop", !req.params.stop.is_empty()),
            ("seed", req.params.seed.is_some()),
            ("presence_penalty", req.params.presence_penalty.is_some()),
            ("frequency_penalty", req.params.frequency_penalty.is_some()),
        ] {
            body.remove(name);
            if requested
                && !params
                    .get(name)
                    .is_some_and(|spec| spec.policy == crate::types::ParamPolicy::Drop)
            {
                return Err(Self::bad_request(format!(
                    "requested parameter '{name}' is not supported by the OpenAI Responses transport"
                )));
            }
        }

        for key in ["temperature", "top_p"] {
            body.remove(key);
            if let Some(requested) = if key == "temperature" {
                req.params.temperature
            } else {
                req.params.top_p
            } {
                let spec = params.get(key);
                if spec.is_some_and(|spec| !spec.supported) {
                    if spec.is_some_and(|spec| spec.policy == crate::types::ParamPolicy::Reject) {
                        return Err(Self::bad_request(format!(
                            "parameter '{key}' is not supported by this model"
                        )));
                    }
                    if spec.is_some_and(|spec| spec.policy == crate::types::ParamPolicy::Drop) {
                        continue;
                    }
                }
                if let Some(spec) =
                    spec.filter(|spec| spec.policy == crate::types::ParamPolicy::Reject)
                {
                    if spec.min.is_some_and(|min| requested < min) {
                        return Err(Self::bad_request(format!(
                            "parameter '{key}' below minimum {}",
                            spec.min.unwrap()
                        )));
                    }
                    if spec.max.is_some_and(|max| requested > max) {
                        return Err(Self::bad_request(format!(
                            "parameter '{key}' above maximum {}",
                            spec.max.unwrap()
                        )));
                    }
                }
                let mut value = requested;
                if let Some(spec) = spec {
                    value = value.max(spec.min.unwrap_or(f64::NEG_INFINITY));
                    value = value.min(spec.max.unwrap_or(f64::INFINITY));
                }
                body.insert(key.to_string(), json!(value));
            } else if let Some(default) = params
                .get(key)
                .filter(|spec| spec.supported)
                .and_then(|spec| spec.default)
            {
                body.insert(key.to_string(), json!(default));
            }
        }

        // The Responses create schema uses max_output_tokens; remove Chat
        // aliases before rebuilding the target-specific token limit.
        // https://developers.openai.com/api/reference/cli/resources/responses/methods/create
        body.remove("max_tokens");
        body.remove("max_completion_tokens");
        body.remove("max_output_tokens");
        if let Some(max_tokens) = req.params.max_tokens {
            let max_tokens = ctx
                .model
                .max_output_tokens
                .map(|max| max.max(0) as u32)
                .map(|max| max_tokens.min(max))
                .unwrap_or(max_tokens);
            body.insert("max_output_tokens".into(), json!(max_tokens));
        }

        body.remove("reasoning_effort");
        body.remove("reasoning");
        if let Some(level) = req.thinking {
            let thinking = ctx.model.thinking();
            if let Some(value) = thinking.levels.get(level.as_key()) {
                if let Some(fields) = value.as_object() {
                    for (path, field_value) in fields {
                        crate::adapters::openai::insert_dotted(body, path, field_value.clone());
                    }
                } else if let Some(field) = thinking.scalar_field() {
                    crate::adapters::openai::insert_dotted(body, field, value.clone());
                }
            }
        }
        Ok(())
    }

    fn apply_model_extra_request(ctx: &UpstreamContext<'_>, body: &mut Map<String, Value>) {
        let extra = ctx.model.extra_request_value();
        if let Some(extra) = extra.as_object() {
            for (key, value) in extra {
                body.entry(key.clone()).or_insert_with(|| value.clone());
            }
        }
    }

    fn usage(response: &Value) -> Option<TokenUsage> {
        let usage = response.get("usage")?;
        Some(TokenUsage {
            input: usage.get("input_tokens").and_then(Value::as_u64),
            output: usage.get("output_tokens").and_then(Value::as_u64),
            cached: usage
                .pointer("/input_tokens_details/cached_tokens")
                .and_then(Value::as_u64),
            cache_write: usage
                .pointer("/input_tokens_details/cache_write_tokens")
                .and_then(Value::as_u64),
            thinking: usage
                .pointer("/output_tokens_details/reasoning_tokens")
                .and_then(Value::as_u64),
        })
    }

    fn output_events(output: &[Value]) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        for (output_index, item) in output.iter().enumerate() {
            match item.get("type").and_then(Value::as_str) {
                Some("message") => {
                    if let Some(content) = item.get("content").and_then(Value::as_array) {
                        for part in content {
                            let part_type = part.get("type").and_then(Value::as_str);
                            let text = match part_type {
                                Some("output_text") => part.get("text").and_then(Value::as_str),
                                Some("refusal") => part.get("refusal").and_then(Value::as_str),
                                _ => None,
                            };
                            if let Some(text) = text.filter(|text| !text.is_empty()) {
                                let event = if part_type == Some("refusal") {
                                    StreamEvent::RefusalDelta(text.to_string())
                                } else {
                                    StreamEvent::TextDelta(text.to_string())
                                };
                                events.push(event);
                            }
                        }
                    }
                }
                Some("function_call") => {
                    let Some(name) = item.get("name").and_then(Value::as_str) else {
                        continue;
                    };
                    let call_id = item
                        .get("call_id")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    events.push(StreamEvent::ToolCallStart {
                        index: output_index as u32,
                        id: call_id,
                        name: name.to_string(),
                        signature: None,
                    });
                    if let Some(arguments) = item.get("arguments").and_then(Value::as_str) {
                        if !arguments.is_empty() {
                            events.push(StreamEvent::ToolCallArgsDelta {
                                index: output_index as u32,
                                args: arguments.to_string(),
                            });
                        }
                    }
                }
                Some("reasoning") => {
                    if let Some(summary) = item.get("summary").and_then(Value::as_array) {
                        for part in summary {
                            if let Some(text) = part.get("text").and_then(Value::as_str) {
                                if !text.is_empty() {
                                    events.push(StreamEvent::ThinkingDelta {
                                        text: text.to_string(),
                                        signature: None,
                                    });
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        events
    }

    fn finish_reason(response: &Value, incomplete_event: bool) -> FinishReason {
        let incomplete_reason = response
            .pointer("/incomplete_details/reason")
            .and_then(Value::as_str);
        if incomplete_event
            || response.get("status").and_then(Value::as_str) == Some("incomplete")
            || incomplete_reason.is_some()
        {
            return match incomplete_reason {
                Some("max_output_tokens") => FinishReason::Length,
                Some("content_filter") => FinishReason::ContentFilter,
                Some(reason) => FinishReason::Other(reason.to_string()),
                None => FinishReason::Other("incomplete".to_string()),
            };
        }
        if response
            .get("output")
            .and_then(Value::as_array)
            .is_some_and(|items| {
                items
                    .iter()
                    .any(|item| item.get("type").and_then(Value::as_str) == Some("function_call"))
            })
        {
            FinishReason::ToolCalls
        } else {
            FinishReason::Stop
        }
    }

    fn terminal_events(response: &Value, incomplete_event: bool) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        if let Some(usage) = Self::usage(response) {
            events.push(StreamEvent::Usage(usage));
        }
        events.push(StreamEvent::Finish(Self::finish_reason(
            response,
            incomplete_event,
        )));
        events
    }

    fn response_events(response: &Value) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        if let Some(output) = response.get("output").and_then(Value::as_array) {
            events.extend(Self::output_events(output));
        }
        events.extend(Self::terminal_events(response, false));
        events
    }
}

impl Default for OpenAiResponsesAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Adapter for OpenAiResponsesAdapter {
    fn wire_format(&self) -> &'static str {
        "openai-responses"
    }

    fn build_url(&self, ctx: &UpstreamContext<'_>) -> Result<String, ProxyError> {
        Ok(format!(
            "{}/responses",
            ctx.provider.base_url.trim_end_matches('/')
        ))
    }

    fn apply_auth(
        &self,
        ctx: &UpstreamContext<'_>,
        req: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder, UpstreamFailure> {
        use crate::types::AuthScheme;
        Ok(match ctx.provider.auth() {
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
        let mut body = Map::new();
        body.insert("model".into(), json!(ctx.model.upstream_id));
        body.insert("input".into(), json!(Self::input_items(req)?));
        body.insert("stream".into(), json!(true));
        // Responses are deliberately not persisted server-side; Kinetix does
        // not implement response-object retrieval or previous_response_id.
        body.insert("store".into(), json!(false));

        if !req.system.is_empty() {
            body.insert("instructions".into(), json!(req.system.join("\n")));
        }
        if let Some(key) = req.extra.get("prompt_cache_key").and_then(Value::as_str) {
            body.insert("prompt_cache_key".into(), json!(key));
        }

        if let Some(tools) = Self::build_tools(req) {
            body.insert("tools".into(), tools);
        }
        if let Some(choice) = Self::build_tool_choice(req)? {
            body.insert("tool_choice".into(), choice);
        }

        Self::apply_model_request_policy(ctx, req, &mut body)?;
        Self::apply_model_extra_request(ctx, &mut body);

        Ok(Value::Object(body))
    }

    fn normalize_passthrough_body(
        &self,
        ctx: &UpstreamContext<'_>,
        req: &InternalRequest,
        body: &mut Value,
    ) -> Result<(), UpstreamFailure> {
        let Some(body) = body.as_object_mut() else {
            return Err(Self::bad_request(
                "OpenAI Responses passthrough body must be a JSON object",
            ));
        };
        Self::apply_model_request_policy(ctx, req, body)?;
        Self::apply_model_extra_request(ctx, body);
        Ok(())
    }

    fn classify_error(
        &self,
        status: u16,
        body: &str,
        headers: &reqwest::header::HeaderMap,
    ) -> UpstreamFailure {
        <crate::adapters::openai::OpenAiAdapter as Adapter>::classify_error(
            &crate::adapters::openai::OpenAiAdapter::new(),
            status,
            body,
            headers,
        )
    }

    fn parse_stream_chunk(&self, data: &str) -> Result<Vec<StreamEvent>, UpstreamFailure> {
        if data.trim() == "[DONE]" {
            return Ok(Vec::new());
        }
        let value: Value = serde_json::from_str(data)
            .map_err(|error| Self::failure(format!("invalid OpenAI Responses event: {error}")))?;
        let event_type = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match event_type {
            "response.output_text.delta" => Ok(value
                .get("delta")
                .and_then(Value::as_str)
                .filter(|delta| !delta.is_empty())
                .map(|delta| vec![StreamEvent::TextDelta(delta.to_string())])
                .unwrap_or_default()),
            "response.refusal.delta" => Ok(value
                .get("delta")
                .and_then(Value::as_str)
                .filter(|delta| !delta.is_empty())
                .map(|delta| vec![StreamEvent::RefusalDelta(delta.to_string())])
                .unwrap_or_default()),
            "response.reasoning_summary_text.delta" => Ok(value
                .get("delta")
                .and_then(Value::as_str)
                .filter(|delta| !delta.is_empty())
                .map(|delta| {
                    vec![StreamEvent::ThinkingDelta {
                        text: delta.to_string(),
                        signature: None,
                    }]
                })
                .unwrap_or_default()),
            "response.output_item.added" => {
                let Some(item) = value.get("item") else {
                    return Ok(Vec::new());
                };
                if item.get("type").and_then(Value::as_str) != Some("function_call") {
                    return Ok(Vec::new());
                }
                let (Some(name), Some(call_id)) = (
                    item.get("name").and_then(Value::as_str),
                    item.get("call_id").and_then(Value::as_str),
                ) else {
                    return Ok(Vec::new());
                };
                Ok(vec![StreamEvent::ToolCallStart {
                    index: value
                        .get("output_index")
                        .and_then(Value::as_u64)
                        .unwrap_or(0) as u32,
                    id: Some(call_id.to_string()),
                    name: name.to_string(),
                    signature: None,
                }])
            }
            "response.function_call_arguments.delta" => Ok(value
                .get("delta")
                .and_then(Value::as_str)
                .filter(|delta| !delta.is_empty())
                .map(|delta| {
                    vec![StreamEvent::ToolCallArgsDelta {
                        index: value
                            .get("output_index")
                            .and_then(Value::as_u64)
                            .unwrap_or(0) as u32,
                        args: delta.to_string(),
                    }]
                })
                .unwrap_or_default()),
            "response.completed" | "response.incomplete" => {
                let response = value.get("response").unwrap_or(&value);
                Ok(Self::terminal_events(
                    response,
                    event_type == "response.incomplete",
                ))
            }
            "response.failed" | "error" => {
                let message = value
                    .pointer("/response/error/message")
                    .or_else(|| value.pointer("/error/message"))
                    .and_then(Value::as_str)
                    .unwrap_or("OpenAI Responses request failed");
                Err(Self::failure(crate::crypto::redact(message)))
            }
            _ => Ok(Vec::new()),
        }
    }

    fn parse_full_response(&self, body: &Value) -> Result<Vec<StreamEvent>, UpstreamFailure> {
        let response = body.get("response").unwrap_or(body);
        if response.get("status").and_then(Value::as_str) == Some("failed") {
            let message = response
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("OpenAI Responses request failed");
            return Err(Self::failure(crate::crypto::redact(message)));
        }
        Ok(Self::response_events(response))
    }

    fn default_models_path(&self) -> &'static str {
        "/models"
    }

    fn parse_model_list(&self, body: &Value) -> Vec<DiscoveredModel> {
        crate::adapters::openai::OpenAiAdapter::new().parse_model_list(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{ModelRow, ProviderRow};
    use crate::types::ThinkingLevel;

    fn provider() -> ProviderRow {
        ProviderRow {
            id: "provider".into(),
            name: "provider".into(),
            base_url: "https://api.example.test/v1".into(),
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
        }
    }

    fn model() -> ModelRow {
        ModelRow {
            id: "model".into(),
            provider_id: "provider".into(),
            upstream_id: "upstream".into(),
            display_name: "model".into(),
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

    fn request() -> InternalRequest {
        InternalRequest {
            requested_model: "model".into(),
            system: vec!["be concise".into()],
            messages: vec![crate::types::Message {
                role: Role::User,
                parts: vec![Part::Text("hello".into())],
            }],
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
    fn responses_endpoint_and_request_dialect_are_consistent() {
        let provider = provider();
        let model = model();
        let ctx = UpstreamContext {
            provider: &provider,
            model: &model,
            account_id: None,
            credential: "secret".into(),
        };
        let adapter = OpenAiResponsesAdapter;
        assert_eq!(
            adapter.build_url(&ctx).unwrap(),
            "https://api.example.test/v1/responses"
        );
        let body = adapter.build_body(&ctx, &request()).unwrap();
        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert!(body.get("messages").is_none());
    }

    #[test]
    fn responses_reasoning_uses_nested_effort_field() {
        let provider = provider();
        let mut model = model();
        model.thinking_map = json!({
            "levels": {"high": "high"},
            "mode": "level",
            "level_field": "reasoning.effort"
        })
        .to_string();
        let ctx = UpstreamContext {
            provider: &provider,
            model: &model,
            account_id: None,
            credential: "secret".into(),
        };
        let mut req = request();
        req.thinking = Some(ThinkingLevel::High);
        let body = OpenAiResponsesAdapter.build_body(&ctx, &req).unwrap();
        assert_eq!(body.pointer("/reasoning/effort"), Some(&json!("high")));
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn responses_reasoning_disable_uses_nested_none_effort() {
        let provider = provider();
        let mut model = model();
        model.thinking_map = json!({
            "levels": {"off": "none"},
            "mode": "level",
            "level_field": "reasoning.effort"
        })
        .to_string();
        let ctx = UpstreamContext {
            provider: &provider,
            model: &model,
            account_id: None,
            credential: "secret".into(),
        };
        let mut req = request();
        req.thinking = Some(ThinkingLevel::Off);
        let body = OpenAiResponsesAdapter.build_body(&ctx, &req).unwrap();
        assert_eq!(body.pointer("/reasoning/effort"), Some(&json!("none")));
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn responses_stream_events_normalize_text_tool_calls_usage_and_finish() {
        let adapter = OpenAiResponsesAdapter;
        let text = adapter
            .parse_stream_chunk(r#"{"type":"response.output_text.delta","delta":"hello"}"#)
            .unwrap();
        assert!(matches!(&text[0], StreamEvent::TextDelta(value) if value == "hello"));
        let start = adapter
            .parse_stream_chunk(r#"{"type":"response.output_item.added","output_index":2,"item":{"type":"function_call","call_id":"call-1","name":"lookup"}}"#)
            .unwrap();
        assert!(
            matches!(&start[0], StreamEvent::ToolCallStart { index: 2, id: Some(id), name, .. } if id == "call-1" && name == "lookup")
        );
        let done = adapter
            .parse_stream_chunk(r#"{"type":"response.completed","response":{"output":[{"type":"function_call"}],"usage":{"input_tokens":7,"output_tokens":4,"input_tokens_details":{"cached_tokens":2,"cache_write_tokens":3},"output_tokens_details":{"reasoning_tokens":1}}}}"#)
            .unwrap();
        assert!(
            matches!(&done[0], StreamEvent::Usage(usage) if usage.input == Some(7) && usage.cached == Some(2) && usage.cache_write == Some(3) && usage.thinking == Some(1))
        );
        assert!(matches!(
            done.last(),
            Some(StreamEvent::Finish(FinishReason::ToolCalls))
        ));
    }

    #[test]
    fn passthrough_normalizes_max_completion_tokens_to_responses_token_limit() {
        let provider = provider();
        let mut model = model();
        model.max_output_tokens = Some(64);
        let ctx = UpstreamContext {
            provider: &provider,
            model: &model,
            account_id: None,
            credential: "secret".into(),
        };
        let mut req = request();
        req.params.max_tokens = Some(512);
        let mut body = json!({
            "max_tokens": 100,
            "max_completion_tokens": 200,
            "max_output_tokens": 300
        });

        OpenAiResponsesAdapter
            .normalize_passthrough_body(&ctx, &req, &mut body)
            .unwrap();

        assert_eq!(body["max_output_tokens"], 64);
        assert!(body.get("max_tokens").is_none());
        assert!(body.get("max_completion_tokens").is_none());
    }

    #[test]
    fn explicitly_dropped_unsupported_parameter_is_not_sent() {
        let provider = provider();
        let mut model = model();
        model.parameters = json!({
            "seed": {"supported": false, "policy": "drop"}
        })
        .to_string();
        let ctx = UpstreamContext {
            provider: &provider,
            model: &model,
            account_id: None,
            credential: "secret".into(),
        };
        let mut req = request();
        req.params.seed = Some(123);
        let body = OpenAiResponsesAdapter.build_body(&ctx, &req).unwrap();
        assert!(body.get("seed").is_none());
    }

    #[test]
    fn chat_only_parameters_fail_closed_instead_of_being_dropped() {
        let provider = provider();
        let model = model();
        let ctx = UpstreamContext {
            provider: &provider,
            model: &model,
            account_id: None,
            credential: "secret".into(),
        };
        let mut req = request();
        req.params.seed = Some(123);
        assert!(OpenAiResponsesAdapter.build_body(&ctx, &req).is_err());
    }

    #[test]
    fn configured_single_field_thinking_mapping_stays_scalar() {
        let provider = provider();
        let mut model = model();
        model.thinking_map = json!({
            "levels": {"high": "high"},
            "mode": "level",
            "level_field": "reasoning_effort"
        })
        .to_string();
        let ctx = UpstreamContext {
            provider: &provider,
            model: &model,
            account_id: None,
            credential: "secret".into(),
        };
        let mut req = request();
        req.thinking = Some(ThinkingLevel::High);
        let body = OpenAiResponsesAdapter.build_body(&ctx, &req).unwrap();
        assert_eq!(body["reasoning_effort"], "high");
        assert!(body.get("reasoning").is_none());
    }

    #[test]
    fn full_responses_refusal_content_is_a_typed_refusal_event() {
        let events = OpenAiResponsesAdapter
            .parse_full_response(&json!({
                "status": "completed",
                "output": [{
                    "type": "message",
                    "content": [{"type": "refusal", "refusal": "I cannot help with that."}]
                }],
                "usage": {"input_tokens": 5, "output_tokens": 6}
            }))
            .unwrap();
        assert!(events.iter().any(
            |event| matches!(event, StreamEvent::RefusalDelta(text) if text == "I cannot help with that.")
        ));
    }

    #[test]
    fn streamed_responses_refusal_delta_is_a_typed_refusal_event() {
        let events = OpenAiResponsesAdapter
            .parse_stream_chunk(
                r#"{"type":"response.refusal.delta","delta":"I cannot help with that."}"#,
            )
            .unwrap();
        assert!(matches!(
            events.as_slice(),
            [StreamEvent::RefusalDelta(text)] if text == "I cannot help with that."
        ));
    }

    #[test]
    fn incomplete_responses_events_collect_usage_and_classify_terminal_reason() {
        let max_tokens = OpenAiResponsesAdapter
            .parse_stream_chunk(
                r#"{"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"usage":{"input_tokens":7,"output_tokens":9}}}"#,
            )
            .unwrap();
        assert!(max_tokens.iter().any(
            |event| matches!(event, StreamEvent::Usage(usage) if usage.input == Some(7) && usage.output == Some(9))
        ));
        assert!(matches!(
            max_tokens.last(),
            Some(StreamEvent::Finish(FinishReason::Length))
        ));

        let content_filter = OpenAiResponsesAdapter
            .parse_full_response(&json!({
                "status": "incomplete",
                "incomplete_details": {"reason": "content_filter"},
                "output": []
            }))
            .unwrap();
        assert!(matches!(
            content_filter.last(),
            Some(StreamEvent::Finish(FinishReason::ContentFilter))
        ));

        let steered = OpenAiResponsesAdapter
            .parse_stream_chunk(
                r#"{"type":"response.incomplete","response":{"incomplete_details":{"reason":"steered"}}}"#,
            )
            .unwrap();
        assert!(matches!(
            steered.last(),
            Some(StreamEvent::Finish(FinishReason::Other(reason))) if reason == "steered"
        ));
    }
}
