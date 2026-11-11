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
# pin + this checkout's plane-llm, inherited workspace inputs and harness content, so a line that runs
# alone does the whole run itself and a stale cache can never answer for different code.
#
#   1. CONTROL           testing/llm-conformance/selftest.sh: known-good vendor/1.5.5 fixtures validate
#   2. NEGATIVE CONTROL  the same selftest plants bad responses, streams and gaps; each MUST be refused
#   3. SUBJECT           busbar's recorded wire, per dialect; every owed row must PASS, none may SKIP
# A dialect is `pass` only when all three hold. A subject that cannot be built or booted is `fail` (armed).
#
# Env: CONFORMANCE_SUITE, CONFORMANCE_OUT, BUSBAR_CHECKOUT, BUSBAR_PIN, CARGO_TARGET_DIR (as run.sh sets).
# Needs: rust, python3 (controller-installed or a hash-pinned venv), curl, git.
#
#   bash testing/conformance/llm.sh --selftest   # the swap bites: BUSBAR_CHECKOUT (busbar at the pin) gets
#                                                # this checkout's plane, and a busbar it cannot swap is refused
set -uo pipefail
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
H="$repo/testing/llm-conformance"
TGT="${CARGO_TARGET_DIR:-$repo/target}"
say() { printf 'llm: %s\n' "$*" >&2; }
sha() { if command -v sha256sum >/dev/null 2>&1; then sha256sum | cut -d' ' -f1; else shasum -a 256 | cut -d' ' -f1; fi; }

# Null-delimited Git paths and length-delimited hash records preserve arbitrary file names.
# Keep this query-only entry point usable by the cache-key regression without Cargo or a build.
cache_key() {
  python3 -I - "$repo" "$BUSBAR_PIN" <<'PYKEY'
import hashlib, os, stat, subprocess, sys
from pathlib import Path
root, pin = Path(sys.argv[1]), sys.argv[2]
inputs = ["Cargo.toml", "Cargo.lock", "plane-llm", "testing/llm-conformance",
          "testing/conformance/requirements-llm.txt", "testing/conformance/llm.sh",
          "testing/conformance/python.sh", "testing/conformance/capabilities.py",
          "testing/conformance/test_llm_subject.py", "testing/conformance/test_llm_offline.py"]
paths = subprocess.check_output(["git", "-C", str(root), "ls-files", "-z", "--cached",
                                 "--others", "--", *inputs]).split(b"\0")
digest = hashlib.sha256()
def record(value):
    digest.update(len(value).to_bytes(8, "big"))
    digest.update(value)
record(pin.encode())
for name in sorted(set(paths) - {b""}):
    record(name)
    path = root / os.fsdecode(name)
    try:
        mode = path.lstat().st_mode
    except FileNotFoundError:
        record(b"deleted")
        continue
    if stat.S_ISLNK(mode):
        record(b"symlink")
        record(os.fsencode(os.readlink(path)))
    elif stat.S_ISREG(mode):
        record(b"executable" if mode & 0o111 else b"file")
    else:
        raise ValueError(f"unsupported conformance input: {name!r}")
    # Following a symlink hashes the bytes Cargo reads as well as its target spelling.
    record(path.read_bytes())
print(digest.hexdigest()[:16])
PYKEY
}
if [ "${1:-}" = --cache-key ]; then
  : "${BUSBAR_PIN:?--cache-key needs BUSBAR_PIN}"
  cache_key
  exit $?
fi

# Judge authority belongs to the trusted runner; validate it before staging or building a host.
# The host shim verifies the pre-fetched executable against the immutable architecture digest.
oracle_preflight() {
  local var
  for var in BUSBAR_ORACLE_RELEASE_REPO BUSBAR_ORACLE_RELEASE_REF \
             BUSBAR_ORACLE_SHA256_LINUX_X86_64 BUSBAR_ORACLE_SHA256_LINUX_AARCH64 \
             BUSBAR_ORACLE_CRATE BUSBAR_ORACLE_BIN BUSBAR_ORACLE_JUDGE_PATH; do
    [ -n "${!var:-}" ] || { say "NO JUDGE: trusted runner sets no $var"; return 1; }
  done
  "${BUSBAR_CHECKOUT:?oracle preflight needs BUSBAR_CHECKOUT}/bin/oracle" tool-dir
}
[ "${1:-}" = --oracle-preflight ] && { oracle_preflight; exit $?; }

