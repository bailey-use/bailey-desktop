#!/usr/bin/env python3
"""Deterministic OpenAI-chat fake provider for the canary's protocol-transform
test. Speaks ONLY OpenAI: a real CLI pointed here through aivo is forced to
have its native protocol (Anthropic/Google/Responses) transformed to/from
OpenAI. Records the paths it receives so the test can prove the transform
happened. No intelligence — canned CANARY_OK reply."""
import json
import os
import sys
import time
from http.server import BaseHTTPRequestHandler, HTTPServer

REPLY = "CANARY_OK"
SEEN = os.environ.get("FAKE_SEEN_LOG", "/tmp/fake_seen.log")


def log_path(method, path):
    with open(SEEN, "a") as f:
        f.write(f"{method} {path}\n")


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass  # quiet

    def _json(self, code, obj):
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        log_path("GET", self.path)
        if self.path.startswith("/v1/models") or self.path.endswith("/models"):
            self._json(200, {"object": "list", "data": [
                {"id": "canary-model", "object": "model", "owned_by": "fake"}]})
        else:
            self._json(404, {"error": {"message": "not found"}})

    def do_POST(self):
        log_path("POST", self.path)
        n = int(self.headers.get("Content-Length", 0))
        raw = self.rfile.read(n) if n else b""
        try:
            req = json.loads(raw)
        except Exception:
            req = {}
        # Only OpenAI chat is implemented; native-protocol probes (/v1/messages,
        # /v1/responses, :generateContent) fall through to 404 so aivo's cascade
        # walks to chat/completions — exercising the fallback too.
        if "chat/completions" not in self.path:
            self._json(404, {"error": {"message": "unsupported endpoint"}})
            return
        model = req.get("model", "canary-model")
        if req.get("stream"):
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            chunks = [
                {"choices": [{"index": 0, "delta": {"role": "assistant"}, "finish_reason": None}]},
                {"choices": [{"index": 0, "delta": {"content": REPLY}, "finish_reason": None}]},
                {"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]},
            ]
            for c in chunks:
                c.update({"id": "chatcmpl-canary", "object": "chat.completion.chunk",
                          "created": 1, "model": model})
                self.wfile.write(f"data: {json.dumps(c)}\n\n".encode())
                self.wfile.flush()
            # Final usage chunk + DONE.
            usage = {"id": "chatcmpl-canary", "object": "chat.completion.chunk",
                     "created": 1, "model": model, "choices": [],
                     "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}}
            self.wfile.write(f"data: {json.dumps(usage)}\n\n".encode())
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
        else:
            self._json(200, {
                "id": "chatcmpl-canary", "object": "chat.completion", "created": 1,
                "model": model,
                "choices": [{"index": 0, "message": {"role": "assistant", "content": REPLY},
                             "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
            })


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 0
    open(SEEN, "w").close()
    srv = HTTPServer(("127.0.0.1", port), Handler)
    print(srv.server_address[1], flush=True)  # print bound port
    srv.serve_forever()
