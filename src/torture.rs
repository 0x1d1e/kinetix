//! Protocol torture + fuzz tests (FR-9.4, FR-9.5).
//!
//! These run in internal CI. They exercise the SSE framer and the outbound
//! adapters against adversarial transport chunking and malformed input, with
//! bounded resources (no unbounded buffers, no panics).

#![cfg(test)]

use crate::adapters::Adapter;
use crate::sse::SseFramer;
use crate::types::StreamEvent;
use serde_json::Value;

/// Feed `input` to `framer` in fixed-size chunks and collect all frames.
fn frames_in_chunks(input: &str, size: usize) -> Vec<String> {
    let mut f = SseFramer::new();
    let bytes = input.as_bytes();
    let mut out = Vec::new();
    for chunk in bytes.chunks(size.max(1)) {
        out.extend(f.push(chunk).unwrap());
    }
    out
}

fn gemini_events(input: &str, size: usize) -> Vec<StreamEvent> {
    let adapter = crate::adapters::gemini::GeminiAdapter;
    let mut evs = Vec::new();
    for frame in frames_in_chunks(input, size) {
        if let Some(payload) = crate::sse::extract_data(&frame) {
            evs.extend(adapter.parse_stream_chunk(&payload).unwrap_or_default());
        }
    }
    evs
}

fn openai_events(input: &str, size: usize) -> Vec<StreamEvent> {
    let adapter = crate::adapters::openai::OpenAiAdapter;
    let mut evs = Vec::new();
    for frame in frames_in_chunks(input, size) {
        if let Some(payload) = crate::sse::extract_data(&frame) {
            evs.extend(adapter.parse_stream_chunk(&payload).unwrap_or_default());
        }
    }
    evs
}

#[test]
fn gemini_text_survives_every_chunk_boundary() {
    let input = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hello world\"}]}}]}\n\n\
                 data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"!\"}]},\"finishReason\":\"STOP\"}],\
                 \"usageMetadata\":{\"promptTokenCount\":5,\"candidatesTokenCount\":3}}\n\n";
    // The reassembled text must be identical regardless of transport chunking.
    for size in 1..=64 {
        let evs = gemini_events(input, size);
        let text: String = evs
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello world!", "chunk size {size}");
    }
}

#[test]
fn gemini_tool_call_args_object() {
    // Gemini delivers functionCall args as a JSON object; the event stream must
    // carry the name and a faithful serialization of the args regardless of
    // transport chunking.
    let input = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"get_weather\",\"args\":{\"city\":\"Paris\"}}}]}}]}\n\n";
    for size in [1usize, 3, 7, 64] {
        let evs = gemini_events(input, size);
        let mut name = String::new();
        let mut args = String::new();
        for e in &evs {
            match e {
                StreamEvent::ToolCallStart { name: n, .. } => name = n.clone(),
                StreamEvent::ToolCallArgsDelta { args: a, .. } => args = a.clone(),
                _ => {}
            }
        }
        assert_eq!(name, "get_weather", "chunk size {size}");
        let parsed: serde_json::Value = serde_json::from_str(&args).expect("args is JSON");
        assert_eq!(parsed["city"], "Paris", "chunk size {size}");
    }
}

#[test]
fn gemini_usage_only_in_final_event() {
    // No usageMetadata until the very last frame; the framer must still deliver
    // the final usage exactly once.
    let input = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hi\"}]}}]}\n\n\
                 data: {\"candidates\":[{\"content\":{\"parts\":[]},\"finishReason\":\"STOP\"}],\
                 \"usageMetadata\":{\"promptTokenCount\":11,\"candidatesTokenCount\":2,\"thoughtsTokenCount\":1}}\n\n";
    for size in [1usize, 5, 200] {
        let evs = gemini_events(input, size);
        let usage = evs.iter().find_map(|e| match e {
            StreamEvent::Usage(u) => Some(u.clone()),
            _ => None,
        });
        let u = usage.expect("usage present");
        assert_eq!(u.input, Some(11));
        assert_eq!(u.output, Some(3));
        assert_eq!(u.thinking, Some(1));
    }
}

