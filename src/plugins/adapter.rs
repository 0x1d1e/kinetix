//! Host-side [`Adapter`] backed by a plugin (`plugin-adapter` world, §6.3, §7.1).
//!
//! The plugin adapter is a *pure translation library*: Kinetix core still owns
//! the outbound streaming HTTP send and the SSE framing, and calls this adapter
//! only for synchronous transforms (`build_url`/`apply_auth`/`build_body`) and
//! response parsing (`classify_error`/`parse_stream_chunk`/`parse_full_response`).
//! The `plugin-adapter` world therefore imports no network capability at all.
//!
//! Because the [`Adapter`] trait is synchronous while guest calls are async, the
//! bridge uses `tokio::task::block_in_place` + `Handle::block_on`. This is sound
//! only on the multi-threaded runtime (Kinetix's server); under a current-thread
//! runtime (some unit tests) `block_in_place` panics, so callers must build the
//! adapter on a multi-thread runtime.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::adapters::{
    parse_plugin_opaque_state_capability, Adapter, DiscoveredModel, OpaqueStateCapabilityKind,
    OpaqueStateCapabilityV1, OpaqueStatePlaceholderStrategy, UpstreamContext,
};
use crate::plugins::manager::PluginManager;
use crate::plugins::runtime::PluginFault;
use crate::types::{
    FailureKind, ImageData, InternalRequest, Part, ProxyError, StreamEvent, UpstreamFailure,
};

/// Register every provider adapter declared by one enabled plugin. This is the
/// single production/E2E registration path so manifest capability flags cannot
/// drift from runtime adapter behavior.
pub async fn register_declared_adapters(
    registry: &crate::adapters::AdapterRegistry,
    manager: PluginManager,
    plugin_id: &str,
    provides: &crate::plugins::types::Provides,
) -> anyhow::Result<()> {
    if provides.provider_adapters.is_empty() {
        return Ok(());
    }

    let adapter: Arc<dyn Adapter> = Arc::new(
        PluginAdapter::new(
            manager,
            plugin_id.to_string(),
            provides.thinking_translation,
        )
        .await?,
    );
    for name in &provides.provider_adapters {
        registry.register_plugin(format!("plugin:{plugin_id}/{name}"), adapter.clone());
    }
    registry.register_plugin(plugin_id.to_string(), adapter);
    Ok(())
}

/// A plugin-backed adapter bound to one `plugin:<id>/<capability>` reference.
pub struct PluginAdapter {
    manager: PluginManager,
    plugin_id: String,
    /// The wire-format name the guest reported (cached at construction).
    wire_format: &'static str,
    /// Explicit manifest opt-in: the guest consumes canonical thinking levels
    /// and is responsible for translating or rejecting them.
    thinking_translation: bool,
}

impl PluginAdapter {
    /// Probe the plugin's adapter world for its wire-format name, returning an
    /// error (so registration can fail closed) if the guest cannot answer.
    pub async fn new(
        manager: PluginManager,
        plugin_id: String,
        thinking_translation: bool,
    ) -> anyhow::Result<Self> {
        let wf = manager
            .adapter_wire_format(&plugin_id)
            .await
            .map_err(|e| anyhow::anyhow!("plugin adapter wire_format: {}", e.message()))?;
        let wire_format: &'static str = Box::leak(wf.into_boxed_str());
        Ok(PluginAdapter {
            manager,
            plugin_id,
            wire_format,
            thinking_translation,
        })
    }

