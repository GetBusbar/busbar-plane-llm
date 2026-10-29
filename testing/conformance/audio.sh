#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright (C) 2026 Busbar Inc and contributors
#
# The one-shot audio suites of testing/conformance/suites.tsv: `audio.sh transcribe` (voice-transcribe,
# OpenAI `POST /v1/audio/transcriptions`) and `audio.sh tts` (voice-tts, OpenAI `POST /v1/audio/speech`).
# busbar serves both through this plane (the openai codec's TRANSCRIPTION and SPEECH operations); the
# streaming plane serves no one-shot route. Tier spec-conformant: no vendor kit exists, so the suite holds
# BOTH sides of busbar's wire to the OpenAI OpenAPI document this repo already pins
# (testing/llm-conformance/spec-digests.tsv), with the llm harness's own schema checker.
#
# THE THREE LEGS (testing/audio-conformance/judge.py):
#   control           known-good captures (the spec's shapes on both sides) PASS every assertion
#   negative control  each planted defect (a form without the file, a field the spec does not declare,
#                     truncated audio, an error body missing what ErrorResponse requires, a provider
#                     reached by an unauthenticated call, ...) MUST be refused by the assertion it targets
#   subject           busbar at the pin with THIS checkout's plane-llm (built once per check by llm.sh
#                     --subject), booted from the shadow oracle's own config against a recording provider
#                     (testing/audio-conformance/mock.py); per op three cases: `ok`, `error` (the
#                     provider answers 400), `unauthenticated` (no busbar key)
# `pass` only when all three hold. A subject that cannot be built or booted is `fail` (armed).
#
# The subject run is done ONCE per check into a cache beside the verdicts; each suite judges its op.
set -uo pipefail
op="${1:?usage: audio.sh transcribe|tts}"
case "$op" in transcribe|tts) ;; *) echo "audio.sh: unknown op $op" >&2; exit 2 ;; esac
: "${CONFORMANCE_SUITE:?}" "${CONFORMANCE_OUT:?}" "${BUSBAR_CHECKOUT:?}" "${BUSBAR_PIN:?}"
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
A="$repo/testing/audio-conformance"
H="$repo/testing/llm-conformance"
TGT="${CARGO_TARGET_DIR:-$repo/target}"
run="$(dirname "$CONFORMANCE_OUT")/audio-run"
export BUSBAR_LLM_SPEC_CACHE="${BUSBAR_LLM_SPEC_CACHE:-$TGT/llm-conformance/spec-cache}"
say() { printf 'audio: %s\n' "$*" >&2; }
sha() { if command -v sha256sum >/dev/null 2>&1; then sha256sum | cut -d' ' -f1; else shasum -a 256 | cut -d' ' -f1; fi; }
port() { python3 -I -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])'; }

