#!/usr/bin/env python3
"""Deterministic v1 path coverage layered on top of the #75/#83 compatibility rig."""

import json
import os
import pathlib
import sys
import time
import urllib.error
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parents[1]
CONTRACT = json.loads((ROOT / "tests/fixtures/protocol-v1-compatibility.json").read_text())
BASE = os.environ.get("KINETIX_BASE", "http://127.0.0.1:8180").rstrip("/")
KEY = os.environ.get("KINETIX_KEY", "")
RESTRICTED_MODELS_KEY = os.environ.get(
    "KINETIX_RESTRICTED_MODELS_KEY",
    "sk-kinetix-compat-restricted-models",
)
PNG = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAusB9Wlq9mQAAAAASUVORK5CYII="


class Failure(RuntimeError):
    pass



def request(path, payload=None, method="POST", key=KEY):
    headers = {"Content-Type": "application/json"}
    if path.startswith("/v1/messages"):
        headers.update({
            "anthropic-version": "2023-06-01",
            "anthropic-beta": "claude-code-20250219,interleaved-thinking-2025-05-14",
            "User-Agent": "claude-cli/protocol-v1-acceptance",
        })
        if key is not None:
            headers["x-api-key"] = key
    else:
        headers["User-Agent"] = "kinetix-protocol-v1-acceptance"
        if key is not None:
            headers["Authorization"] = f"Bearer {key}"
    data = None if payload is None else json.dumps(payload).encode()
    req = urllib.request.Request(BASE + path, data=data, headers=headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=15) as resp:
            return resp.status, {k.lower(): v for k, v in resp.headers.items()}, resp.read().decode()
    except urllib.error.HTTPError as error:
        return error.code, {k.lower(): v for k, v in error.headers.items()}, error.read().decode()

def sse(body):
    out = []
    event = None
    data = []
    for raw in body.splitlines():
        if not raw:
            if data:
                value = "\n".join(data)
                try:
                    value = json.loads(value)
                except Exception:
                    pass
                out.append((event, value))
            event, data = None, []
            continue
        if raw.startswith("event: "):
            event = raw[7:].strip()
        elif raw.startswith("data: "):
            data.append(raw[6:])
    if data:
        value = "\n".join(data)
        try:
            value = json.loads(value)
        except Exception:
            pass
        out.append((event, value))
    return out


def need(condition, message):
    if not condition:
        raise Failure(message)



def chat_payload(model, marker="fixture:chat-fields fixture:vision fixture:thinking", stream=True, nested=False):
    schema = {
        "type": "object",
        "properties": {
            "city": {"type": "string"},
            "config": {
                "type": "object",
                "properties": {"deep_tag": {"type": "string", "enum": ["ok"]}},
            },
        },
        "required": ["city"],
    } if nested else {
        "type": "object",
        "properties": {"city": {"type": "string"}},
    }
    payload = {
        "model": model,
        "stream": stream,
        "reasoning_effort": "high",
        "temperature": 0.2,
        "top_p": 0.8,
        "top_k": 20,
        "max_tokens": 128,
        "stop": ["END"],
        "seed": 7,
        "presence_penalty": 0.1,
        "frequency_penalty": 0.2,
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": marker},
                {"type": "image_url", "image_url": {"url": PNG}},
            ],
        }],
        "tools": [
            {"type": "function", "function": {"name": "get_weather", "description": "weather", "parameters": schema}},
            {"type": "function", "function": {"name": "read_file", "description": "read", "parameters": {"type": "object"}}},
        ],
        "tool_choice": "required",
    }
    if stream:
        payload["stream_options"] = {"include_usage": True}
    return payload


def messages_payload(model, marker="fixture:messages-fields fixture:vision fixture:thinking", stream=True):
    return {
        "model": model,
        "stream": stream,
        "max_tokens": 128,
        "system": [{"type": "text", "text": "fixture:system-instruction"}],
        "temperature": 0.2,
        "top_p": 0.8,
        "top_k": 20,
        "stop_sequences": ["END"],
        "thinking": {"type": "enabled", "budget_tokens": 4096},
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": marker},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": PNG.split(",", 1)[1]}},
            ],
        }],
        "tools": [
            {"name": "get_weather", "description": "weather", "input_schema": {"type": "object", "properties": {"city": {"type": "string"}}}},
            {"name": "read_file", "description": "read", "input_schema": {"type": "object"}},
        ],
        "tool_choice": {"type": "any"},
    }


