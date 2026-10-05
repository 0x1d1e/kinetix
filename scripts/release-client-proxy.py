#!/usr/bin/env python3
"""Transparent release-acceptance proxy that records client-visible protocol evidence."""

import argparse
import http.client
import json
import pathlib
import re
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlsplit

HOP_BY_HOP = {
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "transfer-encoding",
    "upgrade",
}
SESSION_HEADERS = (
    "x-kinetix-session",
    "x-session-id",
    "session-id",
    "x-conversation-id",
    "x-session-affinity",
    "x-claude-code-session-id",
)
LOG_LOCK = threading.Lock()


def walk(value):
    if isinstance(value, dict):
        yield value
        for child in value.values():
            yield from walk(child)
    elif isinstance(value, list):
        for child in value:
            yield from walk(child)


def request_tool_results(raw):
    try:
        value = json.loads(raw.decode() or "{}")
    except Exception:
        return 0
    count = 0
    for item in walk(value):
        if item.get("type") in {"function_call_output", "tool_result"}:
            count += 1
        elif item.get("role") in {"tool", "function"} and item.get("tool_call_id"):
            count += 1
    return count


IMAGE_PART_TYPES = {"image_url", "image", "input_image"}
REASONING_CONTROLS = {"reasoning_effort", "reasoning", "thinking"}
REASONING_REPLAY_KEYS = {"reasoning_content", "reasoning", "reasoning_details", "thinking"}


def request_evidence(raw):
    """Client-visible request features: image parts, reasoning control, replayed
    reasoning in history, and the tool-call ids that tool results answer."""
    try:
        value = json.loads(raw.decode() or "{}")
    except Exception:
        value = {}
    if not isinstance(value, dict):
        value = {}
    images = 0
    result_ids = set()
    replayed = 0
    for item in walk(value):
        if item.get("type") in IMAGE_PART_TYPES:
            images += 1
        if item.get("role") in {"tool", "function"} and item.get("tool_call_id"):
            result_ids.add(str(item["tool_call_id"]))
        if item.get("type") == "function_call_output" and item.get("call_id"):
            result_ids.add(str(item["call_id"]))
        if item.get("type") == "tool_result" and item.get("tool_use_id"):
            result_ids.add(str(item["tool_use_id"]))
        if item.get("role") == "assistant" and any(item.get(key) for key in REASONING_REPLAY_KEYS):
            replayed += 1
    return {
        "request_images": images,
        "request_reasoning_control": sorted(key for key in REASONING_CONTROLS if value.get(key)),
        "request_reasoning_replay": replayed,
        "request_tool_result_ids": sorted(result_ids),
    }


def response_tool_ids(raw):
    text = raw.decode(errors="replace")
    values = []
    try:
        values.append(json.loads(text))
    except Exception:
        pass
    for line in text.splitlines():
        if not line.startswith("data: "):
            continue
        payload = line[6:].strip()
        if not payload or payload == "[DONE]":
            continue
        try:
            values.append(json.loads(payload))
        except Exception:
            continue

    ids = set()
    for value in values:
        for item in walk(value):
            if item.get("type") == "tool_use" and item.get("id"):
                ids.add(str(item["id"]))
            if item.get("type") == "function_call":
                call_id = item.get("call_id") or item.get("id")
                if call_id:
                    ids.add(str(call_id))
            calls = item.get("tool_calls")
            if isinstance(calls, list):
                for call in calls:
                    call_id = call.get("id")
                    if call_id:
                        ids.add(str(call_id))
    return sorted(ids)


class ProxyHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    upstream = None
    log_path = None

    def log_message(self, *_args):
        pass

    def do_GET(self):
        self._forward()

    def do_POST(self):
        self._forward()

    def do_DELETE(self):
        self._forward()

    def do_PUT(self):
        self._forward()

    def _forward(self):
        split = self.upstream
        length = int(self.headers.get("content-length", "0") or 0)
        body = self.rfile.read(length) if length else b""

        base_path = split.path.rstrip("/")
        incoming = self.path if self.path.startswith("/") else "/" + self.path
        path = base_path + incoming

        headers = {
            key: value
            for key, value in self.headers.items()
            if key.lower() not in HOP_BY_HOP and key.lower() not in {"host", "content-length"}
        }
        headers["Host"] = split.netloc
        if body:
            headers["Content-Length"] = str(len(body))

        connection_cls = (
            http.client.HTTPSConnection if split.scheme == "https" else http.client.HTTPConnection
        )
        port = split.port or (443 if split.scheme == "https" else 80)
        conn = connection_cls(split.hostname, port, timeout=120)
        response_bytes = bytearray()
        status = 599
        response_headers = {}
        error = None

        try:
            conn.request(self.command, path, body=body or None, headers=headers)
            upstream_response = conn.getresponse()
            status = upstream_response.status
            response_headers = {key.lower(): value for key, value in upstream_response.getheaders()}

            self.send_response(status)
            for key, value in upstream_response.getheaders():
                lower = key.lower()
                if lower in HOP_BY_HOP or lower == "connection":
                    continue
                self.send_header(key, value)
            self.send_header("Connection", "close")
            self.end_headers()
            self.close_connection = True

            while True:
                # read1() returns data already available from the underlying
                # response/chunk instead of filling the requested buffer across
                # multiple SSE chunks. That keeps real-client acceptance truly
                # incremental for small token/tool events.
                chunk = upstream_response.read1(8192)
                if not chunk:
                    break
                if len(response_bytes) < 4 * 1024 * 1024:
                    response_bytes.extend(chunk[: 4 * 1024 * 1024 - len(response_bytes)])
                self.wfile.write(chunk)
                self.wfile.flush()
        except Exception as exc:
            error = f"{type(exc).__name__}: {exc}"
            if not self.wfile.closed and status == 599:
                payload = json.dumps({"error": error}).encode()
                try:
                    self.send_response(502)
                    self.send_header("content-type", "application/json")
                    self.send_header("content-length", str(len(payload)))
                    self.send_header("Connection", "close")
                    self.end_headers()
                    self.wfile.write(payload)
                except Exception:
                    pass
        finally:
            conn.close()
            sessions = {}
            for name in SESSION_HEADERS:
                value = self.headers.get(name)
                if value:
                    sessions[name] = value
            model = None
            request_stream = None
            try:
                request_json = json.loads(body.decode() or "{}")
                if isinstance(request_json, dict):
                    model = request_json.get("model")
                    request_stream = request_json.get("stream")
            except Exception:
                pass

            error_excerpt = None
            if status >= 400 and response_bytes:
                error_excerpt = re.sub(r"\s+", " ", response_bytes.decode(errors="replace"))[:2000]

            record = {
                "method": self.command,
                "path": self.path,
                "model": model,
                "request_stream": request_stream,
                "session_headers": sessions,
                "request_tool_results": request_tool_results(body),
                "response_status": status,
                "response_content_type": response_headers.get("content-type"),
                "request_id": response_headers.get("x-request-id"),
                "opaque_route_id": response_headers.get("x-kinetix-route-id"),
                "fallback": response_headers.get("x-kinetix-fallback"),
                "warning": response_headers.get("x-kinetix-warning"),
                "tool_call_ids": response_tool_ids(bytes(response_bytes)),
                **request_evidence(body),
                "error": error,
                "response_error": error_excerpt,
            }
            with LOG_LOCK:
                with self.log_path.open("a", encoding="utf-8") as handle:
                    handle.write(json.dumps(record, sort_keys=True) + "\n")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--upstream", required=True)
    parser.add_argument("--log", required=True)
    parser.add_argument("--port-file", required=True)
    args = parser.parse_args()

    upstream = urlsplit(args.upstream.rstrip("/"))
    if upstream.scheme not in {"http", "https"} or not upstream.hostname:
        raise SystemExit("invalid --upstream URL")

    ProxyHandler.upstream = upstream
    ProxyHandler.log_path = pathlib.Path(args.log)
    ProxyHandler.log_path.parent.mkdir(parents=True, exist_ok=True)
    ProxyHandler.log_path.write_text("")

    server = ThreadingHTTPServer(("127.0.0.1", 0), ProxyHandler)
    server.daemon_threads = True
    pathlib.Path(args.port_file).write_text(str(server.server_port))
    server.serve_forever()


if __name__ == "__main__":
    main()
