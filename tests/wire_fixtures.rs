//! Golden wire-output fixtures (NFR-5.5, FR-9.1, FR-9.6).
//!
//! These integration tests drive the public encoders/decoders directly and
//! assert their exact wire output. They are the guardrail for supported wire
//! behavior: if a change alters the bytes Kinetix emits for a supported path,
//! the corresponding fixture here fails and must be updated deliberately
//! (FR-9.6: every regression that changes supported wire output requires a
//! fixture demonstrating the intended behavior).
//!
//! Covered: OpenAI inbound streaming encode, Anthropic inbound streaming
//! encode, OpenAI/Anthropic non-streaming aggregation, format-native error
//! frames, and format-native error bodies.

use kinetix::frontends::{self, EncoderCtx, FrontendFormat};
use kinetix::types::{FinishReason, ProxyError, StreamEvent, TokenUsage};

fn ctx() -> EncoderCtx {
    // A fixed request id + created time makes ids/timestamps deterministic.
    EncoderCtx {
        model_name: "test-model".into(),
        request_id: "req_fixture".into(),
        created: 1_700_000_000,
        responses: frontends::ResponsesResponseFields::default(),
    }
}

/// Concatenate encoder output frames into one string for golden comparison
/// (frames are already newline-terminated).
fn joined(frames: &[bytes::Bytes]) -> String {
    frames
        .iter()
        .map(|b| String::from_utf8_lossy(b).to_string())
        .collect()
}

fn text_and_finish_events() -> Vec<StreamEvent> {
    vec![
        StreamEvent::Start {
            upstream_request_id: Some("up-1".into()),
        },
        StreamEvent::TextDelta("Hello".into()),
        StreamEvent::TextDelta(", world".into()),
        StreamEvent::Usage(TokenUsage {
            input: Some(5),
            output: Some(3),
            cached: None,
            cache_write: None,
            thinking: None,
        }),
        StreamEvent::Finish(FinishReason::Stop),
    ]
}

#[test]
fn openai_streaming_wire_output_is_stable() {
    let mut enc = frontends::Encoder::new(FrontendFormat::OpenAi, ctx());
    let mut out = Vec::new();
    for ev in text_and_finish_events() {
        out.extend(enc.encode(ev));
    }
    out.extend(enc.finalize());

    let expected = concat!(
        "data: {\"id\":\"chatcmpl-reqfixture\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,",
        "\"model\":\"test-model\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"},",
        "\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl-reqfixture\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,",
        "\"model\":\"test-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl-reqfixture\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,",
        "\"model\":\"test-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\", world\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl-reqfixture\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,",
        "\"model\":\"test-model\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"id\":\"chatcmpl-reqfixture\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,",
        "\"model\":\"test-model\",\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":3,\"total_tokens\":8}}\n\n",
        "data: [DONE]\n\n",
    );
    assert_eq!(
        joined(&out),
        expected,
        "OpenAI streaming wire output changed"
    );
}

#[test]
fn anthropic_streaming_wire_output_is_stable() {
    let mut enc = frontends::Encoder::new(FrontendFormat::Anthropic, ctx());
    let mut out = Vec::new();
    for ev in text_and_finish_events() {
        out.extend(enc.encode(ev));
    }
    out.extend(enc.finalize());

    let expected = concat!(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_reqfixture\",",
        "\"type\":\"message\",\"role\":\"assistant\",\"model\":\"test-model\",\"content\":[],",
        "\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":0,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,",
        "\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,",
        "\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,",
        "\"delta\":{\"type\":\"text_delta\",\"text\":\", world\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",",
        "\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":3}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    );
    assert_eq!(
        joined(&out),
        expected,
        "Anthropic streaming wire output changed"
    );
}

#[test]
fn openai_non_streaming_aggregate_is_stable() {
    let mut body = frontends::aggregate(
        FrontendFormat::OpenAi,
        "test-model",
        "req_fixture",
        text_and_finish_events(),
        &TokenUsage {
            input: Some(5),
            output: Some(3),
            cached: None,
            cache_write: None,
            thinking: None,
        },
    );
    // `created` is wall-clock at aggregation time; normalize before comparing.
    assert!(body["created"].is_i64(), "created must be an integer");
    body["created"] = serde_json::json!(0);

    let expected: serde_json::Value = serde_json::from_str(
        r#"{
          "id": "chatcmpl-reqfixture",
          "object": "chat.completion",
          "created": 0,
          "model": "test-model",
          "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": "Hello, world" },
            "finish_reason": "stop"
          }],
          "usage": { "prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8 }
        }"#,
    )
    .unwrap();
    assert_eq!(body, expected, "OpenAI non-streaming aggregate changed");
}