#[test]
fn malformed_and_unknown_fields_do_not_panic() {
    let inputs = [
        "data: not json at all\n\n",
        "data: {\"candidates\":[{}]}\n\n",
        "data: {}\n\n",
        "data: [DONE]\n\n",
        ": keepalive\n\n",
        "event: weird\ndata: {\"unknown\":true,\"extra\":[1,2,3]}\n\n",
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"unknownPart\":{\"x\":1}}]}}]}\n\n",
    ];
    for input in inputs {
        for size in [1usize, 2, 100] {
            // Must not panic; unknown frames simply yield no events.
            let _ = gemini_events(input, size);
            let _ = openai_events(input, size);
        }
    }
}

#[test]
fn openai_tool_args_split_across_frames() {
    let input = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"f\",\"arguments\":\"{\\\"a\\\":\"}}]}}]}\n\n\
                 data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"1}\"}}]}}]}\n\n\
                 data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
                 data: [DONE]\n\n";
    for size in 1..=48 {
        let evs = openai_events(input, size);
        let mut args = String::new();
        for e in &evs {
            if let StreamEvent::ToolCallArgsDelta { args: delta, .. } = e {
                args.push_str(delta);
            }
        }
        assert_eq!(args, "{\"a\":1}", "chunk size {size}");
    }
}

/// Bounded pseudo-random fuzz: random bytes and random re-chunking of a valid
/// stream must never panic or grow unboundedly.
#[test]
fn fuzz_bounded_resources() {
    let mut state: u64 = 0x9E3779B97F4A7C15;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };

    // 1. Random garbage bytes must not panic.
    for _ in 0..500 {
        let len = (next() % 256) as usize;
        let bytes: Vec<u8> = (0..len).map(|_| (next() % 256) as u8).collect();
        let mut f = SseFramer::new();
        let _ = f.push(&bytes);
        // Buffer must never retain more than the last (incomplete) frame.
        assert!(f.pending_bytes() <= crate::sse::DEFAULT_MAX_FRAME_BYTES);
    }

    // 2. Random re-chunking of a valid stream must reassemble deterministically.
    let valid = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"abcdefghij\"}]}}]}\n\n\
                 data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"klmnopqrst\"}]},\"finishReason\":\"STOP\"}]}\n\n";
    let expected_text = "abcdefghijklmnopqrst";
    for _ in 0..300 {
        let mut f = SseFramer::new();
        let bytes = valid.as_bytes();
        let mut i = 0;
        let mut frames = Vec::new();
        while i < bytes.len() {
            let step = (next() % 5 + 1) as usize;
            let end = (i + step).min(bytes.len());
            frames.extend(f.push(&bytes[i..end]).unwrap());
            i = end;
        }
        let adapter = crate::adapters::gemini::GeminiAdapter;
        let mut text = String::new();
        for frame in frames {
            if let Some(p) = crate::sse::extract_data(&frame) {
                for ev in adapter.parse_stream_chunk(&p).unwrap_or_default() {
                    if let StreamEvent::TextDelta(t) = ev {
                        text.push_str(&t);
                    }
                }
            }
        }
        assert_eq!(text, expected_text);
    }
}

#[test]
fn interleaved_parallel_tool_calls() {
    // Two tool calls whose argument fragments interleave across frames
    // (FR-9.4). Reassembly must key on the tool-call index, not arrival order.
    let input = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c0\",\"function\":{\"name\":\"f0\",\"arguments\":\"{\\\"a\\\":\"}},{\"index\":1,\"id\":\"c1\",\"function\":{\"name\":\"f1\",\"arguments\":\"{\\\"b\\\":\"}}]}}]}\n\n\
                 data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"function\":{\"arguments\":\"2}\"}},{\"index\":0,\"function\":{\"arguments\":\"1}\"}}]}}]}\n\n\
                 data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
                 data: [DONE]\n\n";
    for size in 1..=40 {
        let evs = openai_events(input, size);
        let mut names = std::collections::BTreeMap::new();
        let mut args: std::collections::BTreeMap<u32, String> = Default::default();
        for e in &evs {
            match e {
                StreamEvent::ToolCallStart { index, name, .. } => {
                    names.insert(*index, name.clone());
                }
                StreamEvent::ToolCallArgsDelta { index, args: a } => {
                    args.entry(*index).or_default().push_str(a);
                }
                _ => {}
            }
        }
        assert_eq!(names.get(&0).map(String::as_str), Some("f0"), "size {size}");
        assert_eq!(names.get(&1).map(String::as_str), Some("f1"), "size {size}");
        assert_eq!(
            args.get(&0).map(String::as_str),
            Some("{\"a\":1}"),
            "size {size}"
        );
        assert_eq!(
            args.get(&1).map(String::as_str),
            Some("{\"b\":2}"),
            "size {size}"
        );
    }
}

