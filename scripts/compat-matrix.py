#!/usr/bin/env python3
"""Reproducible Coding-Agent Compatibility Matrix Test (FR-9.2, FR-9.3, NFR-7.2).

Verifies real wire protocol compatibility across coding agent client profiles:
  1. Pi Coding Agent (OpenAI Chat Completions wire format, tool calls, session affinity)
  2. Next-Gen Agent / Codex CLI (OpenAI Responses /v1/responses format)
  3. Anthropic Agent / Claude Code (Anthropic Messages /v1/messages format)

Against both same-format passthrough and cross-format translating upstreams.
Runs with zero external dependencies (Python 3 stdlib only).
"""

import json
import os
import sys
import time
import urllib.error
import urllib.request

BASE_URL = os.environ.get("KINETIX_BASE", "http://127.0.0.1:8180")
API_KEY = os.environ.get("KINETIX_KEY", "")

TEST_RESULTS = []


def record_result(client, upstream, capability, passed, detail="", duration_ms=0):
    TEST_RESULTS.append({
        "client": client,
        "upstream": upstream,
        "capability": capability,
        "passed": passed,
        "detail": detail,
        "duration_ms": duration_ms,
    })
    status = "\033[92mPASS\033[0m" if passed else "\033[91mFAIL\033[0m"
    print(f"  [{status}] {client:<24} | {upstream:<14} | {capability:<18} ({duration_ms}ms) {detail}")


def urlopen(req, timeout=15):
    try:
        return urllib.request.urlopen(req, timeout=timeout)
    except urllib.error.HTTPError as error:
        body = error.read().decode("utf-8", errors="replace")
        raise RuntimeError(
            f"HTTP {error.code} {req.full_url}: {body}"
        ) from error


def read_sse_events(response):
    """Parse raw SSE stream into a list of (event_type, data_dict_or_str)."""
    events = []
    current_event = None
    data_lines = []

    for line in response:
        line = line.decode("utf-8", errors="replace").rstrip("\r\n")
        if not line:
            if data_lines:
                data_str = "\n".join(data_lines)
                try:
                    data_val = json.loads(data_str)
                except Exception:
                    data_val = data_str
                events.append((current_event, data_val))
            current_event = None
            data_lines = []
            continue

        if line.startswith("event: "):
            current_event = line[7:].strip()
        elif line.startswith("data: "):
            data_lines.append(line[6:])

    if data_lines:
        data_str = "\n".join(data_lines)
        try:
            data_val = json.loads(data_str)
        except Exception:
            data_val = data_str
        events.append((current_event, data_val))

    return events


# ---------------------------------------------------------------------------
# Client 1: Pi Coding Agent (OpenAI Chat Completions)
# ---------------------------------------------------------------------------

def test_pi_plain_stream(upstream_model):
    t0 = time.time()
    url = f"{BASE_URL}/v1/chat/completions"
    payload = {
        "model": upstream_model,
        "stream": True,
        "messages": [
            {"role": "system", "content": "You are Pi, a coding agent."},
            {"role": "user", "content": "Write hello world."}
        ]
    }
    req = urllib.request.Request(
        url,
        data=json.dumps(payload).encode(),
        headers={
            "Authorization": f"Bearer {API_KEY}",
            "Content-Type": "application/json",
            "User-Agent": "pi (linux; x86_64)",
        }
    )
    with urlopen(req, timeout=15) as resp:
        events = read_sse_events(resp)

    # Check for text chunks and [DONE]
    text_chunks = [e[1] for e in events if isinstance(e[1], dict) and e[1].get("choices")]
    has_done = any(e[1] == "[DONE]" for e in events)
    duration = int((time.time() - t0) * 1000)
    passed = len(text_chunks) > 0 and has_done
    record_result("Pi (OpenAI Chat)", upstream_model, "plain_stream", passed, f"{len(text_chunks)} chunks", duration)


def test_pi_tool_use(upstream_model):
    t0 = time.time()
    url = f"{BASE_URL}/v1/chat/completions"
    payload = {
        "model": upstream_model,
        "stream": True,
        "messages": [
            {"role": "user", "content": "Read file main.rs"}
        ],
        "tools": [{
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read contents of a file",
                "parameters": {
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"]
                }
            }
        }]
    }
    req = urllib.request.Request(
        url,
        data=json.dumps(payload).encode(),
        headers={
            "Authorization": f"Bearer {API_KEY}",
            "Content-Type": "application/json",
            "User-Agent": "pi (linux; x86_64)",
        }
    )
    with urlopen(req, timeout=15) as resp:
        events = read_sse_events(resp)

    # Reassemble tool calls
    tool_chunks = []
    for _, ev in events:
        if isinstance(ev, dict) and ev.get("choices"):
            delta = ev["choices"][0].get("delta", {})
            if "tool_calls" in delta:
                tool_chunks.append(delta["tool_calls"])

    duration = int((time.time() - t0) * 1000)
    passed = len(tool_chunks) > 0
    record_result("Pi (OpenAI Chat)", upstream_model, "tool_use_stream", passed, f"{len(tool_chunks)} tool deltas", duration)