# busbar (a checkout at the pin, $1) staged at $2, with no build output.
stage_busbar() {
  rm -rf "$2"
  mkdir -p "$2" && tar -C "$1" --exclude=./target -cf - . | tar -C "$2" -xf - || return 1
  # The signed recorder rebuilds its run-keyed candidate in product_root/target. Give both builds
  # that same path and the same artifacts; removing this scratch tree removes only the symlink.
  local target
  mkdir -p "$TGT" && target="$(cd "$TGT" && pwd -P)" || return 1
  [ ! -e "$2/target" ] && [ ! -L "$2/target" ] || return 1
  ln -s "$target" "$2/target"
}

# On source-plugin lines the signed recorder builds the shipping candidate with its run key.
# As in bin/oracle ci-run, a metadata-derived line probe supplies only --version for that build.
# Released lines still need the real initial binary because the recorder does not rebuild them.
subject_input() {
  local src="$1" out="$2" info version builds
  info="$(cd "$src" && cargo metadata --locked --no-deps --format-version 1 --manifest-path dist/default/Cargo.toml | python3 -c '
import json, re, sys
m = json.load(sys.stdin)
v = [p["version"] for p in m["packages"] if any(t["name"] == "busbar" and "bin" in t["kind"] for t in p["targets"])]
if len(v) != 1:
    sys.exit("metadata names no unique busbar binary version")
line = ".".join(v[0].split(".")[:2])
rows = [r.split("\t") for r in open(sys.argv[1]) if r.strip() and not r.startswith("#")]
rows = [r for r in rows if len(r) >= 4 and (r[4].strip() if len(r) > 4 and r[4].strip() else "1.5") == line]
def source(r):
    return re.fullmatch(r"src:[0-9a-fA-F]{40}", r[1]) and re.fullmatch(r"build:[A-Za-z0-9._-]+", r[2])
print(v[0], "yes" if rows and all(source(r) for r in rows) else "no")
' "$src/testing/shadow-oracle/plugin-digests.tsv")" || return 1
  read -r version builds <<<"$info"
  if [ "$builds" = yes ]; then
    printf '#!/bin/sh\n[ "$#" = 1 ] && [ "$1" = --version ] || exit 2\nprintf "busbar %%s\\n" "%s"\n' "$version" >"$out/line-probe"
    chmod +x "$out/line-probe" || return 1
    printf '%s\n' "$out/line-probe"
  else
    ( cd "$src" && CARGO_TARGET_DIR="$src/target" cargo build --locked --release --manifest-path dist/default/Cargo.toml --bin busbar ) >&2 || return 1
    printf '%s\n' "$TGT/release/busbar"
  fi
}

# Audio uses the exact executable the signed recorder measured, never a line probe or an unrelated
# preexisting build. On source-plugin lines the signed engine retains it at work/busbar-candidate.
retain_recorded_subject() {
  python3 -I - "$D" "$1" <<'PYSUBJECT'
import hashlib, json, os, pathlib, shutil, sys
root, supplied = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
meta = json.loads((root / "recording/meta.json").read_text())
expected = root / "work/busbar-candidate" if supplied == root / "line-probe" else supplied
actual = pathlib.Path(meta["binary"])
if actual.resolve() != expected.resolve() or not actual.is_file() or not os.access(actual, os.X_OK):
    sys.exit("recording did not name the expected executable subject")
measured = hashlib.sha256(actual.read_bytes()).hexdigest()
if measured != meta["binary_sha256"]:
    sys.exit("recorded subject bytes no longer match the recording digest")
if supplied == root / "line-probe" and measured == hashlib.sha256(supplied.read_bytes()).hexdigest():
    sys.exit("the recorder measured the line probe instead of building its candidate")
shutil.copy2(actual, root / "busbar.new")
os.replace(root / "busbar.new", root / "busbar")
PYSUBJECT
}