#[test]
fn anthropic_non_streaming_aggregate_is_stable() {
    let body = frontends::aggregate(
        FrontendFormat::Anthropic,
        "test-model",
        "req_fixture",
        text_and_finish_events(),
        &TokenUsage {
            input: Some(5),
            output: Some(3),
            cached: None,
            cache_write: None,
            thinking: None,
        },
    );
    // The Anthropic aggregate emits one text block per text delta (multiple
    // text blocks are valid Anthropic content). Locked as current behavior.
    let expected: serde_json::Value = serde_json::from_str(
        r#"{
          "id": "msg_reqfixture",
          "type": "message",
          "role": "assistant",
          "model": "test-model",
          "content": [
            { "type": "text", "text": "Hello" },
            { "type": "text", "text": ", world" }
          ],
          "stop_reason": "end_turn",
          "stop_sequence": null,
          "usage": { "input_tokens": 5, "output_tokens": 3 }
        }"#,
    )
    .unwrap();
    assert_eq!(body, expected, "Anthropic non-streaming aggregate changed");
}

#[test]
fn anthropic_aggregate_maps_inclusive_input_back_to_native_cache_fields() {
    let body = frontends::aggregate(
        FrontendFormat::Anthropic,
        "test-model",
        "req_fixture",
        vec![StreamEvent::Finish(FinishReason::Stop)],
        &TokenUsage {
            input: Some(200),
            output: Some(25),
            cached: Some(70),
            cache_write: Some(30),
            thinking: None,
        },
    );

    assert_eq!(body["usage"]["input_tokens"], 100);
    assert_eq!(body["usage"]["cache_read_input_tokens"], 70);
    assert_eq!(body["usage"]["cache_creation_input_tokens"], 30);
    assert_eq!(body["usage"]["output_tokens"], 25);
}

#[test]
fn openai_error_frame_marks_the_stream_as_failed() {
    // FR-4.6/NFR-2.9: a post-commit failure must not look like a clean finish.
    let mut enc = frontends::Encoder::new(FrontendFormat::OpenAi, ctx());
    let got = joined(&enc.error_frame("upstream stream interrupted"));
    assert!(
        got.contains("\"type\":\"upstream_error\""),
        "error object missing: {got}"
    );
    assert!(
        got.contains("\"finish_reason\":\"error\""),
        "explicit error finish_reason missing: {got}"
    );
    assert!(
        got.ends_with("data: [DONE]\n\n"),
        "stream must be terminated with [DONE]: {got}"
    );
}

#[test]
fn anthropic_error_frame_is_terminal_and_does_not_emit_message_stop() {
    let mut enc = frontends::Encoder::new(FrontendFormat::Anthropic, ctx());
    let got = joined(&enc.error_frame("upstream stream interrupted"));
    assert!(
        got.starts_with("event: error\n"),
        "missing error event: {got}"
    );
    assert!(
        got.contains("\"type\":\"api_error\""),
        "missing api_error type: {got}"
    );
    assert!(
        !got.contains("message_stop"),
        "an error event is terminal; message_stop must not follow: {got}"
    );
}

#[test]
fn responses_streaming_wire_output_is_stable() {
    let mut enc = frontends::Encoder::new(FrontendFormat::OpenAiResponses, ctx());
    let mut out = Vec::new();
    for ev in text_and_finish_events() {
        out.extend(enc.encode(ev));
    }
    out.extend(enc.finalize());

    let joined_out = joined(&out);
    assert!(
        joined_out.contains("event: response.created\n"),
        "missing response.created event"
    );
    assert!(
        joined_out.contains("event: response.in_progress\n"),
        "missing response.in_progress event"
    );
    assert!(
        joined_out.contains("event: response.output_item.added\n"),
        "missing response.output_item.added event"
    );
    assert!(
        joined_out.contains("event: response.output_text.delta\n"),
        "missing response.output_text.delta event"
    );
    assert!(
        joined_out.contains("\"delta\":\"Hello\""),
        "missing Hello delta"
    );
    assert!(
        joined_out.contains("event: response.completed\n"),
        "missing response.completed event"
    );
    assert!(
        joined_out.contains("\"status\":\"completed\""),
        "missing status completed in response object"
    );
    assert!(
        !joined_out.contains("[DONE]"),
        "Responses streaming must terminate with response.completed, not Chat Completions [DONE]"
    );
    let in_progress = joined_out
        .split("event: response.in_progress\n")
        .nth(1)
        .and_then(|rest| rest.split("\n\n").next())
        .expect("response.in_progress frame");
    assert!(
        in_progress.contains("\"response\":{"),
        "response.in_progress must carry a response object"
    );
    assert!(
        !in_progress.contains("\"response_id\":"),
        "response.in_progress must not use the old response_id-only shape"
    );
}