def test_pi_multi_turn(upstream_model):
    t0 = time.time()
    url = f"{BASE_URL}/v1/chat/completions"
    payload = {
        "model": upstream_model,
        "stream": True,
        "messages": [
            {"role": "user", "content": "Read file main.rs"},
            {
                "role": "assistant",
                "content": None,
                "tool_calls": [{
                    "id": "call_pi_1",
                    "type": "function",
                    "function": {"name": "read_file", "arguments": "{\"path\":\"main.rs\"}"}
                }]
            },
            {
                "role": "tool",
                "tool_call_id": "call_pi_1",
                "name": "read_file",
                "content": "fn main() { println!(\"ok\"); }"
            }
        ]
    }
    req = urllib.request.Request(
        url,
        data=json.dumps(payload).encode(),
        headers={
            "Authorization": f"Bearer {API_KEY}",
            "Content-Type": "application/json",
            "User-Agent": "pi (linux; x86_64)",
        }
    )
    with urlopen(req, timeout=15) as resp:
        events = read_sse_events(resp)

    has_chunks = any(isinstance(e[1], dict) and e[1].get("choices") for e in events)
    duration = int((time.time() - t0) * 1000)
    record_result("Pi (OpenAI Chat)", upstream_model, "multi_turn", has_chunks, "turn 2 grounded", duration)


def _target_marker(data, candidates):
    text = json.dumps(data, sort_keys=True)
    for candidate in candidates:
        if f"target:{candidate}" in text:
            return candidate
    return None


def _openai_sticky_request(headers, prompt_cache_key=None):
    payload = {
        "model": "syn-sticky-openai",
        "stream": False,
        "messages": [{"role": "user", "content": "sticky affinity"}],
    }
    if prompt_cache_key:
        payload["prompt_cache_key"] = prompt_cache_key
    req = urllib.request.Request(
        f"{BASE_URL}/v1/chat/completions",
        data=json.dumps(payload).encode(),
        headers={
            "Authorization": f"Bearer {API_KEY}",
            "Content-Type": "application/json",
            "User-Agent": "pi (linux; x86_64)",
            **headers,
        },
    )
    with urlopen(req, timeout=15) as resp:
        return json.loads(resp.read().decode())


def test_pi_session_affinity(_upstream_model):
    t0 = time.time()
    session_id = f"pi_sess_{int(time.time())}"
    headers = {
        "session_id": session_id,
        "X-Client-Request-Id": session_id,
        "X-Session-Affinity": session_id,
    }
    first = _openai_sticky_request(headers, session_id)
    second = _openai_sticky_request(headers, session_id)
    candidates = ("syn-openai-a", "syn-openai-b")
    first_target = _target_marker(first, candidates)
    second_target = _target_marker(second, candidates)
    passed = first_target is not None and first_target == second_target
    duration = int((time.time() - t0) * 1000)
    record_result(
        "Pi (OpenAI Chat)",
        "syn-sticky-openai",
        "session_affinity",
        passed,
        f"{first_target} -> {second_target}",
        duration,
    )


def test_prompt_cache_key_affinity(_upstream_model):
    # prompt_cache_key alone (no session headers) is the cache-affinity key on
    # a translated path; round-robin would otherwise alternate targets.
    t0 = time.time()
    cache_key = f"pck_{time.time_ns()}"
    candidates = ("syn-anthropic-a", "syn-anthropic-b")
    targets = []
    for _ in range(4):
        payload = {
            "model": "syn-sticky-anthropic",
            "stream": False,
            "messages": [{"role": "user", "content": "cache affinity"}],
            "prompt_cache_key": cache_key,
        }
        req = urllib.request.Request(
            f"{BASE_URL}/v1/chat/completions",
            data=json.dumps(payload).encode(),
            headers={"Authorization": f"Bearer {API_KEY}", "Content-Type": "application/json"},
        )
        with urlopen(req, timeout=15) as resp:
            targets.append(_target_marker(json.loads(resp.read().decode()), candidates))
    passed = targets[0] is not None and len(set(targets)) == 1
    record_result(
        "Pi (OpenAI Chat)",
        "syn-sticky-anthropic",
        "prompt_cache_key_affinity",
        passed,
        " -> ".join(str(t) for t in targets),
        int((time.time() - t0) * 1000),
    )


def test_pi_sync_aggregation(upstream_model):
    t0 = time.time()
    url = f"{BASE_URL}/v1/chat/completions"
    payload = {
        "model": upstream_model,
        "stream": False,
        "messages": [{"role": "user", "content": "hello non-stream"}]
    }
    req = urllib.request.Request(
        url,
        data=json.dumps(payload).encode(),
        headers={
            "Authorization": f"Bearer {API_KEY}",
            "Content-Type": "application/json",
            "User-Agent": "pi (linux; x86_64)",
        }
    )
    with urlopen(req, timeout=15) as resp:
        data = json.loads(resp.read().decode())

    passed = data.get("object") == "chat.completion" and len(data.get("choices", [])) > 0
    duration = int((time.time() - t0) * 1000)
    record_result("Pi (OpenAI Chat)", upstream_model, "sync_aggregation", passed, f"id: {data.get('id')}", duration)


# ---------------------------------------------------------------------------
# Client 2: Next-Gen Coding Agent / Codex CLI (OpenAI Responses API)
# ---------------------------------------------------------------------------