# Keep release-mode cfgs and the full locked default distribution for functional probes,
# with light optimization for both compilation and repeated probe execution. The release producer
# owns shipping optimization settings. The signed recorder still builds with its run key
# and verifies the measured executable; these settings apply only to this invocation.
record_subject() {
  CARGO_PROFILE_RELEASE_OPT_LEVEL=1 CARGO_PROFILE_RELEASE_LTO=off \
    CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 \
    "$1/bin/oracle" record --bin "$2" --plane llm --out "$D/recording" 2>&1 \
    | tee "$D/record.log" \
    | python3 -u -c '
import re, sys
count = 0
for line in sys.stdin:
    message = None
    if line.startswith("record --native: building the candidate "):
        message = "llm recorder phase: building run-keyed candidate"
    else:
        match = re.match(r"^\s*(Compiling|Checking|Finished) ([A-Za-z0-9_-]+)(?:\s|$)", line)
        if match:
            message = "llm recorder compiler: " + match[1] + " " + match[2][:100]
    if message is not None and count < 120:
        print(message, flush=True)
        count += 1
' >&2
  local results=("${PIPESTATUS[@]}")
  # Preserve the recorder's refusal; a failed evidence sink is also not a recording.
  [ "${results[0]}" = 0 ] || return "${results[0]}"
  [ "${results[1]}" = 0 ] && [ "${results[2]}" = 0 ]
}

# Swap THIS checkout's plane-llm into the staged busbar at $1 (written at run time, never committed).
# The plane is a git pin of busbar's distribution workspace (dist/default/Cargo.toml's
# [workspace.dependencies]); the swap patches that pin's source with this checkout's crate and moves the
# lock's one busbar-plane-llm entry onto it (`cargo update -p`: nothing else in the lock moves), so the
# subject builds --locked. It is refused unless the lock then names this checkout's crate alone.
swap_plane() {
  local M="$1/dist/default/Cargo.toml" L="$1/dist/default/Cargo.lock" url n
  [ -f "$M" ] && [ -f "$L" ] || { echo "no dist/default/Cargo.toml + Cargo.lock in the busbar checkout"; return 1; }
  url="$(sed -n 's/^busbar-plane-llm = { *git = "\([^"]*\)".*/\1/p' "$M")"
  n="$(printf '%s' "$url" | grep -c .)"
  [ "$n" = 1 ] || { echo "dist/default/Cargo.toml names $n git pin(s) of busbar-plane-llm, not one"; return 1; }
  grep -q "^\[patch\.['\"]$url['\"]\]" "$M" && { echo "dist/default/Cargo.toml already patches $url"; return 1; }
  printf '\n# conformance (llm.sh): this checkout'"'"'s plane-llm, swapped in at run time.\n[patch.'"'"'%s'"'"']\nbusbar-plane-llm = { path = "%s" }\n' \
    "$url" "$repo/plane-llm" >> "$M"
  cargo update --manifest-path "$M" -p busbar-plane-llm || { echo "cargo could not move the lock onto this checkout's plane-llm"; return 1; }
  # The lock's busbar-plane-llm entries: exactly one, with no source (a path crate: this checkout's).
  python3 -I - "$L" <<'PY' || return 1
import re, sys
pk = [b for b in open(sys.argv[1]).read().split("[[package]]") if re.search(r'^name = "busbar-plane-llm"$', b, re.M)]
src = [(re.findall(r'^source = "([^"]*)"$', b, re.M) or ["path"])[0] for b in pk]
if src != ["path"]:
    sys.exit(f"after the swap the lock names busbar-plane-llm as {src}, not this checkout's crate alone")
PY
}

# The swap's own proof, no build: busbar at the pin resolves this checkout's plane, and only it.
selftest() {
  : "${BUSBAR_CHECKOUT:?--selftest needs BUSBAR_CHECKOUT (busbar at the .busbar-ref pin)}"
  local t ok=0 got; t="$(mktemp -d)"
  stage_busbar "$BUSBAR_CHECKOUT" "$t/a" || { echo "selftest FAILED: busbar could not be staged"; rm -rf "$t"; return 1; }
  if swap_plane "$t/a" >"$t/a.log" 2>&1; then
    got="$( cargo metadata --locked --format-version 1 --manifest-path "$t/a/dist/default/Cargo.toml" 2>"$t/a.meta.log" \
            | python3 -I -c 'import json,sys; print("\n".join(p["manifest_path"] for p in json.load(sys.stdin)["packages"] if p["name"]=="busbar-plane-llm"))' )"
    if [ "$got" = "$repo/plane-llm/Cargo.toml" ]; then
      echo "  ok: busbar at the pin resolves this checkout's plane-llm, --locked"
    else
      echo "selftest FAILED: after the swap busbar resolves busbar-plane-llm from [${got:-nothing}], not $repo/plane-llm"; tail -5 "$t/a.meta.log"; ok=1
    fi
  else
    echo "selftest FAILED: this checkout's plane-llm could not be swapped into busbar"; tail -5 "$t/a.log"; ok=1
  fi
  # A busbar whose distribution names no plane-llm pin cannot take the swap: refused, never built as is.
  stage_busbar "$BUSBAR_CHECKOUT" "$t/b" && sed -i.bak '/^busbar-plane-llm = { git = /d' "$t/b/dist/default/Cargo.toml"
  if swap_plane "$t/b" >"$t/b.log" 2>&1; then
    echo "selftest FAILED: a busbar with no plane-llm pin took the swap"; ok=1
  else
    echo "  ok: a busbar with no plane-llm pin is refused"
  fi
  rm -rf "$t"; return $ok
}
[ "${1:-}" = --selftest ] && { selftest; exit $?; }
# `llm.sh --subject` (CONFORMANCE_SUITE=llm-<any>): the shared run, if this check has not made it, then
# the path of the busbar it built with this plane on the last line of stdout (exit 1 when there is
# none). The one-shot audio suites (audio.sh) judge that same build.
mode=verdict; [ "${1:-}" = --subject ] && mode=subject
: "${CONFORMANCE_SUITE:?}" "${CONFORMANCE_OUT:?}" "${BUSBAR_CHECKOUT:?}" "${BUSBAR_PIN:?}"

dialect="${CONFORMANCE_SUITE#llm-}"
case "$dialect" in anthropic|openai|responses|gemini|bedrock|cohere) ;; *) say "unknown suite $CONFORMANCE_SUITE"; exit 2 ;; esac