    /// Run a guest adapter call from a synchronous context, bridging the async
    /// guest invocation onto the current multi-threaded runtime.
    fn block<T>(
        &self,
        fut: impl std::future::Future<Output = Result<T, PluginFault>>,
    ) -> Result<T, PluginFault> {
        tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut))
    }

    fn provider_json(ctx: &UpstreamContext<'_>) -> Result<String, String> {
        let (base_url, models_path) = ctx.provider.resolved_endpoint()?;
        let mut provider = serde_json::to_value(ctx.provider).map_err(|error| error.to_string())?;
        provider["base_url"] = serde_json::json!(base_url);
        provider["models_path"] = serde_json::json!(models_path);
        if let (Some(account_id), Some(object)) = (ctx.account_id, provider.as_object_mut()) {
            object.insert(
                "_kinetix".into(),
                serde_json::json!({ "account_id": account_id }),
            );
        }
        serde_json::to_string(&provider).map_err(|error| error.to_string())
    }

    fn model_json(ctx: &UpstreamContext<'_>) -> String {
        serde_json::to_string(ctx.model).unwrap_or_else(|_| "{}".to_string())
    }

    fn plugin_err(e: PluginFault) -> ProxyError {
        ProxyError::upstream(format!("plugin adapter: {}", e.message()))
    }

    fn plugin_failure(stage: &str, fault: PluginFault) -> UpstreamFailure {
        let kind = match &fault {
            PluginFault::Timeout => FailureKind::Timeout,
            PluginFault::PluginError {
                code, retryable, ..
            } => match code.as_str() {
                "invalid_request" | "unsupported" => FailureKind::BadRequest,
                "unauthorized" => FailureKind::AuthError,
                "protocol_error" | "plugin_internal" => FailureKind::PluginFailure,
                _ => FailureKind::parse(code).unwrap_or({
                    if *retryable {
                        FailureKind::PluginFailure
                    } else {
                        FailureKind::BadRequest
                    }
                }),
            },
            PluginFault::Trap(_)
            | PluginFault::PermissionDenied(_)
            | PluginFault::InvalidResult(_)
            | PluginFault::Internal(_) => FailureKind::PluginFailure,
            PluginFault::Cancelled => FailureKind::ClientCancelled,
        };
        let retry_after_secs = match &fault {
            PluginFault::PluginError { retry_after, .. } => *retry_after,
            _ => None,
        };
        UpstreamFailure {
            kind,
            status: None,
            retry_after_secs,
            message: format!("plugin adapter {stage}: {}", fault.message()),
            quota_reset_at: None,
        }
    }

    fn protocol_failure(stage: &str, message: impl Into<String>) -> UpstreamFailure {
        UpstreamFailure {
            kind: FailureKind::PluginFailure,
            status: None,
            retry_after_secs: None,
            message: format!("plugin adapter {stage}: {}", message.into()),
            quota_reset_at: None,
        }
    }

    fn opaque_state_descriptor_for(
        plugin_id: &str,
        model: &crate::db::ModelRow,
    ) -> Option<OpaqueStateCapabilityV1> {
        if model.opaque_state_plugin != plugin_id {
            return None;
        }
        let discovery: Value = serde_json::from_str(&model.discovery).ok()?;
        let descriptor = parse_plugin_opaque_state_capability(discovery.get("opaque_state")?)?;
        if descriptor.encoding_version != 1
            || !matches!(descriptor.family.as_str(), "gemini" | "claude")
        {
            return None;
        }
        Some(descriptor)
    }

    fn target_is_gemini_three(model: &crate::db::ModelRow) -> bool {
        if crate::adapters::gemini::is_gemini_three(&model.upstream_id) {
            return true;
        }
        serde_json::from_str::<Value>(&model.discovery)
            .ok()
            .and_then(|discovery| {
                discovery
                    .get("canonical_model_id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .is_some_and(|canonical| crate::adapters::gemini::is_gemini_three(&canonical))
    }

    fn opaque_state_target_for(
        plugin_id: &str,
        model: &crate::db::ModelRow,
    ) -> Option<crate::opaque_state::OpaqueStateTarget> {
        let descriptor = Self::opaque_state_descriptor_for(plugin_id, model)?;
        if descriptor.kind != OpaqueStateCapabilityKind::GeminiThoughtSignature {
            return None;
        }
        Some(crate::opaque_state::OpaqueStateTarget {
            kind: crate::opaque_state::OpaqueStateKind::GeminiThoughtSignature,
            provider_id: model.provider_id.clone(),
            family: descriptor.family,
            producer: format!(
                "plugin:{plugin_id}:gemini-thought-signature:v{}",
                descriptor.encoding_version
            ),
            model_id: model.upstream_id.clone(),
        })
    }

    fn opaque_state_placeholder_for(
        plugin_id: &str,
        model: &crate::db::ModelRow,
    ) -> Option<&'static str> {
        let descriptor = Self::opaque_state_descriptor_for(plugin_id, model)?;
        if descriptor.kind != OpaqueStateCapabilityKind::GeminiThoughtSignature
            || descriptor.family != "gemini"
            || descriptor.placeholder_strategy
                != Some(OpaqueStatePlaceholderStrategy::Gemini3SkipValidator)
            || !Self::target_is_gemini_three(model)
        {
            return None;
        }
        Some(crate::adapters::gemini::GEMINI_PLACEHOLDER_THOUGHT_SIGNATURE)
    }
}

#[async_trait]
impl Adapter for PluginAdapter {
    fn wire_format(&self) -> &'static str {
        self.wire_format
    }

    fn handles_thinking_translation(&self) -> bool {
        self.thinking_translation
    }

    fn opaque_state_target(
        &self,
        model: &crate::db::ModelRow,
    ) -> Option<crate::opaque_state::OpaqueStateTarget> {
        Self::opaque_state_target_for(&self.plugin_id, model)
    }

    fn opaque_state_placeholder(&self, model: &crate::db::ModelRow) -> Option<&'static str> {
        Self::opaque_state_placeholder_for(&self.plugin_id, model)
    }

    fn build_url(&self, ctx: &UpstreamContext<'_>) -> Result<String, ProxyError> {
        let (p, m) = (
            Self::provider_json(ctx).map_err(ProxyError::bad_request)?,
            Self::model_json(ctx),
        );
        self.block(self.manager.adapter_build_url(&self.plugin_id, &p, &m))
            .map_err(Self::plugin_err)
    }

    fn apply_auth(
        &self,
        ctx: &UpstreamContext<'_>,
        req: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder, UpstreamFailure> {
        if ctx.provider.auth() == crate::types::AuthScheme::None {
            return Ok(req);
        }
        let p = Self::provider_json(ctx)
            .map_err(|error| Self::protocol_failure("connection", error))?;
        let credential = ctx.credential.clone();
        let headers_json = self
            .block(self.manager.adapter_apply_auth(
                &self.plugin_id,
                &p,
                &credential,
                ctx.session_context,
            ))
            .map_err(|e| Self::plugin_failure("apply_auth", e))?;
        let headers: Vec<(String, String)> = serde_json::from_str(&headers_json).map_err(|e| {
            Self::protocol_failure("apply_auth", format!("invalid header JSON: {e}"))
        })?;

        let mut req = req;
        for (name, value) in headers {
            let name = reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|e| {
                Self::protocol_failure("apply_auth", format!("invalid header name: {e}"))
            })?;
            let value = reqwest::header::HeaderValue::from_str(&value).map_err(|e| {
                Self::protocol_failure("apply_auth", format!("invalid header value: {e}"))
            })?;
            req = req.header(name, value);
        }
        Ok(req)
    }

    fn build_body(
        &self,
        ctx: &UpstreamContext<'_>,
        req: &InternalRequest,
    ) -> Result<Value, UpstreamFailure> {
        let request_json = request_to_json(req);
        let (p, m) = (
            Self::provider_json(ctx)
                .map_err(|error| Self::protocol_failure("connection", error))?,
            Self::model_json(ctx),
        );
        let body = self
            .block(self.manager.adapter_build_body(
                &self.plugin_id,
                &request_json,
                &p,
                &m,
                ctx.session_context,
            ))
            .map_err(|e| Self::plugin_failure("build_body", e))?;
        let body: Value = serde_json::from_str(&body)
            .map_err(|e| Self::protocol_failure("build_body", format!("invalid JSON: {e}")))?;
        if body.is_null() {
            return Err(Self::protocol_failure(
                "build_body",
                "plugin returned a null request body",
            ));
        }
        Ok(body)
    }

    fn classify_error(
        &self,
        status: u16,
        body: &str,
        headers: &reqwest::header::HeaderMap,
    ) -> UpstreamFailure {
        let headers_json = headers_to_json(headers);
        match self.block(self.manager.adapter_classify_error(
            &self.plugin_id,
            status,
            body,
            &headers_json,
        )) {
            Ok(ev) => json_to_failure(&ev).unwrap_or_else(|| fallback_failure(status)),
            Err(_) => fallback_failure(status),
        }
    }

    fn parse_stream_chunk(&self, data: &str) -> Result<Vec<StreamEvent>, UpstreamFailure> {
        let out = self
            .block(
                self.manager
                    .adapter_parse_stream_chunk(&self.plugin_id, data),
            )
            .map_err(|e| UpstreamFailure {
                kind: FailureKind::PluginFailure,
                status: None,
                retry_after_secs: None,
                message: format!("plugin adapter parse_stream_chunk: {}", e.message()),
                quota_reset_at: None,
            })?;
        json_to_events(&out)
    }

    fn parse_full_response(&self, body: &Value) -> Result<Vec<StreamEvent>, UpstreamFailure> {
        let body_json = serde_json::to_string(body).unwrap_or_default();
        let out = self
            .block(
                self.manager
                    .adapter_parse_full_response(&self.plugin_id, &body_json),
            )
            .map_err(|e| UpstreamFailure {
                kind: FailureKind::PluginFailure,
                status: None,
                retry_after_secs: None,
                message: format!("plugin adapter parse_full_response: {}", e.message()),
                quota_reset_at: None,
            })?;
        json_to_events(&out)
    }

    fn default_models_path(&self) -> &'static str {
        // Model discovery is a separate capability (`model-source`); the plugin
        // adapter does not own the discovery path.
        "/models"
    }

    fn parse_model_list(&self, _body: &Value) -> Vec<DiscoveredModel> {
        Vec::new()
    }
}