def test_responses_plain_stream(upstream_model):
    t0 = time.time()
    url = f"{BASE_URL}/v1/responses"
    payload = {
        "model": upstream_model,
        "stream": True,
        "instructions": "You are Codex, an autonomous coding agent.",
        "input": "Fix typo in lib.rs"
    }
    req = urllib.request.Request(
        url,
        data=json.dumps(payload).encode(),
        headers={
            "Authorization": f"Bearer {API_KEY}",
            "Content-Type": "application/json",
            "User-Agent": "codex-cli/0.125.0",
        }
    )
    with urlopen(req, timeout=15) as resp:
        events = read_sse_events(resp)

    event_names = [e[0] for e in events if e[0]]
    has_created = "response.created" in event_names
    has_delta = "response.output_text.delta" in event_names
    has_completed = "response.completed" in event_names
    passed = has_created and has_delta and has_completed
    duration = int((time.time() - t0) * 1000)
    record_result("Codex (Responses API)", upstream_model, "plain_stream", passed, f"events: {len(events)}", duration)


def test_responses_tool_use(upstream_model):
    t0 = time.time()
    url = f"{BASE_URL}/v1/responses"
    payload = {
        "model": upstream_model,
        "stream": True,
        "input": "Search codebase for 'TODO'",
        "tools": [{
            "type": "function",
            "name": "grep_code",
            "description": "Grep across source files",
            "parameters": {
                "type": "object",
                "properties": {"query": {"type": "string"}}
            }
        }]
    }
    req = urllib.request.Request(
        url,
        data=json.dumps(payload).encode(),
        headers={
            "Authorization": f"Bearer {API_KEY}",
            "Content-Type": "application/json",
            "User-Agent": "codex-cli/0.125.0",
        }
    )
    with urlopen(req, timeout=15) as resp:
        events = read_sse_events(resp)

    event_names = [e[0] for e in events if e[0]]
    has_completed = "response.completed" in event_names
    duration = int((time.time() - t0) * 1000)
    record_result("Codex (Responses API)", upstream_model, "tool_use_stream", has_completed, f"events: {len(events)}", duration)


def test_responses_multi_turn(upstream_model):
    t0 = time.time()
    url = f"{BASE_URL}/v1/responses"
    payload = {
        "model": upstream_model,
        "stream": True,
        "input": [
            {"type": "message", "role": "user", "content": "Search for main"},
            {"type": "function_call", "call_id": "call_resp_1", "name": "grep_code", "arguments": "{\"query\":\"main\"}"},
            {"type": "function_call_output", "call_id": "call_resp_1", "output": "src/main.rs:1: fn main()"}
        ]
    }
    req = urllib.request.Request(
        url,
        data=json.dumps(payload).encode(),
        headers={
            "Authorization": f"Bearer {API_KEY}",
            "Content-Type": "application/json",
            "User-Agent": "codex-cli/0.125.0",
        }
    )
    with urlopen(req, timeout=15) as resp:
        events = read_sse_events(resp)

    event_names = [e[0] for e in events if e[0]]
    passed = "response.completed" in event_names
    duration = int((time.time() - t0) * 1000)
    record_result("Codex (Responses API)", upstream_model, "multi_turn", passed, "input chaining", duration)


def test_responses_session_affinity(_upstream_model):
    t0 = time.time()
    session_id = f"codex_sess_{int(time.time())}"
    headers = {
        "Authorization": f"Bearer {API_KEY}",
        "Content-Type": "application/json",
        "User-Agent": "codex-cli/0.125.0",
        "session_id": session_id,
        "X-Client-Request-Id": session_id,
    }

    def request():
        req = urllib.request.Request(
            f"{BASE_URL}/v1/responses",
            data=json.dumps({
                "model": "syn-sticky-openai",
                "stream": False,
                "input": "sticky affinity",
                "prompt_cache_key": session_id,
            }).encode(),
            headers=headers,
        )
        with urlopen(req, timeout=15) as resp:
            return json.loads(resp.read().decode())

    first = request()
    second = request()
    candidates = ("syn-openai-a", "syn-openai-b")
    first_target = _target_marker(first, candidates)
    second_target = _target_marker(second, candidates)
    passed = first_target is not None and first_target == second_target
    duration = int((time.time() - t0) * 1000)
    record_result(
        "Codex (Responses API)",
        "syn-sticky-openai",
        "session_affinity",
        passed,
        f"{first_target} -> {second_target}",
        duration,
    )


def test_responses_sync_aggregation(upstream_model):
    t0 = time.time()
    url = f"{BASE_URL}/v1/responses"
    payload = {
        "model": upstream_model,
        "stream": False,
        "input": "hello sync responses"
    }
    req = urllib.request.Request(
        url,
        data=json.dumps(payload).encode(),
        headers={
            "Authorization": f"Bearer {API_KEY}",
            "Content-Type": "application/json",
            "User-Agent": "codex-cli/0.125.0",
        }
    )
    with urlopen(req, timeout=15) as resp:
        data = json.loads(resp.read().decode())

    passed = data.get("object") == "response" and data.get("status") == "completed" and "output" in data
    duration = int((time.time() - t0) * 1000)
    record_result("Codex (Responses API)", upstream_model, "sync_aggregation", passed, f"id: {data.get('id')}", duration)


# ---------------------------------------------------------------------------
# Client 3: Anthropic Agent (Messages API)
# ---------------------------------------------------------------------------

