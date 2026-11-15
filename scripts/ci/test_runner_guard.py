"""protect-runner and watch-runner keep the GitHub runner alive and watched
while the Ubuntu 26.04 privileged tests run (#1943)."""
import os
import subprocess
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
PROTECT = HERE / "protect-runner"
WATCH = HERE / "watch-runner"


class FakeHost:
    """A stand-in /proc and a sudo and dmesg on PATH, so neither script needs root."""

    def __init__(self, directory):
        self.root = Path(directory)
        self.proc = self.root / "proc"
        self.bin = self.root / "bin"
        self.proc.mkdir()
        self.bin.mkdir()
        (self.proc / "meminfo").write_text("MemTotal: 16000 kB\nMemAvailable: 4096 kB\n")
        (self.proc / "loadavg").write_text("3.50 2.00 1.00 1/200 999\n")
        self.kernel_log = self.root / "kernel-log"
        self.kernel_log.write_text("")
        self.write_tool("sudo", 'if [ "$1" = -n ]; then shift; fi\nexec "$@"\n')
        self.write_tool("dmesg", f'cat "{self.kernel_log}"\n')

    def write_tool(self, name, body):
        tool = self.bin / name
        tool.write_text("#!/bin/sh\n" + body)
        tool.chmod(0o755)

    def process(self, pid, comm):
        directory = self.proc / str(pid)
        directory.mkdir()
        (directory / "comm").write_text(comm + "\n")
        (directory / "oom_score_adj").write_text("0\n")
        return directory

    def run(self, script, *arguments, **extra):
        environment = dict(os.environ, PATH=f"{self.bin}:{os.environ['PATH']}",
                           PROC_ROOT=str(self.proc), **extra)
        return subprocess.run([str(script), *arguments], env=environment,
                              capture_output=True, text=True, timeout=30)


class ProtectRunnerTests(unittest.TestCase):
    def test_only_the_runners_processes_get_minus_1000_and_are_recorded(self):
        with tempfile.TemporaryDirectory() as directory:
            host = FakeHost(directory)
            listener = host.process(101, "Runner.Listener")
            worker = host.process(102, "Runner.Worker")
            provisioner = host.process(103, "provjobd")
            tests = host.process(104, "bash")
            pids = host.root / "runner-pids"
            result = host.run(PROTECT, str(pids))
            self.assertEqual(result.returncode, 0, result.stderr)
            for runner in (listener, worker, provisioner):
                self.assertEqual((runner / "oom_score_adj").read_text().strip(), "-1000")
            self.assertEqual((tests / "oom_score_adj").read_text().strip(), "0")
            self.assertEqual(pids.read_text().splitlines(),
                             ["Runner.Listener 101", "Runner.Worker 102", "provjobd 103"])
            self.assertIn("protected Runner.Worker (pid 102): oom_score_adj -1000", result.stdout)

    def test_a_missing_or_unprotectable_process_is_logged_never_fatal(self):
        with tempfile.TemporaryDirectory() as directory:
            host = FakeHost(directory)
            host.process(102, "Runner.Worker")
            host.write_tool("sudo", "exit 1\n")
            pids = host.root / "runner-pids"
            result = host.run(PROTECT, str(pids))
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("could not protect Runner.Worker (pid 102)", result.stdout)
            self.assertIn("no Runner.Listener process found", result.stdout)
            self.assertNotIn("no provjobd process found", result.stdout)

    def test_a_missing_provjobd_is_expected_and_the_process_list_is_printed_once(self):
        # Ubuntu 26.04 runners have no provjobd (#1948's run); the list says
        # what their helpers are called instead.
        with tempfile.TemporaryDirectory() as directory:
            host = FakeHost(directory)
            host.process(102, "Runner.Worker")
            host.write_tool("ps", 'echo "ps $*"\n')
            result = host.run(PROTECT, str(host.root / "runner-pids"))
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("info: no provjobd process; Ubuntu 26.04 runners have none", result.stdout)
            self.assertEqual(result.stdout.count("ps -eo pid,comm,args"), 1, result.stdout)

    def test_it_refuses_to_run_without_a_pid_file(self):
        result = subprocess.run([str(PROTECT)], capture_output=True, text=True)
        self.assertEqual(result.returncode, 2)
        self.assertIn("usage", result.stderr)


class WatchRunnerTests(unittest.TestCase):
    def watch(self, host, rounds, kernel_lines_per_round=()):
        """Run the watcher, appending one batch of kernel lines before each round."""
        pids = host.root / "runner-pids"
        pids.write_text("Runner.Listener 101\nRunner.Worker 102\n")
        # The fake dmesg appends the next batch each time it runs.
        batches = host.root / "batches"
        batches.mkdir()
        for index, lines in enumerate(kernel_lines_per_round):
            (batches / str(index)).write_text("".join(line + "\n" for line in lines))
        host.write_tool("dmesg", f"""
count=$(cat "{host.root}/dmesg-calls" 2>/dev/null || echo 0)
if [ -f "{batches}/$count" ]; then cat "{batches}/$count" >> "{host.kernel_log}"; fi
echo $((count + 1)) > "{host.root}/dmesg-calls"
cat "{host.kernel_log}"
""")
        result = host.run(WATCH, str(pids), WATCH_INTERVAL="0", WATCH_ROUNDS=str(rounds))
        self.assertEqual(result.returncode, 0, result.stderr)
        return result.stdout.splitlines()

    def test_each_line_keeps_the_resource_fields_and_says_which_runner_processes_live(self):
        with tempfile.TemporaryDirectory() as directory:
            host = FakeHost(directory)
            host.process(101, "Runner.Listener")
            lines = self.watch(host, 1)
            self.assertEqual(len(lines), 1, lines)
            # The report sorts on the second and third `=` fields.
            fields = lines[0].split("=")
            self.assertTrue(fields[0].endswith(" disk_kb"), lines[0])
            self.assertTrue(fields[1].endswith(" mem_kb"), lines[0])
            self.assertTrue(fields[2].startswith("4096 load"), lines[0])
            self.assertIn("load=3.50", lines[0])
            self.assertIn("runners=Runner.Listener:101:up,Runner.Worker:102:gone", lines[0])

    def test_only_new_kernel_oom_lines_are_logged_once_each(self):
        with tempfile.TemporaryDirectory() as directory:
            host = FakeHost(directory)
            lines = self.watch(host, 3, [
                ["[Sat Nov 14 19:00:00 2026] eth0: link up"],
                ["[Sat Nov 14 19:00:05 2026] bun invoked oom-killer: gfp_mask=0x140cca",
                 "[Sat Nov 14 19:00:05 2026] Out of memory: Killed process 4242 (Runner.Worker)"],
                [],
            ])
            kernel = [line for line in lines if line.startswith("kernel: ")]
            self.assertEqual(kernel, [
                "kernel: [Sat Nov 14 19:00:05 2026] bun invoked oom-killer: gfp_mask=0x140cca",
                "kernel: [Sat Nov 14 19:00:05 2026] Out of memory: Killed process 4242 (Runner.Worker)",
            ])
            self.assertEqual(len([line for line in lines if " disk_kb=" in line]), 3)


if __name__ == "__main__":
    unittest.main()
