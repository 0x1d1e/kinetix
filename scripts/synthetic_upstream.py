#!/usr/bin/env python3
"""Deterministic synthetic LLM upstream for Kinetix benchmarking (NFR-1.8).

Speaks two wire formats on one port, selected by path:
  * POST /openai/v1/chat/completions  -- OpenAI SSE (`data:` frames, [DONE])
  * POST /gemini/v1beta/models/<m>:streamGenerateContent?alt=sse -- Gemini SSE

Token cadence is fixed (TOKENS, DELAY_MS) so any measured variance is Kinetix
overhead, not inference. No external dependencies; stdlib only.
"""
import json
import os
import re
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

TOKENS = int(os.environ.get("SYN_TOKENS", "40"))
DELAY_MS = float(os.environ.get("SYN_DELAY_MS", "2"))
TTFT_MS = float(os.environ.get("SYN_TTFT_MS", "5"))

WORD = "lorem"

# Wall-clock time of the most recent successful upstream write, used by the
# cancellation benchmark to measure how fast Kinetix drops a dead client.
LAST_WRITE = time.time()
_LW_LOCK = threading.Lock()


def mark_write():
    global LAST_WRITE
    with _LW_LOCK:
        LAST_WRITE = time.time()



def _split_json_fragments(n: int) -> list:
    """Split a JSON tool-argument payload into n fragments (NFR-1.9).

    The concatenation of the fragments is always valid JSON, so the reassembly
    path can be exercised with large, many-piece arguments.
    """
    payload = '{"city": "' + ("Paris-" * 40) + '", "note": "' + ("x" * 400) + '"}'
    n = max(1, min(n, len(payload)))
    size = (len(payload) + n - 1) // n
    return [payload[i:i + size] for i in range(0, len(payload), size)]

class _Disconnected(Exception):
    """Client went away mid-stream (expected under cancellation benchmarks)."""



def _contains_key(value, names):
    if isinstance(value, dict):
        return any(k in names or _contains_key(v, names) for k, v in value.items())
    if isinstance(value, list):
        return any(_contains_key(v, names) for v in value)
    return False


def _fixture(req, name):
    return f"fixture:{name}" in json.dumps(req, sort_keys=True)


def _path_model(path):
    """The upstream model id from a Gemini `.../models/<id>:method` path.

    The Gemini wire format carries the model in the URL, not the body, so a
    model-aware fixture must read it from here.
    """
    match = re.search(r"/models/([^:?/]+)", path)
    return match.group(1) if match else ""