def test_anthropic_count_tokens(upstream_model):
    t0 = time.time()
    url = f"{BASE_URL}/v1/messages/count_tokens"
    payload = {
        "model": upstream_model,
        "system": "You are Claude Code, an agentic coding partner.",
        "messages": [
            {"role": "user", "content": "Inspect src/main.rs and summarize it."}
        ],
        "tools": [{
            "name": "read_file",
            "description": "Read a file from disk",
            "input_schema": {
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }
        }]
    }
    req = urllib.request.Request(
        url,
        data=json.dumps(payload).encode(),
        headers={
            "x-api-key": API_KEY,
            "anthropic-version": "2023-06-01",
            "anthropic-beta": "prompt-caching-2024-07-31",
            "x-claude-code-session-id": f"claude_count_{int(time.time())}",
            "Content-Type": "application/json",
            "User-Agent": "claude-code/0.2.0",
        }
    )
    with urlopen(req, timeout=15) as resp:
        data = json.loads(resp.read().decode())
        mode = resp.headers.get("x-kinetix-token-count")

    count = data.get("input_tokens")
    passed = isinstance(count, int) and count > 0 and mode in ("exact", "estimated")
    duration = int((time.time() - t0) * 1000)
    record_result(
        "Anthropic Agent",
        upstream_model,
        "count_tokens",
        passed,
        f"input_tokens: {count}, mode: {mode}",
        duration,
    )


def test_anthropic_plain_stream(upstream_model):
    t0 = time.time()
    url = f"{BASE_URL}/v1/messages"
    payload = {
        "model": upstream_model,
        "stream": True,
        "max_tokens": 128,
        "system": "You are Claude Code, an agentic coding partner.",
        "messages": [
            {"role": "user", "content": "Refactor the function foo."}
        ]
    }
    req = urllib.request.Request(
        url,
        data=json.dumps(payload).encode(),
        headers={
            "x-api-key": API_KEY,
            "anthropic-version": "2023-06-01",
            "Content-Type": "application/json",
            "User-Agent": "claude-code/0.2.0",
        }
    )
    with urlopen(req, timeout=15) as resp:
        events = read_sse_events(resp)

    event_names = [e[0] for e in events if e[0]]
    has_start = "message_start" in event_names
    has_delta = "content_block_delta" in event_names
    has_stop = "message_stop" in event_names
    passed = has_start and has_delta and has_stop
    duration = int((time.time() - t0) * 1000)
    record_result("Anthropic Agent", upstream_model, "plain_stream", passed, f"events: {len(events)}", duration)


def test_anthropic_tool_use(upstream_model):
    t0 = time.time()
    url = f"{BASE_URL}/v1/messages"
    payload = {
        "model": upstream_model,
        "stream": True,
        "max_tokens": 128,
        "messages": [
            {"role": "user", "content": "List files in directory"}
        ],
        "tools": [{
            "name": "ls",
            "description": "List directory contents",
            "input_schema": {
                "type": "object",
                "properties": {"path": {"type": "string"}}
            }
        }]
    }
    req = urllib.request.Request(
        url,
        data=json.dumps(payload).encode(),
        headers={
            "x-api-key": API_KEY,
            "anthropic-version": "2023-06-01",
            "Content-Type": "application/json",
            "User-Agent": "claude-code/0.2.0",
        }
    )
    with urlopen(req, timeout=15) as resp:
        events = read_sse_events(resp)

    event_names = [e[0] for e in events if e[0]]
    passed = "message_stop" in event_names
    duration = int((time.time() - t0) * 1000)
    record_result("Anthropic Agent", upstream_model, "tool_use_stream", passed, f"events: {len(events)}", duration)


def test_anthropic_multi_turn(upstream_model):
    t0 = time.time()
    url = f"{BASE_URL}/v1/messages"
    payload = {
        "model": upstream_model,
        "stream": True,
        "max_tokens": 128,
        "messages": [
            {"role": "user", "content": "List files"},
            {
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "id": "toolu_ant_1", "name": "ls", "input": {"path": "."}}
                ]
            },
            {
                "role": "user",
                "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_ant_1", "content": "src/ Cargo.toml"}
                ]
            }
        ]
    }
    req = urllib.request.Request(
        url,
        data=json.dumps(payload).encode(),
        headers={
            "x-api-key": API_KEY,
            "anthropic-version": "2023-06-01",
            "Content-Type": "application/json",
            "User-Agent": "claude-code/0.2.0",
        }
    )
    with urlopen(req, timeout=15) as resp:
        events = read_sse_events(resp)

    event_names = [e[0] for e in events if e[0]]
    passed = "message_stop" in event_names
    duration = int((time.time() - t0) * 1000)
    record_result("Anthropic Agent", upstream_model, "multi_turn", passed, "tool_result turn", duration)


def test_anthropic_session_affinity(_upstream_model):
    t0 = time.time()
    session_id = f"claude_sess_{int(time.time())}"

    def request():
        req = urllib.request.Request(
            f"{BASE_URL}/v1/messages",
            data=json.dumps({
                "model": "syn-sticky-anthropic",
                "stream": False,
                "max_tokens": 64,
                "messages": [{"role": "user", "content": "sticky affinity"}],
            }).encode(),
            headers={
                "x-api-key": API_KEY,
                "anthropic-version": "2023-06-01",
                "anthropic-beta": "claude-code-20250219,interleaved-thinking-2025-05-14",
                "Content-Type": "application/json",
                "User-Agent": "claude-cli/2.1.173 (external, cli)",
                "X-Claude-Code-Session-Id": session_id,
            },
        )
        with urlopen(req, timeout=15) as resp:
            return json.loads(resp.read().decode())

    first = request()
    second = request()
    candidates = ("syn-anthropic-a", "syn-anthropic-b")
    first_target = _target_marker(first, candidates)
    second_target = _target_marker(second, candidates)
    passed = first_target is not None and first_target == second_target
    duration = int((time.time() - t0) * 1000)
    record_result(
        "Anthropic Agent",
        "syn-sticky-anthropic",
        "session_affinity",
        passed,
        f"{first_target} -> {second_target}",
        duration,
    )


