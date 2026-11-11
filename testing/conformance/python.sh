#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright (C) 2026 Busbar Inc and contributors

# Sourced by the LLM and audio owners. The isolated controller supplies a read-only
# capability manifest and an image whose installed Python dependencies match this pin.
llm_python() {
  local root="$1" target="$2" req venv
  req="$root/testing/conformance/requirements-llm.txt"
  if [ -n "${CONFORMANCE_CAPABILITIES+x}" ]; then
    /usr/bin/python3 -I -B - "$root" "$req" <<'PYFIXED' || return 1
import hashlib
import importlib.metadata
import importlib.util
import os
from pathlib import Path
import sys

root, requirements = map(Path, sys.argv[1:])
spec = importlib.util.spec_from_file_location("capabilities", root / "testing/conformance/capabilities.py")
capabilities = importlib.util.module_from_spec(spec)
spec.loader.exec_module(capabilities)
why = capabilities.unavailable(os.environ["CONFORMANCE_CAPABILITIES"], os.environ["CONF_REPO"],
                               os.environ["CONF_HEAD"], os.environ["CONFORMANCE_SUITE"])
if why:
    sys.exit(why)
# This is the requirements file used to construct the controller's immutable tool image.
expected = "90224bfa4d4ca0e474fc31e3cddb26886ddcd029bde0a090ece90c75a1aacf65"
if hashlib.sha256(requirements.read_bytes()).hexdigest() != expected:
    sys.exit("installed Python dependency pin differs from requirements-llm.txt")
if sys.version_info[:2] != (3, 11):
    sys.exit("the isolated judge requires the pinned Python 3.11 runtime")
import yaml
if yaml.__version__ != "6.0.3" or importlib.metadata.version("PyYAML") != "6.0.3":
    sys.exit("the isolated judge requires installed PyYAML 6.0.3")
print("verified installed Python 3.11 and PyYAML 6.0.3 against the requirements pin", file=sys.stderr)
PYFIXED
    LLM_CONFORMANCE_RUNTIME_PYTHON=/usr/bin/python3
    export PATH="/usr/bin:$PATH"
    export BUSBAR_LLM_SPEC_CACHE="${BUSBAR_LLM_SPEC_CACHE:-/opt/busbar-conformance/llm-specs}"
  else
    venv="$target/llm-conformance/venv-$(sha < "$req" | cut -c1-12)"
    if [ ! -x "$venv/bin/python" ] || ! "$venv/bin/python" -c 'import yaml' 2>/dev/null; then
      rm -rf "$venv"
      { "${LLM_CONFORMANCE_PYTHON:-python3}" -m venv "$venv" \
        && "$venv/bin/python" -m pip install --disable-pip-version-check --quiet --require-hashes --only-binary=:all: -r "$req"; } \
        || { rm -rf "$venv"; return 1; }
    fi
    LLM_CONFORMANCE_RUNTIME_PYTHON="$venv/bin/python"
    export PATH="$venv/bin:$PATH"
  fi
  export LLM_CONFORMANCE_RUNTIME_PYTHON
}
