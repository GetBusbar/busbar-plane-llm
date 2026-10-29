#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright (C) 2026 Busbar Inc and contributors
"""The one-shot audio judge (voice-transcribe, voice-tts): busbar's wire on `/v1/audio/transcriptions`
and `/v1/audio/speech`, both sides of it, held to the OpenAI OpenAPI document this repo already pins
(testing/llm-conformance/spec-digests.tsv, `openai`), through the llm harness's own schema checker.

A CAPTURE is one directory per case, written by testing/conformance/audio.sh for busbar (the subject)
and synthesized here for the controls, so the controls run the very code that judges the subject:

    <case>/status       the status busbar answered the caller
    <case>/headers      busbar's response head (curl -D)
    <case>/body         busbar's response body
    <case>/upstream.jsonl  every request the provider (the mock) received during the case:
                        {"method", "path", "headers": {lower-case name: value}, "body_b64"}
    <case>/sent         what the caller sent: the audio file bytes (transcribe) or the JSON body (tts)
    <case>/upstream-answer  what the provider answered: the body bytes it sent busbar

Cases: `ok` (a good request, the provider answers 200), `error` (the provider answers a 400 error
body), `unauthenticated` (no busbar key; the provider must not be called).

    judge.py controls --op transcribe|tts         known-good captures PASS, planted ones are refused
    judge.py subject  --op ... --capture <dir>    busbar's captures; one RESULT row per assertion

Exit 0 only when every row PASSes (controls: every known-good row passes AND every planted defect is
refused). Rows print as `RESULT <id> PASS|FAIL <detail>`.
"""
import argparse
import base64
import importlib.util
import json
import os
import shutil
import sys
import tempfile
from email.parser import BytesParser
from email.policy import HTTP

HERE = os.path.dirname(os.path.abspath(__file__))
LLM = os.path.join(os.path.dirname(HERE), "llm-conformance")


sys.dont_write_bytecode = True  # importing the llm harness must leave no __pycache__ in the tree


def _validate_module():
    spec = importlib.util.spec_from_file_location("llm_validate", os.path.join(LLM, "validate.py"))
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


V = _validate_module()


def openai_doc(cache):
    pins = V.read_digests(os.path.join(LLM, "spec-digests.tsv"))
    return V.load_spec_doc("openai", pins["openai"], cache)


class Spec:
    def __init__(self, doc):
        self.doc = doc
        self.checker = V.Checker(doc)

    def schema(self, name):
        return {"$ref": "#/components/schemas/" + name}

    def ok_response(self, path, ctype):
        return self.doc["paths"][path]["post"]["responses"]["200"]["content"][ctype]["schema"]

    def violations(self, inst, schema):
        out = []
        self.checker.check(inst, schema, "", out)
        return [str(v) for v in out]


# ── reading a capture ───────────────────────────────────────────────────────────────────────────
def read(case, name, default=b""):
    p = os.path.join(case, name)
    return open(p, "rb").read() if os.path.exists(p) else default


def status(case):
    try:
        return int(read(case, "status").strip() or b"0")
    except ValueError:
        return 0


def head(case):
    """busbar's last response head, names lower-case (curl -D writes every head, interim ones too)."""
    blocks = read(case, "headers").decode("latin-1").replace("\r\n", "\n").strip().split("\n\n")
    h = {}
    for ln in (blocks[-1] if blocks else "").split("\n")[1:]:
        if ":" in ln:
            k, v = ln.split(":", 1)
            h[k.strip().lower()] = v.strip()
    return h


def upstream(case):
    rows = []
    for ln in read(case, "upstream.jsonl").decode().splitlines():
        if ln.strip():
            r = json.loads(ln)
            r["body"] = base64.b64decode(r.pop("body_b64", ""))
            rows.append(r)
    return rows


def multipart(ctype, body):
    """The form's fields: name -> (value bytes, filename, content type)."""
    msg = BytesParser(policy=HTTP).parsebytes(b"Content-Type: " + ctype.encode() + b"\r\n\r\n" + body)
    if not msg.is_multipart():
        return None
    fields = {}
    for part in msg.iter_parts():
        name = part.get_param("name", header="content-disposition")
        fields[name] = (part.get_payload(decode=True) or b"", part.get_filename(), part.get_content_type())
    return fields


def coerce(v):
    """A form field is text on the wire; the spec types it. Read it the way a JSON reader would."""
    s = v.decode("utf-8", "replace")
    try:
        return json.loads(s) if s[:1] in "-0123456789tfn[{" else s
    except ValueError:
        return s


