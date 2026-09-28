//! Gemini `generateContent` outbound adapter.
//!
//! Chosen purely by the provider's configured `wire_format = "gemini"`; nothing
//! vendor-specific leaks into the core. URLs, auth scheme, and headers all come
//! from the provider config the admin entered.

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::adapters::{Adapter, DiscoveredModel, UpstreamContext};
use crate::types::{
    FailureKind, FinishReason, ImageData, InternalRequest, Message, Part, ProxyError, Role,
    SamplingParams, StreamEvent, TokenUsage, ToolChoice, UpstreamFailure,
};

/// Google's documented sentinel for replaying a historical `functionCall` whose
/// real `thoughtSignature` is not available to this request (for example a
/// conversation transferred from a different Gemini model). Gemini accepts it
/// in place of a real signature and skips signature validation, which keeps the
/// call in the history instead of failing with HTTP 400. It is *not* real
/// reasoning state; passing it trades reasoning continuity for a working turn.
/// See <https://ai.google.dev/gemini-api/docs/thought-signatures>.
pub const GEMINI_PLACEHOLDER_THOUGHT_SIGNATURE: &str = "skip_thought_signature_validator";

pub struct GeminiAdapter;

impl GeminiAdapter {
    pub fn new() -> Self {
        GeminiAdapter
    }

    fn role_str(role: Role) -> &'static str {
        match role {
            Role::Assistant => "model",
            // System is hoisted to systemInstruction; tool results ride as user parts.
            Role::System | Role::User | Role::Tool => "user",
        }
    }

    fn encode_parts(parts: &[Part], out: &mut Vec<Value>) {
        for p in parts {
            match p {
                Part::Text(t) => out.push(json!({ "text": t })),
                Part::Thinking { text, signature } => {
                    // Preserve Gemini reasoning state when the target can carry
                    // it. Signature-only parts are valid continuation state.
                    if !text.is_empty() || signature.is_some() {
                        let mut part = json!({ "text": text, "thought": true });
                        if let Some(sig) = signature {
                            part["thoughtSignature"] = json!(sig);
                        }
                        out.push(part);
                    }
                }
                Part::Image(img) => match img {
                    ImageData::Base64 { mime, data } => {
                        out.push(json!({ "inlineData": { "mimeType": mime, "data": data } }));
                    }
                    ImageData::Url(url) => {
                        out.push(json!({ "fileData": { "fileUri": url } }));
                    }
                },
                Part::ToolCall {
                    name,
                    arguments,
                    signature,
                    ..
                } => {
                    let args: Value = serde_json::from_str(arguments).unwrap_or(json!({}));
                    let mut part = json!({ "functionCall": { "name": name, "args": args } });
                    if let Some(sig) = signature {
                        part["thoughtSignature"] = json!(sig);
                    }
                    out.push(part);
                }
                Part::ToolResult {
                    name,
                    content,
                    is_error,
                    ..
                } => {
                    let name = name.clone().unwrap_or_else(|| "tool".to_string());
                    // Gemini expects a structured response object.
                    let response = if *is_error {
                        json!({ "error": content })
                    } else {
                        match serde_json::from_str::<Value>(content) {
                            Ok(Value::Object(_)) => serde_json::from_str(content).unwrap(),
                            _ => json!({ "result": content }),
                        }
                    };
                    out.push(json!({
                        "functionResponse": { "name": name, "response": response }
                    }));
                }
            }
        }
    }

    fn build_contents(req: &InternalRequest) -> Vec<Value> {
        let mut contents = Vec::new();
        for m in &req.messages {
            let mut parts = Vec::new();
            Self::encode_parts(&m.parts, &mut parts);
            if parts.is_empty() {
                continue;
            }
            contents.push(json!({ "role": Self::role_str(m.role), "parts": parts }));
        }
        contents
    }

    fn build_generation_config(ctx: &UpstreamContext<'_>, req: &InternalRequest) -> Value {
        let model = ctx.model;
        let params = model.params();
        let mut cfg = serde_json::Map::new();

        let put_number = |policy_key: &str,
                          wire_key: &str,
                          client_val: Option<f64>,
                          cfg: &mut serde_json::Map<String, Value>| {
            let spec = params.get(policy_key);
            if let Some(v) = client_val {
                let (value, keep) = apply_param_spec(spec, v);
                if keep {
                    cfg.insert(wire_key.to_string(), json!(value));
                }
            } else if let Some(spec) = spec {
                // FR-10.6: policy lookup always uses canonical Kinetix names;
                // only the emitted wire key is provider-specific.
                if spec.supported {
                    if let Some(d) = spec.default {
                        cfg.insert(wire_key.to_string(), json!(d));
                    }
                }
            }
        };

        put_number(
            "temperature",
            "temperature",
            req.params.temperature,
            &mut cfg,
        );
        put_number("top_p", "topP", req.params.top_p, &mut cfg);
        put_number("top_k", "topK", req.params.top_k, &mut cfg);

        // max output tokens: clamp to model max if configured.
        if let Some(mt) = req.params.max_tokens {
            let mut v = mt as f64;
            if let Some(max) = model.max_output_tokens {
                v = v.min(max as f64);
            }
            cfg.insert("maxOutputTokens".to_string(), json!(v as i64));
        }
        if !req.params.stop.is_empty() {
            cfg.insert("stopSequences".to_string(), json!(req.params.stop));
        }
        if let Some(seed) = req.params.seed {
            cfg.insert("seed".to_string(), json!(seed));
        }

        // Thinking mapping (FR-10.7): canonical level -> upstream fields.
        if let Some(level) = req.thinking {
            let tmap = model.thinking();
            let key = level.as_key();
            if let Some(v) = tmap.levels.get(key) {
                // Value may be a full object to merge, or a scalar under budget_field.
                if let Some(obj) = v.as_object() {
                    for (k, val) in obj {
                        insert_dotted(&mut cfg, k, val.clone());
                    }
                } else if let Some(field) = tmap.scalar_field() {
                    insert_dotted(&mut cfg, field, v.clone());
                }
            }
        }

        Value::Object(cfg)
    }

    fn build_tools(req: &InternalRequest) -> Result<Option<Value>, UpstreamFailure> {
        if req.tools.is_empty() {
            return Ok(None);
        }
        let decls: Result<Vec<Value>, UpstreamFailure> = req
            .tools
            .iter()
            .map(|tool| {
                let mut declaration = json!({ "name": tool.name });
                if let Some(description) = &tool.description {
                    declaration["description"] = json!(description);
                }
                if !tool.parameters.is_null() {
                    declaration["parametersJsonSchema"] =
                        sanitize_schema(&tool.parameters, &format!("tool '{}'", tool.name))?;
                }
                Ok(declaration)
            })
            .collect();
        Ok(Some(json!([{ "functionDeclarations": decls? }])))
    }

    fn build_tool_config(req: &InternalRequest) -> Option<Value> {
        let mode = match req.tool_choice {
            Some(ToolChoice::Auto) | None => {
                if req.tools.is_empty() {
                    return None;
                }
                "AUTO"
            }
            Some(ToolChoice::None) => "NONE",
            Some(ToolChoice::Required) => "ANY",
            Some(ToolChoice::Specific) => "ANY",
        };
        let mut cfg = json!({ "mode": mode });
        if let Some(name) = &req.tool_choice_name {
            cfg["allowedFunctionNames"] = json!([name]);
        }
        Some(json!({ "functionCallingConfig": cfg }))
    }
}

