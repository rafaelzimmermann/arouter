#!/usr/bin/env python3
"""Stub OpenAI-compatible provider for arouter integration tests.

Behavior is flippable at runtime over HTTP so a single compose topology
can exercise failover, exhaustion and error propagation:

    curl -X POST localhost:9000/__mode -d '{"mode":"500"}'
    curl localhost:9000/__mode            # {"mode":"ok","hits":3}

Modes for POST /v1/chat/completions:
    ok     buffered completion with usage
    stream SSE chunks ending in a usage frame + [DONE]
    500    server error JSON
    429    rate limited, sends Retry-After: 77
    400    non-retryable client error (distinctive marker in the body)
    slow   sleeps 60s before answering
    reset  closes the connection without responding
"""
import json
import os
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(os.environ.get("PORT", "9000"))
MODEL = os.environ.get("MODEL", "stub-model")

lock = threading.Lock()
state = {"mode": os.environ.get("MODE", "ok"), "hits": 0}


class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def _json(self, code, obj, extra_headers=()):
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        for k, v in extra_headers:
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(body)

    def _read_body(self):
        n = int(self.headers.get("content-length") or 0)
        raw = self.rfile.read(n) if n else b"{}"
        try:
            return json.loads(raw)
        except Exception:
            return {}

    def do_GET(self):
        if self.path == "/v1/models":
            self._json(200, {
                "object": "list",
                "data": [{"id": MODEL, "object": "model", "owned_by": "stub"}],
            })
        elif self.path == "/__mode":
            with lock:
                self._json(200, dict(state))
        elif self.path == "/__hits":
            with lock:
                self._json(200, {"hits": state["hits"]})
        else:
            self._json(404, {"error": "not found"})

    def do_POST(self):
        global state
        if self.path == "/__mode":
            body = self._read_body()
            with lock:
                state["mode"] = body.get("mode", state["mode"])
                self._json(200, dict(state))
            return
        if self.path == "/__reset_hits":
            with lock:
                state["hits"] = 0
                self._json(200, {"hits": 0})
            return
        if self.path != "/v1/chat/completions":
            self._json(404, {"error": "not found"})
            return

        self._read_body()  # consume request body regardless of mode
        with lock:
            state["hits"] += 1
            mode = state["mode"]

        if mode == "reset":
            self.connection.close()
            return
        if mode == "slow":
            time.sleep(60)

        if mode == "500":
            self._json(500, {"error": {"message": "stub injected 500", "type": "server_error"}})
            return
        if mode == "429":
            self._json(429, {"error": {"message": "stub injected 429", "type": "rate_limit"}},
                       extra_headers=[("retry-after", "77")])
            return
        if mode == "400":
            self._json(400, {"error": {"message": "stub_bad_request_marker", "type": "invalid_request_error"}})
            return
        if mode == "stream":
            self.send_response(200)
            self.send_header("content-type", "text/event-stream")
            self.send_header("cache-control", "no-cache")
            # close-delimited body: the SSE stream ends when we hang up
            self.send_header("connection", "close")
            self.end_headers()
            self.close_connection = True
            chunks = [
                {"id": "stub", "object": "chat.completion.chunk", "model": MODEL,
                 "choices": [{"index": 0, "delta": {"role": "assistant"}}]},
                {"id": "stub", "object": "chat.completion.chunk", "model": MODEL,
                 "choices": [{"index": 0, "delta": {"content": f"stub stream from {MODEL}"}}]},
                {"id": "stub", "object": "chat.completion.chunk", "model": MODEL,
                 "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                 "usage": {"prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18}},
            ]
            for c in chunks:
                self.wfile.write(b"data: " + json.dumps(c).encode() + b"\n\n")
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
            return

        self._json(200, {
            "id": "stub-cmpl-1", "object": "chat.completion", "created": int(time.time()),
            "model": MODEL,
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": f"stub reply from {MODEL}"},
                "finish_reason": "stop",
            }],
            "usage": {"prompt_tokens": 17, "completion_tokens": 23, "total_tokens": 40},
        })


if __name__ == "__main__":
    ThreadingHTTPServer(("0.0.0.0", PORT), H).serve_forever()
