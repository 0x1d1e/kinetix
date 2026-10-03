//! Golden request contract tests for the plugin-adapter host boundary.

use kinetix::frontends::{self, FrontendFormat};
use kinetix::plugins::adapter::request_to_json;
use kinetix::types::{Part, ThinkingLevel};
use serde_json::{json, Value};

const MIXED: &str =
    include_str!("../wit/fixtures/plugin-request/v1/mixed-vision-tools-reasoning.json");

#[test]
fn mixed_vision_tools_and_reasoning_match_v1_plugin_request_contract() {
    let body = json!({
        "model": "plugin-model",
        "stream": true,
        "stream_options": {"include_usage": true},
        "reasoning_effort": "high",
        "messages": [
            {"role": "system", "content": "system"},
            {
                "role": "user",
                "content": [
                    {"type": "text", "text": "fixture:plugin-mixed inspect image"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,QUJD"}}
                ]
            },
            {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_plugin_1",
                    "type": "function",
                    "function": {
                        "name": "read_file",
                        "arguments": "{\"path\":\"src/main.rs\"}"
                    }
                }]
            },
            {
                "role": "tool",
                "tool_call_id": "call_plugin_1",
                "content": "fn main() {}"
            }
        ],
        "tools": [{
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read a file",
                "parameters": {
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"]
                }
            },
            "defer_loading": true
        }]
    });

    let req = frontends::decode(FrontendFormat::OpenAi, body).expect("decode mixed request");
    assert_eq!(req.thinking, Some(ThinkingLevel::High));
    assert!(frontends::translation_unsupported(&req.extra).is_none());

    let result_name = req
        .messages
        .iter()
        .flat_map(|message| &message.parts)
        .find_map(|part| match part {
            Part::ToolResult { name, .. } => name.as_deref(),
            _ => None,
        });
    assert_eq!(result_name, Some("read_file"));

    let actual: Value = serde_json::from_str(&request_to_json(&req)).expect("plugin request JSON");
    let expected: Value = serde_json::from_str(MIXED).expect("fixture JSON");
    assert_eq!(actual, expected);
}

#[test]
fn explicit_tool_loading_flags_survive_and_unknown_stays_omitted() {
    let body = json!({
        "model": "plugin-model",
        "messages": [{"role": "user", "content": "continue"}],
        "tools": [
            {"name": "deferred", "defer_loading": true},
            {"name": "eager", "defer_loading": false},
            {"name": "unspecified"}
        ]
    });
    let req = frontends::decode(FrontendFormat::OpenAi, body).expect("decode tool flags");
    let actual: Value = serde_json::from_str(&request_to_json(&req)).expect("plugin request JSON");

    assert_eq!(actual["tools"][0]["defer_loading"], true);
    assert_eq!(actual["tools"][1]["defer_loading"], false);
    assert!(actual["tools"][2].get("defer_loading").is_none());
}