impl Default for GeminiAdapter {
    fn default() -> Self {
        Self::new()
    }
}

/// Apply the admin's parameter policy (FR-10.6).
/// Returns (value, keep). `drop`/out-of-range reject handled by caller via error.
fn apply_param_spec(spec: Option<&crate::types::ParamSpec>, v: f64) -> (f64, bool) {
    let Some(spec) = spec else {
        // Unconfigured parameter: forward unchanged.
        return (v, true);
    };
    if !spec.supported {
        return (v, false); // drop
    }
    let mut value = v;
    if let Some(min) = spec.min {
        if value < min {
            value = min;
        }
    }
    if let Some(max) = spec.max {
        if value > max {
            value = max;
        }
    }
    (value, true)
}

/// Insert a value at a dotted path (`a.b.c`) inside a JSON object, creating
/// intermediate objects as needed.
fn insert_dotted(obj: &mut serde_json::Map<String, Value>, path: &str, value: Value) {
    let parts: Vec<&str> = path.split('.').collect();
    insert_rec(obj, &parts, value);
}

fn insert_rec(obj: &mut serde_json::Map<String, Value>, path: &[&str], value: Value) {
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
        insert_rec(map, &path[1..], value);
    }
}

/// Return a request-local failure for a tool schema Kinetix cannot translate
/// to Gemini without weakening its validation semantics.
fn schema_error(path: &str, message: impl Into<String>) -> UpstreamFailure {
    UpstreamFailure {
        kind: FailureKind::BadRequest,
        status: None,
        retry_after_secs: None,
        message: format!("Gemini tool schema at {path}: {}", message.into()),
        quota_reset_at: None,
    }
}

fn sanitize_schema_list(value: &Value, path: &str) -> Result<Vec<Value>, UpstreamFailure> {
    value
        .as_array()
        .ok_or_else(|| schema_error(path, "expected an array of schemas"))?
        .iter()
        .enumerate()
        .map(|(index, schema)| sanitize_schema_node(schema, &format!("{path}[{index}]")))
        .collect()
}

fn merge_schema_maps(
    target: &mut serde_json::Map<String, Value>,
    incoming: &serde_json::Map<String, Value>,
    path: &str,
) -> Result<(), UpstreamFailure> {
    for (key, value) in incoming {
        match key.as_str() {
            "properties" | "$defs" => {
                let source = value
                    .as_object()
                    .ok_or_else(|| schema_error(&format!("{path}.{key}"), "must be an object"))?;
                let destination = target
                    .entry(key.clone())
                    .or_insert_with(|| json!({}))
                    .as_object_mut()
                    .expect("schema map initialized as object");
                for (name, schema) in source {
                    if let Some(previous) = destination.get(name) {
                        if previous != schema {
                            return Err(schema_error(
                                &format!("{path}.{key}.{name}"),
                                "conflicting allOf schemas cannot be represented safely",
                            ));
                        }
                    } else {
                        destination.insert(name.clone(), schema.clone());
                    }
                }
            }
            "required" => {
                let source = value
                    .as_array()
                    .ok_or_else(|| schema_error(&format!("{path}.required"), "must be an array"))?;
                let destination = target
                    .entry("required".to_string())
                    .or_insert_with(|| json!([]))
                    .as_array_mut()
                    .expect("required initialized as array");
                for item in source {
                    if !destination.contains(item) {
                        destination.push(item.clone());
                    }
                }
            }
            "title" | "description" => {
                target.entry(key.clone()).or_insert_with(|| value.clone());
            }
            _ => {
                if let Some(previous) = target.get(key) {
                    if previous != value {
                        return Err(schema_error(
                            &format!("{path}.{key}"),
                            "conflicting allOf constraints cannot be represented safely",
                        ));
                    }
                } else {
                    target.insert(key.clone(), value.clone());
                }
            }
        }
    }
    Ok(())
}

fn add_nullable_type(
    schema: &mut serde_json::Map<String, Value>,
    path: &str,
) -> Result<(), UpstreamFailure> {
    if let Some(kind) = schema.get_mut("type") {
        match kind {
            Value::String(existing) if existing != "null" => {
                *kind = json!([existing.clone(), "null"]);
                return Ok(());
            }
            Value::Array(types) => {
                if !types.iter().any(|value| value.as_str() == Some("null")) {
                    types.push(json!("null"));
                }
                return Ok(());
            }
            Value::String(_) => return Ok(()),
            _ => {
                return Err(schema_error(
                    path,
                    "nullable requires a string or array type",
                ))
            }
        }
    }

    if let Some(any_of) = schema.get_mut("anyOf").and_then(Value::as_array_mut) {
        if !any_of
            .iter()
            .any(|branch| branch.get("type").and_then(Value::as_str) == Some("null"))
        {
            any_of.push(json!({ "type": "null" }));
        }
        return Ok(());
    }

    Err(schema_error(
        path,
        "nullable without type or anyOf cannot be normalized safely",
    ))
}

/// Normalize JSON Schema for Gemini `parametersJsonSchema`.
///
/// Supported keywords are explicitly enumerated. Safe compatibility transforms:
/// - legacy `definitions` -> `$defs` and matching local refs
/// - `const` -> singleton `enum`
/// - `oneOf` -> `anyOf` (Gemini treats them equivalently)
/// - draft-07 tuple `items: []` -> `prefixItems`
/// - OpenAPI `nullable` -> JSON Schema null type
/// - lossless object-style `allOf` merges
///
/// Documented-supported validation keywords are preserved. Unknown validation
/// keywords fail closed instead of being forwarded to Google or silently discarded.
fn sanitize_schema(schema: &Value, root_path: &str) -> Result<Value, UpstreamFailure> {
    sanitize_schema_node(schema, root_path)
}