subject_run() {
  local fail=""
  # python with PyYAML, from the llm suites' hash-pinned file (the same venv llm.sh builds)
  local req="$repo/testing/conformance/requirements-llm.txt" venv
  venv="$TGT/llm-conformance/venv-$(sha < "$req" | cut -c1-12)"
  if [ ! -x "$venv/bin/python" ] || ! "$venv/bin/python" -c 'import yaml' 2>/dev/null; then
    rm -rf "$venv"
    { "${LLM_CONFORMANCE_PYTHON:-python3}" -m venv "$venv" \
      && "$venv/bin/python" -m pip install --disable-pip-version-check --quiet --require-hashes --only-binary=:all: -r "$req"; } \
      >"$run/venv.log" 2>&1 || { rm -rf "$venv"; fail="the hash-pinned python environment could not be built (see venv.log)"; }
  fi
  echo "$venv/bin/python" >"$run/python"
  [ -n "$fail" ] || bash "$H/vendor.sh" >"$run/vendor.log" 2>&1 || fail="the pinned specs could not be vendored (see vendor.log)"

  # the subject: busbar at the pin with this checkout's plane-llm, the build the llm suites judge
  local bin=""
  if [ -z "$fail" ]; then
    bin="$(CONFORMANCE_SUITE=llm-openai CONFORMANCE_OUT="$(dirname "$CONFORMANCE_OUT")/llm-subject" \
      bash "$repo/testing/conformance/llm.sh" --subject 2>"$run/subject.log" | tail -n 1)"
    [ -n "$bin" ] && [ -x "$bin" ] || fail="busbar with this plane could not be built (see subject.log: $(tail -n 2 "$run/subject.log" | tr '\n' ' '))"
  fi

  # the shadow oracle's engine, at busbar's pin (its config is the one every oracle cell boots)
  local W="$run/boot" eng="$run/engine" pinf="$BUSBAR_CHECKOUT/testing/shadow-oracle/oracle-rust.pin"
  if [ -z "$fail" ]; then
    mkdir -p "$W"
    export MODERNCI_CACHE_DIR="${MODERNCI_CACHE_DIR:-$TGT/llm-conformance/oracle-cache}"
    for pair in BUSBAR_ORACLE_RELEASE_REPO=release_repo BUSBAR_ORACLE_RELEASE_REF=release_ref \
                BUSBAR_ORACLE_SHA256_LINUX_X86_64=sha256_linux_x86_64 BUSBAR_ORACLE_SHA256_LINUX_AARCH64=sha256_linux_aarch64 \
                BUSBAR_ORACLE_CRATE=crate BUSBAR_ORACLE_BIN=bin; do
      var="${pair%%=*}"; k="${pair#*=}"
      [ -n "${!var:-}" ] || export "$var=$(sed -n "s/^$k=//p" "$pinf" | head -1)"
    done
    printf '#!/usr/bin/env bash\nexec %q "$@"\n' "$BUSBAR_CHECKOUT/bin/oracle" >"$eng"; chmod +x "$eng"
  fi

  local L Ad M mpid="" bpid=""
  if [ -z "$fail" ]; then
    L="$(port)"; Ad="$(port)"; M="$(port)"
    echo ok >"$run/mode"; : >"$run/upstream.jsonl"
    "$(cat "$run/python")" -I "$A/mock.py" "$M" "$run/upstream.jsonl" "$run/mode" >"$run/mock.log" 2>&1 & mpid=$!
    "$eng" config "$W" "$L" "$Ad" "$M" --bin "$bin" >"$run/config.log" 2>&1 || fail="the oracle config could not be written (see config.log)"
  fi
  if [ -z "$fail" ]; then
    # a binary that refuses the oracle's config gets its own --migrate-config, then the operator's
    # upgrade step (the oracle's migrate-on-candidate-boot, harness::oracle_write_config_for_bin)
    if ! BUSBAR_CONFIG="$W/config.yaml" BUSBAR_PROVIDERS="$W/providers.yaml" ORACLE_UPSTREAM_KEY=unused \
         BUSBAR_ADMIN_TOKEN=shadow-oracle-admin "$bin" --validate >"$run/validate.log" 2>&1; then
      "$bin" --migrate-config "$W/config.yaml" >"$W/config.migrated" 2>>"$run/validate.log" && [ -s "$W/config.migrated" ] \
        && mv "$W/config.migrated" "$W/config.yaml"
      "$eng" upgrade-config "$bin" "$W/config.yaml" >>"$run/validate.log" 2>&1 || true
    fi
    bpid="$("$eng" spawn --work "$W" --admin-token shadow-oracle-admin "$run/busbar.log" "$bin" 2>>"$run/config.log")"
    local i=0
    while [ $i -lt 400 ]; do
      curl -fsS -m 1 -o /dev/null "http://127.0.0.1:$L/healthz" 2>/dev/null && break
      kill -0 "$bpid" 2>/dev/null || break
      sleep 0.05; i=$((i + 1))
    done
    curl -fsS -m 1 -o /dev/null "http://127.0.0.1:$L/healthz" 2>/dev/null \
      || fail="busbar did not come up (see busbar.log: $(tail -n 3 "$run/busbar.log" | tr '\n' ' ' | cut -c1-300))"
  fi
  local keys=""
  if [ -z "$fail" ]; then
    keys="$("$eng" mint-keys --admin-token shadow-oracle-admin "$Ad" 2>>"$run/config.log")" && eval "$keys" \
      && [ -n "${ORACLE_TOKEN_OK:-}" ] || fail="the oracle's keys could not be minted (see config.log)"
  fi
  if [ -z "$fail" ]; then
    local py; py="$(cat "$run/python")"
    capture() { # capture <op> <case> <mode> <auth: yes|no>
      local d="$run/$1/$2" url auth=()
      mkdir -p "$d"; echo "$3" >"$run/mode"; : >"$run/upstream.jsonl"
      [ "$4" = yes ] && auth=(-H "Authorization: Bearer $ORACLE_TOKEN_OK")
      "$py" -I -c 'import sys; sys.path.insert(0, sys.argv[1]); import judge, json
op, out = sys.argv[2], sys.argv[3]
if op == "transcribe":
    open(out + "/sent", "wb").write(judge.WAV); a = json.dumps({"text": judge.TEXT}).encode()
else:
    open(out + "/sent", "w").write(json.dumps({"model": "m-openai-chat", "input": judge.TEXT, "voice": "alloy"})); a = judge.AUDIO
open(out + "/upstream-answer", "wb").write(json.dumps(judge.ERROR).encode() if sys.argv[4] == "error" else a)' "$A" "$1" "$d" "$3"
      if [ "$1" = transcribe ]; then
        curl -sS -m 30 -o "$d/body" -D "$d/headers" -w '%{http_code}' ${auth[@]+"${auth[@]}"} \
          -F "file=@$d/sent;filename=audio.wav;type=audio/wav" -F "model=m-openai-chat" \
          "http://127.0.0.1:$L/v1/audio/transcriptions" >"$d/status" 2>"$d/curl.err"
      else
        curl -sS -m 30 -o "$d/body" -D "$d/headers" -w '%{http_code}' ${auth[@]+"${auth[@]}"} \
          -H 'Content-Type: application/json' --data-binary "@$d/sent" \
          "http://127.0.0.1:$L/v1/audio/speech" >"$d/status" 2>"$d/curl.err"
      fi
      sleep 0.1; cp "$run/upstream.jsonl" "$d/upstream.jsonl"
    }
    local o
    for o in transcribe tts; do
      capture "$o" ok ok yes; capture "$o" error error yes; capture "$o" unauthenticated ok no
    done
  fi
  [ -n "$bpid" ] && { kill "$bpid" 2>/dev/null; wait "$bpid" 2>/dev/null; }
  [ -n "$mpid" ] && { kill "$mpid" 2>/dev/null; wait "$mpid" 2>/dev/null; }
  [ -z "$fail" ] || echo "$fail" >"$run/subject-failed"
}