def responses_payload(model, marker="fixture:responses-fields fixture:vision fixture:thinking", stream=True):
    return {
        "model": model,
        "stream": stream,
        "instructions": "fixture:responses-instructions",
        "reasoning": {"effort": "high"},
        "temperature": 0.2,
        "top_p": 0.8,
        "top_k": 20,
        "max_output_tokens": 128,
        "presence_penalty": 0.1,
        "frequency_penalty": 0.2,
        "prompt_cache_key": "protocol-v1-fixture",
        "input": [{
            "type": "message",
            "role": "user",
            "content": [
                {"type": "input_text", "text": marker},
                {"type": "input_image", "image_url": PNG},
            ],
        }],
        "tools": [
            {"type": "function", "name": "get_weather", "description": "weather", "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}},
            {"type": "function", "name": "read_file", "description": "read", "parameters": {"type": "object"}},
        ],
        "tool_choice": "required",
    }

def chat_tool_identity(events):
    names = set()
    ids = set()
    for _, item in events:
        if not isinstance(item, dict) or not item.get("choices"):
            continue
        for tool in item["choices"][0].get("delta", {}).get("tool_calls", []):
            name = tool.get("function", {}).get("name")
            tool_id = tool.get("id")
            if name:
                names.add(name)
            if tool_id:
                ids.add(tool_id)
    return names, ids


def anthro_tools(events):
    return {
        item.get("content_block", {}).get("name")
        for event, item in events
        if event == "content_block_start"
        and isinstance(item, dict)
        and item.get("content_block", {}).get("type") == "tool_use"
        and item.get("content_block", {}).get("name")
    }



def expect_mixed(profile, model, payload):
    path = {
        "chat": "/v1/chat/completions",
        "messages": "/v1/messages",
        "responses": "/v1/responses",
    }[profile]
    status, _, body = request(path, payload)
    need(status == 200, f"{path} returned {status}: {body}")
    stream = bool(payload.get("stream"))
    if not stream:
        data = json.loads(body)
        need("usage" in data, f"{profile} sync response missing usage")
        if profile == "chat":
            message = data.get("choices", [{}])[0].get("message", {})
            tools = message.get("tool_calls", [])
            need(any(t.get("function", {}).get("name") == "get_weather" for t in tools),
                 "Chat sync response lost tool identity")
        elif profile == "messages":
            content = data.get("content", [])
            need(any(item.get("type") == "tool_use" and item.get("name") == "get_weather" for item in content),
                 "Messages sync response lost tool identity")
        else:
            output = data.get("output", [])
            need(any(item.get("type") == "function_call" and item.get("name") == "get_weather" for item in output),
                 "Responses sync response lost function-call identity")
        return

    events = sse(body)
    need("usage" in body, f"{profile} stream missing usage")
    if profile == "chat":
        need(any(value == "[DONE]" for _, value in events), "Chat stream missing [DONE]")
        names, ids = chat_tool_identity(events)
        need("get_weather" in names, "Chat stream lost tool name")
        need(bool(ids), "Chat stream lost tool-call id")
    elif profile == "messages":
        need(any(name == "message_stop" for name, _ in events), "Messages stream missing message_stop")
        need("get_weather" in anthro_tools(events), "Messages stream lost tool identity")
    else:
        names = {name for name, _ in events}
        need("response.completed" in names, "Responses stream missing response.completed")
        need("function_call" in body and "get_weather" in body, "Responses stream lost function call")


def expect_rejected(path, base_payload, variants, expected_status=422):
    for label, patch in variants:
        payload = json.loads(json.dumps(base_payload))
        payload.update(patch)
        status, _, body = request(path, payload)
        need(status == expected_status, f"{label}: expected {expected_status}, got {status}: {body}")


def run_http_case(case_id):
    path_cases = {
        "chat.native.openai.sync": ("chat", "syn-openai", False),
        "chat.native.openai.stream": ("chat", "syn-openai", True),
        "chat.translate.gemini.sync": ("chat", "syn-gemini-3", False),
        "chat.translate.gemini.stream": ("chat", "syn-gemini-3", True),
        "chat.translate.anthropic.sync": ("chat", "syn-anthropic", False),
        "chat.translate.anthropic.stream": ("chat", "syn-anthropic", True),
        "messages.native.anthropic.sync": ("messages", "syn-anthropic", False),
        "messages.native.anthropic.stream": ("messages", "syn-anthropic", True),
        "messages.translate.gemini.sync": ("messages", "syn-gemini-3", False),
        "messages.translate.gemini.stream": ("messages", "syn-gemini-3", True),
        "messages.translate.openai.sync": ("messages", "syn-openai", False),
        "messages.translate.openai.stream": ("messages", "syn-openai", True),
        "responses.translate.openai.sync": ("responses", "syn-openai", False),
        "responses.translate.openai.stream": ("responses", "syn-openai", True),
        "responses.translate.gemini.sync": ("responses", "syn-gemini-3", False),
        "responses.translate.gemini.stream": ("responses", "syn-gemini-3", True),
        "responses.translate.anthropic.sync": ("responses", "syn-anthropic", False),
        "responses.translate.anthropic.stream": ("responses", "syn-anthropic", True),
    }
    if case_id in path_cases:
        profile, model, stream = path_cases[case_id]
        builder = {"chat": chat_payload, "messages": messages_payload, "responses": responses_payload}[profile]
        return expect_mixed(profile, model, builder(model, stream=stream))


    if case_id == "chat.image.variants":
        for label, image_url in [
            ("data-url", PNG),
            ("url", "https://example.invalid/fixture.png"),
        ]:
            payload = chat_payload("syn-gemini-3", "fixture:vision")
            payload.pop("reasoning_effort")
            payload["messages"][0]["content"] = [
                {"type": "text", "text": f"fixture:vision {label}"},
                {"type": "image_url", "image_url": {"url": image_url}},
            ]
            status, _, body = request("/v1/chat/completions", payload)
            need(status == 200, f"{label}: {body}")
        return

    if case_id == "chat.tool_choice.variants":
        for label, choice in [
            ("auto", "auto"),
            ("none", "none"),
            ("required", "required"),
            ("specific", {"type": "function", "function": {"name": "get_weather"}}),
        ]:
            payload = chat_payload("syn-gemini-3", f"fixture:tool-choice-{label}")
            payload["tool_choice"] = choice
            status, _, body = request("/v1/chat/completions", payload)
            need(status == 200, f"{label}: {body}")
        return

    if case_id == "messages.system.variants":
        for label, system in [
            ("string", "fixture:system-variant"),
            ("blocks", [{"type": "text", "text": "fixture:system-variant"}]),
        ]:
            payload = messages_payload("syn-gemini-3", "fixture:system-variant", stream=False)
            payload["system"] = system
            payload["messages"] = [{"role": "user", "content": "hello"}]
            payload.pop("thinking")
            status, _, body = request("/v1/messages", payload)
            need(status == 200, f"{label}: {body}")
        return

    if case_id == "messages.image.variants":
        variants = [
            ("base64", {"type": "base64", "media_type": "image/png", "data": PNG.split(",", 1)[1]}),
            ("url", {"type": "url", "url": "https://example.invalid/fixture.png"}),
        ]
        for label, source in variants:
            payload = messages_payload("syn-gemini-3", "fixture:vision", stream=False)
            payload["messages"][0]["content"] = [
                {"type": "text", "text": label},
                {"type": "image", "source": source},
            ]
            payload.pop("thinking")
            status, _, body = request("/v1/messages", payload)
            need(status == 200, f"{label}: {body}")
        return

    if case_id == "messages.tool_choice.variants":
        for label, choice in [
            ("auto", {"type": "auto"}),
            ("none", {"type": "none"}),
            ("required", {"type": "any"}),
            ("specific", {"type": "tool", "name": "get_weather"}),
        ]:
            payload = messages_payload("syn-gemini-3", f"fixture:tool-choice-{label}", stream=False)
            payload["tool_choice"] = choice
            status, _, body = request("/v1/messages", payload)
            need(status == 200, f"{label}: {body}")
        return

    if case_id == "responses.input.variants":
        variants = [
            ("string", "fixture:responses-input"),
            ("message", [{"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": "fixture:responses-input"}
            ]}]),
        ]
        for label, input_value in variants:
            payload = {"model": "syn-gemini-3", "input": input_value}
            status, _, body = request("/v1/responses", payload)
            need(status == 200, f"{label}: {body}")
        return

    if case_id == "responses.image.variants":
        for label, image_url in [
            ("data-url", PNG),
            ("url", "https://example.invalid/fixture.png"),
        ]:
            payload = {
                "model": "syn-anthropic",
                "input": [{"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": f"fixture:vision {label}"},
                    {"type": "input_image", "image_url": image_url},
                ]}],
            }
            status, _, body = request("/v1/responses", payload)
            need(status == 200, f"{label}: {body}")
        return

    if case_id == "responses.tool_choice.variants":
        for label, choice in [
            ("auto", "auto"),
            ("none", "none"),
            ("required", "required"),
            ("specific", {"type": "function", "name": "get_weather"}),
        ]:
            payload = responses_payload("syn-gemini-3", f"fixture:tool-choice-{label}", stream=False)
            payload["tool_choice"] = choice
            status, _, body = request("/v1/responses", payload)
            need(status == 200, f"{label}: {body}")
        return

    if case_id == "responses.translate.openai.tool_continuation":
        payload = {
            "model": "syn-openai",
            "stream": False,
            "input": [
                {"type": "function_call", "call_id": "call_keep_resp", "name": "get_weather",
                 "arguments": "{\"city\":\"Paris\"}"},
                {"type": "function_call_output", "call_id": "call_keep_resp",
                 "output": "fixture:tool-continuation 18C"},
            ],
        }
        status, _, body = request("/v1/responses", payload)
        need(status == 200, body)
        return

    if case_id == "responses.supported_options":
        payload = {
            "model": "syn-gemini-3",
            "input": "hello",
            "store": False,
            "background": False,
            "include": [],
            "text": {"format": {"type": "text"}},
            "truncation": "disabled",
            "stream_options": {"include_obfuscation": False},
        }
        status, _, body = request("/v1/responses", payload)
        need(status == 200, body)
        return

    if case_id == "chat.translate.gemini.parallel_tools":
        payload = chat_payload("syn-gemini-3", "fixture:chat-fields fixture:vision fixture:thinking fixture:multi-tools")
        status, _, body = request("/v1/chat/completions", payload)
        events = sse(body)
        need(status == 200, body)
        names, ids = chat_tool_identity(events)
        need({"get_weather", "read_file"} <= names, "parallel Chat tool names were not preserved")
        need(len(ids) >= 2, "parallel Chat tool-call ids were not distinct")
        return

    if case_id == "messages.translate.gemini.parallel_tools":
        payload = messages_payload("syn-gemini-3", "fixture:messages-fields fixture:vision fixture:thinking fixture:multi-tools")
        status, _, body = request("/v1/messages", payload)
        need(status == 200, body)
        events = sse(body)
        need({"get_weather", "read_file"} <= anthro_tools(events),
             "parallel Messages tool identities were not preserved")
        return

    if case_id == "responses.translate.anthropic.parallel_tools":
        payload = responses_payload("syn-anthropic", "fixture:responses-fields fixture:vision fixture:thinking fixture:multi-tools")
        status, _, body = request("/v1/responses", payload)
        need(status == 200, body)
        events = sse(body)
        call_ids = {
            item.get("item", {}).get("call_id")
            for event, item in events
            if event == "response.output_item.added"
            and isinstance(item, dict)
            and item.get("item", {}).get("type") == "function_call"
        }
        call_ids.discard(None)
        need(len(call_ids) >= 2, "parallel Responses tool-call ids were not distinct")
        return

    if case_id == "chat.translate.gemini.nested_schema":
        payload = chat_payload("syn-gemini-3", "fixture:nested-schema", nested=True)
        payload.pop("reasoning_effort")
        payload["messages"][0]["content"] = [{"type": "text", "text": "fixture:nested-schema"}]
        status, _, body = request("/v1/chat/completions", payload)
        need(status == 200, body)
        return

    if case_id == "chat.translate.gemini.tool_signature_continuation":
        # Gemini hands back an opaque `thoughtSignature` beside a function call.
        # The OpenAI/Pi client protocol cannot represent it, so the client never
        # sees or returns it; Kinetix must persist it server-side under the
        # client-visible tool-call id and restore it on the next turn. The
        # synthetic upstream fails closed (400) if the restored function call
        # lacks the exact signature, which is the exact failure this fix
        # addresses.
        marker = "fixture:gemini-signature-continuation"
        tool = {
            "type": "function",
            "function": {
                "name": "get_weather",
                "description": "weather",
                "parameters": {"type": "object", "properties": {"city": {"type": "string"}}},
            },
        }

        def continuation_turn(stream):
            first = {
                "model": "syn-gemini-3",
                "stream": stream,
                "messages": [{"role": "user", "content": marker}],
                "tools": [tool],
            }
            status, _, body = request("/v1/chat/completions", first)
            need(status == 200, f"stream={stream} turn 1: {status}: {body}")
            if stream:
                call_ids = chat_tool_identity(sse(body))[1]
                need(len(call_ids) == 1, f"stream={stream} turn 1 ids: {call_ids}")
                call_id = next(iter(call_ids))
            else:
                message = json.loads(body)["choices"][0]["message"]
                calls = message.get("tool_calls") or []
                need(len(calls) == 1, f"stream={stream} turn 1 calls: {calls}")
                call_id = calls[0]["id"]
            need(bool(call_id), f"stream={stream} turn 1 tool call had no client-visible id")

            second = {
                "model": "syn-gemini-3",
                "stream": stream,
                "messages": [
                    {"role": "user", "content": marker},
                    {"role": "assistant", "content": None, "tool_calls": [{
                        "id": call_id,
                        "type": "function",
                        "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"},
                    }]},
                    {"role": "tool", "tool_call_id": call_id, "content": "18C"},
                ],
                "tools": [tool],
            }
            status, _, body = request("/v1/chat/completions", second)
            need(status == 200, f"stream={stream} turn 2 (signature not restored?): {status}: {body}")

            # Negative control: a tool-call id that was never captured must not
            # be given an invented signature. The strict upstream rejects the
            # unsigned continuation, proving the positive turn above really did
            # replay stored state rather than passing vacuously.
            unknown = json.loads(json.dumps(second))
            unknown["messages"][1]["tool_calls"][0]["id"] = "call_never_captured"
            unknown["messages"][2]["tool_call_id"] = "call_never_captured"
            status, _, _ = request("/v1/chat/completions", unknown)
            need(status != 200, f"stream={stream} unknown id was accepted without stored state")

        continuation_turn(False)
        continuation_turn(True)
        return

    if case_id in (
        "chat.translate.gemini.cross_model_placeholder",
        "chat.translate.gemini.cross_model_placeholder_direct",
    ):
        # A trace that originated on one Gemini model and is continued on
        # another. The stored signature is real but belongs to a different
        # model: replaying it is wrong, and stripping it makes the provider
        # reject the unsigned historical call. Gemini documents a placeholder
        # for exactly this transfer, and the strict upstream accepts only that
        # placeholder on the second model. The first case continues through the
        # `syn-gemini-3-pro` Route (strip_with_warning); the second targets the
        # bare upstream model id directly, so no Route policy applies. Both must
        # translate with the placeholder rather than refusing.
        marker = "fixture:gemini-cross-model-placeholder"
        tool = {
            "type": "function",
            "function": {
                "name": "get_weather",
                "description": "weather",
                "parameters": {"type": "object", "properties": {"city": {"type": "string"}}},
            },
        }
        # `syn-gemini-3-pro` resolves to the Route of that name; the
        # `provider/model` form forces a direct target with no Route policy.
        turn2_model = (
            "Synth Gemini/syn-gemini-3-pro"
            if case_id.endswith("_direct")
            else "syn-gemini-3-pro"
        )

        first = {
            "model": "syn-gemini-3",
            "stream": False,
            "messages": [{"role": "user", "content": marker}],
            "tools": [tool],
        }
        status, _, body = request("/v1/chat/completions", first)
        need(status == 200, f"cross-model turn 1: {status}: {body}")
        calls = json.loads(body)["choices"][0]["message"].get("tool_calls") or []
        need(len(calls) == 1, f"cross-model turn 1 calls: {calls}")
        call_id = calls[0]["id"]
        need(bool(call_id), "cross-model turn 1 tool call had no client-visible id")

        second = {
            "model": turn2_model,
            "stream": False,
            "messages": [
                {"role": "user", "content": marker},
                {"role": "assistant", "content": None, "tool_calls": [{
                    "id": call_id,
                    "type": "function",
                    "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"},
                }]},
                {"role": "tool", "tool_call_id": call_id, "content": "18C"},
            ],
            "tools": [tool],
        }
        status, headers, body = request("/v1/chat/completions", second)
        need(
            status == 200,
            f"{case_id} turn 2 (placeholder not substituted?): {status}: {body}",
        )
        need(
            bool(headers.get("x-kinetix-warning")),
            "a substituted cross-model placeholder must be reported to the client",
        )

        # Negative control: an id that was never captured has no state at all,
        # so no placeholder is painted and the strict upstream rejects it. This
        # proves the positive turn really used stored state rather than passing
        # because the model accepts anything.
        unknown = json.loads(json.dumps(second))
        unknown["messages"][1]["tool_calls"][0]["id"] = "call_never_captured"
        unknown["messages"][2]["tool_call_id"] = "call_never_captured"
        status, _, _ = request("/v1/chat/completions", unknown)
        need(status != 200, "an uncaptured id must not be given an invented placeholder")
        return

    if case_id == "chat.translate.gemini.legacy_model_strip_without_placeholder":
        # Gemini 3 -> Gemini 2.5. The stored signature is still not portable,
        # but Gemini 2.5 predates signature validation and never documented the
        # Gemini 3 validator-bypass sentinel, so Kinetix must strip the state.
        # The strict upstream rejects any thoughtSignature on this model, so an
        # injected sentinel fails the case; the warning header proves the
        # stored (non-portable) state was actually found and handled.
        marker = "fixture:gemini-legacy-model-continuation"
        tool = {
            "type": "function",
            "function": {
                "name": "get_weather",
                "description": "weather",
                "parameters": {"type": "object", "properties": {"city": {"type": "string"}}},
            },
        }
        first = {
            "model": "syn-gemini-3",
            "stream": False,
            "messages": [{"role": "user", "content": marker}],
            "tools": [tool],
        }
        status, _, body = request("/v1/chat/completions", first)
        need(status == 200, f"legacy-model turn 1: {status}: {body}")
        calls = json.loads(body)["choices"][0]["message"].get("tool_calls") or []
        need(len(calls) == 1, f"legacy-model turn 1 calls: {calls}")
        call_id = calls[0]["id"]
        need(bool(call_id), "legacy-model turn 1 tool call had no client-visible id")

        second = {
            "model": "syn-gemini-legacy",
            "stream": False,
            "messages": [
                {"role": "user", "content": marker},
                {"role": "assistant", "content": None, "tool_calls": [{
                    "id": call_id,
                    "type": "function",
                    "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"},
                }]},
                {"role": "tool", "tool_call_id": call_id, "content": "18C"},
            ],
            "tools": [tool],
        }
        status, headers, body = request("/v1/chat/completions", second)
        need(
            status == 200,
            f"{case_id} turn 2 (Gemini 3 placeholder injected into 2.5?): {status}: {body}",
        )
        need(
            bool(headers.get("x-kinetix-warning")),
            "stripping non-portable state must be reported to the client",
        )
        return

    if case_id == "chat.fallback.tool_continuation":
        payload = {
            "model": "syn-fallback",
            "stream": False,
            "messages": [
                {"role": "assistant", "content": None, "tool_calls": [{
                    "id": "call_keep_1", "type": "function",
                    "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"},
                }]},
                {"role": "tool", "tool_call_id": "call_keep_1", "content": "fixture:tool-continuation 18C"},
            ],
        }
        status, _, body = request("/v1/chat/completions", payload)
        need(status == 200, body)
        return

    if case_id == "messages.translate.openai.tool_continuation":
        payload = {
            "model": "syn-openai",
            "stream": False,
            "max_tokens": 64,
            "messages": [
                {"role": "assistant", "content": [{
                    "type": "tool_use", "id": "toolu_keep_1", "name": "get_weather",
                    "input": {"city": "Paris"},
                }]},
                {"role": "user", "content": [{
                    "type": "tool_result", "tool_use_id": "toolu_keep_1",
                    "content": "fixture:tool-continuation 18C",
                }]},
            ],
        }
        status, _, body = request("/v1/messages", payload)
        need(status == 200, body)
        return

    if case_id == "chat.fallback.opaque_reasoning":
        payload = {
            "model": "syn-fallback",
            "stream": False,
            "messages": [
                {"role": "assistant", "content": "prior", "reasoning_content": "opaque", "reasoning_signature": "sig-secret"},
                {"role": "user", "content": "fixture:opaque-fallback"},
            ],
        }
        status, headers, body = request("/v1/chat/completions", payload)
        need(status == 200, body)
        need(bool(headers.get("x-kinetix-warning")), "portability warning header missing")
        return


    if case_id == "chat.native.openai.reasoning_replay":
        payload = {
            "model": "syn-openai",
            "stream": False,
            "messages": [
                {"role": "assistant", "content": "prior", "reasoning_content": "earlier thought", "reasoning_signature": "sig-own"},
                {"role": "user", "content": "fixture:chat-reasoning-replay"},
            ],
        }
        status, headers, body = request("/v1/chat/completions", payload)
        need(status == 200, body)
        need(not headers.get("x-kinetix-warning"), "same-format reasoning replay must not warn")
        return

    if case_id == "messages.translate.gemini.opaque_reasoning":
        payload = {
            "model": "syn-fallback",
            "stream": False,
            "max_tokens": 64,
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "opaque", "signature": "sig-secret"},
                    {"type": "text", "text": "prior"},
                ]},
                {"role": "user", "content": "fixture:opaque-fallback"},
            ],
        }
        status, headers, body = request("/v1/messages", payload)
        need(status == 200, body)
        need(bool(headers.get("x-kinetix-warning")), "Messages portability warning header missing")
        return

    if case_id == "chat.translate.gemini.nested_content.reject":
        base = {"model": "syn-gemini-3", "messages": [{"role": "user", "content": "hello"}]}
        variants = [
            ("file content", {"messages": [{"role": "user", "content": [{"type": "file", "file": {"file_id": "file_1"}}]}]}),
            ("audio", {"messages": [{"role": "assistant", "content": "x", "audio": {"id": "audio_1"}}]}),
            ("function_call", {"messages": [{"role": "assistant", "content": "x", "function_call": {"name": "old", "arguments": "{}"}}]}),
            ("refusal", {"messages": [{"role": "assistant", "content": "x", "refusal": "no"}]}),
            ("reasoning_details", {"messages": [{"role": "assistant", "content": "x", "reasoning_details": [{"type": "summary", "text": "hidden"}]}]}),
        ]
        expect_rejected("/v1/chat/completions", base, variants, expected_status=400)
        return

    if case_id == "messages.translate.multimodal_tool_result.reject":
        base = {"model": "syn-gemini-3", "max_tokens": 64, "messages": []}
        variants = [
            ("multimodal tool_result", {"messages": [{"role": "user", "content": [{
                "type": "tool_result",
                "tool_use_id": "toolu_missing",
                "content": [{"type": "image", "source": {
                    "type": "base64", "media_type": "image/png", "data": "AA=="
                }}],
            }]}]}),
            ("document block", {"messages": [{"role": "user", "content": [{
                "type": "document",
                "source": {"type": "base64", "media_type": "application/pdf", "data": "AA=="},
            }]}]}),
            ("server tool block", {"messages": [{"role": "assistant", "content": [{
                "type": "server_tool_use", "id": "srv_1", "name": "web_search", "input": {}
            }]}]}),
            ("server tool definition", {
                "messages": [{"role": "user", "content": "hello"}],
                "tools": [{"type": "web_search_20250305", "name": "web_search"}],
            }),
        ]
        expect_rejected("/v1/messages", base, variants, expected_status=400)
        return

    if case_id == "chat.native.openai.provider_extensions":
        payload = {
            "model": "syn-openai",
            "stream": False,
            "messages": [{"role": "user", "content": "fixture:openai-extra"}],
            "n": 2,
            "vendor_extension": {"must_survive": True},
        }
        status, _, body = request("/v1/chat/completions", payload)
        need(status == 200, body)
        return

    if case_id == "messages.native.anthropic.provider_extensions":
        payload = {
            "model": "syn-anthropic",
            "stream": False,
            "max_tokens": 64,
            "messages": [{"role": "user", "content": "fixture:anthropic-extra"}],
            "vendor_extension": {"must_survive": True},
        }
        status, _, body = request("/v1/messages", payload)
        need(status == 200, body)
        return

    if case_id == "chat.translate.gemini.unsupported_fields.reject":
        variants = [
            ("n", {"n": 2}),
            ("logprobs", {"logprobs": True, "top_logprobs": 2}),
            ("response_format.json_schema", {"response_format": {
                "type": "json_schema",
                "json_schema": {"name": "answer", "schema": {"type": "object"}},
            }}),
            ("modalities/audio", {"modalities": ["text", "audio"], "audio": {"voice": "alloy", "format": "wav"}}),
            ("prediction", {"prediction": {"type": "content", "content": "expected"}}),
        ]
        passthrough_markers = {
            "n": "passthrough-n",
            "logprobs": "passthrough-logprobs",
            "response_format.json_schema": "passthrough-response-format",
            "modalities/audio": "passthrough-modalities-audio",
            "prediction": "passthrough-prediction",
        }
        for label, patch in variants:
            native = {
                "model": "syn-openai",
                "messages": [{"role": "user", "content": f"fixture:{passthrough_markers[label]}"}],
            }
            native.update(patch)
            status, _, body = request("/v1/chat/completions", native)
            need(status == 200, f"{label}: same-format passthrough failed: {status}: {body}")
        translated = {"model": "syn-gemini-3", "messages": [{"role": "user", "content": "translated"}]}
        expect_rejected("/v1/chat/completions", translated, variants, expected_status=400)
        return

    if case_id == "responses.unsupported_fields.reject":
        base = {"model": "syn-gemini-3", "input": "hello"}
        variants = [
            ("store", {"store": True}),
            ("background", {"background": True}),
            ("include", {"include": ["message.output_text.logprobs"]}),
            ("text.format", {"text": {"format": {"type": "json_schema"}}}),
            ("truncation", {"truncation": "auto"}),
            ("stream_options", {"stream_options": {"include_obfuscation": True}}),
            ("metadata", {"metadata": {"fixture": "value"}}),
            ("parallel_tool_calls", {"parallel_tool_calls": True}),
            ("unknown top-level field", {"future_semantics": {"enabled": True}}),
            ("unknown input item", {"input": [{
                "type": "input_file", "file_id": "file_1"
            }]}),
            ("hosted tool", {"tools": [{
                "type": "web_search_preview"
            }]}),
        ]
        expect_rejected("/v1/responses", base, variants)
        # reasoning.summary is a same-format control: accepted at decode, then
        # refused by the translation gate (400) on a translating target.
        expect_rejected(
            "/v1/responses",
            base,
            [("reasoning summary", {"reasoning": {"effort": "high", "summary": "auto"}})],
            expected_status=400,
        )
        return

    if case_id.startswith("messages.count_tokens."):
        models = {
            "messages.count_tokens.native_exact": ("syn-anthropic", "exact"),
            "messages.count_tokens.translated_estimated": ("syn-gemini-3", "estimated"),
            "messages.count_tokens.openai_estimated": ("syn-openai", "estimated"),
            "messages.count_tokens.heterogeneous_estimated": ("syn-count-heterogeneous", "estimated"),
        }
        model, expected = models[case_id]
        payload = {
            "model": model,
            "system": "count system",
            "messages": [{"role": "user", "content": "count this"}],
            "tools": [{"name": "count_tool", "description": "fixture", "input_schema": {"type": "object"}}],
        }
        status, headers, body = request("/v1/messages/count_tokens", payload)
        data = json.loads(body)
        need(status == 200 and isinstance(data.get("input_tokens"), int) and data["input_tokens"] > 0, body)
        need(headers.get("x-kinetix-token-count") == expected,
             f"expected token count mode {expected}, got {headers.get('x-kinetix-token-count')}")
        return

    if case_id == "models.visible":
        status, _, body = request("/v1/models", None, "GET")
        data = json.loads(body)
        ids = {item.get("id") for item in data.get("data", [])}
        need(status == 200 and data.get("object") == "list" and isinstance(data.get("data"), list),
             f"invalid models list shape: {body}")

        # The documented selector classes must all be discoverable: a bare
        # provider model id, an alias, and a Route name.
        expected_classes = {"syn-openai", "syn-openai-alias", "syn-fallback"}
        missing = expected_classes - ids
        need(not missing, f"model selector classes missing from discovery: {sorted(missing)}")

        restricted_status, _, restricted_body = request(
            "/v1/models", None, "GET", key=RESTRICTED_MODELS_KEY
        )
        restricted = json.loads(restricted_body)
        restricted_ids = {item.get("id") for item in restricted.get("data", [])}
        need(
            restricted_status == 200
            and restricted.get("object") == "list"
            and isinstance(restricted.get("data"), list),
            f"restricted model list has invalid shape: {restricted_body}",
        )
        need(
            restricted_ids == expected_classes,
            "restricted virtual key leaked or hid model selectors: "
            f"expected {sorted(expected_classes)}, got {sorted(restricted_ids)}",
        )
        need(
            "syn-gemini-3" not in restricted_ids,
            f"restricted key exposed disallowed provider model: {sorted(restricted_ids)}",
        )

        # Listings expose accepted capabilities and Kinetix metadata only.
        by_id = {item.get("id"): item for item in data["data"]}
        direct = by_id["syn-openai"]
        need(
            direct.get("capabilities", {}).get("tools") is True
            and direct["capabilities"].get("images") is True
            and direct["capabilities"].get("streaming") is True,
            f"accepted capabilities missing from model listing: {direct}",
        )
        need(
            direct.get("kinetix", {}).get("state") == "accepted"
            and direct["kinetix"].get("transport") == "openai",
            f"Kinetix model metadata missing: {direct}",
        )
        public_sources = {"operator", "models.dev", "plugin", "probe", "provider"}
        for item in data["data"]:
            kinetix = item.get("kinetix", {})
            need(
                set(kinetix) <= {"state", "transport", "provenance"}
                and set(kinetix.get("provenance", {}).values()) <= public_sources,
                f"model listing exposes non-public metadata: {item}",
            )

        unauth_status, _, _ = request("/v1/models", None, "GET", key=None)
        need(unauth_status in (401, 403), f"models endpoint accepted unauthenticated request: {unauth_status}")
        return

    raise Failure(f"no HTTP implementation for declared contract case {case_id}")
def main():
    cases = [case for case in CONTRACT["cases"] if case["runner"] == "http"]
    failures = []
    started = time.time()
    print("==> Protocol v1 native/translated path matrix")
    for case in cases:
        t0 = time.time()
        try:
            run_http_case(case["id"])
            print(f"  PASS {case['id']:<56} {int((time.time() - t0) * 1000)}ms")
        except Exception as error:
            failures.append((case["id"], str(error)))
            print(f"  FAIL {case['id']:<56} {error}")
    print(f"==> {len(cases) - len(failures)}/{len(cases)} protocol cases passed in {time.time() - started:.2f}s")
    if failures:
        print("\nFailures:", file=sys.stderr)
        for case_id, error in failures:
            print(f"- {case_id}: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
