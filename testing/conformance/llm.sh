#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright (C) 2026 Busbar Inc and contributors
#
# The `llm-*` suites of testing/conformance/suites.tsv (llm-anthropic, llm-openai, llm-responses,
# llm-gemini, llm-bedrock, llm-cohere): busbar's LLM wire output judged against each vendor's PUBLISHED,
# machine-readable spec through this repo's OWN harness (testing/llm-conformance), in the three legs.
#
# ONE SHARED RUN, SIX VERDICTS. The expensive part is one thing: build busbar (at $BUSBAR_CHECKOUT, the
# .busbar-ref pin) with THIS checkout's plane-llm swapped in, record the LLM cells of
# testing/shadow-oracle/cells.json, and validate the recording. The first suite line to run does it and
# caches the result under $CARGO_TARGET_DIR/llm-conformance/<key>/; the other five read it. The key is the
# pin + this checkout's plane-llm and harness content, so a line that runs alone does the whole run itself
# and a stale cache can never answer for different code.
#
#   1. CONTROL           testing/llm-conformance/selftest.sh: known-good vendor/1.5.5 fixtures validate
#   2. NEGATIVE CONTROL  the same selftest plants bad responses, streams and gaps; each MUST be refused
#   3. SUBJECT           busbar's recorded wire, per dialect; every owed row must PASS, none may SKIP
# A dialect is `pass` only when all three hold. A subject that cannot be built or booted is `fail` (armed).
#
# Env: CONFORMANCE_SUITE, CONFORMANCE_OUT, BUSBAR_CHECKOUT, BUSBAR_PIN, CARGO_TARGET_DIR (as run.sh sets).
# Needs: rust, python3 with venv (PyYAML comes from a hash-pinned requirements file), curl, git.
set -uo pipefail
: "${CONFORMANCE_SUITE:?}" "${CONFORMANCE_OUT:?}" "${BUSBAR_CHECKOUT:?}" "${BUSBAR_PIN:?}"
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
H="$repo/testing/llm-conformance"
TGT="${CARGO_TARGET_DIR:-$repo/target}"
say() { printf 'llm: %s\n' "$*" >&2; }
sha() { if command -v sha256sum >/dev/null 2>&1; then sha256sum | cut -d' ' -f1; else shasum -a 256 | cut -d' ' -f1; fi; }

dialect="${CONFORMANCE_SUITE#llm-}"
case "$dialect" in anthropic|openai|responses|gemini|bedrock|cohere) ;; *) say "unknown suite $CONFORMANCE_SUITE"; exit 2 ;; esac

# The cache key: everything the answer depends on.
key="$( { printf 'pin=%s\n' "$BUSBAR_PIN"
          git -C "$repo" ls-files -s -- plane-llm testing/llm-conformance testing/conformance/requirements-llm.txt
          git -C "$repo" diff HEAD -- plane-llm testing/llm-conformance testing/conformance/requirements-llm.txt
        } | sha | cut -c1-16)"
D="$TGT/llm-conformance/$key"
mkdir -p "$D"

