//! Versioned canonical response/event contract for provider-adapter plugins.
//!
//! Both streaming parser calls and full-response parser calls return the same
//! `kinetix.plugin.response` envelope. The host validates the envelope and
//! every event before converting it into internal `StreamEvent` values.

use serde_json::{json, Value};

use crate::types::{FailureKind, FinishReason, StreamEvent, TokenUsage, UpstreamFailure};

pub const RESPONSE_SCHEMA: &str = "kinetix.plugin.response";
pub const RESPONSE_SCHEMA_VERSION: u64 = 2;

/// Serialize successful internal events into the canonical v2 response envelope.
pub fn events_to_json(events: &[StreamEvent]) -> String {
    let events: Vec<Value> = events.iter().map(event_to_value).collect();
    let value = json!({
        "schema": RESPONSE_SCHEMA,
        "schema_version": RESPONSE_SCHEMA_VERSION,
        "events": events,
    });
    serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string())
}

/// Decode and strictly validate a canonical plugin response.
///
/// Versioned v1 envelopes are the canonical contract. A bare event array is
/// accepted only as a migration shim for plugins published before the response
/// contract was versioned; the events inside it are still validated with v1
/// field/type rules.
pub fn json_to_events(s: &str) -> Result<Vec<StreamEvent>, UpstreamFailure> {
    let value: Value =
        serde_json::from_str(s).map_err(|e| contract_failure(format!("invalid JSON: {e}")))?;

    if let Some(events) = value.as_array() {
        tracing::warn!(
            "plugin adapter returned legacy unversioned event array; update it to kinetix.plugin.response v2"
        );
        return decode_events(events, 1);
    }

    let object = value
        .as_object()
        .ok_or_else(|| contract_failure("response payload must be an object"))?;

    let schema = object
        .get("schema")
        .and_then(Value::as_str)
        .ok_or_else(|| contract_failure("missing or non-string 'schema'"))?;
    if schema != RESPONSE_SCHEMA {
        return Err(contract_failure(format!(
            "unsupported schema '{schema}', expected '{RESPONSE_SCHEMA}'"
        )));
    }

    let version = object
        .get("schema_version")
        .and_then(Value::as_u64)
        .ok_or_else(|| contract_failure("missing or non-integer 'schema_version'"))?;
    if !matches!(version, 1 | RESPONSE_SCHEMA_VERSION) {
        return Err(contract_failure(format!(
            "unsupported schema_version {version}; supported versions are 1 and {RESPONSE_SCHEMA_VERSION}"
        )));
    }

    let events = object
        .get("events")
        .and_then(Value::as_array)
        .ok_or_else(|| contract_failure("missing or non-array 'events'"))?;

    decode_events(events, version)
}

fn decode_events(events: &[Value], version: u64) -> Result<Vec<StreamEvent>, UpstreamFailure> {
    let mut out = Vec::with_capacity(events.len());

    for event in events {
        event
            .as_object()
            .ok_or_else(|| contract_failure("event must be an object"))?;
        let event_type = required_str(event, "type")?;

        match event_type {
            "warning" => {
                validate_warning(event)?;
            }
            "error" => {
                if events.len() != 1 {
                    return Err(contract_failure(
                        "terminal error event must be the only event in its response envelope",
                    ));
                }
                return Err(error_event_to_failure(event)?);
            }
            _ => out.push(value_to_event(event, version)?),
        }
    }

    Ok(out)
}

