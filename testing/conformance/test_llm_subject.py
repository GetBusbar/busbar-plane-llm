"""Targeted shell contract probes; no host build or oracle substitution."""
import hashlib
import json
import os
from pathlib import Path
import selectors
import signal
import subprocess
import tempfile
import unittest


class SubjectContract(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="llm-subject-")
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.src = self.root / "host"
        self.out = self.root / "run"
        self.target = self.root / "target"
        for path in (self.src, self.out, self.target):
            path.mkdir()
        (self.src / "target").symlink_to(self.target, target_is_directory=True)
        data = self.src / "testing/shadow-oracle"
        data.mkdir(parents=True)
        self.pins = data / "plugin-digests.tsv"
        commands = self.root / "commands"
        commands.mkdir()
        cargo = commands / "cargo"
        cargo.write_text('''#!/usr/bin/env python3
import json, os, pathlib, sys
with open(os.environ["COMMAND_LOG"], "a") as f:
    f.write(json.dumps(sys.argv[1:]) + "\\n")
if sys.argv[1] == "metadata":
    print(json.dumps({"packages": [{"version": "1.6.0", "targets": [{"name": "busbar", "kind": ["bin"]}]}]}))
elif sys.argv[1] == "build":
    path = pathlib.Path(os.environ["CARGO_TARGET_DIR"]) / "release/busbar"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("released binary fixture")
else:
    sys.exit(2)
''')
        cargo.chmod(0o755)
        self.log = self.root / "commands.jsonl"
        self.env = dict(os.environ, PATH=str(commands) + ":" + os.environ["PATH"],
                        COMMAND_LOG=str(self.log), D=str(self.out), SRC=str(self.src),
                        TGT=str(self.target))
        script = Path(__file__).with_name("llm.sh").read_text()
        self.helpers = script[script.index("subject_input()") : script.index("# Swap THIS checkout")]

    def shell(self, command):
        return subprocess.run(["bash", "-c", self.helpers + "\n" + command],
                              env=self.env, text=True, capture_output=True)

    def test_source_line_only_resolves_metadata_and_probe_cannot_serve(self):
        self.pins.write_text("plugin\tsrc:" + "a" * 40 + "\tbuild:plugin\t-\t1.6\n")
        result = self.shell('subject_input "$SRC" "$D"')
        self.assertEqual(result.returncode, 0, result.stderr)
        probe = self.out / "line-probe"
        self.assertEqual(result.stdout.strip(), str(probe))
        commands = [json.loads(line) for line in self.log.read_text().splitlines()]
        self.assertEqual(len(commands), 1)
        self.assertEqual(commands[0][0], "metadata")
        self.assertEqual(subprocess.check_output([probe, "--version"], text=True), "busbar 1.6.0\n")
        self.assertEqual(subprocess.run([probe, "--validate"]).returncode, 2)

    def test_released_line_keeps_locked_distribution_release_build(self):
        self.pins.write_text("plugin\tv1\tplugin.tar.gz\tdigest\t1.6\n")
        result = self.shell('subject_input "$SRC" "$D"')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), str(self.target / "release/busbar"))
        self.assertFalse((self.out / "line-probe").exists())
        commands = [json.loads(line) for line in self.log.read_text().splitlines()]
        self.assertEqual(commands[1], ["build", "--locked", "--release", "--manifest-path",
                                       "dist/default/Cargo.toml", "--bin", "busbar"])

    def test_recording_uses_functional_profile_and_keeps_identity_arguments_and_failure(self):
        (self.src / "bin").mkdir()
        oracle = self.src / "bin/oracle"
        oracle.write_text('''#!/usr/bin/env python3
import json, os, sys
with open(os.environ["COMMAND_LOG"], "w") as f:
    json.dump({"args": sys.argv[1:], "lto": os.environ["CARGO_PROFILE_RELEASE_LTO"],
               "units": os.environ["CARGO_PROFILE_RELEASE_CODEGEN_UNITS"],
               "judge": os.environ["BUSBAR_ORACLE_JUDGE_PATH"]}, f)
for i in range(140):
    print("record --native: building the candidate private detail " + str(i), file=sys.stderr, flush=True)
print("Authorization: credential fixture", file=sys.stderr, flush=True)
sys.exit(int(os.environ["RECORDER_EXIT"]))
''')
        oracle.chmod(0o755)
        self.env["BUSBAR_ORACLE_JUDGE_PATH"] = "/controller/immutable-judge"
        for code in (0, 7):
            with self.subTest(exit_code=code):
                self.env["RECORDER_EXIT"] = str(code)
                result = self.shell('record_subject "$SRC" "$D/line-probe"')
                self.assertEqual(result.returncode, code, result.stderr)
                invocation = json.loads(self.log.read_text())
                self.assertEqual(invocation, {
                    "args": ["record", "--bin", str(self.out / "line-probe"),
                             "--plane", "llm", "--out", str(self.out / "recording")],
                    "lto": "thin", "units": "16", "judge": "/controller/immutable-judge"})
                self.assertEqual(len(result.stderr.splitlines()), 120)
                self.assertIn("llm recorder phase: building run-keyed candidate", result.stderr)
                self.assertNotIn("private detail", result.stderr)
                self.assertNotIn("credential fixture", result.stderr)
                self.assertEqual(len((self.out / "record.log").read_text().splitlines()), 141)

    def test_recording_phase_is_visible_before_recorder_exit(self):
        (self.src / "bin").mkdir()
        oracle = self.src / "bin/oracle"
        release = self.root / "release-recorder"
        self.env["RELEASE_RECORDER"] = str(release)
        oracle.write_text('''#!/usr/bin/env python3
import os, pathlib, sys, time
print("record --native: building the candidate private detail", file=sys.stderr, flush=True)
while not pathlib.Path(os.environ["RELEASE_RECORDER"]).exists():
    time.sleep(0.01)
''')
        oracle.chmod(0o755)
        proc = subprocess.Popen(["bash", "-c", self.helpers + '\nrecord_subject "$SRC" "$D/line-probe"'],
                                env=self.env, stderr=subprocess.PIPE, start_new_session=True)
        try:
            with selectors.DefaultSelector() as ready:
                ready.register(proc.stderr, selectors.EVENT_READ)
                self.assertTrue(ready.select(3), "phase output was buffered until recorder exit")
            self.assertEqual(proc.stderr.readline(),
                             b"llm recorder phase: building run-keyed candidate\n")
            self.assertIsNone(proc.poll())
        finally:
            release.touch()
            try:
                proc.wait(timeout=3)
            except subprocess.TimeoutExpired:
                os.killpg(proc.pid, signal.SIGKILL)
                proc.wait(timeout=3)
            proc.stderr.close()
        self.assertEqual(proc.returncode, 0)

    def test_recorded_candidate_path_and_digest_are_required(self):
        probe = self.out / "line-probe"
        probe.write_text("line probe fixture")
        candidate = self.out / "work/busbar-candidate"
        candidate.parent.mkdir()
        candidate.write_text("#!/bin/sh\nexit 99\n")
        candidate.chmod(0o755)
        recording = self.out / "recording"
        recording.mkdir()
        meta = recording / "meta.json"
        digest = hashlib.sha256(candidate.read_bytes()).hexdigest()
        for binary, measured in ((probe, hashlib.sha256(probe.read_bytes()).hexdigest()),
                                 (candidate, "0" * 64), (self.out / "other", digest)):
            meta.write_text(json.dumps({"binary": str(binary), "binary_sha256": measured}))
            result = self.shell('retain_recorded_subject "$D/line-probe"')
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse((self.out / "busbar").exists())
        original = candidate.read_bytes()
        candidate.write_bytes(probe.read_bytes())
        meta.write_text(json.dumps({"binary": str(candidate),
                                    "binary_sha256": hashlib.sha256(candidate.read_bytes()).hexdigest()}))
        self.assertNotEqual(self.shell('retain_recorded_subject "$D/line-probe"').returncode, 0)
        candidate.write_bytes(original)
        meta.write_text(json.dumps({"binary": str(candidate), "binary_sha256": digest}))
        result = self.shell('retain_recorded_subject "$D/line-probe"')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.out / "busbar").read_bytes(), candidate.read_bytes())
        self.assertTrue(os.access(self.out / "busbar", os.X_OK))


if __name__ == "__main__":
    unittest.main()
