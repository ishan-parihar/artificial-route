#!/usr/bin/env python3
"""Always-200 chat upstream for the soak harness.

Soaks must measure the proxy on the path traffic actually takes: pointing
them at the fixtures' dummy keys soaks the *failure* path (connect refused,
verdict emission), whose allocation profile is a different animal. The stub
answers every POST with the dialect's shapes: stream:false OpenAI JSON,
stream:true chunked SSE carrying a usage chunk, and an embeddings-shaped
body for the /v1/embeddings route.

The request body's `stream` flag decides; unparseable bodies answer the
non-stream shape. Host fixed to loopback; ports are a CLI flag.
`python scripts/serve_stub_upstreams.py --ports 19001` before the soak.
"""

from __future__ import annotations

import argparse
import json
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

USAGE = {"prompt_tokens": 7, "completion_tokens": 3, "total_tokens": 10}

REPLY = {
    "id": "chatcmpl-stub",
    "object": "chat.completion",
    "created": 0,
    "model": "stub",
    "choices": [
        {
            "index": 0,
            "message": {"role": "assistant", "content": "stub."},
            "finish_reason": "stop",
        }
    ],
    "usage": USAGE,
}

EMBED_REPLY = {
    "object": "list",
    "data": [{"object": "embedding", "embedding": [0.1, 0.2], "index": 0}],
    "model": "stub",
    "usage": {"prompt_tokens": 1, "total_tokens": 1},
}


def _frame(payload: dict) -> bytes:
    return ("data: " + json.dumps(payload) + "\n\n").encode()


class Stub(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args):
        return

    def _send_json(self, payload: dict) -> None:
        body = json.dumps(payload).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self) -> None:
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length) if length else b""
        if "embeddings" in self.path:
            self._send_json(EMBED_REPLY)
            return
        try:
            streaming = bool(json.loads(raw or b"{}").get("stream"))
        except Exception:
            streaming = False
        if not streaming:
            self._send_json(REPLY)
            return
        # Chunked SSE: content delta, then the usage chunk the tee holds, then
        # the terminator — the relay paths the soak is meant to exercise.
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Transfer-Encoding", "chunked")
        self.end_headers()
        for frame in (
            _frame({"id": "s", "object": "chat.completion.chunk", "created": 0, "model": "stub",
                    "choices": [{"index": 0, "delta": {"content": "stub."}, "finish_reason": None}]}),
            _frame({"id": "s", "object": "chat.completion.chunk", "created": 0, "model": "stub",
                    "choices": [], "usage": USAGE}),
            b"data: [DONE]\n\n",
        ):
            self.wfile.write(f"{len(frame):x}\r\n".encode() + frame + b"\r\n")
        self.wfile.write(b"0\r\n\r\n")

    def do_GET(self) -> None:
        if "models" in self.path:
            self._send_json({"object": "list", "data": [{"id": "stub", "object": "model"}]})
        else:
            self._send_json({"ok": True})


def main() -> None:
    p = argparse.ArgumentParser()
    p.add_argument("--ports", type=str, default="19001")
    p.add_argument("--host", type=str, default="127.0.0.1")
    args = p.parse_args()
    servers = []
    for raw in args.ports.split(","):
        httpd = ThreadingHTTPServer((args.host, int(raw.strip())), Stub)
        threading.Thread(target=httpd.serve_forever, daemon=True).start()
        servers.append(httpd)
        print(f"stub chat upstream on http://{args.host}:{raw.strip()}", flush=True)
    while True:
        time.sleep(3600)


if __name__ == "__main__":
    main()