fn sanitize_schema_node(node: &Value, path: &str) -> Result<Value, UpstreamFailure> {
    let map = node
        .as_object()
        .ok_or_else(|| schema_error(path, "schema nodes must be JSON objects"))?;
    let mut out = serde_json::Map::new();
    let mut nullable = false;
    let mut const_value: Option<Value> = None;
    let mut all_of: Option<&Value> = None;

    for (key, value) in map {
        match key.as_str() {
            "$schema" | "$comment" | "strict" | "default" | "examples" | "example"
            | "deprecated" | "readOnly" | "writeOnly" => {}

            "definitions" | "$defs" => {
                let definitions = value
                    .as_object()
                    .ok_or_else(|| schema_error(&format!("{path}.{key}"), "must be an object"))?;
                let mut sanitized = serde_json::Map::new();
                for (name, schema) in definitions {
                    sanitized.insert(
                        name.clone(),
                        sanitize_schema_node(schema, &format!("{path}.{key}.{name}"))?,
                    );
                }
                let incoming =
                    serde_json::Map::from_iter([("$defs".to_string(), Value::Object(sanitized))]);
                merge_schema_maps(&mut out, &incoming, path)?;
            }

            "$ref" => {
                let reference = value
                    .as_str()
                    .ok_or_else(|| schema_error(&format!("{path}.$ref"), "must be a string"))?;
                let reference = reference
                    .strip_prefix("#/definitions/")
                    .map(|suffix| format!("#/$defs/{suffix}"))
                    .unwrap_or_else(|| reference.to_string());
                out.insert("$ref".to_string(), json!(reference));
            }

            "properties" => {
                let properties = value.as_object().ok_or_else(|| {
                    schema_error(&format!("{path}.properties"), "must be an object")
                })?;
                let mut sanitized = serde_json::Map::new();
                for (name, schema) in properties {
                    sanitized.insert(
                        name.clone(),
                        sanitize_schema_node(schema, &format!("{path}.properties.{name}"))?,
                    );
                }
                out.insert("properties".to_string(), Value::Object(sanitized));
            }

            "items" => {
                if let Some(items) = value.as_array() {
                    let sanitized: Result<Vec<_>, _> = items
                        .iter()
                        .enumerate()
                        .map(|(index, schema)| {
                            sanitize_schema_node(schema, &format!("{path}.items[{index}]"))
                        })
                        .collect();
                    out.insert("prefixItems".to_string(), Value::Array(sanitized?));
                } else {
                    out.insert(
                        "items".to_string(),
                        sanitize_schema_node(value, &format!("{path}.items"))?,
                    );
                }
            }

            "prefixItems" | "anyOf" => {
                out.insert(
                    key.clone(),
                    Value::Array(sanitize_schema_list(value, &format!("{path}.{key}"))?),
                );
            }

            "oneOf" => {
                if out.contains_key("anyOf") {
                    return Err(schema_error(
                        &format!("{path}.oneOf"),
                        "cannot combine oneOf and anyOf safely",
                    ));
                }
                out.insert(
                    "anyOf".to_string(),
                    Value::Array(sanitize_schema_list(value, &format!("{path}.oneOf"))?),
                );
            }

            "allOf" => all_of = Some(value),
            "const" => const_value = Some(value.clone()),

            "additionalProperties" => {
                let normalized = match value {
                    Value::Bool(_) => value.clone(),
                    Value::Object(_) => {
                        sanitize_schema_node(value, &format!("{path}.additionalProperties"))?
                    }
                    _ => {
                        return Err(schema_error(
                            &format!("{path}.additionalProperties"),
                            "must be a boolean or schema object",
                        ))
                    }
                };
                out.insert("additionalProperties".to_string(), normalized);
            }

            "nullable" => {
                nullable = value.as_bool().ok_or_else(|| {
                    schema_error(&format!("{path}.nullable"), "must be a boolean")
                })?;
            }

            "$id" | "$anchor" | "type" | "format" | "title" | "description" | "enum"
            | "minLength" | "maxLength" | "minItems" | "maxItems" | "minimum" | "maximum"
            | "pattern" | "minProperties" | "maxProperties" | "required" | "propertyOrdering" => {
                out.insert(key.clone(), value.clone());
            }

            other => {
                return Err(schema_error(
                    &format!("{path}.{other}"),
                    format!("unsupported JSON Schema keyword '{other}'"),
                ));
            }
        }
    }

    if let Some(value) = const_value {
        if let Some(existing) = out.get("enum").and_then(Value::as_array) {
            if !existing.contains(&value) {
                return Err(schema_error(
                    &format!("{path}.const"),
                    "const conflicts with enum",
                ));
            }
        }
        out.insert("enum".to_string(), Value::Array(vec![value]));
    }

    if let Some(branches) = all_of {
        let sanitized = sanitize_schema_list(branches, &format!("{path}.allOf"))?;
        let mut merged = serde_json::Map::new();
        for (index, branch) in sanitized.iter().enumerate() {
            let branch = branch.as_object().ok_or_else(|| {
                schema_error(
                    &format!("{path}.allOf[{index}]"),
                    "allOf branch must be an object schema",
                )
            })?;
            merge_schema_maps(&mut merged, branch, &format!("{path}.allOf[{index}]"))?;
        }
        merge_schema_maps(&mut out, &merged, path)?;
    }

    if nullable {
        add_nullable_type(&mut out, path)?;
    }

    if out.contains_key("$ref") && out.keys().any(|key| !key.starts_with('$')) {
        return Err(schema_error(
            path,
            "$ref cannot be combined with non-$ sibling constraints",
        ));
    }

    Ok(Value::Object(out))
}