if [ ! -f "$run/done" ]; then
  rm -rf "$run"; mkdir -p "$run"
  subject_run
  touch "$run/done"
fi
cp "$run"/*.log "$CONFORMANCE_OUT/" 2>/dev/null || true
PY="$(cat "$run/python" 2>/dev/null)"; [ -x "$PY" ] || PY=python3

"$PY" -I "$A/judge.py" controls --op "$op" >"$CONFORMANCE_OUT/controls.txt" 2>&1; crc=$?
src=1
if [ ! -f "$run/subject-failed" ]; then
  "$PY" -I "$A/judge.py" subject --op "$op" --capture "$run/$op" >"$CONFORMANCE_OUT/subject.txt" 2>&1; src=$?
fi
grep -v ' PASS ' "$CONFORMANCE_OUT/controls.txt" "$CONFORMANCE_OUT/subject.txt" 2>/dev/null | head -n 20 >&2
python3 -I - "$op" "$run" "$CONFORMANCE_OUT" "$CONFORMANCE_SUITE" "$crc" "$src" "$H/spec-digests.tsv" <<'PY'
import json, os, sys
op, run, out, suite, crc, src, digests = sys.argv[1:8]
standard = {"transcribe": "OpenAI Audio API: POST /v1/audio/transcriptions, the openai-openapi document",
            "tts": "OpenAI Audio API: POST /v1/audio/speech, the openai-openapi document"}[op]
spec_version = "openai"
for ln in open(digests):
    p = ln.rstrip("\n").split("\t")
    if len(p) >= 4 and p[0] == "openai" and not ln.startswith("#"):
        spec_version = f"openai@sha256:{p[2]}"
def rows(name):
    p = os.path.join(out, name)
    r = [ln.split(" ", 3) for ln in open(p).read().splitlines() if ln.startswith("RESULT ")] if os.path.exists(p) else []
    return [x + [""] * (4 - len(x)) for x in r]
def verdict(status, reason):
    v = {"suite": suite, "status": status, "armed": True, "standard": standard, "spec_version": spec_version,
         "plan": "busbar's request to the provider and its answer to the caller, ok / provider-error / "
                 "unauthenticated, held to the published spec; control: known-good captures pass; "
                 "negative control: planted defects are refused"}
    v["reason"] = reason
    with open(os.path.join(out, "verdict.json"), "w") as f:
        json.dump(v, f, indent=2, sort_keys=True); f.write("\n")
    print(status, reason); sys.exit(0 if status == "pass" else 1)
ctl = rows("controls.txt")
neg = [r for r in ctl if r[1].startswith("negative.")]
if int(crc) != 0 or not ctl or len(neg) < 10:
    bad = [f"{r[1]}: {r[3][:120]}" for r in ctl if r[2] != "PASS"][:4]
    verdict("fail", f"the control / negative control did not hold ({len(ctl)} rows, {len(neg)} planted): " + "; ".join(bad))
sf = os.path.join(run, "subject-failed")
if os.path.exists(sf):
    verdict("fail", "the subject could not be run: " + open(sf).read().strip())
sub = rows("subject.txt")
if len(sub) < 4:
    verdict("fail", f"the subject wrote {len(sub)} rows, not 4 (see subject.txt)")
bad = [r for r in sub if r[2] != "PASS"]
if bad or int(src) != 0:
    verdict("fail", f"{len(bad)} of {len(sub)} assertions failed: " + "; ".join(f"{r[1]}: {r[3][:200]}" for r in bad[:4]))
verdict("pass", f"{len(sub)} assertions held on busbar's wire (request to the provider, answer to the caller, "
                f"provider error, unauthenticated); control held; all {len(neg)} planted defects refused")
PY