def test_anthropic_sync_aggregation(upstream_model):
    t0 = time.time()
    url = f"{BASE_URL}/v1/messages"
    payload = {
        "model": upstream_model,
        "stream": False,
        "max_tokens": 64,
        "messages": [{"role": "user", "content": "hello anthropic sync"}]
    }
    req = urllib.request.Request(
        url,
        data=json.dumps(payload).encode(),
        headers={
            "x-api-key": API_KEY,
            "anthropic-version": "2023-06-01",
            "Content-Type": "application/json",
            "User-Agent": "claude-code/0.2.0",
        }
    )
    with urlopen(req, timeout=15) as resp:
        data = json.loads(resp.read().decode())

    passed = data.get("type") == "message" and data.get("role") == "assistant" and len(data.get("content", [])) > 0
    duration = int((time.time() - t0) * 1000)
    record_result("Anthropic Agent", upstream_model, "sync_aggregation", passed, f"id: {data.get('id')}", duration)



# ---------------------------------------------------------------------------
# Current coding-agent edge fixtures
# ---------------------------------------------------------------------------

_ONE_PIXEL_PNG = (
    "data:image/png;base64,"
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAusB9Wlq9mQAAAAASUVORK5CYII="
)


def test_pi_openrouter_affinity(_upstream_model):
    t0 = time.time()
    session_id = f"pi_openrouter_{int(time.time())}"
    headers = {"X-Session-Id": session_id}
    first = _openai_sticky_request(headers)
    second = _openai_sticky_request(headers)
    candidates = ("syn-openai-a", "syn-openai-b")
    first_target = _target_marker(first, candidates)
    second_target = _target_marker(second, candidates)
    passed = first_target is not None and first_target == second_target
    record_result(
        "Pi (OpenAI Chat)",
        "syn-sticky-openai",
        "affinity_openrouter",
        passed,
        f"{first_target} -> {second_target}",
        int((time.time()-t0)*1000),
    )


def test_pi_parallel_tools(upstream_model):
    t0 = time.time()
    payload = {
        "model": upstream_model,
        "stream": True,
        "messages": [{"role": "user", "content": "fixture:multi-tools call both tools"}],
        "tools": [
            {"type": "function", "function": {"name": "get_weather", "parameters": {"type": "object"}}},
            {"type": "function", "function": {"name": "read_file", "parameters": {"type": "object"}}},
        ],
    }
    req = urllib.request.Request(
        f"{BASE_URL}/v1/chat/completions",
        data=json.dumps(payload).encode(),
        headers={"Authorization": f"Bearer {API_KEY}", "Content-Type": "application/json", "User-Agent": "pi (linux; x86_64)"},
    )
    with urlopen(req, timeout=15) as resp:
        events = read_sse_events(resp)
    names = set()
    for _, ev in events:
        if not isinstance(ev, dict) or not ev.get("choices"):
            continue
        for call in ev["choices"][0].get("delta", {}).get("tool_calls", []):
            name = call.get("function", {}).get("name")
            if name:
                names.add(name)
    passed = {"get_weather", "read_file"} <= names
    record_result("Pi (OpenAI Chat)", upstream_model, "parallel_tools", passed, ",".join(sorted(names)), int((time.time()-t0)*1000))


def test_pi_vision(upstream_model):
    t0 = time.time()
    events, _ = _pi_stream({
        "model": upstream_model,
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": "fixture:vision describe this"},
                {"type": "image_url", "image_url": {"url": _ONE_PIXEL_PNG}},
            ],
        }],
    })
    passed = bool(_pi_stream_text(events))
    record_result("Pi (OpenAI Chat)", upstream_model, "vision", passed, "data-url image", int((time.time()-t0)*1000))


def test_pi_thinking(upstream_model):
    t0 = time.time()
    events, _ = _pi_stream({
        "model": upstream_model,
        "reasoning_effort": "high",
        "messages": [{"role": "user", "content": "fixture:thinking solve carefully"}],
    })
    passed = bool(_pi_stream_text(events))
    record_result("Pi (OpenAI Chat)", upstream_model, "reasoning_control", passed, "reasoning_effort=high", int((time.time()-t0)*1000))


def _pi_stream(payload, session_id=None):
    """Send a request shaped like Pi's OpenAI-compatible client: streaming, Pi
    user agent, and `x-session-id` when `compat.sendSessionAffinityHeaders` is
    set (docs/pi-compatibility.md)."""
    headers = {
        "Authorization": f"Bearer {API_KEY}",
        "Content-Type": "application/json",
        "User-Agent": "pi (linux; x86_64)",
    }
    if session_id:
        headers["x-session-id"] = session_id
    req = urllib.request.Request(
        f"{BASE_URL}/v1/chat/completions",
        data=json.dumps({**payload, "stream": True}).encode(),
        headers=headers,
    )
    with urlopen(req, timeout=15) as resp:
        return read_sse_events(resp), resp.headers


def _pi_stream_tool_calls(events):
    calls = {}
    for _, ev in events:
        if not isinstance(ev, dict) or not ev.get("choices"):
            continue
        for call in ev["choices"][0].get("delta", {}).get("tool_calls", []) or []:
            slot = calls.setdefault(call.get("index", 0), {"id": None, "name": None, "arguments": ""})
            slot["id"] = call.get("id") or slot["id"]
            fn = call.get("function", {})
            slot["name"] = fn.get("name") or slot["name"]
            slot["arguments"] += fn.get("arguments") or ""
    return [calls[i] for i in sorted(calls)]