def _function_call_parts(value):
    """Return every `functionCall` part (the enclosing part, so the sibling
    `thoughtSignature` is visible) in a Gemini body."""
    found = []

    def walk(node):
        if isinstance(node, dict):
            for key, child in node.items():
                if key == "functionCall" and isinstance(child, dict):
                    found.append(node)
                walk(child)
        elif isinstance(node, list):
            for child in node:
                walk(child)

    walk(value)
    return found


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):  # silence
        pass

    def do_GET(self):
        # Model discovery endpoint.
        if self.path.endswith("/_last_write"):
            body = json.dumps({"last_write": LAST_WRITE}).encode()
            self.send_response(200)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if self.path.endswith("/models"):
            body = json.dumps(
                {
                    "data": [
                        {"id": "syn-openai", "context_window": 200000, "max_output_tokens": 8192},
                        {"id": "syn-gemini-3", "context_window": 200000, "max_output_tokens": 8192},
                    ]
                }
            ).encode()
            self.send_response(200)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        self.send_response(404)
        self.send_header("content-length", "0")
        self.end_headers()

    def do_POST(self):
        length = int(self.headers.get("content-length", "0") or 0)
        raw = self.rfile.read(length) if length else b"{}"
        try:
            req = json.loads(raw or b"{}")
        except Exception:
            req = {}
        model = req.get("model", "syn")
        stream = bool(req.get("stream"))

        want_tools = bool(req.get("tools"))
        # NFR-1.9: a large tool argument delivered as many small fragments, to
        # exercise incremental argument reassembly under load. The benchmark
        # client sets this on the request body.
        tool_fragments = int(req.get("tool_fragments", 0) or 0)

        if "/anthropic/" in self.path:
            self._anthropic(req, model, stream, want_tools)
            return

        if "syn-fail" in self.path or model == "syn-fail":
            self._json(503, {"error": {"message": "synthetic primary failure"}})
            return

        if "/gemini/" in self.path:
            if _fixture(req, "chat-fields"):
                required = {"temperature", "topP", "topK", "maxOutputTokens", "stopSequences", "seed", "toolConfig", "thinkingConfig"}
                missing = sorted(key for key in required if not _contains_key(req, {key}))
                if missing:
                    self._json(400, {"error": {"message": f"chat field semantics missing: {missing}"}})
                    return
            if _fixture(req, "messages-fields"):
                required = {"systemInstruction", "temperature", "topP", "topK", "maxOutputTokens", "stopSequences", "toolConfig", "thinkingConfig"}
                missing = sorted(key for key in required if not _contains_key(req, {key}))
                if missing:
                    self._json(400, {"error": {"message": f"messages field semantics missing: {missing}"}})
                    return
            if _fixture(req, "responses-fields"):
                required = {"systemInstruction", "temperature", "topP", "topK", "maxOutputTokens", "toolConfig", "thinkingConfig"}
                missing = sorted(key for key in required if not _contains_key(req, {key}))
                if missing:
                    self._json(400, {"error": {"message": f"responses field semantics missing: {missing}"}})
                    return
            if _fixture(req, "system-variant") and not _contains_key(req, {"systemInstruction"}):
                self._json(400, {"error": {"message": "system variant was not translated"}})
                return
            for fixture, expected_mode in [
                ("tool-choice-auto", "AUTO"),
                ("tool-choice-none", "NONE"),
                ("tool-choice-required", "ANY"),
                ("tool-choice-specific", "ANY"),
            ]:
                if _fixture(req, fixture):
                    config = req.get("toolConfig", {}).get("functionCallingConfig", {})
                    if config.get("mode") != expected_mode:
                        self._json(400, {"error": {"message": f"{fixture} translated to wrong Gemini mode"}})
                        return
                    if fixture == "tool-choice-specific" and config.get("allowedFunctionNames") != ["get_weather"]:
                        self._json(400, {"error": {"message": "specific tool choice lost function name"}})
                        return
            if "client_only_unknown" in req:
                self._json(400, {"error": {"message": "unknown client field leaked upstream"}})
                return
            if _fixture(req, "thinking") and not _contains_key(req, {"thinkingConfig"}):
                self._json(400, {"error": {"message": "thinking control was not translated"}})
                return
            if _fixture(req, "vision") and not _contains_key(req, {"inlineData", "inline_data", "fileData", "file_data"}):
                self._json(400, {"error": {"message": "image was not translated"}})
                return
            if _fixture(req, "nested-schema") and not _contains_key(req, {"deep_tag"}):
                self._json(400, {"error": {"message": "nested tool schema was not preserved"}})
                return
            if _fixture(req, "tool-continuation") and not _contains_key(req, {"functionResponse"}):
                self._json(400, {"error": {"message": "tool result identity was not preserved"}})
                return
            if _fixture(req, "opaque-fallback") and _contains_key(
                req, {"thoughtSignature", "reasoning_signature", "reasoning_content"}
            ):
                self._json(400, {"error": {"message": "opaque reasoning state crossed portability boundary"}})
                return
            if _fixture(req, "gemini-cross-model-placeholder") and _contains_key(req, {"functionResponse"}):
                # Cross-model continuation: the trace originated on `syn-gemini-3`
                # (real signature OPAQUE_SIG_FLASH) but is being continued on
                # `syn-gemini-3-pro`, which cannot carry that signature. Gemini
                # documents the placeholder `skip_thought_signature_validator`
                # for exactly this transfer, and rejects both an unsigned call
                # and a foreign model's real signature.
                calls = _function_call_parts(req)
                if not calls:
                    self._json(400, {"error": {"message": "continuation lost the originating function call"}})
                    return
                wire_model = _path_model(self.path) or model
                # A model accepts its own real signature, or the documented
                # placeholder for a trace transferred from another model. It
                # rejects an unsigned call and another model's real signature.
                own = "OPAQUE_SIG_PRO" if "pro" in wire_model else "OPAQUE_SIG_FLASH"
                allowed = {own, "skip_thought_signature_validator"}
                if any(part.get("thoughtSignature") not in allowed for part in calls):
                    self._json(400, {"error": {"message": (
                        "Function call is missing a thought_signature in functionCall parts. "
                        "This is required for tools to work correctly."
                    )}})
                    return
            if _fixture(req, "gemini-legacy-model-continuation") and _contains_key(req, {"functionResponse"}):
                # Gemini 2.5 predates the Gemini 3 signature validation: an
                # unsigned historical function call is accepted there, but the
                # Gemini 3 validator-bypass sentinel is not documented and must
                # not be injected. Reject any thoughtSignature so the test can
                # only pass by actually stripping the incompatible state.
                calls = _function_call_parts(req)
                if not calls:
                    self._json(400, {"error": {"message": "continuation lost the originating function call"}})
                    return
                if any(part.get("thoughtSignature") is not None for part in calls):
                    self._json(400, {"error": {"message": (
                        "unexpected thoughtSignature on a Gemini 2.5 function call: "
                        "the Gemini 3 validator-bypass placeholder must not be injected"
                    )}})
                    return
            if _fixture(req, "gemini-signature-continuation") and _contains_key(req, {"functionResponse"}):
                # Strict Gemini behavior: once a signed function call has been
                # continued, every function-call part in the history must carry
                # its exact thoughtSignature. This mirrors the real provider's
                # "Function call is missing a thought_signature" rejection and is
                # what Kinetix's server-side replay must satisfy. The
                # originating function call must also still be present at all.
                calls = _function_call_parts(req)
                if not calls:
                    self._json(400, {"error": {"message": "continuation lost the originating function call"}})
                    return
                if any(
                    part.get("thoughtSignature") != "OPAQUE_SIG_CONTINUATION" for part in calls
                ):
                    self._json(400, {"error": {"message": (
                        "Function call is missing a thought_signature in functionCall parts. "
                        "This is required for tools to work correctly."
                    )}})
                    return
            self._gemini(model, want_tools, req)
        else:
            self._openai(model, stream, want_tools, tool_fragments, req)

    def _json(self, status, obj, headers=None):
        body = json.dumps(obj).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        for name, value in (headers or {}).items():
            self.send_header(name, value)
        self.end_headers()
        self.wfile.write(body)

    # -- OpenAI-compatible SSE -------------------------------------------
    def _openai(self, model, stream, want_tools=False, tool_fragments=0, req=None):
        req = req or {}
        if _fixture(req, "chat-fields"):
            required = {"temperature", "top_p", "top_k", "max_tokens", "stop", "seed", "presence_penalty", "frequency_penalty", "tool_choice"}
            missing = sorted(key for key in required if not _contains_key(req, {key}))
            if missing:
                self._json(400, {"error": {"message": f"chat field semantics missing: {missing}"}})
                return
        if _fixture(req, "messages-fields"):
            required = {"temperature", "top_p", "top_k", "max_tokens", "stop", "tool_choice", "reasoning_effort"}
            missing = sorted(key for key in required if not _contains_key(req, {key}))
            if missing:
                self._json(400, {"error": {"message": f"messages field semantics missing: {missing}"}})
                return
        if _fixture(req, "responses-fields"):
            required = {"temperature", "top_p", "top_k", "max_tokens", "presence_penalty", "frequency_penalty", "tool_choice", "reasoning_effort", "prompt_cache_key"}
            missing = sorted(key for key in required if not _contains_key(req, {key}))
            if missing:
                self._json(400, {"error": {"message": f"responses field semantics missing: {missing}"}})
                return
        if _fixture(req, "openai-extra"):
            if "vendor_extension" not in req or req.get("n") != 2:
                self._json(400, {"error": {"message": "OpenAI same-format extensions were not preserved"}})
                return
        passthrough_requirements = {
            "passthrough-n": {"n"},
            "passthrough-logprobs": {"logprobs", "top_logprobs"},
            "passthrough-response-format": {"response_format"},
            "passthrough-modalities-audio": {"modalities", "audio"},
            "passthrough-prediction": {"prediction"},
        }
        for fixture, required in passthrough_requirements.items():
            if _fixture(req, fixture):
                missing = sorted(key for key in required if key not in req)
                if missing:
                    self._json(400, {"error": {"message": f"{fixture} fields missing: {missing}"}})
                    return
        if _fixture(req, "thinking") and not _contains_key(req, {"reasoning_effort", "reasoning"}):
            self._json(400, {"error": {"message": "reasoning control was not translated"}})
            return
        if _fixture(req, "vision") and not _contains_key(req, {"image_url"}):
            self._json(400, {"error": {"message": "image was not translated"}})
            return
        if _fixture(req, "nested-schema") and not _contains_key(req, {"deep_tag"}):
            self._json(400, {"error": {"message": "nested tool schema was not preserved"}})
            return
        if _fixture(req, "tool-continuation") and not _contains_key(req, {"tool_call_id"}):
            self._json(400, {"error": {"message": "tool result identity was not preserved"}})
            return

        if model == "syn-truncated-openai":
            self.send_response(200)
            self.send_header("content-type", "text/event-stream")
            self.send_header("connection", "close")
            self.end_headers()
            self.close_connection = True

            def raw_frame(obj):
                self.wfile.write(b"data: " + json.dumps(obj).encode() + b"\n\n")
                self.wfile.flush()
                mark_write()

            raw_frame({
                "id": "upstream-openai-truncated",
                "object": "chat.completion.chunk",
                "model": model,
                "choices": [{
                    "index": 0,
                    "delta": {"role": "assistant"},
                    "finish_reason": None,
                }],
            })
            raw_frame({
                "id": "upstream-openai-truncated",
                "object": "chat.completion.chunk",
                "model": model,
                "choices": [{
                    "index": 0,
                    "delta": {"content": "partial"},
                    "finish_reason": None,
                }],
            })
            return

        usage = (
            {"prompt_tokens": 100}
            if _fixture(req, "partial-usage")
            else {"prompt_tokens": 100, "completion_tokens": TOKENS, "total_tokens": 100 + TOKENS}
        )

        if not stream:
            message = {
                "role": "assistant",
                "content": (
                    f"target:{model}"
                    if model in ("syn-openai-a", "syn-openai-b")
                    else WORD * 4
                ),
            }
            finish_reason = "stop"
            if want_tools:
                message = {
                    "role": "assistant",
                    "content": None,
                    "tool_calls": [{
                        "id": "call_syn_sync_1",
                        "type": "function",
                        "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"},
                    }],
                }
                finish_reason = "tool_calls"
            body = json.dumps(
                {
                    "id": "syn-1",
                    "object": "chat.completion",
                    "model": model,
                    "choices": [{
                        "index": 0,
                        "message": message,
                        "finish_reason": finish_reason,
                    }],
                    "usage": usage,
                }
            ).encode()
            self.send_response(200)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return

        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("cache-control", "no-cache")
        self.send_header("connection", "close")
        self.end_headers()
        # No content-length on a stream: signal end-of-body by closing.
        self.close_connection = True

        def frame(obj):
            try:
                self.wfile.write(b"data: " + json.dumps(obj).encode() + b"\n\n")
                self.wfile.flush()
                mark_write()
            except (BrokenPipeError, ConnectionResetError):
                raise _Disconnected()

        try:
            time.sleep(TTFT_MS / 1000.0)
            frame({"id": "syn-1", "object": "chat.completion.chunk", "model": model,
                   "choices": [{"index": 0, "delta": {"role": "assistant"}, "finish_reason": None}]})
            if _fixture(req, "hold-budget"):
                time.sleep(5)
            if want_tools:
                # Emit a function call with the arguments split across frames so
                # the tool-argument reassembly path is exercised.
                frame({"id": "syn-1", "object": "chat.completion.chunk", "model": model,
                       "choices": [{"index": 0, "delta": {"tool_calls": [
                           {"index": 0, "id": "call_syn_1", "type": "function",
                            "function": {"name": "get_weather", "arguments": ""}}]},
                           "finish_reason": None}]})
                frags = (
                    ['{"city"', ': "Par', 'is"}']
                    if tool_fragments <= 0
                    else _split_json_fragments(tool_fragments)
                )
                for frag in frags:
                    frame({"id": "syn-1", "object": "chat.completion.chunk", "model": model,
                           "choices": [{"index": 0, "delta": {"tool_calls": [
                               {"index": 0, "function": {"arguments": frag}}]},
                               "finish_reason": None}]})
                frame({"id": "syn-1", "object": "chat.completion.chunk", "model": model,
                       "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]})
                frame({"id": "syn-1", "object": "chat.completion.chunk", "model": model,
                       "choices": [], "usage": {"prompt_tokens": 100, "completion_tokens": 12,
                                                "total_tokens": 112}})
                self.wfile.write(b"data: [DONE]\n\n")
                self.wfile.flush()
                return
            stream_word = (
                f"target:{model}"
                if model in ("syn-openai-a", "syn-openai-b")
                else WORD
            )
            for _ in range(TOKENS):
                frame({"id": "syn-1", "object": "chat.completion.chunk", "model": model,
                       "choices": [{"index": 0, "delta": {"content": stream_word}, "finish_reason": None}]})
                if DELAY_MS:
                    time.sleep(DELAY_MS / 1000.0)
            frame({"id": "syn-1", "object": "chat.completion.chunk", "model": model,
                   "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]})
            frame({"id": "syn-1", "object": "chat.completion.chunk", "model": model,
                   "choices": [], "usage": usage})
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
        except (_Disconnected, BrokenPipeError, ConnectionResetError):
            pass

    # -- Anthropic Messages -------------------------------------------------
    def _anthropic(self, req, model, stream, want_tools=False):
        if _fixture(req, "chat-fields"):
            required = {"temperature", "top_p", "top_k", "max_tokens", "stop_sequences", "tool_choice", "thinking"}
            missing = sorted(key for key in required if not _contains_key(req, {key}))
            if missing:
                self._json(400, {"type": "error", "error": {"type": "invalid_request_error", "message": f"chat field semantics missing: {missing}"}})
                return
        if _fixture(req, "messages-fields"):
            required = {"system", "temperature", "top_p", "top_k", "max_tokens", "stop_sequences", "tool_choice", "thinking"}
            missing = sorted(key for key in required if not _contains_key(req, {key}))
            if missing:
                self._json(400, {"type": "error", "error": {"type": "invalid_request_error", "message": f"messages field semantics missing: {missing}"}})
                return
        if _fixture(req, "responses-fields"):
            required = {"system", "temperature", "top_p", "top_k", "max_tokens", "tool_choice", "thinking"}
            missing = sorted(key for key in required if not _contains_key(req, {key}))
            if missing:
                self._json(400, {"type": "error", "error": {"type": "invalid_request_error", "message": f"responses field semantics missing: {missing}"}})
                return
        if _fixture(req, "anthropic-extra") and "vendor_extension" not in req:
            self._json(400, {"type": "error", "error": {"type": "invalid_request_error", "message": "Anthropic provider extension was not preserved"}})
            return
        if self.path.endswith("/messages/count_tokens"):
            self._json(200, {"input_tokens": 123})
            return

        if _fixture(req, "claude-protocol"):
            if self.headers.get("anthropic-version") != "2023-06-01":
                self._json(400, {"type": "error", "error": {"type": "invalid_request_error", "message": "missing anthropic-version"}})
                return
            if not self.headers.get("anthropic-beta"):
                self._json(400, {"type": "error", "error": {"type": "invalid_request_error", "message": "missing anthropic-beta"}})
                return

        if _fixture(req, "thinking") and not _contains_key(req, {"thinking"}):
            self._json(400, {"type": "error", "error": {"type": "invalid_request_error", "message": "thinking control missing"}})
            return
        if _fixture(req, "vision") and not _contains_key(req, {"source"}):
            self._json(400, {"type": "error", "error": {"type": "invalid_request_error", "message": "image missing"}})
            return
        if _fixture(req, "nested-schema") and not _contains_key(req, {"deep_tag"}):
            self._json(400, {"type": "error", "error": {"type": "invalid_request_error", "message": "nested tool schema missing"}})
            return
        if _fixture(req, "tool-continuation") and not _contains_key(req, {"tool_use_id"}):
            self._json(400, {"type": "error", "error": {"type": "invalid_request_error", "message": "tool result identity missing"}})
            return

        if model == "syn-malformed":
            self.send_response(200)
            self.send_header("content-type", "text/event-stream")
            self.send_header("connection", "close")
            self.end_headers()
            self.close_connection = True
            self.wfile.write(b"event: message_start\n")
            self.wfile.write(b"data: {not-json}\n\n")
            self.wfile.flush()
            return

        if model == "syn-truncated":
            self.send_response(200)
            self.send_header("content-type", "text/event-stream")
            self.send_header("connection", "close")
            self.end_headers()
            self.close_connection = True
            start = {
                "type": "message_start",
                "message": {
                    "id": "msg_truncated",
                    "type": "message",
                    "role": "assistant",
                    "model": model,
                    "content": [],
                    "stop_reason": None,
                    "usage": {"input_tokens": 10, "output_tokens": 0},
                },
            }
            self.wfile.write(b"event: message_start\n")
            self.wfile.write(b"data: " + json.dumps(start).encode() + b"\n\n")
            self.wfile.write(b"event: content_block_delta\n")
            self.wfile.write(b'data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial"}}\n\n')
            self.wfile.flush()
            return

        if not stream:
            text = (
                f"target:{model}"
                if model in ("syn-anthropic-a", "syn-anthropic-b")
                else WORD * 4
            )
            content = [{"type": "text", "text": text}]
            stop_reason = "end_turn"
            if want_tools:
                content = [{
                    "type": "tool_use",
                    "id": "toolu_syn_sync_1",
                    "name": "get_weather",
                    "input": {"city": "Paris"},
                }]
                stop_reason = "tool_use"
            self._json(200, {
                "id": "msg_syn_1",
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": content,
                "stop_reason": stop_reason,
                "usage": {"input_tokens": 100, "output_tokens": 12},
            }, {"request-id": "req_syn_anthropic"})
            return

        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("cache-control", "no-cache")
        self.send_header("connection", "close")
        self.end_headers()
        self.close_connection = True

        def event(name, obj):
            try:
                self.wfile.write(f"event: {name}\n".encode())
                self.wfile.write(b"data: " + json.dumps(obj).encode() + b"\n\n")
                self.wfile.flush()
                mark_write()
            except (BrokenPipeError, ConnectionResetError):
                raise _Disconnected()

        try:
            event("message_start", {
                "type": "message_start",
                "message": {
                    "id": "msg_syn_1",
                    "type": "message",
                    "role": "assistant",
                    "model": model,
                    "content": [],
                    "stop_reason": None,
                    "usage": {"input_tokens": 100, "output_tokens": 0},
                },
            })
            if want_tools:
                calls = [("toolu_syn_1", "get_weather", {"city": "Paris"})]
                if _fixture(req, "multi-tools"):
                    calls.append(("toolu_syn_2", "read_file", {"path": "src/main.rs"}))
                for index, (tool_id, name, args) in enumerate(calls):
                    event("content_block_start", {
                        "type": "content_block_start",
                        "index": index,
                        "content_block": {"type": "tool_use", "id": tool_id, "name": name, "input": {}},
                    })
                    event("content_block_delta", {
                        "type": "content_block_delta",
                        "index": index,
                        "delta": {"type": "input_json_delta", "partial_json": json.dumps(args)},
                    })
                    event("content_block_stop", {"type": "content_block_stop", "index": index})
                event("message_delta", {
                    "type": "message_delta",
                    "delta": {"stop_reason": "tool_use", "stop_sequence": None},
                    "usage": {"output_tokens": 12},
                })
            else:
                event("content_block_start", {
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {"type": "text", "text": ""},
                })
                text = (
                    f"target:{model}"
                    if model in ("syn-anthropic-a", "syn-anthropic-b")
                    else WORD
                )
                event("content_block_delta", {
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "text_delta", "text": text},
                })
                event("content_block_stop", {"type": "content_block_stop", "index": 0})
                event("message_delta", {
                    "type": "message_delta",
                    "delta": {"stop_reason": "end_turn", "stop_sequence": None},
                    "usage": {"output_tokens": 4},
                })
            event("message_stop", {"type": "message_stop"})
        except (_Disconnected, BrokenPipeError, ConnectionResetError):
            pass

    # -- Gemini SSE -------------------------------------------------------
    def _gemini(self, model, want_tools=False, req=None):
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("cache-control", "no-cache")
        self.send_header("connection", "close")
        self.end_headers()
        self.close_connection = True

        def frame(obj):
            try:
                self.wfile.write(b"data: " + json.dumps(obj).encode() + b"\r\n\r\n")
                self.wfile.flush()
                mark_write()
            except (BrokenPipeError, ConnectionResetError):
                raise _Disconnected()

        try:
            time.sleep(TTFT_MS / 1000.0)
            if want_tools:
                if req is not None and (
                    _fixture(req, "gemini-cross-model-placeholder")
                    or _fixture(req, "gemini-legacy-model-continuation")
                ):
                    # The originating model's real signature; the other model must
                    # never receive it (it gets the documented placeholder).
                    origin_sig = (
                        "OPAQUE_SIG_PRO"
                        if "pro" in _path_model(self.path)
                        else "OPAQUE_SIG_FLASH"
                    )
                    calls = [{
                        "functionCall": {"name": "get_weather", "args": {"city": "Paris"}},
                        "thoughtSignature": origin_sig,
                    }]
                elif req is not None and _fixture(req, "gemini-signature-continuation"):
                    # Deliberately omit functionCall.id so Kinetix must generate
                    # the client-visible id; the test then continues with the id
                    # Kinetix actually exposed.
                    calls = [{
                        "functionCall": {"name": "get_weather", "args": {"city": "Paris"}},
                        "thoughtSignature": "OPAQUE_SIG_CONTINUATION",
                    }]
                else:
                    calls = [
                        {"functionCall": {"name": "get_weather", "args": {"city": "Paris"}}}
                    ]
                if req is not None and _fixture(req, "multi-tools"):
                    calls.append(
                        {"functionCall": {"name": "read_file", "args": {"path": "src/main.rs"}}}
                    )
                frame({"candidates": [{"content": {"role": "model", "parts": calls}}]})
                frame({"candidates": [{"content": {"role": "model", "parts": []}, "finishReason": "STOP"}],
                       "usageMetadata": {"promptTokenCount": 100, "candidatesTokenCount": 10,
                                         "totalTokenCount": 110}})
                return
            for _ in range(TOKENS):
                frame({"candidates": [{"content": {"role": "model", "parts": [{"text": WORD}]}}]})
                if DELAY_MS:
                    time.sleep(DELAY_MS / 1000.0)
            frame({"candidates": [{"content": {"role": "model", "parts": []}, "finishReason": "STOP"}],
                   "usageMetadata": {"promptTokenCount": 100, "candidatesTokenCount": TOKENS,
                                     "totalTokenCount": 100 + TOKENS}})
        except (_Disconnected, BrokenPipeError, ConnectionResetError):
            pass


def main():
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 9099
    srv = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    srv.daemon_threads = True
    srv.serve_forever()


if __name__ == "__main__":
    main()