#[async_trait]
impl Adapter for GeminiAdapter {
    fn wire_format(&self) -> &'static str {
        "gemini"
    }

    fn build_url(&self, ctx: &UpstreamContext<'_>) -> Result<String, ProxyError> {
        let base = ctx.provider.base_url.trim_end_matches('/');
        // Streaming-first: Kinetix always consumes an upstream stream (the
        // non-streaming path aggregates it), so always use the streaming method.
        Ok(format!(
            "{base}/models/{}:streamGenerateContent?alt=sse",
            ctx.model.upstream_id
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
                    .unwrap_or_else(|| "x-goog-api-key".to_string());
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

        // System instruction.
        if !req.system.is_empty() {
            let text = req.system.join("\n\n");
            body.insert(
                "systemInstruction".to_string(),
                json!({ "parts": [{ "text": text }] }),
            );
        }

        body.insert("contents".to_string(), json!(Self::build_contents(req)));

        let gen_cfg = Self::build_generation_config(ctx, req);
        if gen_cfg.as_object().map(|o| !o.is_empty()).unwrap_or(false) {
            body.insert("generationConfig".to_string(), gen_cfg);
        }

        if let Some(tools) = Self::build_tools(req)? {
            body.insert("tools".to_string(), tools);
        }
        if let Some(tc) = Self::build_tool_config(req) {
            body.insert("toolConfig".to_string(), tc);
        }

        // Merge admin extra request fields (FR-10.8).
        let extra = ctx.model.extra_request_value();
        merge_extra(&mut body, &extra);

        Ok(Value::Object(body))
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
        let gstatus = parsed
            .pointer("/error/status")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let retry_after_secs = headers
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(parse_retry_after)
            .or_else(|| parse_retry_delay(&parsed));

        // A short retry hint (Gemini's `retryDelay` for a per-minute rate limit)
        // means the window rolls over quickly, so treat it as a rate limit even
        // when the message mentions "quota" (e.g. free-tier RPM caps). A long
        // hint indicates a daily/quota reset and stays an exhaustion.
        let short_retry = retry_after_secs.map(|s| s <= 90).unwrap_or(false);

        let lower = message.to_ascii_lowercase();
        let credential_error = status == 401
            || gstatus == "UNAUTHENTICATED"
            || lower.contains("api key not valid")
            || lower.contains("invalid api key")
            || lower.contains("invalid authentication credentials");

        let kind = match status {
            400 if credential_error => FailureKind::AuthError,
            400 | 422 => FailureKind::BadRequest,
            401 => FailureKind::AuthError,
            403 if credential_error => FailureKind::AuthError,
            403 | 404 => FailureKind::TargetError,
            429 if short_retry => FailureKind::RateLimit,
            429 => classify_429(gstatus, &message),
            s if s >= 500 => FailureKind::ServerError,
            _ => FailureKind::ServerError,
        };

        let safe_message = if message.is_empty() {
            format!("upstream returned HTTP {status}")
        } else {
            crate::crypto::redact(&message)
        };

        UpstreamFailure {
            kind,
            status: Some(status),
            retry_after_secs,
            message: safe_message,
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
        Ok(events_from_gemini(&v))
    }

    fn parse_full_response(&self, body: &Value) -> Result<Vec<StreamEvent>, UpstreamFailure> {
        Ok(events_from_gemini(body))
    }

    fn default_models_path(&self) -> &'static str {
        "/models"
    }

    fn parse_model_list(&self, body: &Value) -> Vec<DiscoveredModel> {
        let mut out = Vec::new();
        if let Some(models) = body.get("models").and_then(|m| m.as_array()) {
            for m in models {
                let Some(name) = m.get("name").and_then(|n| n.as_str()) else {
                    continue;
                };
                let id = name.strip_prefix("models/").unwrap_or(name).to_string();
                // Only surface models that can actually generate content.
                let methods = m
                    .get("supportedGenerationMethods")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>())
                    .unwrap_or_default();
                if !methods.is_empty() && !methods.iter().any(|x| x.contains("generateContent")) {
                    continue;
                }
                out.push(DiscoveredModel {
                    id,
                    display_name: m
                        .get("displayName")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                    context_window: m.get("inputTokenLimit").and_then(|v| v.as_i64()),
                    max_output_tokens: m.get("outputTokenLimit").and_then(|v| v.as_i64()),
                });
            }
        }
        out
    }

    /// Native Gemini opts into automatic opaque `thoughtSignature`
    /// persistence/replay (§6).
    ///
    /// The producer string is an adapter *encoding* version, bumped only if
    /// Kinetix's interpretation of the signature payload changes
    /// incompatibly — never the crate release version.
    fn opaque_state_target(
        &self,
        model: &crate::db::ModelRow,
    ) -> Option<crate::opaque_state::OpaqueStateTarget> {
        Some(crate::opaque_state::OpaqueStateTarget {
            kind: crate::opaque_state::OpaqueStateKind::GeminiThoughtSignature,
            provider_id: model.provider_id.clone(),
            family: "gemini".to_string(),
            producer: "native:gemini:v1".to_string(),
            // `generateContent` only accepts a thought signature on the model
            // that produced it, so state is keyed by the exact model id and a
            // model switch is reported non-portable rather than handed a
            // maybe-invalid signature.
            model_id: model.upstream_id.clone(),
        })
    }

    /// `generateContent` documents an explicit placeholder for a historical
    /// `functionCall` whose real signature cannot be reused: the sentinel keeps
    /// the call in the conversation (with degraded reasoning continuity)
    /// instead of failing the whole request with HTTP 400.
    ///
    /// This is a Gemini 3 mechanism. Only that family validates the signature
    /// of a replayed `functionCall` (and therefore documents the bypass);
    /// Gemini 2.5 and older treat the signature as optional, so injecting the
    /// sentinel there would be inventing protocol state the upstream never
    /// asked for. A later major family is *not* assumed to inherit the Gemini 3
    /// contract either, since nothing documents the bypass for it yet. The gate
    /// is deliberately a model-id check for now — there is no
    /// operator-configurable capability that expresses "Gemini 3 signature
    /// semantics" yet, and hardcoding a broader vendor preset in the pipeline
    /// core is forbidden.
    ///
    /// Source: <https://ai.google.dev/gemini-api/docs/thought-signatures>
    /// (signature validation on replayed function calls, and the sentinel for
    /// history transferred from another model, both apply to Gemini 3).
    fn opaque_state_placeholder(&self, model: &crate::db::ModelRow) -> Option<&'static str> {
        is_gemini_three(&model.upstream_id).then_some(GEMINI_PLACEHOLDER_THOUGHT_SIGNATURE)
    }
}

/// Whether `upstream_id` names a Gemini 3 model.
///
/// The id is matched as `...gemini-<major>...`, so both bare ids
/// (`gemini-3-pro-preview`) and path-qualified ids (`models/gemini-3-flash`)
/// are recognised. An id that does not carry a Gemini version number is not
/// treated as Gemini 3.
///
/// The match is `major == 3`, not `major >= 3`: Google documents the
/// thought-signature validation rule (and the `skip_thought_signature_validator`
/// bypass for it) for Gemini 3 only. There is no evidence that a later major
/// family keeps that contract, so a `gemini-4-*` id must fall back to the
/// ordinary strip/reject portability path rather than be handed a
/// validator-bypass sentinel. See
/// <https://ai.google.dev/gemini-api/docs/thought-signatures> for the Gemini 3
/// signature semantics this gate reflects.
pub(crate) fn is_gemini_three(upstream_id: &str) -> bool {
    let Some((_, rest)) = upstream_id.rsplit_once("gemini-") else {
        return false;
    };
    let major: String = rest.chars().take_while(char::is_ascii_digit).collect();
    major.parse::<u32>().is_ok_and(|version| version == 3)
}

