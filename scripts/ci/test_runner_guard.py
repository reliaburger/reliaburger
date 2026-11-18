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
HEADROOM = HERE / "assert-headroom"
FREE_DISK = HERE / "free-runner-disk"


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

    def test_under_the_disk_floor_it_stops_the_named_process_once_and_says_so(self):
        # A full / takes the hosted runner with it and no log survives (18
        # Nov 2026). Stopping the tests first leaves a failed step instead.
        with tempfile.TemporaryDirectory() as directory:
            host = FakeHost(directory)
            host.write_tool("df", 'printf "Avail\\n1500000\\n"\n')
            host.write_tool("pkill", f'echo "$*" >> "{host.root}/pkill-calls"\n')
            pids = host.root / "runner-pids"
            pids.write_text("")
            result = host.run(WATCH, str(pids), WATCH_INTERVAL="0", WATCH_ROUNDS="3",
                              WATCH_DISK_FLOOR_KB="2097152", WATCH_STOP="cargo-nextest")
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual((host.root / "pkill-calls").read_text(), "-TERM -x cargo-nextest\n")
            floor = [line for line in result.stdout.splitlines() if line.startswith("disk_floor: ")]
            self.assertEqual(len(floor), 1, result.stdout)
            self.assertIn("1500000 kB free on / is under 2097152 kB: stopping cargo-nextest", floor[0])

    def test_without_a_floor_it_never_stops_anything(self):
        with tempfile.TemporaryDirectory() as directory:
            host = FakeHost(directory)
            host.write_tool("df", 'printf "Avail\\n1500000\\n"\n')
            host.write_tool("pkill", f'echo "$*" >> "{host.root}/pkill-calls"\n')
            pids = host.root / "runner-pids"
            pids.write_text("")
            result = host.run(WATCH, str(pids), WATCH_INTERVAL="0", WATCH_ROUNDS="2")
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertFalse((host.root / "pkill-calls").exists())
            self.assertNotIn("disk_floor", result.stdout)


class AssertHeadroomTests(unittest.TestCase):
    """The report step fails while there is still disk left to say so."""

    LOG = (
        "12:04:19 disk_kb=76792508 mem_kb=14801980 load=1.41 runners=Runner.Worker:2012:up\n"
        "12:06:15 disk_kb=75917720 mem_kb=1505772 load=3.68 runners=Runner.Worker:2012:up\n"
        "kernel: [Wed Nov 18 12:10:00 2026] eth0: oom test line\n"
        "12:30:34 disk_kb=5321696 mem_kb=15133296 load=3.96 runners=Runner.Worker:2012:up\n"
    )

    def check(self, text, minimum_kb):
        with tempfile.TemporaryDirectory() as directory:
            log = Path(directory) / "resources.log"
            log.write_text(text)
            return subprocess.run([str(HEADROOM), str(log), str(minimum_kb)],
                                  capture_output=True, text=True, timeout=30)

    def test_it_prints_the_lowest_disk_and_memory_samples_and_passes_above_the_floor(self):
        result = self.check(self.LOG, 5 * 1024 * 1024)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("lowest free disk: 5.1 GiB at 12:30:34", result.stdout)
        self.assertIn("lowest free memory: 1.4 GiB at 12:06:15", result.stdout)
        self.assertEqual(result.stderr, "")

    def test_a_low_water_mark_under_the_floor_fails_with_an_annotation(self):
        # The passing run of #2100 (44293229949) bottomed out at 5.1 GiB.
        result = self.check(self.LOG, 10 * 1024 * 1024)
        self.assertEqual(result.returncode, 1)
        self.assertIn("::error::free disk on / fell to 5.1 GiB at 12:30:34, under the 10.0 GiB floor",
                      result.stdout)

    def test_a_watcher_stop_is_reported_even_if_disk_came_back(self):
        log = self.LOG + "disk_floor: 1500000 kB free on / is under 2097152 kB: stopping cargo-nextest\n"
        result = self.check(log, 5 * 1024 * 1024)
        self.assertEqual(result.returncode, 1)
        self.assertIn("::error::the watcher stopped the run: disk_floor: 1500000 kB", result.stdout)

    def test_no_samples_is_a_failure_not_a_pass(self):
        result = self.check("", 1)
        self.assertEqual(result.returncode, 1)
        self.assertIn("::error::no disk samples", result.stdout)

    def test_it_refuses_to_run_without_its_arguments(self):
        result = subprocess.run([str(HEADROOM)], capture_output=True, text=True)
        self.assertEqual(result.returncode, 2)
        self.assertIn("usage", result.stderr)


class FreeRunnerDiskTests(unittest.TestCase):
    """The preinstalled toolchains no job of ours uses go; everything else stays."""

    def test_it_removes_the_unused_toolchains_and_keeps_the_rest(self):
        with tempfile.TemporaryDirectory() as directory:
            host = FakeHost(directory)
            root = host.root / "image"
            doomed = ["usr/local/lib/android", "usr/share/dotnet", "opt/ghc", "usr/local/.ghcup",
                      "opt/hostedtoolcache/CodeQL", "usr/share/swift", "usr/local/share/powershell"]
            kept = ["opt/hostedtoolcache/Python", "usr/bin", "home/runner/.cargo"]
            for path in doomed + kept:
                (root / path).mkdir(parents=True)
                (root / path / "file").write_text("x")
            host.write_tool("df", 'echo "df $*"\n')
            result = host.run(FREE_DISK, FREE_ROOT=str(root))
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            for path in doomed:
                self.assertFalse((root / path).exists(), path)
            for path in kept:
                self.assertTrue((root / path / "file").exists(), path)
            self.assertIn("removed /usr/local/lib/android", result.stdout)
            self.assertEqual(result.stdout.count("df -h /"), 2, result.stdout)

    def test_a_path_the_image_lacks_is_skipped_never_fatal(self):
        with tempfile.TemporaryDirectory() as directory:
            host = FakeHost(directory)
            root = host.root / "image"
            root.mkdir()
            host.write_tool("df", "true\n")
            result = host.run(FREE_DISK, FREE_ROOT=str(root))
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertIn("absent /opt/ghc", result.stdout)


if __name__ == "__main__":
    unittest.main()
