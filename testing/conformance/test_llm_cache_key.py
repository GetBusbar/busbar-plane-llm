#!/usr/bin/env python3
"""Exercise llm.sh's actual cache key against disposable Git working trees; no Cargo."""

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


class CacheKeyTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.script = self.root / "testing/conformance/llm.sh"
        self.script.parent.mkdir(parents=True)
        shutil.copyfile(Path(__file__).with_name("llm.sh"), self.script)
        (self.root / "plane-llm/src").mkdir(parents=True)
        (self.root / "Cargo.toml").write_text('[workspace]\nmembers = ["plane-llm"]\n')
        (self.root / "Cargo.lock").write_text("version = 4\n")
        (self.root / "plane-llm/src/lib.rs").write_text("mod generated;\n")
        (self.root / "plane-llm/src/data.bin").write_bytes(b"\x00first\xff")
        self.git("init", "-q")
        self.git("add", ".")

    def git(self, *args):
        return subprocess.check_output(["git", "-C", str(self.root), *args])

    def key(self, pin="sdk-one"):
        return subprocess.check_output(
            ["bash", str(self.script), "--cache-key"],
            env={**os.environ, "BUSBAR_PIN": pin}, text=True,
        ).strip()

    def test_untracked_module_creation_and_edit_invalidate(self):
        before = self.key()
        source = self.root / "plane-llm/src/generated.rs"
        source.write_text("pub const VALUE: u8 = 1;\n")
        added = self.key()
        source.write_text("pub const VALUE: u8 = 2;\n")
        self.assertEqual(len({before, added, self.key()}), 3)

    def test_ignored_generated_module_content_still_invalidates(self):
        (self.root / ".gitignore").write_text("plane-llm/src/generated.rs\n")
        source = self.root / "plane-llm/src/generated.rs"
        source.write_text("pub const VALUE: u8 = 1;\n")
        before = self.key()
        source.write_text("pub const VALUE: u8 = 2;\n")
        self.assertNotEqual(before, self.key())

    def test_binary_worktree_changes_do_not_share_a_key(self):
        source = self.root / "plane-llm/src/data.bin"
        before = self.key()
        source.write_bytes(b"\x00second\xff")
        second = self.key()
        source.write_bytes(b"\x00third\xff")
        self.assertEqual(len({before, second, self.key()}), 3)

    def test_staged_and_unstaged_binary_content_are_distinguished(self):
        source = self.root / "plane-llm/src/data.bin"
        source.write_bytes(b"\x00staged\xff")
        self.git("add", "plane-llm/src/data.bin")
        staged = self.key()
        source.write_bytes(b"\x00unstaged\xff")
        self.assertNotEqual(staged, self.key())

    def test_tracked_deletion_and_restoration(self):
        source = self.root / "plane-llm/src/lib.rs"
        original = source.read_bytes()
        before = self.key()
        source.unlink()
        self.assertNotEqual(before, self.key())
        source.write_bytes(original)
        self.assertEqual(before, self.key())

    def test_unusual_names_and_renames_are_not_ambiguous(self):
        source = self.root / "plane-llm/src/with space\nand newline.bin"
        source.write_bytes(b"\x00payload")
        before = self.key()
        source.rename(self.root / "plane-llm/src/other name.bin")
        self.assertNotEqual(before, self.key())

    def test_symlink_target_bytes_and_identity_are_hashed(self):
        target = self.root / "external.bin"
        target.write_bytes(b"one")
        link = self.root / "plane-llm/src/link.bin"
        link.symlink_to("../../external.bin")
        before = self.key()
        target.write_bytes(b"two")
        changed = self.key()
        self.assertNotEqual(before, changed)
        other = self.root / "other.bin"
        other.write_bytes(b"two")
        link.unlink()
        link.symlink_to("../../other.bin")
        self.assertNotEqual(changed, self.key())

    def test_inherited_manifest_lock_and_pin_invalidate(self):
        before = self.key()
        (self.root / "Cargo.toml").write_text('[workspace]\nresolver = "2"\n')
        manifest = self.key()
        (self.root / "Cargo.lock").write_text("version = 3\n")
        lock = self.key()
        self.assertEqual(len({before, manifest, lock, self.key("sdk-two")}), 4)

    def test_stable_inputs_have_stable_keys_and_unrelated_outputs_do_not_affect_them(self):
        before = self.key()
        (self.root / "target").mkdir()
        (self.root / "target/output.bin").write_bytes(b"new build output")
        self.assertEqual(before, self.key())
        self.assertEqual(before, self.key())


if __name__ == "__main__":
    unittest.main()
