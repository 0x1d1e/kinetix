//! Inbound decode fixtures (FR-9.1, FR-9.6).
//!
//! The fixture suite must cover plain chat, tools, parallel tools, reasoning
//! state, images, usage, errors, and unknown fields for every supported
//! frontend. These tests assert the canonical decode of representative client
//! bodies, so a regression in inbound decoding is caught.

use kinetix::frontends::{self, FrontendFormat};
use kinetix::types::{Part, Role, ThinkingLevel};

fn openai(body: &str) -> kinetix::types::InternalRequest {
    frontends::decode(FrontendFormat::OpenAi, serde_json::from_str(body).unwrap())
        .expect("openai decode")
}

fn anthropic(body: &str) -> kinetix::types::InternalRequest {
    frontends::decode(
        FrontendFormat::Anthropic,
        serde_json::from_str(body).unwrap(),
    )
    .expect("anthropic decode")
}

fn responses(body: &str) -> kinetix::types::InternalRequest {
    frontends::decode(
        FrontendFormat::OpenAiResponses,
        serde_json::from_str(body).unwrap(),
    )
    .expect("responses decode")
}

#[test]
fn openai_plain_chat_and_system_hoist() {
    let req = openai(
        r#"{
          "model": "gpt-x",
          "stream": true,
          "temperature": 0.4,
          "max_tokens": 128,
          "messages": [
            { "role": "system", "content": "be terse" },
            { "role": "user", "content": "hi" }
          ]
        }"#,
    );
    assert_eq!(req.requested_model, "gpt-x");
    assert!(req.stream);
    assert_eq!(req.system, vec!["be terse".to_string()]);
    assert_eq!(req.messages.len(), 1);
    assert_eq!(req.messages[0].role, Role::User);
    assert_eq!(req.messages[0].parts, vec![Part::Text("hi".into())]);
    assert_eq!(req.params.temperature, Some(0.4));
    assert_eq!(req.params.max_tokens, Some(128));
}

