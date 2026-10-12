"""Real nextest lists: three hash shards run every test of the unsharded list once.

The portable macOS owner runs as three `--partition hash:K/3` shards, and
the evidence gate merges their lists with contracts.partitioned. This builds
a small throwaway crate and checks that merge against what the pinned nextest
actually prints. CI's own merge then checks the real suite on every run.
"""
from pathlib import Path
import json
import os
import shutil
import subprocess
import tempfile
import unittest

import contracts as c
import workflow_assembly as w

LIBRARY = "\n".join(f"#[test] fn unit_{index}() {{}}" for index in range(40))
INTEGRATION = "\n".join(f"#[test] fn integration_{index}() {{}}" for index in range(15)) + """
#[test] #[ignore] fn ignored_elsewhere() {}
"""


@unittest.skipUnless(shutil.which("cargo") and shutil.which("cargo-nextest"),
                     "needs cargo and cargo-nextest (run with make test-ci-scripts on a Rust host)")
class ShardPartition(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.TemporaryDirectory()
        root = Path(cls.tmp.name)
        (root / "src").mkdir()
        (root / "tests").mkdir()
        (root / "Cargo.toml").write_text('[package]\nname = "shards"\nversion = "0.1.0"\nedition = "2021"\n'
                                         '\n[workspace]\n')
        (root / "src/lib.rs").write_text(LIBRARY + "\n")
        (root / "tests/other.rs").write_text(INTEGRATION)
        cls.root = root
        cls.env = {**os.environ, "CARGO_TARGET_DIR": str(root / "target")}
        cls.env.pop("NEXTEST_PROFILE", None)

    @classmethod
    def tearDownClass(cls):
        cls.tmp.cleanup()

    def listed(self, *arguments):
        result = subprocess.run(["cargo", "nextest", "list", "--message-format", "json", *arguments],
                                cwd=self.root, env=self.env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        return json.loads(result.stdout)

    def test_the_shards_together_run_the_unsharded_list_exactly_once(self):
        whole = c.selected(self.listed())
        count = w.SHARDS["portable-darwin"]
        ran = set()
        for shard in range(1, count + 1):
            own, everything = c.partitioned(self.listed("--partition", w.partition("portable-darwin", shard)))
            self.assertEqual(everything, whole, shard)
            self.assertFalse(own & ran, shard)
            self.assertTrue(own, shard)
            ran |= own
        self.assertEqual(ran, whole)
        self.assertNotIn(("shards::other", "ignored_elsewhere"), whole)


if __name__ == "__main__":
    unittest.main()