# ── the assertions (shared by the controls and the subject) ─────────────────────────────────────
def judge(spec, op, cap):
    """Yield (row id, ok, detail) for every assertion about one capture directory."""
    path = {"transcribe": "/audio/transcriptions", "tts": "/audio/speech"}[op]
    ok, err, anon = (os.path.join(cap, c) for c in ("ok", "error", "unauthenticated"))
    sent = read(ok, "sent")
    answer = read(ok, "upstream-answer")

    # ok: the request the provider received
    ups = upstream(ok)
    rid = f"{op}.ok.upstream-request"
    if len(ups) != 1:
        yield rid, False, f"the provider received {len(ups)} request(s), not one"
    else:
        u = ups[0]
        probs = []
        if u["method"] != "POST":
            probs.append(f"method {u['method']}")
        if not u["path"].split("?")[0].endswith(path):
            probs.append(f"path {u['path']} does not end {path}")
        ctype = u["headers"].get("content-type", "")
        if op == "transcribe":
            form = multipart(ctype, u["body"]) if ctype.startswith("multipart/form-data") else None
            if form is None:
                probs.append(f"content-type {ctype!r} is not a multipart form")
            else:
                inst = {k: ("<binary>" if k == "file" else coerce(v[0])) for k, v in form.items()}
                probs += spec.violations(inst, spec.schema("CreateTranscriptionRequest"))
                if "file" in form and form["file"][0] != sent:
                    probs.append(f"the file part is {len(form['file'][0])} bytes, not the {len(sent)} the caller sent")
        else:
            try:
                inst = json.loads(u["body"])
            except ValueError:
                inst = None
                probs.append("the body is not JSON")
            if inst is not None:
                probs += spec.violations(inst, spec.schema("CreateSpeechRequest"))
                caller = json.loads(sent)
                for k in ("input", "voice"):
                    if inst.get(k) != caller.get(k):
                        probs.append(f"{k} is {inst.get(k)!r}, the caller sent {caller.get(k)!r}")
            if not ctype.startswith("application/json"):
                probs.append(f"content-type {ctype!r} is not application/json")
        yield rid, not probs, "; ".join(probs) or f"POST {u['path']}, valid against the spec"

    # ok: what busbar answered the caller
    rid = f"{op}.ok.response"
    st, h, body = status(ok), head(ok), read(ok, "body")
    probs = [] if st == 200 else [f"status {st}, not 200 ({read(ok, 'body')[:240].decode('utf-8', 'replace')})"]
    if op == "transcribe":
        try:
            inst = json.loads(body)
        except ValueError:
            inst = None
            probs.append("the body is not JSON")
        if inst is not None:
            probs += spec.violations(inst, spec.ok_response(path, "application/json"))
            want = json.loads(answer).get("text") if answer else None
            if isinstance(inst, dict) and inst.get("text") != want:
                probs.append(f"text is {inst.get('text')!r}, the provider said {want!r}")
    else:
        if body != answer:
            probs.append(f"the body is {len(body)} bytes, not the provider's {len(answer)} audio bytes")
        ct = h.get("content-type", "")
        if not ct.startswith("audio/") and not ct.startswith("application/octet-stream"):
            probs.append(f"content-type {ct!r} is not audio")
    yield rid, not probs, "; ".join(probs) or f"200, valid against the spec, the provider's answer relayed"

    # error: a provider's 4xx reaches the caller as the dialect's error body
    rid = f"{op}.error.relay"
    st, body = status(err), read(err, "body")
    probs = [] if 400 <= st < 500 else [f"status {st}, not the provider's 4xx"]
    try:
        inst = json.loads(body)
        probs += spec.violations(inst, spec.schema("ErrorResponse"))
    except ValueError:
        probs.append("the body is not JSON")
    if len(upstream(err)) != 1:
        probs.append(f"the provider received {len(upstream(err))} request(s), not one")
    yield rid, not probs, "; ".join(probs) or f"{st}, an ErrorResponse"

    # unauthenticated: refused as the dialect's error, the provider never called
    rid = f"{op}.unauthenticated"
    st, body = status(anon), read(anon, "body")
    probs = [] if st == 401 else [f"status {st}, not 401"]
    try:
        probs += spec.violations(json.loads(body), spec.schema("ErrorResponse"))
    except ValueError:
        probs.append("the body is not JSON")
    if upstream(anon):
        probs.append(f"the provider was called {len(upstream(anon))} time(s)")
    yield rid, not probs, "; ".join(probs) or "401, an ErrorResponse, the provider not called"