def _pi_stream_text(events):
    return "".join(
        ev["choices"][0].get("delta", {}).get("content") or ""
        for _, ev in events
        if isinstance(ev, dict) and ev.get("choices")
    )


_PI_WEATHER_TOOL = {
    "type": "function",
    "function": {
        "name": "get_weather",
        "description": "weather",
        "parameters": {"type": "object", "properties": {"city": {"type": "string"}}},
    },
}


def test_pi_tool_loop(upstream_model):
    """A real two-turn Pi tool loop: turn 2 answers the exact tool-call id
    Kinetix returned in turn 1. The strict upstream rejects turn 2 unless the
    tool result reaches it as a native tool result."""
    t0 = time.time()
    session_id = f"pi_loop_{upstream_model}_{int(time.time())}"
    user = {"role": "user", "content": "What is the weather in Paris?"}
    events, _ = _pi_stream({"model": upstream_model, "messages": [user], "tools": [_PI_WEATHER_TOOL]}, session_id)
    calls = _pi_stream_tool_calls(events)
    passed = len(calls) == 1 and bool(calls[0]["id"]) and calls[0]["name"] == "get_weather"
    detail = f"turn 1 calls={[(c['id'], c['name']) for c in calls]}"
    if passed:
        call = calls[0]
        events, _ = _pi_stream({
            "model": upstream_model,
            "tools": [_PI_WEATHER_TOOL],
            "messages": [
                user,
                {"role": "assistant", "content": None, "tool_calls": [{
                    "id": call["id"],
                    "type": "function",
                    "function": {"name": call["name"], "arguments": call["arguments"] or "{}"},
                }]},
                {"role": "tool", "tool_call_id": call["id"], "content": "fixture:tool-continuation 18C"},
            ],
        }, session_id)
        # The strict upstream returned 200, so the result reached it natively.
        passed = any(isinstance(e[1], dict) and e[1].get("choices") for e in events) and any(
            e[1] == "[DONE]" for e in events
        )
        detail = f"id {call['id']} answered"
    record_result("Pi (OpenAI Chat)", upstream_model, "tool_loop", passed, detail, int((time.time()-t0)*1000))


def test_pi_signature_continuation():
    """Pi never sends Gemini thought signatures back. Kinetix must restore the
    stored signature for the tool call Pi answers, or the strict upstream
    rejects the continuation like real Gemini 3 does."""
    t0 = time.time()
    session_id = f"pi_sig_{int(time.time())}"
    user = {"role": "user", "content": "fixture:gemini-signature-continuation weather?"}
    tool = _PI_WEATHER_TOOL
    events, _ = _pi_stream({"model": "syn-gemini-3", "messages": [user], "tools": [tool]}, session_id)
    calls = _pi_stream_tool_calls(events)
    passed = len(calls) == 1 and bool(calls[0]["id"])
    detail = f"turn 1 calls={len(calls)}"
    if passed:
        call = calls[0]
        events, _ = _pi_stream({
            "model": "syn-gemini-3",
            "tools": [tool],
            "messages": [
                user,
                {"role": "assistant", "content": None, "tool_calls": [{
                    "id": call["id"],
                    "type": "function",
                    "function": {"name": "get_weather", "arguments": call["arguments"] or "{}"},
                }]},
                {"role": "tool", "tool_call_id": call["id"], "content": "18C"},
            ],
        }, session_id)
        passed = any(e[1] == "[DONE]" for e in events)
        detail = "signature restored for Pi continuation"
    record_result("Pi (OpenAI Chat)", "syn-gemini-3", "signature_contin", passed, detail, int((time.time()-t0)*1000))


def test_pi_fallback_stream():
    t0 = time.time()
    events, headers = _pi_stream({"model": "syn-fallback", "messages": [{"role": "user", "content": "fallback"}]})
    fallback = headers.get("x-kinetix-fallback")
    passed = bool(_pi_stream_text(events)) and fallback == "1"
    record_result("Pi (OpenAI Chat)", "syn-fallback", "fallback_stream", passed, f"header={fallback}", int((time.time()-t0)*1000))


def test_unknown_client_field_is_rejected(upstream_model):
    # A translating path never silently drops a field it cannot honor.
    t0 = time.time()
    payload = {
        "model": upstream_model,
        "stream": False,
        "messages": [{"role": "user", "content": "unknown-field compatibility"}],
        "client_only_unknown": {"future": True},
    }
    req = urllib.request.Request(
        f"{BASE_URL}/v1/chat/completions",
        data=json.dumps(payload).encode(),
        headers={"Authorization": f"Bearer {API_KEY}", "Content-Type": "application/json", "User-Agent": "pi (linux; x86_64)"},
    )
    try:
        urllib.request.urlopen(req, timeout=15)
        status, body = 200, ""
    except urllib.error.HTTPError as error:
        status, body = error.code, error.read().decode()
    passed = status == 400 and "client_only_unknown" in body
    record_result("Pi (OpenAI Chat)", upstream_model, "unknown_fields", passed, f"HTTP {status}", int((time.time()-t0)*1000))