// ---------------------------------------------------------------------------
// Canonical JSON encoding across the host boundary
// ---------------------------------------------------------------------------

/// Serialize an internal request into the JSON the guest adapter consumes.
/// `raw_body`/`extra` carry provider-specific fields; the model/params are
/// modelled explicitly.
pub fn request_to_json(req: &InternalRequest) -> String {
    let messages: Vec<Value> = req
        .messages
        .iter()
        .map(|m| {
            let parts: Vec<Value> = m
                .parts
                .iter()
                .map(|p| match p {
                    Part::Text(t) => json!({ "type": "text", "text": t }),
                    Part::Image(ImageData::Base64 { mime, data }) => {
                        json!({ "type": "image", "mime": mime, "data": data })
                    }
                    Part::Image(ImageData::Url(url)) => json!({ "type": "image_url", "url": url }),
                    Part::ToolCall {
                        id,
                        name,
                        arguments,
                        signature,
                    } => json!({
                        "type": "tool_call", "id": id, "name": name,
                        "arguments": arguments, "signature": signature
                    }),
                    Part::ToolResult {
                        tool_call_id,
                        name,
                        content,
                        is_error,
                    } => json!({
                        "type": "tool_result", "tool_call_id": tool_call_id,
                        "name": name, "content": content, "is_error": is_error
                    }),
                    Part::Thinking { text, signature } => {
                        json!({ "type": "thinking", "text": text, "signature": signature })
                    }
                    Part::RedactedThinking { data } => {
                        json!({ "type": "redacted_thinking", "data": data })
                    }
                })
                .collect();
            json!({ "role": format!("{:?}", m.role).to_lowercase(), "parts": parts })
        })
        .collect();
    let tools: Vec<Value> = req
        .tools
        .iter()
        .map(
            |t| json!({ "name": t.name, "description": t.description, "parameters": t.parameters }),
        )
        .collect();
    let tool_choice = {
        let (mode, name) = match req.tool_choice {
            Some(crate::types::ToolChoice::None) => ("none", None),
            Some(crate::types::ToolChoice::Required) => ("required", None),
            Some(crate::types::ToolChoice::Specific) => {
                ("specific", req.tool_choice_name.as_deref())
            }
            Some(crate::types::ToolChoice::Auto) | None => ("auto", None),
        };
        json!({ "mode": mode, "name": name })
    };
    let thinking = req.thinking.map(|level| json!({ "level": level.as_key() }));
    let value = json!({
        "schema": "kinetix.plugin.request",
        "schema_version": 1,
        "requested_model": req.requested_model,
        "system": req.system,
        "messages": messages,
        "tools": tools,
        "tool_choice": tool_choice,
        "stream": req.stream,
        "include_usage": req.include_usage,
        "temperature": req.params.temperature,
        "top_p": req.params.top_p,
        "top_k": req.params.top_k,
        "max_tokens": req.params.max_tokens,
        "stop": req.params.stop,
        "seed": req.params.seed,
        "presence_penalty": req.params.presence_penalty,
        "frequency_penalty": req.params.frequency_penalty,
        "thinking": thinking,
        "extra": req.extra,
    });
    serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string())
}