fn event_to_value(event: &StreamEvent) -> Value {
    match event {
        StreamEvent::Start {
            upstream_request_id,
        } => json!({
            "type": "start",
            "upstream_request_id": upstream_request_id,
        }),
        StreamEvent::ThinkingBlockStart {
            index,
            thinking,
            signature,
        } => json!({
            "type": "thinking_block_start",
            "index": index,
            "thinking": thinking,
            "signature": signature,
        }),
        StreamEvent::ThinkingDelta {
            block_index,
            text,
            signature,
        } => json!({
            "type": "thinking_delta",
            "block_index": block_index,
            "text": text,
            "signature": signature,
        }),
        StreamEvent::ThinkingBlockStop { index } => json!({
            "type": "thinking_block_stop",
            "index": index,
        }),
        StreamEvent::RedactedThinking { index, data } => json!({
            "type": "redacted_thinking",
            "index": index,
            "data": data,
        }),
        StreamEvent::TextDelta(text) | StreamEvent::RefusalDelta(text) => json!({
            "type": "text_delta",
            "text": text,
        }),
        StreamEvent::ToolCallStart {
            index,
            id,
            name,
            signature,
        } => json!({
            "type": "tool_call_start",
            "index": index,
            "id": id,
            "name": name,
            "signature": signature,
        }),
        StreamEvent::ToolCallArgsDelta { index, args } => json!({
            "type": "tool_call_args_delta",
            "index": index,
            "args": args,
        }),
        StreamEvent::Usage(usage) => json!({
            "type": "usage",
            "input": usage.input,
            "output": usage.output,
            "cached": usage.cached,
            "cache_write": usage.cache_write,
            "thinking": usage.thinking,
        }),
        StreamEvent::Finish(reason) => json!({
            "type": "finish",
            "reason": reason.as_str(),
        }),
    }
}

fn value_to_event(value: &Value, version: u64) -> Result<StreamEvent, UpstreamFailure> {
    let event_type = required_str(value, "type")?;

    match event_type {
        "start" => Ok(StreamEvent::Start {
            upstream_request_id: optional_str(value, "upstream_request_id")?.map(str::to_string),
        }),
        "thinking_delta" => Ok(StreamEvent::ThinkingDelta {
            // In v1 this is an unknown additive field, so ignore it. Its
            // meaning is defined only by v2; known-field validation stays strict.
            block_index: if version >= 2 {
                optional_u32(value, "block_index")?
            } else {
                None
            },
            text: required_str(value, "text")?.to_string(),
            signature: optional_str(value, "signature")?.map(str::to_string),
        }),
        "thinking_block_start" if version >= 2 => Ok(StreamEvent::ThinkingBlockStart {
            index: required_u32(value, "index")?,
            thinking: required_str(value, "thinking")?.to_string(),
            signature: optional_str(value, "signature")?.map(str::to_string),
        }),
        "thinking_block_stop" if version >= 2 => Ok(StreamEvent::ThinkingBlockStop {
            index: required_u32(value, "index")?,
        }),
        "redacted_thinking" if version >= 2 => Ok(StreamEvent::RedactedThinking {
            index: required_u32(value, "index")?,
            data: required_str(value, "data")?.to_string(),
        }),
        "text_delta" => Ok(StreamEvent::TextDelta(
            required_str(value, "text")?.to_string(),
        )),
        "tool_call_start" => Ok(StreamEvent::ToolCallStart {
            index: required_u32(value, "index")?,
            id: optional_str(value, "id")?.map(str::to_string),
            name: required_str(value, "name")?.to_string(),
            signature: optional_str(value, "signature")?.map(str::to_string),
        }),
        "tool_call_args_delta" => Ok(StreamEvent::ToolCallArgsDelta {
            index: required_u32(value, "index")?,
            args: required_str(value, "args")?.to_string(),
        }),
        "usage" => Ok(StreamEvent::Usage(TokenUsage {
            input: optional_u64(value, "input")?,
            output: optional_u64(value, "output")?,
            cached: optional_u64(value, "cached")?,
            cache_write: optional_u64(value, "cache_write")?,
            thinking: optional_u64(value, "thinking")?,
        })),
        "finish" => {
            let reason = required_str(value, "reason")?;
            if reason.is_empty() {
                return Err(contract_failure("finish.reason must not be empty"));
            }
            Ok(StreamEvent::Finish(match reason {
                "stop" => FinishReason::Stop,
                "length" => FinishReason::Length,
                "tool_calls" => FinishReason::ToolCalls,
                "content_filter" => FinishReason::ContentFilter,
                other => FinishReason::Other(other.to_string()),
            }))
        }
        other => Err(contract_failure(format!("unknown event type '{other}'"))),
    }
}

