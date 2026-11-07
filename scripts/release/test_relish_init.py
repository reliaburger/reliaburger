"""`relish init` from the bench and memory scripts (#709).

From 0.3.0, `relish init` writes the root CA's key only into a backup the
operator holds, and refuses to run without a terminal unless it's told where
the backup goes. The scripts also measure published releases, whose `relish`
doesn't know the flags, so the helper passes them only when `relish init
--help` lists `--root-backup`.
"""
import os
from pathlib import Path
import stat
import sys
import tempfile
import textwrap
import unittest

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE / "profiling"))

import relish_init  # noqa: E402


def fake_relish(directory: Path, knows_root_backup: bool) -> Path:
    """A `relish` that prints `init --help` with or without `--root-backup`
    and records every other call's arguments, one per line."""
    help_text = "Options:\n      --cluster-name <CLUSTER_NAME>\n"
    if knows_root_backup:
        help_text += "      --root-backup <ROOT_BACKUP>\n"
    path = directory / "relish"
    path.write_text(textwrap.dedent(f"""\
        #!/usr/bin/env python3
        import sys, pathlib
        if sys.argv[1:3] == ["init", "--help"]:
            print({help_text!r})
            sys.exit(0)
        record = pathlib.Path({str(directory / "calls")!r})
        args = sys.argv[1:]
        if "--passphrase-file" in args:
            passphrase = pathlib.Path(args[args.index("--passphrase-file") + 1])
            mode = passphrase.stat().st_mode & 0o777
            args.append(f"passphrase-mode={{mode:o}}")
        record.write_text("\\n".join(args))
        """))
    path.chmod(0o755)
    return path


class InitTests(unittest.TestCase):
    def setUp(self):
        self.work = Path(tempfile.mkdtemp(prefix="relish-init-test-"))

    def calls(self):
        return (self.work / "calls").read_text().splitlines()

    def test_a_relish_that_knows_root_backup_gets_a_backup_and_a_private_passphrase(self):
        relish = fake_relish(self.work, knows_root_backup=True)
        relish_init.init_cluster(str(relish), self.work / "cluster", "bench", "node-01", self.work)
        calls = self.calls()
        self.assertEqual(calls[:6], ["init", str(self.work / "cluster"), "--cluster-name", "bench",
                                     "--node-id", "node-01"])
        self.assertIn("--root-backup", calls)
        self.assertEqual(calls[calls.index("--root-backup") + 1], str(self.work / "root-backup.age"))
        self.assertIn("passphrase-mode=600", calls)
        # The passphrase is gone once init has used it.
        self.assertFalse((self.work / "root-passphrase").exists())

    def test_a_published_relish_without_the_flag_gets_the_old_command_line(self):
        relish = fake_relish(self.work, knows_root_backup=False)
        relish_init.init_cluster(str(relish), self.work / "cluster", "bench", "node-01", self.work)
        self.assertEqual(self.calls(), ["init", str(self.work / "cluster"), "--cluster-name", "bench",
                                        "--node-id", "node-01"])
        self.assertFalse((self.work / "root-passphrase").exists())

    def test_the_command_line_entry_point_runs_init(self):
        relish = fake_relish(self.work, knows_root_backup=True)
        relish_init.main([str(relish), str(self.work / "cluster"), "bench", "node-01", str(self.work)])
        self.assertIn("--root-backup", self.calls())

    def test_each_bench_and_memory_script_initialises_through_the_helper(self):
        for script in ("job_bench_rounds.sh", "profiling/profile_session.sh", "idle_memory.py"):
            text = (HERE / script).read_text()
            with self.subTest(script=script):
                self.assertIn("relish_init", text)
                self.assertNotIn("relish init cluster", text)

    def test_the_script_is_executable(self):
        mode = (HERE / "profiling" / "relish_init.py").stat().st_mode
        self.assertTrue(mode & stat.S_IXUSR)


if __name__ == "__main__":
    unittest.main()