fn headers_to_json(headers: &reqwest::header::HeaderMap) -> String {
    let mut map = serde_json::Map::new();
    for (name, value) in headers.iter() {
        if let Ok(v) = value.to_str() {
            map.insert(name.as_str().to_string(), json!(v));
        }
    }
    serde_json::to_string(&Value::Object(map)).unwrap_or_else(|_| "{}".to_string())
}

/// Encode/decode the versioned canonical plugin response contract.
pub use crate::plugins::response_contract::{events_to_json, json_to_events};

/// Encode classified failure evidence for the guest.
pub fn failure_to_json(f: &UpstreamFailure) -> String {
    let value = json!({
        "kind": failure_kind_str(f.kind),
        "status": f.status,
        "retry_after_secs": f.retry_after_secs,
        "message": f.message,
        "quota_reset_at": f.quota_reset_at.map(|d| d.to_rfc3339()),
    });
    serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string())
}

/// Decode classified failure evidence from the guest.
pub fn json_to_failure(s: &str) -> Option<UpstreamFailure> {
    let v: Value = serde_json::from_str(s).ok()?;
    let kind = FailureKind::parse(v.get("kind").and_then(Value::as_str)?)?;
    Some(UpstreamFailure {
        kind,
        status: v.get("status").and_then(|x| x.as_u64()).map(|x| x as u16),
        retry_after_secs: v.get("retry_after_secs").and_then(|x| x.as_u64()),
        message: v
            .get("message")
            .and_then(|x| x.as_str())
            .unwrap_or("upstream error")
            .to_string(),
        quota_reset_at: v
            .get("quota_reset_at")
            .and_then(|x| x.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&chrono::Utc)),
    })
}