/// Convert a Gemini chunk/response into internal stream events.
fn events_from_gemini(v: &Value) -> Vec<StreamEvent> {
    let mut events = Vec::new();
    let mut tool_index = 0u32;
    let mut finishes = Vec::new();

    if let Some(candidates) = v.get("candidates").and_then(|c| c.as_array()) {
        for cand in candidates {
            if let Some(parts) = cand.pointer("/content/parts").and_then(|p| p.as_array()) {
                for part in parts {
                    let text = part.get("text").and_then(|t| t.as_str()).unwrap_or("");
                    let is_thought = part
                        .get("thought")
                        .and_then(|t| t.as_bool())
                        .unwrap_or(false);
                    let signature = part
                        .get("thoughtSignature")
                        .and_then(|s| s.as_str())
                        .map(String::from);
                    if is_thought && (!text.is_empty() || signature.is_some()) {
                        events.push(StreamEvent::ThinkingDelta {
                            text: text.to_string(),
                            signature,
                        });
                    } else if !text.is_empty() {
                        events.push(StreamEvent::TextDelta(text.to_string()));
                    }

                    if let Some(fc) = part.get("functionCall") {
                        let name = fc
                            .get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or("tool")
                            .to_string();
                        let id = fc.get("id").and_then(|i| i.as_str()).map(String::from);
                        let args = fc.get("args").cloned().unwrap_or(json!({}));
                        let signature = part
                            .get("thoughtSignature")
                            .and_then(|s| s.as_str())
                            .map(String::from);
                        events.push(StreamEvent::ToolCallStart {
                            index: tool_index,
                            id,
                            name,
                            signature,
                        });
                        events.push(StreamEvent::ToolCallArgsDelta {
                            index: tool_index,
                            args: args.to_string(),
                        });
                        tool_index += 1;
                    }
                }
            }
            if let Some(reason) = cand.get("finishReason").and_then(|r| r.as_str()) {
                finishes.push(StreamEvent::Finish(map_finish(reason)));
            }
        }
    }

    // Usage must precede Finish when both arrive in one Gemini chunk. Client
    // encoders flush their final usage frame while handling Finish; emitting
    // usage afterward would silently drop it from translated streams.
    if let Some(usage) = v.get("usageMetadata") {
        let candidates = usage.get("candidatesTokenCount").and_then(Value::as_u64);
        let thinking = usage.get("thoughtsTokenCount").and_then(Value::as_u64);
        let output = match (candidates, thinking) {
            (Some(candidates), Some(thinking)) => Some(candidates.saturating_add(thinking)),
            (Some(candidates), None) => Some(candidates),
            (None, Some(thinking)) => Some(thinking),
            (None, None) => None,
        };
        events.push(StreamEvent::Usage(TokenUsage {
            input: usage.get("promptTokenCount").and_then(Value::as_u64),
            output,
            cached: usage.get("cachedContentTokenCount").and_then(Value::as_u64),
            cache_write: None,
            thinking,
        }));
    }

    events.extend(finishes);
    events
}

fn map_finish(reason: &str) -> FinishReason {
    match reason {
        "STOP" => FinishReason::Stop,
        "MAX_TOKENS" => FinishReason::Length,
        "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" => {
            FinishReason::ContentFilter
        }
        other => FinishReason::Other(other.to_string()),
    }
}

fn classify_429(status: &str, message: &str) -> FailureKind {
    let lower = message.to_lowercase();
    let quota_like = lower.contains("quota")
        || lower.contains("exceeded your current quota")
        || lower.contains("billing")
        || status == "RESOURCE_EXHAUSTED" && lower.contains("free_tier");
    // Rate limits are short-lived; quota exhaustion resets on a schedule.
    if quota_like
        && (lower.contains("per_day") || lower.contains("per day") || lower.contains("free_tier"))
    {
        FailureKind::QuotaExhausted
    } else if lower.contains("rate limit") || lower.contains("requests per minute") {
        FailureKind::RateLimit
    } else if quota_like {
        FailureKind::QuotaExhausted
    } else {
        FailureKind::RateLimit
    }
}

fn parse_retry_after(value: &str) -> Option<u64> {
    if let Ok(secs) = value.trim().parse::<u64>() {
        return Some(secs);
    }
    // HTTP-date form.
    if let Ok(dt) = chrono::DateTime::parse_from_rfc2822(value) {
        let delta = dt.with_timezone(&chrono::Utc) - chrono::Utc::now();
        return Some(delta.num_seconds().max(0) as u64);
    }
    None
}

/// Gemini puts `"retryDelay": "23s"` inside error details.
fn parse_retry_delay(v: &Value) -> Option<u64> {
    let details = v.pointer("/error/details")?.as_array()?;
    for d in details {
        if let Some(delay) = d.get("retryDelay").and_then(|x| x.as_str()) {
            let num = delay.trim_end_matches('s').parse::<f64>().ok()?;
            return Some(num.ceil() as u64);
        }
    }
    None
}

fn merge_extra(body: &mut serde_json::Map<String, Value>, extra: &Value) {
    let Some(obj) = extra.as_object() else { return };
    for (k, v) in obj {
        // set-if-absent semantics by default.
        if !body.contains_key(k) {
            body.insert(k.clone(), v.clone());
        } else if let (Some(existing), Some(new)) = (
            body.get_mut(k).and_then(|x| x.as_object_mut()),
            v.as_object(),
        ) {
            for (k2, v2) in new {
                existing.entry(k2.clone()).or_insert_with(|| v2.clone());
            }
        }
    }
}

/// Re-export for the tool-result name lookup in the translator.
pub fn sampling_defaults() -> SamplingParams {
    SamplingParams::default()
}

pub fn message_has_tool_result(m: &Message) -> bool {
    m.parts.iter().any(|p| matches!(p, Part::ToolResult { .. }))
}

#[cfg(test)]
mod schema_tests {
    use super::*;

    /// Minimal `ModelRow` for adapter-level capability decisions.
    fn model_row(upstream_id: &str) -> crate::db::ModelRow {
        crate::db::ModelRow {
            id: "m".into(),
            provider_id: "ai-studio".into(),
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
            discovery: "{}".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            opaque_state_plugin: String::new(),
        }
    }