fn validate_warning(value: &Value) -> Result<(), UpstreamFailure> {
    let code = required_str(value, "code")?;
    let message = required_str(value, "message")?;
    if code.is_empty() {
        return Err(contract_failure("warning.code must not be empty"));
    }
    if message.is_empty() {
        return Err(contract_failure("warning.message must not be empty"));
    }

    let message = crate::crypto::redact(message);
    tracing::warn!(code = %code, message = %message, "plugin adapter warning event");
    Ok(())
}

fn error_event_to_failure(value: &Value) -> Result<UpstreamFailure, UpstreamFailure> {
    let kind_name = required_str(value, "kind")?;
    let kind = FailureKind::parse(kind_name)
        .ok_or_else(|| contract_failure(format!("unknown error.kind '{kind_name}'")))?;

    let message = required_str(value, "message")?;
    if message.is_empty() {
        return Err(contract_failure("error.message must not be empty"));
    }

    let quota_reset_at = match optional_str(value, "quota_reset_at")? {
        Some(raw) => Some(
            chrono::DateTime::parse_from_rfc3339(raw)
                .map_err(|_| contract_failure("error.quota_reset_at must be RFC3339"))?
                .with_timezone(&chrono::Utc),
        ),
        None => None,
    };

    Ok(UpstreamFailure {
        kind,
        status: optional_u16(value, "status")?,
        retry_after_secs: optional_u64(value, "retry_after_secs")?,
        message: crate::crypto::redact(message),
        quota_reset_at,
    })
}

fn required_str<'a>(value: &'a Value, field: &str) -> Result<&'a str, UpstreamFailure> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| contract_failure(format!("'{field}' must be a string")))
}

fn optional_str<'a>(value: &'a Value, field: &str) -> Result<Option<&'a str>, UpstreamFailure> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.as_str())),
        Some(_) => Err(contract_failure(format!(
            "'{field}' must be a string or null"
        ))),
    }
}

fn required_u64(value: &Value, field: &str) -> Result<u64, UpstreamFailure> {
    value
        .get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| contract_failure(format!("'{field}' must be a non-negative integer")))
}

fn optional_u64(value: &Value, field: &str) -> Result<Option<u64>, UpstreamFailure> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(number) => number.as_u64().map(Some).ok_or_else(|| {
            contract_failure(format!("'{field}' must be a non-negative integer or null"))
        }),
    }
}

fn required_u32(value: &Value, field: &str) -> Result<u32, UpstreamFailure> {
    let number = required_u64(value, field)?;
    u32::try_from(number).map_err(|_| contract_failure(format!("'{field}' exceeds u32 range")))
}

fn optional_u32(value: &Value, field: &str) -> Result<Option<u32>, UpstreamFailure> {
    match optional_u64(value, field)? {
        Some(number) => u32::try_from(number)
            .map(Some)
            .map_err(|_| contract_failure(format!("'{field}' exceeds u32 range"))),
        None => Ok(None),
    }
}

fn optional_u16(value: &Value, field: &str) -> Result<Option<u16>, UpstreamFailure> {
    match optional_u64(value, field)? {
        Some(number) => u16::try_from(number)
            .map(Some)
            .map_err(|_| contract_failure(format!("'{field}' exceeds u16 range"))),
        None => Ok(None),
    }
}

fn contract_failure(message: impl Into<String>) -> UpstreamFailure {
    UpstreamFailure {
        kind: FailureKind::PluginFailure,
        status: None,
        retry_after_secs: None,
        message: format!("plugin response contract: {}", message.into()),
        quota_reset_at: None,
    }
}