#[test]
fn responses_stream_does_not_expose_raw_provider_reasoning() {
    let mut enc = frontends::Encoder::new(FrontendFormat::OpenAiResponses, ctx());
    let mut out = Vec::new();
    out.extend(enc.encode(StreamEvent::Start {
        upstream_request_id: Some("up-1".into()),
    }));
    out.extend(enc.encode(StreamEvent::ThinkingDelta {
        block_index: None,
        text: "private provider reasoning".into(),
        signature: Some("opaque".into()),
    }));
    out.extend(enc.encode(StreamEvent::TextDelta("answer".into())));
    out.extend(enc.encode(StreamEvent::Finish(FinishReason::Stop)));

    let got = joined(&out);
    assert!(!got.contains("response.reasoning_text"));
    assert!(!got.contains("private provider reasoning"));
    assert!(got.contains("response.output_text.delta"));
    assert!(got.contains("response.completed"));
}

#[test]
fn responses_non_streaming_aggregation_is_stable() {
    let usage = TokenUsage {
        input: Some(10),
        output: Some(5),
        cached: Some(2),
        cache_write: Some(3),
        thinking: Some(4),
    };
    let agg = frontends::aggregate(
        FrontendFormat::OpenAiResponses,
        "test-model",
        "req_fixture",
        text_and_finish_events(),
        &usage,
    );
    assert_eq!(agg["object"], "response");
    assert_eq!(agg["status"], "completed");
    assert_eq!(agg["model"], "test-model");
    assert_eq!(agg["id"], "resp_reqfixture");
    let output = agg["output"].as_array().expect("output array");
    assert_eq!(output.len(), 1);
    assert_eq!(output[0]["type"], "message");
    assert_eq!(output[0]["role"], "assistant");
    assert_eq!(output[0]["content"][0]["text"], "Hello, world");
    assert_eq!(agg["usage"]["total_tokens"], 15);
    assert_eq!(agg["usage"]["input_tokens"], 10);
    assert_eq!(agg["usage"]["output_tokens"], 5);
    assert_eq!(agg["usage"]["input_tokens_details"]["cached_tokens"], 2);
    assert_eq!(
        agg["usage"]["input_tokens_details"]["cache_write_tokens"],
        3
    );
    assert_eq!(agg["usage"]["output_tokens_details"]["reasoning_tokens"], 4);
}

#[test]
fn responses_error_frame_is_native() {
    let mut enc = frontends::Encoder::new(FrontendFormat::OpenAiResponses, ctx());
    let got = joined(&enc.error_frame("upstream failed"));
    assert!(
        got.starts_with("event: response.failed\n"),
        "missing response.failed event: {got}"
    );
    assert!(
        got.contains("\"type\":\"response.failed\""),
        "missing type field: {got}"
    );
    let failed = got
        .split("event: response.failed\n")
        .nth(1)
        .and_then(|rest| rest.split("\n\n").next())
        .expect("response.failed frame");
    assert!(
        failed.contains("\"response\":{"),
        "response.failed must carry the failed response object: {got}"
    );
    assert!(
        got.contains("\"status\":\"failed\""),
        "failed response status missing: {got}"
    );
}

#[test]
fn format_native_error_bodies_are_stable() {
    // A 429 rate-limit error maps to the correct per-format error body shape.
    let err = ProxyError::rate_limited("slow down", Some(7));
    let openai = frontends::models::error_body(FrontendFormat::OpenAi, &err);
    assert_eq!(openai["error"]["type"], "rate_limit_error");
    assert_eq!(openai["error"]["message"], "slow down");

    let anthropic = frontends::models::error_body(FrontendFormat::Anthropic, &err);
    assert_eq!(anthropic["type"], "error");
    assert_eq!(anthropic["error"]["type"], "rate_limit_error");

    // An upstream failure is upstream_error for OpenAI and api_error for
    // Anthropic.
    let up = ProxyError::upstream("boom");
    assert_eq!(
        frontends::models::error_body(FrontendFormat::OpenAi, &up)["error"]["type"],
        "upstream_error"
    );
    assert_eq!(
        frontends::models::error_body(FrontendFormat::Anthropic, &up)["error"]["type"],
        "api_error"
    );
}