    #[test]
    fn signature_only_thinking_decodes_for_round_trip() {
        let events = events_from_gemini(&json!({
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{
                        "thought": true,
                        "thoughtSignature": "sig-only"
                    }]
                }
            }]
        }));

        assert!(matches!(
            &events[0],
            StreamEvent::ThinkingDelta {
                text,
                signature: Some(signature)
            } if text.is_empty() && signature == "sig-only"
        ));
    }

    #[test]
    fn thinking_signature_round_trips_into_gemini_history() {
        let mut out = Vec::new();
        GeminiAdapter::encode_parts(
            &[Part::Thinking {
                text: String::new(),
                signature: Some("sig-thinking".into()),
            }],
            &mut out,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["thought"], true);
        assert_eq!(out[0]["thoughtSignature"], "sig-thinking");
    }

    #[test]
    fn function_call_with_same_part_signature_decodes_to_tool_call_start() {
        let events = events_from_gemini(&json!({
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{
                        "functionCall": {
                            "id": "call_1",
                            "name": "bash",
                            "args": {}
                        },
                        "thoughtSignature": "SIG_A"
                    }]
                }
            }]
        }));

        let start = events
            .iter()
            .find(|e| matches!(e, StreamEvent::ToolCallStart { .. }))
            .expect("tool call start event");
        assert!(matches!(
            start,
            StreamEvent::ToolCallStart {
                id: Some(id),
                name,
                signature: Some(signature),
                ..
            } if id == "call_1" && name == "bash" && signature == "SIG_A"
        ));
    }

    #[test]
    fn parallel_function_calls_only_first_carries_signature() {
        // Gemini 3 commonly signs only the first function call in a parallel
        // function-call step; the other calls must decode with no signature
        // rather than inheriting the first one's.
        let events = events_from_gemini(&json!({
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [
                        {
                            "functionCall": { "id": "A", "name": "read", "args": {} },
                            "thoughtSignature": "SIG_A"
                        },
                        {
                            "functionCall": { "id": "B", "name": "bash", "args": {} }
                        }
                    ]
                }
            }]
        }));

        let starts: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, StreamEvent::ToolCallStart { .. }))
            .collect();
        assert_eq!(starts.len(), 2);
        assert!(matches!(
            starts[0],
            StreamEvent::ToolCallStart { id: Some(id), signature: Some(sig), .. }
            if id == "A" && sig == "SIG_A"
        ));
        assert!(matches!(
            starts[1],
            StreamEvent::ToolCallStart { id: Some(id), signature: None, .. }
            if id == "B"
        ));
    }

    #[test]
    fn native_gemini_opts_into_opaque_state_by_family_not_exact_model() {
        let model = crate::db::ModelRow {
            id: "m".into(),
            provider_id: "ai-studio".into(),
            upstream_id: "gemini-3.8-flash".into(),
            display_name: "Gemini 3.8 Flash".into(),
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
        };
        let adapter = GeminiAdapter::new();
        let target = adapter.opaque_state_target(&model).expect("gemini opts in");
        assert_eq!(target.provider_id, "ai-studio");
        assert_eq!(target.family, "gemini");
        assert_eq!(target.producer, "native:gemini:v1");
        assert_eq!(target.model_id, "gemini-3.8-flash");

        // A different exact model id on the same provider keeps the same
        // family/producer provenance but a distinct model id, so state is
        // never replayed across models (§6).
        let mut other_model = model.clone();
        other_model.upstream_id = "gemini-3.8-pro".into();
        let other_target = adapter
            .opaque_state_target(&other_model)
            .expect("gemini opts in");
        assert_eq!(other_target.family, target.family);
        assert_eq!(other_target.producer, target.producer);
        assert_ne!(other_target.model_id, target.model_id);
    }

    #[test]
    fn native_gemini_declares_the_documented_placeholder_signature() {
        // Cross-model continuation keeps exact signatures model-scoped, so the
        // target cannot carry the stored value; Gemini documents this sentinel
        // for exactly that case, and the pipeline paints it instead of sending
        // an unsigned (rejected) historical function call.
        assert_eq!(
            GeminiAdapter::new().opaque_state_placeholder(&model_row("gemini-3.8-pro")),
            Some("skip_thought_signature_validator")
        );
        assert_eq!(
            GeminiAdapter::new().opaque_state_placeholder(&model_row("gemini-3-flash-preview")),
            Some("skip_thought_signature_validator")
        );
    }

    #[test]
    fn older_gemini_models_do_not_declare_the_gemini_3_placeholder() {
        // The sentinel is a Gemini 3 `generateContent` mechanism. Gemini 2.5
        // and older treat a replayed function-call signature as optional, so
        // Kinetix must not inject the Gemini 3 validator-bypass token there.
        for upstream_id in [
            "gemini-2.5-pro",
            "gemini-2.5-flash",
            "gemini-2.0-flash",
            "gemini-1.5-pro",
            // An id without a Gemini version number is not assumed Gemini 3.
            "gemini-experimental",
        ] {
            assert_eq!(
                GeminiAdapter::new().opaque_state_placeholder(&model_row(upstream_id)),
                None,
                "{upstream_id} must not receive the Gemini 3 placeholder"
            );
        }
    }

    #[test]
    fn later_gemini_majors_do_not_inherit_the_gemini_3_placeholder() {
        // The validator bypass is documented for Gemini 3 only: Google's
        // thought-signatures doc names the Gemini 3 preview models
        // (gemini-3-pro-preview, gemini-3-flash-preview) as the family that
        // enforces replay validation and documents
        // `skip_thought_signature_validator` as the escape hatch
        // (<https://ai.google.dev/gemini-api/docs/thought-signatures>).
        // Nothing states a later major keeps that contract, so Kinetix must
        // not hand a future family a validator-bypass token it cannot justify;
        // a `gemini-4-*` target falls back to the ordinary strip/reject
        // portability path.
        for upstream_id in [
            "gemini-4-pro",
            "gemini-4-flash",
            "models/gemini-4.0-pro",
            "gemini-5-pro",
        ] {
            assert_eq!(
                GeminiAdapter::new().opaque_state_placeholder(&model_row(upstream_id)),
                None,
                "{upstream_id} must not receive the Gemini 3 placeholder"
            );
        }
    }

    #[test]
    fn gemini_three_detection_handles_path_qualified_ids() {
        assert!(is_gemini_three("gemini-3-pro-preview"));
        assert!(is_gemini_three("models/gemini-3-flash"));
        assert!(!is_gemini_three("models/gemini-2.5-pro"));
        assert!(!is_gemini_three("gemini-4-pro"));
        assert!(!is_gemini_three("gemini"));
    }

    #[test]
    fn generation_config_uses_canonical_policy_keys_for_gemini_wire_names() {
        let provider = crate::db::ProviderRow {
            id: "p".into(),
            name: "p".into(),
            base_url: "https://example.com".into(),
            wire_format: "gemini".into(),
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
        };
        let model = crate::db::ModelRow {
            id: "m".into(),
            provider_id: "p".into(),
            upstream_id: "gemini".into(),
            display_name: "Gemini".into(),
            enabled: 1,
            context_window: None,
            max_output_tokens: None,
            capabilities: "{}".into(),
            prices: "{}".into(),
            parameters: json!({
                "top_p": { "supported": true, "max": 0.8, "policy": "clamp" },
                "top_k": { "supported": true, "max": 40.0, "policy": "clamp" }
            })
            .to_string(),
            thinking_map: json!({
                "mode": "level",
                "levels": {"high": "high"},
                "level_field": "thinkingConfig.thinkingLevel"
            })
            .to_string(),
            extra_request: "{}".into(),
            discovery: "{}".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            opaque_state_plugin: String::new(),
        };
        let ctx = UpstreamContext {
            provider: &provider,
            model: &model,
            account_id: None,
            credential: "k".into(),
        };
        let req = InternalRequest {
            requested_model: "m".into(),
            system: vec![],
            messages: vec![],
            tools: vec![],
            tool_choice: None,
            tool_choice_name: None,
            params: SamplingParams {
                top_p: Some(0.95),
                top_k: Some(99.0),
                ..Default::default()
            },
            stream: true,
            include_usage: false,
            thinking: Some(crate::types::ThinkingLevel::High),
            extra: Default::default(),
            raw_body: None,
        };

        let cfg = GeminiAdapter::build_generation_config(&ctx, &req);
        assert_eq!(cfg["topP"], 0.8);
        assert_eq!(cfg["topK"], 40.0);
        assert_eq!(cfg["thinkingConfig"]["thinkingLevel"], "high");
        assert!(cfg.get("top_p").is_none());
        assert!(cfg.get("top_k").is_none());
    }

    #[test]
    fn usage_precedes_finish_when_gemini_combines_them() {
        let events = events_from_gemini(&json!({
            "candidates": [{
                "content": {"role": "model", "parts": []},
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 100,
                "candidatesTokenCount": 12,
                "totalTokenCount": 112
            }
        }));

        assert!(matches!(events[0], StreamEvent::Usage(_)));
        assert!(matches!(events[1], StreamEvent::Finish(FinishReason::Stop)));
    }

    #[test]
    fn usage_includes_thinking_in_total_output() {
        let events = events_from_gemini(&json!({
            "candidates": [],
            "usageMetadata": {
                "promptTokenCount": 100,
                "cachedContentTokenCount": 40,
                "candidatesTokenCount": 50,
                "thoughtsTokenCount": 30
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
        assert_eq!(usage.cache_write, None);
        assert_eq!(usage.thinking, Some(30));
    }

    #[test]
    fn sanitize_schema_preserves_supported_refs_and_defs() {
        let input = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$ref": "#/definitions/Envelope",
            "definitions": {
                "Envelope": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["payload"],
                    "properties": {
                        "payload": { "$ref": "#/definitions/Payload" }
                    }
                },
                "Payload": {
                    "type": "object",
                    "properties": {
                        "kind": { "const": "ok" }
                    },
                    "required": ["kind"]
                }
            }
        });
        let got = sanitize_schema(&input, "tool 'read'").unwrap();

        assert!(got.get("$schema").is_none());
        assert_eq!(got["$ref"], "#/$defs/Envelope");
        assert_eq!(
            got.pointer("/$defs/Envelope/properties/payload/$ref"),
            Some(&json!("#/$defs/Payload"))
        );
        assert_eq!(
            got.pointer("/$defs/Payload/properties/kind/enum"),
            Some(&json!(["ok"]))
        );
        assert_eq!(
            got.pointer("/$defs/Envelope/additionalProperties"),
            Some(&json!(false))
        );
    }

    #[test]
    fn sanitize_schema_normalizes_legacy_unions_and_tuple_items() {
        let input = json!({
            "type": "object",
            "properties": {
                "value": { "type": ["string", "null"] },
                "choice": { "oneOf": [{"type":"string"}, {"type":"number"}] },
                "tuple": {
                    "type":"array",
                    "items":[{"type":"string"},{"type":"integer"}]
                },
                "legacy_nullable": { "type": "string", "nullable": true }
            }
        });
        let got = sanitize_schema(&input, "tool 'edit'").unwrap();

        assert_eq!(
            got.pointer("/properties/value/type"),
            Some(&json!(["string", "null"]))
        );
        assert!(got.pointer("/properties/choice/oneOf").is_none());
        assert_eq!(
            got.pointer("/properties/choice/anyOf/1/type"),
            Some(&json!("number"))
        );
        assert_eq!(
            got.pointer("/properties/tuple/prefixItems/0/type"),
            Some(&json!("string"))
        );
        assert_eq!(
            got.pointer("/properties/legacy_nullable/type"),
            Some(&json!(["string", "null"]))
        );
    }

    #[test]
    fn sanitize_schema_keeps_supported_nested_constraints() {
        let input = json!({
            "type": "object",
            "strict": true,
            "additionalProperties": false,
            "properties": {
                "config": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "mode": { "const": "fast", "description": "execution mode" },
                        "items": {
                            "type": "array",
                            "minItems": 1,
                            "maxItems": 8,
                            "items": {
                                "type": "object",
                                "additionalProperties": false,
                                "properties": {
                                    "count": {
                                        "type": "integer",
                                        "minimum": 0,
                                        "maximum": 10
                                    }
                                }
                            }
                        }
                    }
                }
            }
        });
        let got = sanitize_schema(&input, "tool 'batch'").unwrap();

        assert!(got.get("strict").is_none());
        assert_eq!(got["additionalProperties"], false);
        assert_eq!(
            got.pointer("/properties/config/properties/mode/enum"),
            Some(&json!(["fast"]))
        );
        assert_eq!(
            got.pointer("/properties/config/properties/items/minItems"),
            Some(&json!(1))
        );
        assert_eq!(
            got.pointer("/properties/config/properties/items/items/properties/count/maximum"),
            Some(&json!(10))
        );
    }

    #[test]
    fn sanitize_schema_preserves_string_length_constraints_recursively() {
        let input = json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "search text",
                    "minLength": 1,
                    "maxLength": 10000
                },
                "items": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "label": {
                                "type": "string",
                                "minLength": 2,
                                "maxLength": 64
                            }
                        }
                    }
                }
            },
            "required": ["query"]
        });
        let got = sanitize_schema(&input, "tool 'chrome_devtools_load'").unwrap();

        assert_eq!(got.pointer("/properties/query/minLength"), Some(&json!(1)));
        assert_eq!(
            got.pointer("/properties/query/maxLength"),
            Some(&json!(10000))
        );
        assert_eq!(
            got.pointer("/properties/items/items/properties/label/minLength"),
            Some(&json!(2))
        );
        assert_eq!(
            got.pointer("/properties/items/items/properties/label/maxLength"),
            Some(&json!(64))
        );
        assert_eq!(
            got.pointer("/properties/query/description"),
            Some(&json!("search text"))
        );
        assert_eq!(got["required"], json!(["query"]));
    }

    #[test]
    fn sanitize_schema_preserves_documented_validation_keywords() {
        let input = json!({
            "type": "object",
            "properties": {
                "resumeFromRunId": {
                    "type": "string",
                    "pattern": "^[A-Za-z0-9_-]+$",
                    "minLength": 1
                },
                "labels": {
                    "type": "object",
                    "minProperties": 1,
                    "maxProperties": 4,
                    "additionalProperties": {"type": "string"}
                }
            },
            "required": ["resumeFromRunId"]
        });
        let got = sanitize_schema(&input, "tool 'SubagentWorkflow'").unwrap();

        assert_eq!(
            got.pointer("/properties/resumeFromRunId/pattern"),
            Some(&json!("^[A-Za-z0-9_-]+$"))
        );
        assert_eq!(
            got.pointer("/properties/resumeFromRunId/minLength"),
            Some(&json!(1))
        );
        assert_eq!(
            got.pointer("/properties/labels/minProperties"),
            Some(&json!(1))
        );
        assert_eq!(
            got.pointer("/properties/labels/maxProperties"),
            Some(&json!(4))
        );
        assert_eq!(
            got.pointer("/properties/labels/additionalProperties/type"),
            Some(&json!("string"))
        );
        assert_eq!(got["required"], json!(["resumeFromRunId"]));
    }

    #[test]
    fn sanitize_schema_rejects_unverified_validation_keywords() {
        for (keyword, value) in [
            ("exclusiveMinimum", json!(0)),
            ("exclusiveMaximum", json!(10)),
            ("multipleOf", json!(0.5)),
            ("propertyNames", json!({"pattern":"^[a-z]+$"})),
            ("uniqueItems", json!(true)),
        ] {
            let mut value_schema = json!({ "type": "string" });
            value_schema
                .as_object_mut()
                .unwrap()
                .insert(keyword.to_string(), value);
            let input = json!({
                "type": "object",
                "properties": { "value": value_schema }
            });
            let err = sanitize_schema(&input, "tool 'agent'").unwrap_err();

            assert_eq!(err.kind, FailureKind::BadRequest);
            assert!(
                err.message.contains(keyword),
                "expected {keyword} in: {}",
                err.message
            );
        }
    }

    #[test]
    fn sanitize_schema_still_rejects_unknown_keywords() {
        let input = json!({
            "type": "object",
            "properties": {
                "value": {
                    "type": "string",
                    "x-unknown-validation": true
                }
            }
        });
        let err = sanitize_schema(&input, "tool 'agent'").unwrap_err();

        assert_eq!(err.kind, FailureKind::BadRequest);
        assert!(err.message.contains("x-unknown-validation"));
    }

    #[test]
    fn build_tools_preserves_subagent_workflow_pattern() {
        let req = InternalRequest {
            requested_model: "gemini".into(),
            system: vec![],
            messages: vec![],
            tools: vec![crate::types::ToolDef {
                name: "SubagentWorkflow".into(),
                description: Some("resume a subagent workflow".into()),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "resumeFromRunId": {
                            "type": "string",
                            "pattern": "^[A-Za-z0-9_-]+$",
                            "description": "run id"
                        }
                    }
                }),
                defer_loading: None,
            }],
            tool_choice: None,
            tool_choice_name: None,
            params: Default::default(),
            stream: true,
            include_usage: false,
            thinking: None,
            extra: Default::default(),
            raw_body: None,
        };

        let tools = GeminiAdapter::build_tools(&req).unwrap().unwrap();
        let schema = &tools[0]["functionDeclarations"][0]["parametersJsonSchema"];

        assert_eq!(
            schema.pointer("/properties/resumeFromRunId/pattern"),
            Some(&json!("^[A-Za-z0-9_-]+$"))
        );
        assert_eq!(
            schema.pointer("/properties/resumeFromRunId/description"),
            Some(&json!("run id"))
        );
    }

    #[test]
    fn sanitize_schema_rejects_ref_siblings_and_lossy_all_of() {
        let ref_sibling = json!({
            "$ref": "#/$defs/Value",
            "description": "not allowed beside ref",
            "$defs": {
                "Value": { "type": "string" }
            }
        });
        assert!(sanitize_schema(&ref_sibling, "tool 'agent'").is_err());

        let conflicting = json!({
            "allOf": [
                {"type":"object","properties":{"x":{"type":"string"}}},
                {"type":"object","properties":{"x":{"type":"integer"}}}
            ]
        });
        assert!(sanitize_schema(&conflicting, "tool 'agent'").is_err());
    }

    #[test]
    fn build_tools_preserves_json_schema_length_constraints() {
        let req = InternalRequest {
            requested_model: "gemini".into(),
            system: vec![],
            messages: vec![],
            tools: vec![crate::types::ToolDef {
                name: "chrome_devtools_load".into(),
                description: Some("load a DevTools resource".into()),
                parameters: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "resource query",
                            "minLength": 1,
                            "maxLength": 10000
                        }
                    },
                    "required": ["query"]
                }),
                defer_loading: None,
            }],
            tool_choice: None,
            tool_choice_name: None,
            params: Default::default(),
            stream: true,
            include_usage: false,
            thinking: None,
            extra: Default::default(),
            raw_body: None,
        };

        let tools = GeminiAdapter::build_tools(&req).unwrap().unwrap();
        let declaration = &tools[0]["functionDeclarations"][0];
        let schema = &declaration["parametersJsonSchema"];

        assert!(declaration.get("parameters").is_none());
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["required"], json!(["query"]));
        assert_eq!(
            schema.pointer("/properties/query/description"),
            Some(&json!("resource query"))
        );
        assert_eq!(
            schema.pointer("/properties/query/minLength"),
            Some(&json!(1))
        );
        assert_eq!(
            schema.pointer("/properties/query/maxLength"),
            Some(&json!(10000))
        );
    }
}

#[cfg(test)]
mod error_scope_tests {
    use super::*;
    use crate::adapters::Adapter;

    #[test]
    fn permission_denied_does_not_poison_gemini_credential() {
        let adapter = GeminiAdapter::new();
        let forbidden = adapter.classify_error(
            403,
            r#"{"error":{"status":"PERMISSION_DENIED","message":"project is not allowed to use this model"}}"#,
            &reqwest::header::HeaderMap::new(),
        );
        assert_eq!(forbidden.kind, FailureKind::TargetError);

        let invalid_key = adapter.classify_error(
            400,
            r#"{"error":{"status":"INVALID_ARGUMENT","message":"API key not valid. Please pass a valid API key."}}"#,
            &reqwest::header::HeaderMap::new(),
        );
        assert_eq!(invalid_key.kind, FailureKind::AuthError);
    }
}