# ── the shared run ───────────────────────────────────────────────────────────────────────────────
shared_run() {
  local fail=""   # the first reason the run cannot be judged at all; every suite inherits it
  # Python with PyYAML, from the hash-pinned file, once per requirements content.
  local req="$repo/testing/conformance/requirements-llm.txt" venv
  venv="$TGT/llm-conformance/venv-$(sha < "$req" | cut -c1-12)"
  if [ ! -x "$venv/bin/python" ] || ! "$venv/bin/python" -c 'import yaml' 2>/dev/null; then
    rm -rf "$venv"
    { "${LLM_CONFORMANCE_PYTHON:-python3}" -m venv "$venv" \
      && "$venv/bin/python" -m pip install --disable-pip-version-check --quiet --require-hashes --only-binary=:all: -r "$req"; } \
      >"$D/venv.log" 2>&1 || { rm -rf "$venv"; fail="the hash-pinned python environment could not be built (see venv.log)"; }
  fi
  [ -z "$fail" ] && export PATH="$venv/bin:$PATH"

  # busbar at the pin with this checkout's plane swapped in (written at run time, never committed).
  local SRC="$D/busbar-src" P rc
  if [ -z "$fail" ]; then
    rm -rf "$SRC"; mkdir -p "$SRC" \
      && tar -C "$BUSBAR_CHECKOUT" --exclude=./target -cf - . | tar -C "$SRC" -xf - || fail="busbar could not be staged"
  fi
  if [ -z "$fail" ]; then
    P="$SRC/crates/busbar-plane-llm"
    # busbar's manifest for the crate (path dependencies, its examples) stays; the code is this repo's.
    mv "$P/Cargo.toml" "$D/plane.Cargo.toml" && mv "$P/examples" "$D/plane.examples" \
      && rm -rf "$P" && cp -R "$repo/plane-llm" "$P" \
      && cp "$D/plane.Cargo.toml" "$P/Cargo.toml" && cp -R "$D/plane.examples" "$P/examples" \
      || fail="this checkout's plane-llm could not be swapped into busbar"
  fi

  # 1+2. CONTROL and NEGATIVE CONTROL: the harness's own selftest (BUSBAR_CHECKOUT supplies the shared verdict).
  local st_rc=0
  if [ -z "$fail" ]; then
    say "=== control and negative control: selftest.sh"
    BUSBAR_CHECKOUT="$SRC" bash "$H/selftest.sh" >"$D/selftest.log" 2>&1 || st_rc=$?
    tail -3 "$D/selftest.log" >&2
  fi

  # 3. SUBJECT: build, record the llm cells, validate.
  local build_rc=0 rec_rc=0 run_rc=0
  if [ -z "$fail" ]; then
    say "=== subject: build busbar with this plane"
    ( cd "$SRC" && CARGO_TARGET_DIR="$TGT" cargo build --locked --release -p busbar --bin busbar ) >"$D/build.log" 2>&1 || build_rc=$?
    if [ "$build_rc" -ne 0 ]; then tail -15 "$D/build.log" >&2; fail="busbar with this plane could not be built (exit $build_rc; see build.log)"; fi
  fi
  if [ -z "$fail" ]; then
    say "=== subject: record the llm cells"
    export MODERNCI_CACHE_DIR="${MODERNCI_CACHE_DIR:-$TGT/llm-conformance/oracle-cache}"
    rm -rf "$D/recording"
    "$SRC/bin/oracle" record --bin "$TGT/release/busbar" --plane llm --out "$D/recording" >"$D/record.log" 2>&1 || rec_rc=$?
    if [ "$rec_rc" -ne 0 ]; then tail -15 "$D/record.log" >&2; fail="the llm cells could not be recorded (exit $rec_rc; see record.log)"; fi
  fi
  if [ -z "$fail" ]; then
    say "=== subject: validate against the published specs"
    BUSBAR_CHECKOUT="$SRC" BUSBAR_ORACLE_SHIM="$SRC/bin/oracle" \
      bash "$H/run.sh" --recording "$D/recording" --out "$D/out" >"$D/run.log" 2>&1 || run_rc=$?
    tail -12 "$D/run.log" >&2
  fi
  python3 -I - "$D/shared.json" "$fail" "$st_rc" "$run_rc" <<'PY'
import json, sys
out, fail, st, rc = sys.argv[1:5]
json.dump({"fail": fail, "selftest_rc": int(st), "run_rc": int(rc)}, open(out, "w"))
PY
  rm -rf "$SRC"   # the staged tree is large and only this run needed it
}

if [ ! -s "$D/shared.json" ]; then shared_run; else say "shared run cached at $D"; fi

# ── this suite's verdict, from the shared ledger ─────────────────────────────────────────────────
python3 -I - "$dialect" "$D" "$H/spec-digests.tsv" "$CONFORMANCE_OUT/verdict.json" <<'PY'
import json, os, sys
dialect, D, digests, out = sys.argv[1:5]
SPEC = {"openai": "openai", "responses": "openai", "anthropic": "anthropic", "gemini": "gemini",
        "bedrock": "bedrock", "cohere": "cohere"}[dialect]