#[test]
fn reasoning_and_text_interleaving() {
    // Reasoning and visible text may alternate; both streams must be preserved
    // in order (FR-9.4).
    let input = "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"think \"}}]}\n\n\
                 data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\n\
                 data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"more\"}}]}\n\n\
                 data: {\"choices\":[{\"delta\":{\"content\":\"b\"}}]}\n\n\
                 data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n";
    for size in 1..=32 {
        let evs = openai_events(input, size);
        let mut text = String::new();
        let mut think = String::new();
        for e in &evs {
            match e {
                StreamEvent::TextDelta(t) => text.push_str(t),
                StreamEvent::ThinkingDelta { text: t, .. } => think.push_str(t),
                _ => {}
            }
        }
        assert_eq!(text, "ab", "size {size}");
        assert_eq!(think, "think more", "size {size}");
    }
}

#[test]
fn zero_token_response_is_not_invented() {
    // A model that emits no content and reports zero completion tokens must not
    // produce fabricated text or coerced usage (FR-6.2).
    let input = "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\
                 \"usage\":{\"prompt_tokens\":7,\"completion_tokens\":0}}\n\n\
                 data: [DONE]\n\n";
    for size in [1usize, 3, 500] {
        let evs = openai_events(input, size);
        assert!(
            !evs.iter().any(|e| matches!(e, StreamEvent::TextDelta(_))),
            "size {size}"
        );
        let usage = evs
            .iter()
            .find_map(|e| match e {
                StreamEvent::Usage(u) => Some(u.clone()),
                _ => None,
            })
            .expect("usage present");
        assert_eq!(usage.input, Some(7));
        assert_eq!(usage.output, Some(0));
    }
}

#[test]
fn large_tool_call_reassembles_across_many_frames() {
    // A large tool-call argument delivered in many small fragments (FR-9.4).
    let payload: String = "x".repeat(4000);
    let mut stream = String::new();
    stream.push_str("data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"big\",\"function\":{\"name\":\"f\",\"arguments\":\"{\\\"d\\\":\\\"\"}}]}}]}\n\n");
    for chunk in payload.as_bytes().chunks(97) {
        let frag = std::str::from_utf8(chunk).unwrap();
        stream.push_str(&format!(
            "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,\"function\":{{\"arguments\":\"{frag}\"}}}}]}}}}]}}\n\n"
        ));
    }
    stream.push_str("data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"}\"}}]}}]}\n\n");
    stream.push_str("data: [DONE]\n\n");

    let evs = openai_events(&stream, 64);
    let args: String = evs
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolCallArgsDelta { args, .. } => Some(args.clone()),
            _ => None,
        })
        .collect();
    let v: Value = serde_json::from_str(&args).expect("valid JSON");
    assert_eq!(v["d"].as_str().unwrap().len(), 4000);
}

#[test]
fn anthropic_tool_args_split_across_frames() {
    // Anthropic input_json_delta fragments split at arbitrary byte boundaries.
    let input = "event: content_block_start\n\
                 data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tu_1\",\"name\":\"f\"}}\n\n\
                 event: content_block_delta\n\
                 data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"a\\\":\"}}\n\n\
                 event: content_block_delta\n\
                 data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"1}\"}}\n\n";
    for size in 1..=48 {
        let adapter = crate::adapters::anthropic::AnthropicAdapter;
        let mut name = None;
        let mut args = String::new();
        for frame in frames_in_chunks(input, size) {
            if let Some(p) = crate::sse::extract_data(&frame) {
                for ev in adapter.parse_stream_chunk(&p).unwrap_or_default() {
                    match ev {
                        StreamEvent::ToolCallStart { name: n, .. } => name = Some(n),
                        StreamEvent::ToolCallArgsDelta { args: a, .. } => args.push_str(&a),
                        _ => {}
                    }
                }
            }
        }
        assert_eq!(name.as_deref(), Some("f"), "size {size}");
        assert_eq!(args, "{\"a\":1}", "size {size}");
    }
}