# The path crate inherits workspace settings. Hash its actual working-tree bytes, including
# untracked modules and binary inputs; index entries and textual diffs cannot identify those.
key="$(cache_key)" || { say "could not identify conformance inputs"; exit 1; }
D="$TGT/llm-conformance/$key"
mkdir -p "$D"
# shellcheck source=testing/conformance/python.sh
. "$repo/testing/conformance/python.sh"

# Even a completed shared run cannot hide a missing or changed isolated runtime.
runtime_fail=""
if [ -n "${CONFORMANCE_CAPABILITIES+x}" ]; then
  llm_python "$repo" "$TGT" >"$D/venv.log" 2>&1 || runtime_fail="the pinned installed Python environment is unavailable (see venv.log)"
  if [ -z "$runtime_fail" ]; then
    bash "$H/vendor.sh" --check >"$D/vendor.log" 2>&1 \
      || runtime_fail="the pinned offline spec cache is incomplete (see vendor.log)"
  fi
fi

# ── the shared run ───────────────────────────────────────────────────────────────────────────────
shared_run() {
  local fail="$runtime_fail"   # the first reason the run cannot be judged at all; every suite inherits it
  oracle_preflight >"$D/judge.log" 2>&1 || {
    tail -n 30 "$D/judge.log" >&2
    fail="the trusted oracle judge is unavailable (see judge.log)"
  }
  if [ -z "$fail" ] && [ -z "${CONFORMANCE_CAPABILITIES+x}" ]; then
    llm_python "$repo" "$TGT" >"$D/venv.log" 2>&1 || fail="the hash-pinned python environment could not be built (see venv.log)"
  fi

  # busbar at the pin with this checkout's plane swapped in (written at run time, never committed).
  local SRC="$D/busbar-src"
  if [ -z "$fail" ]; then
    stage_busbar "$BUSBAR_CHECKOUT" "$SRC" || fail="busbar could not be staged"
  fi
  if [ -z "$fail" ]; then
    swap_plane "$SRC" >"$D/swap.log" 2>&1 || { tail -15 "$D/swap.log" >&2; fail="this checkout's plane-llm could not be swapped into busbar (see swap.log)"; }
  fi

  # 1+2. CONTROL and NEGATIVE CONTROL: the harness's own selftest (BUSBAR_CHECKOUT supplies the shared verdict).
  local st_rc=0
  if [ -z "$fail" ]; then
    say "=== control and negative control: selftest.sh"
    BUSBAR_CHECKOUT="$SRC" bash "$H/selftest.sh" >"$D/selftest.log" 2>&1 || st_rc=$?
    grep -vE '^PASS' "$D/selftest.log" | head -60 >&2
  fi

  # 3. SUBJECT: let the signed recorder build source-plugin lines, then validate its wire.
  local rec_rc=0 run_rc=0 subject_bin=""
  if [ -z "$fail" ]; then
    say "=== subject: resolve recorder input"
    subject_bin="$(subject_input "$SRC" "$D" 2>"$D/build.log")" || {
      tail -15 "$D/build.log" >&2
      fail="the recorder subject input could not be prepared (see build.log)"
    }
  fi
  if [ -z "$fail" ]; then
    say "=== subject: record the llm cells (functional release profile: opt-level 1, LTO off, 16 codegen units)"
    export MODERNCI_CACHE_DIR="${MODERNCI_CACHE_DIR:-$TGT/llm-conformance/oracle-cache}"
    # The trusted runner supplies the immutable judge identity and pre-fetched judge path.
    # bin/oracle verifies that path against the supplied architecture digest before executing it.
    # Never derive judge authority from a candidate/host file or fetch a judge inside this run.
    rm -rf "$D/recording"
    record_subject "$SRC" "$subject_bin" || rec_rc=$?
    if [ "$rec_rc" -ne 0 ]; then tail -15 "$D/record.log" >&2; fail="the llm cells could not be recorded (exit $rec_rc; see record.log)"; fi
    if [ -z "$fail" ]; then
      retain_recorded_subject "$subject_bin" >>"$D/record.log" 2>&1 || fail="the recording did not retain its measured subject (see record.log)"
    fi
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

# A cached run answers when it is a COMPLETE one (the subject was built, recorded and judged, the controls
# held) or when this very check run made it (the other five lines of the same run). A cached infrastructure
# failure from an earlier check run is not an answer: the line tries again.
marker="$(dirname "$CONFORMANCE_OUT")/.llm-shared-$key"
complete() { python3 -I -c 'import json,sys; d=json.load(open(sys.argv[1])); sys.exit(0 if not d["fail"] and d["selftest_rc"]==0 else 1)' "$D/shared.json" 2>/dev/null; }
if [ -z "$runtime_fail" ] && [ -s "$D/shared.json" ] && { [ -e "$marker" ] || complete; }; then
  say "shared run cached at $D"
else
  rm -f "$D/shared.json"; shared_run; : > "$marker"
fi
if [ "$mode" = subject ]; then
  python3 -I -c 'import json,sys; sys.exit(1 if json.load(open(sys.argv[1]))["fail"] else 0)' "$D/shared.json" \
    || { say "the shared run could not produce a measured subject ($D)"; exit 1; }
  [ -x "$D/busbar" ] || { say "the shared run built no busbar ($D)"; exit 1; }
  echo "$D/busbar"; exit 0
fi

# ── this suite's verdict, from the shared ledger ─────────────────────────────────────────────────
# The evidence this suite's verdict rests on, kept with the check's outputs (target/conformance/<id>/).
for f in judge.log swap.log selftest.log build.log record.log run.log venv.log vendor.log out/report.md out/ledger.tsv shared.json; do
  if [ -f "$D/$f" ]; then
    case "$f" in build.log) tail -n 200 "$D/$f" ;; *) cat "$D/$f" ;; esac > "$CONFORMANCE_OUT/$(basename "$f")" 2>/dev/null || true
  fi
done
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
    problems.append("the named-gaps reconciliation did not pass: " + (gate[0][3] if gate and len(gate[0]) > 3 else "no row"))
if os.path.exists(os.path.join(OUT, "stale-gaps.txt")) and os.path.getsize(os.path.join(OUT, "stale-gaps.txt")):
    problems.append("named-gaps.json has stale entries")
if problems:
    finish("fail", "; ".join(problems))
finish("pass", f"{len(mine)} rows PASS (every owed request and response row for {dialect}), 0 named gaps; "
               f"control and negative control held")
PY