# ── the controls: known-good captures, and each planted defect ──────────────────────────────────
WAV = b"RIFF$\x00\x00\x00WAVEfmt \x10\x00\x00\x00\x01\x00\x01\x00@\x1f\x00\x00\x80>\x00\x00\x02\x00\x10\x00data\x00\x00\x00\x00"
AUDIO = b"ID3\x04\x00\x00\x00\x00\x00\x00" + bytes(range(256)) * 4
TEXT = "The quick brown fox jumped over the lazy dog."
ERROR = {"error": {"message": "Invalid file format.", "type": "invalid_request_error", "param": "file", "code": "invalid_value"}}


def form(fields, boundary="conformance"):
    out = b""
    for name, value, filename, ctype in fields:
        disp = f'form-data; name="{name}"' + (f'; filename="{filename}"' if filename else "")
        out += f"--{boundary}\r\nContent-Disposition: {disp}\r\n".encode()
        out += (f"Content-Type: {ctype}\r\n" if ctype else "").encode() + b"\r\n" + value + b"\r\n"
    return f"multipart/form-data; boundary={boundary}", out + f"--{boundary}--\r\n".encode()


def write_case(d, st, ctype, body, ups, sent, answer):
    os.makedirs(d, exist_ok=True)
    open(os.path.join(d, "status"), "w").write(str(st))
    open(os.path.join(d, "headers"), "w").write(f"HTTP/1.1 {st} X\r\ncontent-type: {ctype}\r\n\r\n")
    open(os.path.join(d, "body"), "wb").write(body)
    open(os.path.join(d, "sent"), "wb").write(sent)
    open(os.path.join(d, "upstream-answer"), "wb").write(answer)
    with open(os.path.join(d, "upstream.jsonl"), "w") as f:
        for m, p, h, b in ups:
            f.write(json.dumps({"method": m, "path": p, "headers": h, "body_b64": base64.b64encode(b).decode()}) + "\n")


def good(op):
    """A capture a conformant gateway writes: the spec's own shapes on both sides."""
    if op == "transcribe":
        ct, b = form([("file", WAV, "audio.wav", "audio/wav"), ("model", b"whisper-1", None, None)])
        req = ("POST", "/v1/audio/transcriptions", {"content-type": ct}, b)
        ans = json.dumps({"text": TEXT}).encode()
        return {"ok": (200, "application/json", ans, [req], WAV, ans),
                "error": (400, "application/json", json.dumps(ERROR).encode(), [req], WAV, json.dumps(ERROR).encode()),
                "unauthenticated": (401, "application/json", json.dumps(ERROR).encode(), [], WAV, b"")}
    sent = json.dumps({"model": "tts-1", "input": TEXT, "voice": "alloy"}).encode()
    req = ("POST", "/v1/audio/speech", {"content-type": "application/json"}, sent)
    return {"ok": (200, "audio/mpeg", AUDIO, [req], sent, AUDIO),
            "error": (400, "application/json", json.dumps(ERROR).encode(), [req], sent, json.dumps(ERROR).encode()),
            "unauthenticated": (401, "application/json", json.dumps(ERROR).encode(), [], sent, b"")}


