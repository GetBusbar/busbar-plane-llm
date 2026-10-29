#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright (C) 2026 Busbar Inc and contributors
"""The provider the one-shot audio suites point busbar at: OpenAI's `/v1/audio/transcriptions` and
`/v1/audio/speech`, answering from judge.py's own constants, and recording every request it receives.

    mock.py <port> <log.jsonl> <mode file>

The mode file holds `ok` (a 200 in the spec's shape) or `error` (a 400 ErrorResponse); the log gets
one JSON line per request: {"method", "path", "headers": {lower-case: value}, "body_b64"}.
"""
import base64
import json
import os
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from judge import AUDIO, ERROR, TEXT  # noqa: E402  the answers the judge holds busbar to

PORT, LOG, MODE = int(sys.argv[1]), sys.argv[2], sys.argv[3]


class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def answer(self, status, ctype, body):
        self.send_response(status)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        self.answer(200 if self.path == "/" else 404, "text/plain", b"audio mock\n")

    def do_POST(self):
        n = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(n) if n else b""
        if not n and "chunked" in (self.headers.get("Transfer-Encoding") or "").lower():
            while True:
                size = int(self.rfile.readline().split(b";")[0].strip() or b"0", 16)
                if size == 0:
                    self.rfile.readline()
                    break
                body += self.rfile.read(size)
                self.rfile.readline()
        with open(LOG, "a") as f:
            f.write(json.dumps({"method": "POST", "path": self.path,
                                "headers": {k.lower(): v for k, v in self.headers.items()},
                                "body_b64": base64.b64encode(body).decode()}) + "\n")
        mode = open(MODE).read().strip() if os.path.exists(MODE) else "ok"
        path = self.path.split("?")[0]
        if mode == "error":
            return self.answer(400, "application/json", json.dumps(ERROR).encode())
        if path.endswith("/audio/transcriptions"):
            return self.answer(200, "application/json", json.dumps({"text": TEXT}).encode())
        if path.endswith("/audio/speech"):
            return self.answer(200, "audio/mpeg", AUDIO)
        self.answer(404, "application/json", json.dumps({"error": {"message": f"no route {path}",
                    "type": "invalid_request_error", "param": None, "code": None}}).encode())


ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