def test_route_fallback():
    t0 = time.time()
    payload = {"model": "syn-fallback", "stream": False, "messages": [{"role": "user", "content": "fallback"}]}
    req = urllib.request.Request(
        f"{BASE_URL}/v1/chat/completions",
        data=json.dumps(payload).encode(),
        headers={"Authorization": f"Bearer {API_KEY}", "Content-Type": "application/json"},
    )
    with urlopen(req, timeout=15) as resp:
        data = json.loads(resp.read().decode())
        fallback = resp.headers.get("x-kinetix-fallback")
    passed = bool(data.get("choices")) and fallback == "1"
    record_result("Routing", "syn-fallback", "route_fallback", passed, f"header={fallback}", int((time.time()-t0)*1000))


def _claude_headers(session_id=None):
    headers = {
        "x-api-key": API_KEY,
        "anthropic-version": "2023-06-01",
        "anthropic-beta": "claude-code-20250219,interleaved-thinking-2025-05-14",
        "Content-Type": "application/json",
        "User-Agent": "claude-cli/2.1.173 (external, cli)",
    }
    if session_id:
        headers["X-Claude-Code-Session-Id"] = session_id
    return headers


def test_claude_native_protocol():
    t0 = time.time()
    payload = {
        "model": "syn-anthropic",
        "stream": False,
        "max_tokens": 64,
        "messages": [{"role": "user", "content": "fixture:claude-protocol hello"}],
    }
    req = urllib.request.Request(
        f"{BASE_URL}/v1/messages",
        data=json.dumps(payload).encode(),
        headers=_claude_headers("claude-protocol-session"),
    )
    with urlopen(req, timeout=15) as resp:
        data = json.loads(resp.read().decode())
    passed = data.get("type") == "message"
    record_result("Claude Code", "syn-anthropic", "protocol_headers", passed, "version+beta+session", int((time.time()-t0)*1000))


def test_claude_exact_count_tokens():
    t0 = time.time()
    payload = {
        "model": "syn-anthropic",
        "messages": [{"role": "user", "content": "count me"}],
    }
    req = urllib.request.Request(
        f"{BASE_URL}/v1/messages/count_tokens",
        data=json.dumps(payload).encode(),
        headers=_claude_headers("claude-count-session"),
    )
    with urlopen(req, timeout=15) as resp:
        data = json.loads(resp.read().decode())
        mode = resp.headers.get("x-kinetix-token-count")
    passed = data.get("input_tokens") == 123 and mode == "exact"
    record_result("Claude Code", "syn-anthropic", "count_tokens_exact", passed, f"mode={mode}", int((time.time()-t0)*1000))


def test_claude_parallel_tools():
    t0 = time.time()
    payload = {
        "model": "syn-anthropic",
        "stream": True,
        "max_tokens": 128,
        "messages": [{"role": "user", "content": "fixture:multi-tools use both"}],
        "tools": [
            {"name": "get_weather", "input_schema": {"type": "object"}},
            {"name": "read_file", "input_schema": {"type": "object"}},
        ],
    }
    req = urllib.request.Request(
        f"{BASE_URL}/v1/messages",
        data=json.dumps(payload).encode(),
        headers=_claude_headers("claude-tools-session"),
    )
    with urlopen(req, timeout=15) as resp:
        events = read_sse_events(resp)
    names = {
        ev.get("content_block", {}).get("name")
        for name, ev in events
        if name == "content_block_start" and isinstance(ev, dict)
        and ev.get("content_block", {}).get("type") == "tool_use"
    }
    names.discard(None)
    passed = {"get_weather", "read_file"} <= names
    record_result("Claude Code", "syn-anthropic", "parallel_tools", passed, ",".join(sorted(names)), int((time.time()-t0)*1000))


def test_claude_vision_and_thinking():
    t0 = time.time()
    payload = {
        "model": "syn-anthropic",
        "stream": False,
        "max_tokens": 128,
        "thinking": {"type": "enabled", "budget_tokens": 4096},
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": "fixture:vision fixture:thinking inspect"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": _ONE_PIXEL_PNG.split(",", 1)[1]}},
            ],
        }],
    }
    req = urllib.request.Request(
        f"{BASE_URL}/v1/messages",
        data=json.dumps(payload).encode(),
        headers=_claude_headers("claude-vision-session"),
    )
    with urlopen(req, timeout=15) as resp:
        data = json.loads(resp.read().decode())
    passed = data.get("type") == "message"
    record_result("Claude Code", "syn-anthropic", "vision+thinking", passed, "native blocks", int((time.time()-t0)*1000))


def test_openai_truncated_passthrough_identity():
    t0 = time.time()
    req = urllib.request.Request(
        f"{BASE_URL}/v1/chat/completions",
        data=json.dumps({
            "model": "syn-truncated-openai",
            "stream": True,
            "messages": [{"role": "user", "content": "truncate"}],
        }).encode(),
        headers={
            "Authorization": f"Bearer {API_KEY}",
            "Content-Type": "application/json",
            "User-Agent": "pi (linux; x86_64)",
        },
    )
    with urlopen(req, timeout=15) as resp:
        events = read_sse_events(resp)

    chunks = [
        event for _, event in events
        if isinstance(event, dict) and event.get("choices")
    ]
    errors = [
        event for _, event in events
        if isinstance(event, dict) and isinstance(event.get("error"), dict)
    ]
    ids = {event.get("id") for event in chunks}
    models = {event.get("model") for event in chunks}
    synthesized_error_chunks = [
        event for event in chunks
        if event.get("choices", [{}])[0].get("finish_reason") == "error"
    ]
    identity_neutral_error = all(
        "id" not in event and "model" not in event for event in errors
    )
    passed = (
        ids == {"upstream-openai-truncated"}
        and models == {"syn-truncated-openai"}
        and len(errors) == 1
        and identity_neutral_error
        and not synthesized_error_chunks
        and any(event == "[DONE]" for _, event in events)
    )
    record_result(
        "Pi (OpenAI Chat)",
        "syn-truncated-openai",
        "truncated_identity",
        passed,
        f"ids={ids}, models={models}, errors={len(errors)}",
        int((time.time()-t0)*1000),
    )