def plants(op):
    """(name, the row it must turn red, the case it edits, edit(case tuple) -> case tuple)."""
    def body(b):
        return lambda c: (c[0], c[1], b, c[3], c[4], c[5])

    def st(s):
        return lambda c: (s, c[1], c[2], c[3], c[4], c[5])

    def ups(f):
        return lambda c: (c[0], c[1], c[2], f(c[3]), c[4], c[5])

    common = [
        ("the provider's 4xx answered as 502", f"{op}.error.relay", "error", st(502)),
        ("an error body without `error`", f"{op}.error.relay", "error", body(b'{"message":"bad"}')),
        ("an error missing the required `param` and `code`", f"{op}.error.relay", "error",
         body(json.dumps({"error": {"message": "bad", "type": "invalid_request_error"}}).encode())),
        ("an unauthenticated call answered 200", f"{op}.unauthenticated", "unauthenticated", st(200)),
        ("an unauthenticated call that reached the provider", f"{op}.unauthenticated", "unauthenticated",
         ups(lambda _: good(op)["ok"][3])),
        ("the provider called twice", f"{op}.ok.upstream-request", "ok", ups(lambda u: u + u)),
        ("the request sent to the wrong path", f"{op}.ok.upstream-request", "ok",
         ups(lambda u: [(u[0][0], "/v1/chat/completions", u[0][2], u[0][3])])),
        ("busbar answered 500", f"{op}.ok.response", "ok", st(500)),
    ]
    if op == "transcribe":
        ct, nofile = form([("model", b"whisper-1", None, None)])
        ct2, extra = form([("file", WAV, "a.wav", "audio/wav"), ("model", b"whisper-1", None, None), ("modle", b"x", None, None)])
        ct3, cut = form([("file", WAV[:10], "a.wav", "audio/wav"), ("model", b"whisper-1", None, None)])
        return common + [
            ("a form without the file", f"{op}.ok.upstream-request", "ok", ups(lambda u: [(u[0][0], u[0][1], {"content-type": ct}, nofile)])),
            ("a form field the spec does not declare", f"{op}.ok.upstream-request", "ok", ups(lambda u: [(u[0][0], u[0][1], {"content-type": ct2}, extra)])),
            ("the audio truncated on the way", f"{op}.ok.upstream-request", "ok", ups(lambda u: [(u[0][0], u[0][1], {"content-type": ct3}, cut)])),
            ("the request sent as JSON", f"{op}.ok.upstream-request", "ok", ups(lambda u: [(u[0][0], u[0][1], {"content-type": "application/json"}, b'{"model":"whisper-1"}')])),
            ("a transcription without `text`", f"{op}.ok.response", "ok", body(b'{"transcript":"x"}')),
            ("`text` that is not a string", f"{op}.ok.response", "ok", body(b'{"text":7}')),
            ("text other than the provider's", f"{op}.ok.response", "ok", body(b'{"text":"something else"}')),
        ]
    return common + [
        ("a speech request without `voice`", f"{op}.ok.upstream-request", "ok",
         ups(lambda u: [(u[0][0], u[0][1], u[0][2], json.dumps({"model": "tts-1", "input": TEXT}).encode())])),
        ("a speech field the spec does not declare", f"{op}.ok.upstream-request", "ok",
         ups(lambda u: [(u[0][0], u[0][1], u[0][2], json.dumps({"model": "tts-1", "input": TEXT, "voice": "alloy", "tone": "x"}).encode())])),
        ("the input rewritten", f"{op}.ok.upstream-request", "ok",
         ups(lambda u: [(u[0][0], u[0][1], u[0][2], json.dumps({"model": "tts-1", "input": "other", "voice": "alloy"}).encode())])),
        ("the audio truncated on the way back", f"{op}.ok.response", "ok", body(AUDIO[:100])),
        ("the audio answered as JSON", f"{op}.ok.response", "ok",
         lambda c: (c[0], "application/json", c[2], c[3], c[4], c[5])),
    ]


def controls(spec, op):
    rows, t = [], tempfile.mkdtemp()
    try:
        cap = os.path.join(t, "good")
        for name, c in good(op).items():
            write_case(os.path.join(cap, name), *c)
        for rid, ok, detail in judge(spec, op, cap):
            rows.append((f"control.{rid}", ok, detail))
        for i, (name, target, case, edit) in enumerate(plants(op)):
            cap = os.path.join(t, f"plant{i}")
            for n, c in good(op).items():
                write_case(os.path.join(cap, n), *(edit(c) if n == case else c))
            got = {rid: ok for rid, ok, _ in judge(spec, op, cap)}
            bit = got.get(target) is False
            rows.append((f"negative.{name}", bit, f"{target} refused it" if bit else f"{target} PASSED a planted defect"))
    finally:
        shutil.rmtree(t, ignore_errors=True)
    return rows


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("mode", choices=["controls", "subject"])
    ap.add_argument("--op", required=True, choices=["transcribe", "tts"])
    ap.add_argument("--capture")
    ap.add_argument("--spec-cache", default=os.environ.get("BUSBAR_LLM_SPEC_CACHE") or os.path.expanduser("~/.cache/busbar-llm-specs"))
    a = ap.parse_args()
    spec = Spec(openai_doc(a.spec_cache))
    rows = controls(spec, a.op) if a.mode == "controls" else list(judge(spec, a.op, a.capture))
    for rid, ok, detail in rows:
        print(f"RESULT {rid} {'PASS' if ok else 'FAIL'} {detail}")
    return 0 if rows and all(ok for _, ok, _ in rows) else 1


if __name__ == "__main__":
    sys.exit(main())
