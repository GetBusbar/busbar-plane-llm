"""Targeted offline owner-runtime probes, without building or replacing the judge."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


REPO = Path(__file__).resolve().parents[2]


class OfflineRuntime(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="llm-offline-")
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.owner = self.root / "owner"
        harness = self.owner / "testing/conformance"
        harness.mkdir(parents=True)
        for name in ("python.sh", "capabilities.py", "requirements-llm.txt"):
            shutil.copyfile(REPO / "testing/conformance" / name, harness / name)
        self.manifest = self.root / "capabilities.json"
        self.record = {
            "schema": "busbar.conformance.capabilities/1", "repo": "GetBusbar/busbar-plane-llm",
            "source": "a" * 40, "toolchain_ready": True,
            "suites": {"llm-openai": {"available": True}},
        }
        self.manifest.write_text(json.dumps(self.record))
        self.attempts = self.root / "bootstrap-attempts"
        bootstrap = self.root / "bootstrap"
        bootstrap.write_text('#!/bin/sh\necho attempt >> "$BOOTSTRAP_ATTEMPTS"\nexit 97\n')
        bootstrap.chmod(0o755)
        self.env = dict(os.environ, CONFORMANCE_CAPABILITIES=str(self.manifest),
                        CONF_REPO=self.record["repo"], CONF_HEAD=self.record["source"],
                        CONFORMANCE_SUITE="llm-openai", OWNER=str(self.owner),
                        CARGO_TARGET_DIR=str(self.root / "target"),
                        LLM_CONFORMANCE_PYTHON=str(bootstrap), BOOTSTRAP_ATTEMPTS=str(self.attempts))

    def prepare(self):
        return subprocess.run(["bash", "-c", '''
repo="$OWNER"
TGT="$CARGO_TARGET_DIR"
. "$repo/testing/conformance/python.sh"
llm_python "$repo" "$TGT" || exit $?
printf '%s\\n' "$LLM_CONFORMANCE_RUNTIME_PYTHON"
'''], env=self.env, capture_output=True, text=True, timeout=30)

    def test_changed_dependency_pin_refuses_without_bootstrap(self):
        requirements = self.owner / "testing/conformance/requirements-llm.txt"
        requirements.write_text(requirements.read_text().replace("6.0.3", "6.0.2"))
        result = self.prepare()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("dependency pin differs", result.stderr)
        self.assertFalse(self.attempts.exists())
        self.assertFalse((self.root / "target").exists())

    def test_capability_and_source_binding_are_required(self):
        for key, value in (("source", "b" * 40), ("toolchain_ready", False),
                           ("suites", {})):
            with self.subTest(key=key):
                self.manifest.write_text(json.dumps({**self.record, key: value}))
                self.assertNotEqual(self.prepare().returncode, 0)
                self.assertFalse(self.attempts.exists())
        self.manifest.unlink()
        self.assertNotEqual(self.prepare().returncode, 0)
        self.assertFalse(self.attempts.exists())

    def test_installed_runtime_is_used_when_available(self):
        installed = subprocess.run(["/usr/bin/python3", "-I", "-B", "-c",
                                    "import sys, yaml, importlib.metadata; "
                                    "assert sys.version_info[:2] == (3, 11); "
                                    "assert yaml.__version__ == importlib.metadata.version('PyYAML') == '6.0.3'"],
                                   capture_output=True, timeout=30)
        result = self.prepare()
        self.assertFalse(self.attempts.exists())
        self.assertFalse((self.root / "target").exists())
        if installed.returncode:
            self.assertNotEqual(result.returncode, 0)
            self.skipTest("fixed Python 3.11 with PyYAML 6.0.3 is absent on this host")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "/usr/bin/python3")

    def test_cached_subject_cannot_mask_unavailable_offline_inputs(self):
        script = self.owner / "testing/conformance/llm.sh"
        shutil.copyfile(REPO / "testing/conformance/llm.sh", script)
        subprocess.run(["git", "-C", str(self.owner), "init", "-q"], check=True, timeout=30)
        self.env["BUSBAR_PIN"] = "b" * 40
        self.env["BUSBAR_CHECKOUT"] = str(self.owner)
        output = self.root / "output"
        output.mkdir()
        self.env["CONFORMANCE_OUT"] = str(output)
        self.env["BUSBAR_LLM_SPEC_CACHE"] = str(self.root / "missing-spec-cache")
        # The fresh run stops at the missing runtime/cache before any oracle or build.
        for name in tuple(self.env):
            if name.startswith("BUSBAR_ORACLE_"):
                self.env.pop(name)
        key = subprocess.check_output(["bash", str(script), "--cache-key"], env=self.env,
                                      text=True, timeout=30).strip()
        cached = Path(self.env["CARGO_TARGET_DIR"]) / "llm-conformance" / key
        cached.mkdir(parents=True)
        shared = cached / "shared.json"
        shared.write_text(json.dumps({"fail": "", "selftest_rc": 0, "run_rc": 0}))
        subject = cached / "busbar"
        subject.write_text("#!/bin/sh\nexit 99\n")
        subject.chmod(0o755)
        result = subprocess.run(["bash", str(script), "--subject"], env=self.env,
                                capture_output=True, text=True, timeout=30)
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn(str(subject), result.stdout)
        self.assertTrue(json.loads(shared.read_text())["fail"])
        self.assertFalse(self.attempts.exists())


class OfflineSpecs(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="llm-offline-specs-")
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.cache = self.root / "cache"
        self.pins = self.root / "pins.tsv"
        self.data = b'{"fixture":true}\n'
        self.digest = hashlib.sha256(self.data).hexdigest()
        self.pins.write_text(f"fixture\traw\t{self.digest}\thttps://example.invalid/spec.json\n")
        commands = self.root / "commands"
        commands.mkdir()
        self.network = self.root / "network-attempts"
        curl = commands / "curl"
        curl.write_text('#!/bin/sh\necho attempt >> "$NETWORK_ATTEMPTS"\nexit 98\n')
        curl.chmod(0o755)
        self.env = dict(os.environ, CONFORMANCE_CAPABILITIES="controller-fixture",
                        PATH=str(commands) + ":" + os.environ["PATH"],
                        NETWORK_ATTEMPTS=str(self.network))

    def vendor(self, *args):
        return subprocess.run(["bash", str(REPO / "testing/llm-conformance/vendor.sh"),
                               "--digests", str(self.pins), "--cache-dir", str(self.cache), *args],
                              env=self.env, capture_output=True, text=True, timeout=30)

    def test_missing_and_drifted_public_specs_refuse_without_network(self):
        for mode in ((), ("--check",)):
            self.assertEqual(self.vendor(*mode).returncode, 3)
            self.assertFalse(self.network.exists())
        path = self.cache / "fixture" / self.digest / "spec.json"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(b"drifted")
        for mode in ((), ("--check",)):
            self.assertEqual(self.vendor(*mode).returncode, 3)
            self.assertFalse(self.network.exists())

    def test_verified_controller_cache_is_consumed_without_writes(self):
        directory = self.cache / "fixture" / self.digest
        directory.mkdir(parents=True)
        (directory / "spec.json").write_bytes(self.data)
        (directory / ".digest").write_text("unchanged sidecar\n")
        (directory / "spec.parsed.json").write_text("unchanged unused parse\n")
        before = {p.name: p.read_bytes() for p in directory.iterdir()}
        for mode in ((), ("--check",)):
            result = self.vendor(*mode)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual({p.name: p.read_bytes() for p in directory.iterdir()}, before)
            self.assertFalse(self.network.exists())

    def test_vendored_copy_keeps_its_digest_check_offline(self):
        source = self.root / "specs" / "spec.json"
        source.parent.mkdir()
        source.write_bytes(self.data)
        self.pins.write_text(f"fixture\traw\t{self.digest}\tvendored:specs/spec.json\n")
        result = self.vendor()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.cache / "fixture" / self.digest / "spec.json").read_bytes(), self.data)
        shutil.rmtree(self.cache)
        source.write_bytes(b"drifted")
        result = self.vendor()
        self.assertEqual(result.returncode, 3)
        self.assertIn("COMMITTED copy", result.stderr)
        self.assertFalse(self.network.exists())


if __name__ == "__main__":
    unittest.main()