def test_anthropic_broken_stream(model, capability):
    t0 = time.time()
    payload = {
        "model": model,
        "stream": True,
        "max_tokens": 64,
        "messages": [{"role": "user", "content": "broken stream"}],
    }
    req = urllib.request.Request(
        f"{BASE_URL}/v1/messages",
        data=json.dumps(payload).encode(),
        headers=_claude_headers("claude-broken-session"),
    )
    try:
        with urlopen(req, timeout=15) as resp:
            events = read_sse_events(resp)
        passed = capability == "truncated_stream" and any(
            name == "error" for name, _ in events
        )
        detail = f"events={len(events)}"
    except RuntimeError as error:
        message = str(error)
        passed = (
            capability == "malformed_stream"
            and "HTTP 502" in message
            and '"type":"error"' in message
            and '"type":"api_error"' in message
        )
        detail = message
    record_result(
        "Claude Code",
        model,
        capability,
        passed,
        detail,
        int((time.time()-t0)*1000),
    )


# ---------------------------------------------------------------------------
# Main Matrix Driver
# ---------------------------------------------------------------------------

def run_matrix():
    print("=" * 80)
    print("           KINETIX CODING-AGENT COMPATIBILITY MATRIX RUNNER")
    print("=" * 80)
    print(f"Target URL: {BASE_URL}")
    print()

    # 1. Pi Agent against Synth OpenAI (passthrough) and Synth Gemini (translation)
    print("==> Testing Pi Agent (OpenAI Chat Completions Wire Format)")
    test_pi_plain_stream("syn-openai")
    test_pi_tool_use("syn-openai")
    test_pi_multi_turn("syn-openai")
    test_pi_session_affinity("syn-openai")
    test_prompt_cache_key_affinity("syn-openai")
    test_pi_openrouter_affinity("syn-openai")
    test_pi_sync_aggregation("syn-openai")
    test_openai_truncated_passthrough_identity()

    test_pi_plain_stream("syn-gemini-3")
    test_pi_tool_use("syn-gemini-3")
    test_pi_multi_turn("syn-gemini-3")
    test_pi_parallel_tools("syn-gemini-3")
    test_pi_vision("syn-gemini-3")
    test_pi_thinking("syn-gemini-3")
    test_unknown_client_field_is_rejected("syn-gemini-3")
    test_route_fallback()

    # Pi acceptance profile (#201 tier 1): the request shapes Pi really sends,
    # each checked by a strict synthetic upstream.
    test_pi_tool_loop("syn-openai")
    test_pi_tool_loop("syn-gemini-3")
    test_pi_vision("syn-openai")
    test_pi_thinking("syn-openai")
    test_pi_signature_continuation()
    test_pi_fallback_stream()

    # 2. Next-Gen Coding Agent / Codex (OpenAI Responses API)
    print("\n==> Testing Next-Gen Coding Agent (OpenAI Responses API)")
    test_responses_plain_stream("syn-openai")
    test_responses_tool_use("syn-openai")
    test_responses_multi_turn("syn-openai")
    test_responses_session_affinity("syn-openai")
    test_responses_sync_aggregation("syn-openai")

    test_responses_plain_stream("syn-gemini-3")

    # 3. Anthropic Agent / Claude Code (Messages API)
    print("\n==> Testing Anthropic Agent (Claude Code / Anthropic Wire Format)")
    test_anthropic_count_tokens("syn-gemini-3")
    test_anthropic_plain_stream("syn-gemini-3")
    test_anthropic_tool_use("syn-gemini-3")
    test_anthropic_multi_turn("syn-gemini-3")
    test_anthropic_session_affinity("syn-gemini-3")
    test_anthropic_sync_aggregation("syn-gemini-3")

    test_claude_native_protocol()
    test_claude_exact_count_tokens()
    test_claude_parallel_tools()
    test_claude_vision_and_thinking()
    test_anthropic_broken_stream("syn-malformed", "malformed_stream")
    test_anthropic_broken_stream("syn-truncated", "truncated_stream")

    test_anthropic_plain_stream("syn-openai")

    # Summary
    print("\n" + "=" * 80)
    print("                              MATRIX SUMMARY")
    print("=" * 80)
    total = len(TEST_RESULTS)
    passed = sum(1 for r in TEST_RESULTS if r["passed"])
    failed = total - passed

    print(f"{'Client Profile':<24} | {'Upstream':<14} | {'Capability':<18} | {'Status':<6} | {'Time'}")
    print("-" * 80)
    for r in TEST_RESULTS:
        st = "PASS" if r["passed"] else "FAIL"
        print(f"{r['client']:<24} | {r['upstream']:<14} | {r['capability']:<18} | {st:<6} | {r['duration_ms']}ms")

    print("-" * 80)
    print(f"Total: {total} | Passed: {passed} | Failed: {failed} ({passed/total*100:.1f}% compatible)")
    print("=" * 80)

    if failed > 0:
        sys.exit(1)


if __name__ == "__main__":
    run_matrix()