#[test]
fn openai_stream_options_include_usage_is_preserved() {
    let requested = openai(
        r#"{"model":"m","stream":true,"stream_options":{"include_usage":true},"messages":[]}"#,
    );
    assert!(requested.include_usage);

    let defaulted = openai(r#"{"model":"m","stream":true,"messages":[]}"#);
    assert!(!defaulted.include_usage);

    let explicit_false = openai(
        r#"{"model":"m","stream":true,"stream_options":{"include_usage":false},"messages":[]}"#,
    );
    assert!(!explicit_false.include_usage);
}

#[test]
fn openai_parallel_tool_calls_and_tool_result() {
    let req = openai(
        r#"{
          "model": "gpt-x",
          "messages": [
            { "role": "assistant", "content": null, "tool_calls": [
                { "id": "call_a", "type": "function",
                  "function": { "name": "get_weather", "arguments": "{\"city\":\"Paris\"}" } },
                { "id": "call_b", "type": "function",
                  "function": { "name": "get_time", "arguments": "{\"tz\":\"UTC\"}" } }
            ]},
            { "role": "tool", "tool_call_id": "call_a", "content": "18C" }
          ],
          "tools": [
            { "type": "function", "function": { "name": "get_weather",
              "description": "w", "parameters": { "type": "object" } } }
          ]
        }"#,
    );
    // Two parallel tool calls survive as distinct parts with stable ids.
    assert_eq!(req.messages.len(), 2);
    let calls: Vec<_> = req.messages[0]
        .parts
        .iter()
        .filter_map(|p| match p {
            Part::ToolCall { id, name, .. } => Some((id.clone(), name.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        calls,
        vec![
            (Some("call_a".into()), "get_weather".into()),
            (Some("call_b".into()), "get_time".into()),
        ]
    );
    // The tool result carries its tool_call_id.
    match &req.messages[1].parts[0] {
        Part::ToolResult {
            tool_call_id,
            name,
            content,
            ..
        } => {
            assert_eq!(tool_call_id, "call_a");
            assert_eq!(name.as_deref(), Some("get_weather"));
            assert_eq!(content, "18C");
        }
        other => panic!("expected tool result, got {other:?}"),
    }
    assert_eq!(req.tools.len(), 1);
    assert_eq!(req.tools[0].name, "get_weather");
}

#[test]
fn openai_image_data_url_is_detected() {
    let req = openai(
        r#"{
          "model": "gpt-x",
          "messages": [
            { "role": "user", "content": [
              { "type": "text", "text": "what is this" },
              { "type": "image_url", "image_url": { "url": "data:image/png;base64,QUJD" } }
            ]}
          ]
        }"#,
    );
    let parts = &req.messages[0].parts;
    assert!(matches!(parts[0], Part::Text(_)));
    match &parts[1] {
        Part::Image(kinetix::types::ImageData::Base64 { mime, data }) => {
            assert_eq!(mime, "image/png");
            assert_eq!(data, "QUJD");
        }
        other => panic!("expected a base64 image part, got {other:?}"),
    }
}

#[test]
fn openai_reasoning_effort_maps_to_thinking_level() {
    assert_eq!(
        openai(r#"{"model":"m","reasoning_effort":"high","messages":[]}"#).thinking,
        Some(ThinkingLevel::High)
    );
    assert_eq!(
        openai(r#"{"model":"m","reasoning_effort":"minimal","messages":[]}"#).thinking,
        Some(ThinkingLevel::Minimal)
    );
    assert_eq!(
        openai(r#"{"model":"m","reasoning_effort":"low","messages":[]}"#).thinking,
        Some(ThinkingLevel::Low)
    );
    assert_eq!(
        openai(r#"{"model":"m","reasoning_effort":"xhigh","messages":[]}"#).thinking,
        Some(ThinkingLevel::XHigh)
    );
    assert_eq!(
        openai(r#"{"model":"m","reasoning_effort":"max","messages":[]}"#).thinking,
        Some(ThinkingLevel::Max)
    );
    assert_eq!(
        openai(r#"{"model":"m","reasoning_effort":"none","messages":[]}"#).thinking,
        Some(ThinkingLevel::Off)
    );
    assert_eq!(
        responses(r#"{"model":"m","input":"hi","reasoning":{"effort":"max"}}"#).thinking,
        Some(ThinkingLevel::Max)
    );
    // Absent reasoning control means no thinking request (nothing invented).
    assert_eq!(openai(r#"{"model":"m","messages":[]}"#).thinking, None);
}

#[test]
fn openai_assistant_reasoning_history_is_retained_as_opaque_state() {
    let req = openai(
        r#"{
          "model":"m",
          "messages":[{
            "role":"assistant",
            "content":"answer",
            "reasoning_content":"private chain state",
            "reasoning_signature":"sig-r"
          }]
        }"#,
    );
    assert!(req.messages[0].parts.iter().any(|part| matches!(
        part,
        Part::Thinking { text, signature }
            if text == "private chain state" && signature.as_deref() == Some("sig-r")
    )));
}

#[test]
fn unsupported_nested_content_is_marked_for_translation_rejection() {
    let openai_file = openai(
        r#"{
          "model":"m",
          "messages":[{"role":"user","content":[{"type":"file","file_id":"file_1"}]}]
        }"#,
    );
    assert!(frontends::translation_unsupported(&openai_file.extra).is_some());

    let anthropic_tool_image = anthropic(
        r#"{
          "model":"m",
          "messages":[{
            "role":"user",
            "content":[{
              "type":"tool_result",
              "tool_use_id":"toolu_1",
              "content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"AA=="}}]
            }]
          }]
        }"#,
    );
    assert!(frontends::translation_unsupported(&anthropic_tool_image.extra).is_some());

    let responses_file = frontends::decode(
        FrontendFormat::OpenAiResponses,
        serde_json::json!({
            "model":"m",
            "input":[{"type":"file_search_call","id":"fs_1"}]
        }),
    );
    assert!(responses_file.is_err());
}

#[test]
fn openai_unknown_top_level_fields_are_captured_as_extra() {
    // FR-2.8/2.10: unknown fields are captured, not lost, so the translation
    // path can decide (reject/preserve) rather than silently dropping them.
    let req = openai(r#"{"model":"m","messages":[],"n":2,"logprobs":true,"my_vendor_flag":"x"}"#);
    assert_eq!(req.extra.get("n").and_then(|v| v.as_i64()), Some(2));
    assert_eq!(
        req.extra.get("logprobs").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        req.extra.get("my_vendor_flag").and_then(|v| v.as_str()),
        Some("x")
    );
    // Behaviorally-significant fields are rejected on a translating path...
    assert!(frontends::translation_unsupported(&req.extra).is_some());
    // ...but a purely cosmetic/unknown field alone is not.
    let cosmetic = openai(r#"{"model":"m","messages":[],"my_vendor_flag":"x"}"#);
    assert!(frontends::translation_unsupported(&cosmetic.extra).is_none());
}

#[test]
fn structured_response_formats_are_rejected_on_translating_paths() {
    // A json_object hint has no equivalent on other transports; dropping it
    // would silently change the output contract the client asked for.
    for format in [
        r#"{"type":"json_object"}"#,
        r#"{"type":"json_schema","json_schema":{"name":"a","schema":{"type":"object"}}}"#,
    ] {
        let req = openai(&format!(
            r#"{{"model":"m","messages":[],"response_format":{format}}}"#
        ));
        let issue = frontends::translation_unsupported(&req.extra)
            .unwrap_or_else(|| panic!("{format} must be rejected"));
        assert!(issue.contains("response_format"), "{issue}");
    }
    let plain = openai(r#"{"model":"m","messages":[],"response_format":{"type":"text"}}"#);
    assert!(frontends::translation_unsupported(&plain.extra).is_none());
}

#[test]
fn anthropic_plain_chat_tools_and_thinking_round_trip() {
    let req = anthropic(
        r#"{
          "model": "claude-x",
          "system": "be terse",
          "max_tokens": 64,
          "messages": [
            { "role": "user", "content": "hi" },
            { "role": "assistant", "content": [
                { "type": "thinking", "thinking": "hmm", "signature": "sig-1" },
                { "type": "tool_use", "id": "toolu_1", "name": "get_weather",
                  "input": { "city": "Paris" } }
            ]},
            { "role": "user", "content": [
                { "type": "tool_result", "tool_use_id": "toolu_1", "content": "18C" }
            ]}
          ],
          "tools": [
            { "name": "get_weather", "description": "w",
              "input_schema": { "type": "object" } }
          ],
          "thinking": { "type": "enabled", "budget_tokens": 4096 }
        }"#,
    );
    assert_eq!(req.system, vec!["be terse".to_string()]);
    assert_eq!(req.params.max_tokens, Some(64));
    // thinking budget 4096 -> Medium on the canonical scale.
    assert_eq!(req.thinking, Some(ThinkingLevel::Medium));
    // The opaque signature survives decode for later round-trip (FR-2.10).
    match &req.messages[1].parts[0] {
        Part::Thinking { text, signature } => {
            assert_eq!(text, "hmm");
            assert_eq!(signature.as_deref(), Some("sig-1"));
        }
        other => panic!("expected thinking part, got {other:?}"),
    }
    assert_eq!(req.tools[0].name, "get_weather");
}

#[test]
fn anthropic_output_format_is_marked_nonportable_for_translation() {
    let req = anthropic(
        r#"{
          "model":"claude-x",
          "messages":[{"role":"user","content":"hi"}],
          "output_config":{
            "format":{
              "type":"json_schema",
              "schema":{"type":"object","properties":{"answer":{"type":"string"}}}
            }
          }
        }"#,
    );

    let issue = frontends::translation_unsupported(&req.extra)
        .expect("structured output format must fail closed on translation");
    assert!(issue.contains("output_config.format"));
}

#[test]
fn anthropic_adaptive_effort_maps_to_extended_thinking_level() {
    let max = anthropic(
        r#"{
          "model":"claude-x",
          "messages":[{"role":"user","content":"hi"}],
          "thinking":{"type":"adaptive"},
          "output_config":{"effort":"max"}
        }"#,
    );
    assert_eq!(max.thinking, Some(ThinkingLevel::Max));
    assert!(max.extra.get("output_config").is_none());

    let defaulted = anthropic(
        r#"{
          "model":"claude-x",
          "messages":[{"role":"user","content":"hi"}],
          "thinking":{"type":"adaptive"}
        }"#,
    );
    assert_eq!(defaulted.thinking, Some(ThinkingLevel::Default));

    let effort_only = anthropic(
        r#"{
          "model":"claude-x",
          "messages":[{"role":"user","content":"hi"}],
          "output_config":{"effort":"medium"}
        }"#,
    );
    assert_eq!(effort_only.thinking, None);
    assert!(frontends::translation_unsupported(&effort_only.extra).is_some());
}

#[test]
fn responses_plain_string_input_and_instructions() {
    let req = responses(
        r#"{
          "model": "gpt-5",
          "instructions": "You are a helpful coding assistant.",
          "input": "Solve 2+2",
          "stream": true,
          "temperature": 0.2,
          "max_output_tokens": 256
        }"#,
    );
    assert_eq!(req.requested_model, "gpt-5");
    assert!(req.stream);
    assert_eq!(
        req.system,
        vec!["You are a helpful coding assistant.".to_string()]
    );
    assert_eq!(req.params.temperature, Some(0.2));
    assert_eq!(req.params.max_tokens, Some(256));
    assert_eq!(req.messages.len(), 1);
    assert_eq!(req.messages[0].role, Role::User);
    match &req.messages[0].parts[0] {
        Part::Text(t) => assert_eq!(t, "Solve 2+2"),
        other => panic!("expected text part, got {other:?}"),
    }
}

#[test]
fn responses_prompt_cache_key_is_preserved_as_portable_extension() {
    let req = responses(
        r#"{
          "model":"gpt-5",
          "input":"hi",
          "prompt_cache_key":"session-123"
        }"#,
    );
    assert_eq!(
        req.extra
            .get("prompt_cache_key")
            .and_then(|value| value.as_str()),
        Some("session-123")
    );

    let invalid = frontends::decode(
        FrontendFormat::OpenAiResponses,
        serde_json::json!({
            "model":"gpt-5",
            "input":"hi",
            "prompt_cache_key":123
        }),
    );
    assert!(invalid.is_err());
}

#[test]
fn responses_rejects_stateful_hosted_and_untranslated_semantics() {
    let cases = [
        serde_json::json!({
            "model":"m",
            "input":"hi",
            "previous_response_id":"resp_prev"
        }),
        serde_json::json!({
            "model":"m",
            "input":"hi",
            "tools":[{"type":"web_search"}]
        }),
        serde_json::json!({
            "model":"m",
            "input":"hi",
            "include":["reasoning.encrypted_content"]
        }),
        serde_json::json!({
            "model":"m",
            "input":"hi",
            "text":{"format":{"type":"json_schema","name":"x","schema":{"type":"object"}}}
        }),
        serde_json::json!({
            "model":"m",
            "input":"hi",
            "reasoning":{"effort":"high","summary":5}
        }),
        serde_json::json!({
            "model":"m",
            "input":"hi",
            "reasoning":{"generate_summary":"auto"}
        }),
        serde_json::json!({
            "model":"m",
            "input":"hi",
            "tools":[{"type":"function","name":"f","parameters":{},"strict":true}]
        }),
        serde_json::json!({
            "model":"m",
            "input":"hi",
            "unknown_future_semantic":{"enabled":true}
        }),
    ];

    for body in cases {
        let result = frontends::decode(FrontendFormat::OpenAiResponses, body);
        assert!(
            result.is_err(),
            "unsupported Responses semantic was accepted"
        );
    }
}

#[test]
fn responses_reasoning_summary_is_same_format_only() {
    let req =
        responses(r#"{"model":"m","input":"hi","reasoning":{"effort":"high","summary":"auto"}}"#);
    assert_eq!(req.thinking, Some(kinetix::types::ThinkingLevel::High));
    let message = frontends::translation_unsupported(&req.extra).expect("translation refuses it");
    assert!(message.contains("reasoning.summary"), "{message}");

    let plain = responses(r#"{"model":"m","input":"hi","reasoning":{"effort":"high"}}"#);
    assert!(frontends::translation_unsupported(&plain.extra).is_none());
}

#[test]
fn responses_accepts_explicit_safe_defaults() {
    let req = responses(
        r#"{
          "model":"m",
          "input":"hi",
          "store":false,
          "background":false,
          "truncation":"disabled",
          "text":{"format":{"type":"text"}},
          "stream_options":{"include_obfuscation":false}
        }"#,
    );
    assert_eq!(req.requested_model, "m");
    assert_eq!(req.messages.len(), 1);
}

#[test]
fn responses_multi_turn_with_function_call_and_output() {
    let req = responses(
        r#"{
          "model": "gpt-5",
          "input": [
            { "type": "message", "role": "user", "content": "What is the weather in Berlin?" },
            { "type": "function_call", "call_id": "call_berlin", "name": "get_weather", "arguments": "{\"city\":\"Berlin\"}" },
            { "type": "function_call_output", "call_id": "call_berlin", "output": "{\"temp\":\"15C\"}" }
          ],
          "tools": [
            { "type": "function", "name": "get_weather", "description": "Fetch weather", "parameters": { "type": "object" } }
          ],
          "reasoning": { "effort": "high" }
        }"#,
    );
    assert_eq!(req.requested_model, "gpt-5");
    assert_eq!(req.thinking, Some(ThinkingLevel::High));
    assert_eq!(req.tools.len(), 1);
    assert_eq!(req.tools[0].name, "get_weather");
    assert_eq!(req.messages.len(), 3);
    assert_eq!(req.messages[0].role, Role::User);
    assert_eq!(req.messages[1].role, Role::Assistant);
    match &req.messages[1].parts[0] {
        Part::ToolCall {
            id,
            name,
            arguments,
            ..
        } => {
            assert_eq!(id.as_deref(), Some("call_berlin"));
            assert_eq!(name, "get_weather");
            assert_eq!(arguments, "{\"city\":\"Berlin\"}");
        }
        other => panic!("expected ToolCall, got {other:?}"),
    }
    assert_eq!(req.messages[2].role, Role::Tool);
    match &req.messages[2].parts[0] {
        Part::ToolResult {
            tool_call_id,
            name,
            content,
            ..
        } => {
            assert_eq!(tool_call_id, "call_berlin");
            assert_eq!(name.as_deref(), Some("get_weather"));
            assert_eq!(content, "{\"temp\":\"15C\"}");
        }
        other => panic!("expected ToolResult, got {other:?}"),
    }
}

#[test]
fn decode_rejects_malformed_bodies_with_a_format_native_error() {
    // A body that is valid JSON but has the wrong shape must be a bad_request,
    // never a panic.
    let err = frontends::decode(
        FrontendFormat::OpenAi,
        serde_json::json!({ "messages": "not-an-array" }),
    );
    assert!(err.is_err());

    let err_resp = frontends::decode(
        FrontendFormat::OpenAiResponses,
        serde_json::json!({ "input": 12345 }),
    );
    assert!(err_resp.is_err());
}