#[test]
fn keepalive_cadence_well_under_cloudflare_idle_timeout() {
    // FR-9.4: silent-thinking phases must be kept alive under the ~100s idle
    // window that Cloudflare enforces on proxied connections.
    const {
        assert!(
            crate::pipeline::KEEPALIVE_INTERVAL_SECS >= 1
                && crate::pipeline::KEEPALIVE_INTERVAL_SECS < 100,
            "keepalive interval must fit inside the ~100s Cloudflare idle timeout"
        );
    }
}

/// A synthetic upstream that emits keepalive comments while it "thinks" for
/// longer than the Cloudflare idle window, then produces a final answer. The
/// framer must pass the comments through and still deliver the answer.
#[test]
fn long_silent_thinking_with_keepalives() {
    let mut stream = String::new();
    // ~125s of silent thinking represented as keepalive comments, emitted at the
    // production keepalive cadence.
    let ticks = 125 / crate::pipeline::KEEPALIVE_INTERVAL_SECS;
    for _ in 0..ticks {
        stream.push_str(": keepalive\n\n");
    }
    stream.push_str("data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"done\"}]},\"finishReason\":\"STOP\"}]}\n\n");
    for size in [1usize, 7, 4096] {
        let evs = gemini_events(&stream, size);
        let text: String = evs
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "done", "size {size}");
    }
}

/// FR-9.4: an upstream 429 received before the commit point is a key-level
/// failure, so a Route must fall over to the next target rather than surfacing
/// the error. This exercises the classification that drives fallback.
#[test]
fn rate_limit_before_commit_is_key_level() {
    let adapter = crate::adapters::openai::OpenAiAdapter;
    let body = r#"{"error":{"message":"Rate limit reached for gpt-x","type":"rate_limit_error"}}"#;
    let f = adapter.classify_error(429, body, &reqwest::header::HeaderMap::new());
    assert_eq!(f.kind, crate::types::FailureKind::RateLimit);
    assert!(
        f.kind.is_key_level(),
        "a 429 must be key-level so Route fallback applies"
    );
}

/// FR-9.4: a retryable 5xx before commit may fall back, but it is
/// request-local and must not mutate account health.
#[test]
fn server_error_before_commit_is_request_local() {
    let adapter = crate::adapters::openai::OpenAiAdapter;
    let f = adapter.classify_error(
        503,
        "upstream unavailable",
        &reqwest::header::HeaderMap::new(),
    );
    assert_eq!(f.kind, crate::types::FailureKind::ServerError);
    assert!(f.kind.is_retryable());
    assert!(!f.kind.is_key_level());
}

/// FR-9.4: timeout/connection failures are retryable but request-local.
#[test]
fn connection_failure_is_request_local() {
    for kind in [
        crate::types::FailureKind::Timeout,
        crate::types::FailureKind::ConnectionError,
    ] {
        assert!(kind.is_retryable());
        assert!(!kind.is_key_level());
    }
}

/// FR-9.4/FR-4.8: a request-level 400 (invalid request) is NOT key-level, so it
/// must not trigger fallback — it is returned to the client as-is.
#[test]
fn bad_request_does_not_trigger_fallback() {
    let adapter = crate::adapters::openai::OpenAiAdapter;
    let f = adapter.classify_error(400, "invalid request", &reqwest::header::HeaderMap::new());
    assert_eq!(f.kind, crate::types::FailureKind::BadRequest);
    assert!(!f.kind.is_key_level(), "400 must not fall back");
}

/// FR-9.4/FR-12.7: a 429 carrying a short retry hint is a rate limit (short
/// cooldown), whereas an explicit quota message is exhaustion (benched until
/// reset) — the two must be distinguishable.
#[test]
fn rate_limit_and_quota_are_distinguished() {
    let adapter = crate::adapters::gemini::GeminiAdapter;
    let mut h = reqwest::header::HeaderMap::new();
    h.insert("retry-after", "3".parse().unwrap());
    let rl = adapter.classify_error(
        429,
        r#"{"error":{"message":"Quota exceeded for metric","details":[{"retryDelay":"3s"}]}}"#,
        &h,
    );
    assert_eq!(rl.kind, crate::types::FailureKind::RateLimit);
    let mut h2 = reqwest::header::HeaderMap::new();
    h2.insert("retry-after", "86400".parse().unwrap());
    let q = adapter.classify_error(
        429,
        r#"{"error":{"message":"Quota exceeded for quota metric: free_tier per_day"}}"#,
        &h2,
    );
    assert_eq!(q.kind, crate::types::FailureKind::QuotaExhausted);
}