fn failure_kind_str(kind: FailureKind) -> &'static str {
    kind.as_str()
}

/// Status-only classification used when the guest cannot be reached.
fn fallback_failure(status: u16) -> UpstreamFailure {
    let kind = match status {
        401 => FailureKind::AuthError,
        403 | 404 => FailureKind::TargetError,
        429 => FailureKind::RateLimit,
        400..=499 => FailureKind::BadRequest,
        _ => FailureKind::ServerError,
    };
    UpstreamFailure {
        kind,
        status: Some(status),
        retry_after_secs: None,
        message: format!("upstream returned {status}"),
        quota_reset_at: None,
    }
}

#[allow(dead_code)]
fn _assert_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Arc<PluginAdapter>>();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FinishReason, TokenUsage};

    #[test]
    fn stream_events_round_trip() {
        let events = vec![
            StreamEvent::Start {
                upstream_request_id: Some("req_1".into()),
            },
            StreamEvent::ThinkingDelta {
                block_index: None,
                text: "hmm".into(),
                signature: Some("sig".into()),
            },
            StreamEvent::TextDelta("hi".into()),
            StreamEvent::ToolCallStart {
                index: 2,
                id: Some("call_1".into()),
                name: "fn".into(),
                signature: None,
            },
            StreamEvent::ToolCallArgsDelta {
                index: 2,
                args: "{}".into(),
            },
            StreamEvent::Usage(TokenUsage {
                input: Some(10),
                output: Some(4),
                cached: None,
                cache_write: Some(1),
                thinking: Some(2),
            }),
            StreamEvent::Finish(FinishReason::ToolCalls),
        ];
        let json = events_to_json(&events);
        let back = json_to_events(&json).expect("decode");
        assert_eq!(back.len(), events.len());
        match &back[0] {
            StreamEvent::Start {
                upstream_request_id,
            } => assert_eq!(upstream_request_id.as_deref(), Some("req_1")),
            _ => panic!("expected start"),
        }
        match &back[5] {
            StreamEvent::Usage(u) => {
                assert_eq!(u.input, Some(10));
                assert_eq!(u.cache_write, Some(1));
                assert_eq!(u.thinking, Some(2));
            }
            _ => panic!("expected usage"),
        }
        assert!(matches!(
            back[6],
            StreamEvent::Finish(FinishReason::ToolCalls)
        ));
    }

    #[test]
    fn invalid_event_json_is_rejected() {
        assert!(json_to_events("not json").is_err());
        assert!(json_to_events("{\"type\":\"bogus\"}").is_err());
        // A bare object (not an array) is rejected.
        assert!(json_to_events("{\"type\":\"text_delta\"}").is_err());
    }

    #[test]
    fn failure_evidence_round_trip() {
        let f = UpstreamFailure {
            kind: FailureKind::RateLimit,
            status: Some(429),
            retry_after_secs: Some(7),
            message: "slow down".into(),
            quota_reset_at: None,
        };
        let back = json_to_failure(&failure_to_json(&f)).expect("decode");
        assert_eq!(back.kind, FailureKind::RateLimit);
        assert_eq!(back.status, Some(429));
        assert_eq!(back.retry_after_secs, Some(7));
        assert_eq!(back.message, "slow down");
    }

    #[test]
    fn fallback_classification_maps_status() {
        assert_eq!(fallback_failure(401).kind, FailureKind::AuthError);
        assert_eq!(fallback_failure(429).kind, FailureKind::RateLimit);
        assert_eq!(fallback_failure(400).kind, FailureKind::BadRequest);
        assert_eq!(fallback_failure(503).kind, FailureKind::ServerError);
    }

    #[test]
    fn plugin_failure_codec_round_trips_the_shared_taxonomy() {
        for kind in [
            FailureKind::RateLimit,
            FailureKind::QuotaExhausted,
            FailureKind::AuthError,
            FailureKind::TargetError,
            FailureKind::ServerError,
            FailureKind::ConnectionError,
            FailureKind::Timeout,
            FailureKind::BadRequest,
            FailureKind::MalformedUpstream,
            FailureKind::PluginFailure,
            FailureKind::PolicyRejected,
            FailureKind::ClientCancelled,
        ] {
            let failure = UpstreamFailure {
                kind,
                status: None,
                retry_after_secs: None,
                message: "failure".into(),
                quota_reset_at: None,
            };
            let round_trip = json_to_failure(&failure_to_json(&failure)).unwrap();
            assert_eq!(round_trip.kind, kind);
        }
    }

    #[test]
    fn request_json_exposes_versioned_canonical_contract() {
        use crate::types::{
            InternalRequest, Message, Part, Role, SamplingParams, ThinkingLevel, ToolChoice,
        };
        let mut extra = serde_json::Map::new();
        extra.insert("provider_hint".into(), json!("value"));
        let req = InternalRequest {
            requested_model: "gemini-3".into(),
            system: vec!["sys".into()],
            messages: vec![Message {
                role: Role::User,
                parts: vec![Part::Text("hello".into())],
            }],
            tools: Vec::new(),
            tool_choice: Some(ToolChoice::Specific),
            tool_choice_name: Some("read".into()),
            params: SamplingParams {
                temperature: Some(0.2),
                top_p: Some(0.9),
                top_k: Some(20.0),
                max_tokens: Some(512),
                stop: vec!["END".into()],
                seed: Some(7),
                presence_penalty: Some(0.3),
                frequency_penalty: Some(0.4),
            },
            stream: true,
            include_usage: true,
            thinking: Some(ThinkingLevel::High),
            extra,
            raw_body: None,
        };
        let v: Value = serde_json::from_str(&request_to_json(&req)).unwrap();
        assert_eq!(v["schema"], "kinetix.plugin.request");
        assert_eq!(v["schema_version"], 1);
        assert_eq!(v["requested_model"], "gemini-3");
        assert_eq!(v["messages"][0]["role"], "user");
        assert_eq!(v["messages"][0]["parts"][0]["type"], "text");
        assert_eq!(v["tool_choice"]["mode"], "specific");
        assert_eq!(v["tool_choice"]["name"], "read");
        assert_eq!(v["thinking"]["level"], "high");
        assert_eq!(v["presence_penalty"], 0.3);
        assert_eq!(v["frequency_penalty"], 0.4);
        assert_eq!(v["include_usage"], true);
        assert_eq!(v["extra"]["provider_hint"], "value");
    }

    fn model_with_opaque_state(
        plugin_id: &str,
        upstream_id: &str,
        family: &str,
        placeholder: bool,
    ) -> crate::db::ModelRow {
        crate::db::ModelRow {
            id: "model_test".into(),
            provider_id: "provider_test".into(),
            upstream_id: upstream_id.into(),
            display_name: upstream_id.into(),
            enabled: 1,
            context_window: None,
            max_output_tokens: None,
            capabilities: "{}".into(),
            prices: "{}".into(),
            parameters: "{}".into(),
            thinking_map: "{}".into(),
            extra_request: "{}".into(),
            discovery: serde_json::json!({
                "canonical_model_id": if family == "gemini" {
                    serde_json::Value::String("google/gemini-3.8-flash".into())
                } else {
                    serde_json::Value::Null
                },
                "opaque_state": {
                    "kind": "gemini_thought_signature",
                    "family": family,
                    "encoding_version": 1,
                    "placeholder_strategy": if placeholder {
                        serde_json::Value::String("gemini3_skip_validator".into())
                    } else {
                        serde_json::Value::Null
                    }
                }
            })
            .to_string(),
            created_at: "now".into(),
            opaque_state_plugin: plugin_id.into(),
        }
    }

    #[test]
    fn plugin_opaque_state_descriptor_requires_matching_provenance() {
        let row = model_with_opaque_state("plugin.test", "gemini-3.8-flash-high", "gemini", true);
        let descriptor =
            PluginAdapter::opaque_state_descriptor_for("plugin.test", &row).expect("descriptor");
        assert_eq!(descriptor.family, "gemini");
        assert_eq!(descriptor.encoding_version, 1);

        let mut no_provenance = row.clone();
        no_provenance.opaque_state_plugin.clear();
        assert!(
            PluginAdapter::opaque_state_descriptor_for("plugin.test", &no_provenance).is_none()
        );
        assert!(PluginAdapter::opaque_state_descriptor_for("plugin.other", &row).is_none());

        let mut malformed = row;
        malformed.discovery = serde_json::json!({
            "opaque_state": {
                "kind": "gemini_thought_signature",
                "family": "gemini",
                "encoding_version": 0
            }
        })
        .to_string();
        assert!(PluginAdapter::opaque_state_descriptor_for("plugin.test", &malformed).is_none());
    }

    #[test]
    fn plugin_opaque_state_target_uses_exact_model_and_typed_producer() {
        let row = model_with_opaque_state("plugin.test", "gemini-3.8-flash-high", "gemini", true);
        let target =
            PluginAdapter::opaque_state_target_for("plugin.test", &row).expect("opaque target");
        assert_eq!(
            target.kind,
            crate::opaque_state::OpaqueStateKind::GeminiThoughtSignature
        );
        assert_eq!(target.provider_id, "provider_test");
        assert_eq!(target.family, "gemini");
        assert_eq!(
            target.producer,
            "plugin:plugin.test:gemini-thought-signature:v1"
        );
        assert_eq!(target.model_id, "gemini-3.8-flash-high");

        assert_eq!(
            PluginAdapter::opaque_state_placeholder_for("plugin.test", &row),
            Some(crate::adapters::gemini::GEMINI_PLACEHOLDER_THOUGHT_SIGNATURE)
        );

        let claude =
            model_with_opaque_state("plugin.test", "claude-opus-4-6-thinking", "claude", false);
        let target =
            PluginAdapter::opaque_state_target_for("plugin.test", &claude).expect("claude target");
        assert_eq!(target.family, "claude");
        assert_eq!(target.model_id, "claude-opus-4-6-thinking");
        assert_eq!(
            PluginAdapter::opaque_state_placeholder_for("plugin.test", &claude),
            None
        );
    }

    #[test]
    fn plugin_gemini_three_gate_accepts_exact_ids_and_resolved_aliases_only() {
        let exact = model_with_opaque_state("plugin.test", "gemini-3.8-flash-high", "gemini", true);
        assert!(PluginAdapter::target_is_gemini_three(&exact));

        let alias = model_with_opaque_state("plugin.test", "gemini-pro-agent", "gemini", true);
        assert!(PluginAdapter::target_is_gemini_three(&alias));

        let older = model_with_opaque_state("plugin.test", "gemini-2.5-flash", "gemini", true);
        let mut older = older;
        older.discovery = serde_json::json!({
            "canonical_model_id": "google/gemini-2.5-flash",
            "opaque_state": {
                "kind": "gemini_thought_signature",
                "family": "gemini",
                "encoding_version": 1,
                "placeholder_strategy": "gemini3_skip_validator"
            }
        })
        .to_string();
        assert!(!PluginAdapter::target_is_gemini_three(&older));

        let claude =
            model_with_opaque_state("plugin.test", "claude-opus-4-6-thinking", "claude", false);
        assert!(!PluginAdapter::target_is_gemini_three(&claude));
    }

    #[test]
    fn plugin_faults_preserve_fallback_classification() {
        let retryable = PluginAdapter::plugin_failure(
            "build_body",
            PluginFault::PluginError {
                code: "server_error".into(),
                message: "temporary".into(),
                retryable: true,
                retry_after: Some(5),
            },
        );
        assert_eq!(retryable.kind, FailureKind::ServerError);

        let request_error = PluginAdapter::plugin_failure(
            "build_body",
            PluginFault::PluginError {
                code: "bad_request".into(),
                message: "bad input".into(),
                retryable: false,
                retry_after: None,
            },
        );
        assert_eq!(request_error.kind, FailureKind::BadRequest);

        let unknown_non_retryable = PluginAdapter::plugin_failure(
            "build_body",
            PluginFault::PluginError {
                code: "policy_rejected".into(),
                message: "not allowed".into(),
                retryable: false,
                retry_after: None,
            },
        );
        assert_eq!(unknown_non_retryable.kind, FailureKind::PolicyRejected);

        let cancelled = PluginAdapter::plugin_failure("parse_stream_chunk", PluginFault::Cancelled);
        assert_eq!(cancelled.kind, FailureKind::ClientCancelled);

        let timeout = PluginAdapter::plugin_failure("apply_auth", PluginFault::Timeout);
        assert_eq!(timeout.kind, FailureKind::Timeout);
    }
}