STANDARD = {
    "anthropic": "Anthropic Messages API (/v1/messages), the OpenAPI document Anthropic publishes",
    "openai": "OpenAI Chat Completions API (/chat/completions), the openai-openapi document",
    "responses": "OpenAI Responses API (/responses), the openai-openapi document",
    "gemini": "Google Gemini generateContent API, the generativelanguage v1beta discovery document",
    "bedrock": "AWS Bedrock Runtime Converse/ConverseStream, botocore service-2.json",
    "cohere": "Cohere Chat API v2 (/v2/chat), the cohere-openapi document",
}[dialect]
spec_version = SPEC
for ln in open(digests):
    p = ln.rstrip("\n").split("\t")
    if len(p) >= 4 and p[0] == SPEC and not ln.startswith("#"):
        spec_version = f"{SPEC}@sha256:{p[2]}"
v = {"suite": "llm-" + dialect, "armed": True, "standard": STANDARD,
     "plan": "Validate busbar's recorded wire for this ingress dialect (requests, JSON and SSE/event-stream "
             "responses, error bodies) against the vendor's published spec; controls: harness selftest "
             "(known-good fixtures validate, planted bad responses are refused)",
     "spec_version": spec_version}

def finish(status, reason):
    v["status"] = status
    v["reason"] = reason
    json.dump(v, open(out, "w"), indent=2); open(out, "a").write("\n")
    print(json.dumps(v)); sys.exit(0 if status == "pass" else 1)

sh = json.load(open(os.path.join(D, "shared.json")))
if sh["fail"]:
    finish("fail", sh["fail"])
if sh["selftest_rc"] != 0:
    finish("fail", f"the control / negative control (testing/llm-conformance/selftest.sh) failed (exit {sh['selftest_rc']}); no subject verdict counts (see selftest.log)")

OUT = os.path.join(D, "out")
try:
    rows = [ln.rstrip("\n").split("\t") for ln in open(os.path.join(OUT, "ledger.tsv")) if ln.strip()]
    owed_all = [ln.strip() for ln in open(os.path.join(OUT, "owed-all.txt")) if ln.strip()]
except OSError as e:
    finish("fail", f"the validation wrote no ledger ({e}); run.sh exit {sh['run_rc']} (see run.log)")
if not rows:
    finish("fail", "zero rows: nothing was judged (see run.log)")

pre = f"llm|{dialect}|"
owed = [i for i in owed_all if i.startswith(pre)]
mine = {}
for r in rows:
    if r[0].startswith(pre):
        mine.setdefault(r[0], r)
problems = []
if not owed:
    problems.append(f"no row is owed for {dialect} (the cell universe lists none)")
missing = [i for i in owed if i not in mine]
if missing:
    problems.append(f"{len(missing)} owed id(s) with no row, e.g. {missing[0]}")
fails = [r for r in mine.values() if r[1] == "FAIL"]
skips = [r for r in mine.values() if r[1] == "SKIP"]
other = [r for r in mine.values() if r[1] not in ("PASS", "FAIL", "SKIP")]
def brief(r):
    return f"{r[0]}: {(r[3] if len(r) > 3 and r[3] else r[2])[:240]}"
if fails:
    problems.append(f"{len(fails)} row(s) violate the spec, e.g. " + " || ".join(brief(r) for r in fails[:3]))
if skips:
    problems.append(f"{len(skips)} named gap(s) (SKIP, never a pass; owner rule 22 allows none), e.g. " + " || ".join(brief(r) for r in skips[:3]))
if other:
    problems.append(f"{len(other)} row(s) with an unknown status")
gate = [r for r in rows if r[0] == "gate|llm-conformance|named-gaps"]
if not gate or gate[0][1] != "PASS":
    problems.append("the named-gaps reconciliation did not pass: " + (gate[0][3] if gate and len(gate) > 3 else "no row"))
if os.path.exists(os.path.join(OUT, "stale-gaps.txt")) and os.path.getsize(os.path.join(OUT, "stale-gaps.txt")):
    problems.append("named-gaps.json has stale entries")
if problems:
    finish("fail", "; ".join(problems))
finish("pass", f"{len(mine)} rows PASS (every owed request and response row for {dialect}), 0 named gaps; "
               f"control and negative control held")
PY
