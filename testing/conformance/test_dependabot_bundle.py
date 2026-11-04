#!/usr/bin/env python3
"""Run the checked-in workflow shell with local Git/GitHub fixtures; no network or Cargo."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

HERE = Path(__file__).resolve().parent
WORKFLOW = HERE.parents[1] / ".github/workflows/dependabot-bundle.yml"


def workflow_shell():
    lines = WORKFLOW.read_text().splitlines()
    start = next(i for i, line in enumerate(lines) if line == "      - run: |") + 1
    return "\n".join(line[10:] if line.startswith("          ") else line
                     for line in lines[start:]) + "\n"


class BundleTests(unittest.TestCase):
    def run_case(self, case):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / "bin").mkdir()
            events = root / "events.jsonl"
            for command in ["gh", "git"]:
                executable = root / "bin" / command
                shutil.copyfile(HERE / "support" / f"template_bundle_{command}.py", executable)
                executable.chmod(0o755)
            shell = workflow_shell()
            syntax = subprocess.run(["bash", "-n"], input=shell, text=True, capture_output=True)
            self.assertEqual(syntax.returncode, 0, syntax.stderr)
            run = subprocess.run(
                ["bash", "-c", shell], cwd=root, text=True, capture_output=True,
                env={**os.environ, "PATH": str(root / "bin") + ":" + os.environ["PATH"],
                     "CASE": case, "EVENTS": str(events), "GITHUB_REPOSITORY": "fixture/plugin",
                     "TARGET": "dev", "BUNDLE": "deps/bundle"},
            )
            recorded = [json.loads(line) for line in events.read_text().splitlines()]
            self.assertFalse(any(e.get("args", [])[:2] == ["pr", "close"] for e in recorded))
            self.assertFalse(any(e.get("new_unbundled_head_closed") for e in recorded))
            return run, recorded

    def test_verified_head_is_named_exactly_without_closing_source(self):
        run, events = self.run_case("normal")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertTrue(any("#1@" + f"{1:040x}" in e.get("body", "") for e in events))
        self.assertTrue(any("push" in e.get("args", []) for e in events))

    def test_head_advancing_after_bundle_publication_is_never_closed(self):
        run, events = self.run_case("synchronize-on-close")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertTrue(any(e.get("source_head_advanced") == "f" * 40 for e in events))
        self.assertTrue(any("#1@" + f"{1:040x}" in e.get("body", "") for e in events))
        self.assertFalse(any("#1@" + "f" * 40 in e.get("body", "") for e in events))

    def test_changed_head_or_unsigned_foreign_commit_is_not_merged(self):
        for case in ["head-moved", "fetch-moved", "mixed-author", "foreign-committer", "unverified"]:
            with self.subTest(case=case):
                run, events = self.run_case(case)
                self.assertNotEqual(run.returncode, 0)
                self.assertFalse(any("push" in e.get("args", []) for e in events))
                self.assertFalse(any("merge" in e.get("args", []) and
                                     e["args"][-1] == "FETCH_HEAD" for e in events))

    def test_foreign_pr_is_neither_merged_nor_closed(self):
        run, events = self.run_case("foreign-pr")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertFalse(any("push" in e.get("args", []) or "fetch" in e.get("args", [])
                             for e in events))

    def test_all_pages_preserve_each_immutable_head(self):
        run, events = self.run_case("pagination")
        self.assertEqual(run.returncode, 0, run.stderr)
        bodies = "\n".join(e.get("body", "") for e in events)
        for number in range(1, 32):
            self.assertIn(f"#{number}@{number:040x}", bodies)


if __name__ == "__main__":
    unittest.main()
