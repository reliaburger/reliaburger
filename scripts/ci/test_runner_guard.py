"""protect-runner and watch-runner keep the GitHub runner alive and watched
while the Ubuntu 26.04 privileged tests run (#1943)."""
import os
import subprocess
import tempfile
import time
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
PROTECT = HERE / "protect-runner"
WATCH = HERE / "watch-runner"
HEADROOM = HERE / "assert-headroom"
FREE_DISK = HERE / "free-runner-disk"
GIVE_SWAP = HERE / "give-swap"
SNAPSHOT = HERE / "runner-snapshot"
WATCH_MACOS = HERE / "watch-macos-memory"


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

    def meminfo(self, available, swap_total, swap_free):
        (self.proc / "meminfo").write_text(
            f"MemTotal: 16000000 kB\nMemAvailable: {available} kB\n"
            f"SwapTotal: {swap_total} kB\nSwapFree: {swap_free} kB\n")

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
            host.write_tool("pgrep", 'echo 4242\necho 4243\n')
            pids = host.root / "runner-pids"
            pids.write_text("")
            result = host.run(WATCH, str(pids), WATCH_INTERVAL="0", WATCH_ROUNDS="3",
                              WATCH_DISK_FLOOR_KB="2097152", WATCH_STOP="cargo-nextest")
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual((host.root / "pkill-calls").read_text(), "-TERM -x cargo-nextest\n")
            floor = [line for line in result.stdout.splitlines() if line.startswith("disk_floor: ")]
            self.assertEqual(len(floor), 1, result.stdout)
            # The count shows whether the name matched anything at all.
            self.assertIn("1500000 kB free on / is under 2097152 kB: stopping cargo-nextest (2 processes)",
                          floor[0])

    def test_a_stop_name_that_matches_nothing_says_zero_processes(self):
        with tempfile.TemporaryDirectory() as directory:
            host = FakeHost(directory)
            host.write_tool("df", 'printf "Avail\\n1500000\\n"\n')
            host.write_tool("pkill", "exit 1\n")
            host.write_tool("pgrep", "exit 1\n")
            pids = host.root / "runner-pids"
            pids.write_text("")
            result = host.run(WATCH, str(pids), WATCH_INTERVAL="0", WATCH_ROUNDS="1",
                              WATCH_DISK_FLOOR_KB="2097152", WATCH_STOP="cargo-nextest")
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("stopping cargo-nextest (0 processes)", result.stdout)

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

    def test_each_line_also_says_how_much_swap_is_in_use_and_the_memory_stall(self):
        # A lost run thrashes before it goes: swap in use and the kernel's
        # "full" memory pressure (every task stalled on memory) say so.
        with tempfile.TemporaryDirectory() as directory:
            host = FakeHost(directory)
            host.meminfo(available=4096, swap_total=3145728, swap_free=1048576)
            (host.proc / "pressure").mkdir()
            (host.proc / "pressure" / "memory").write_text(
                "some avg10=40.00 avg60=20.00 avg300=5.00 total=1\n"
                "full avg10=31.50 avg60=12.00 avg300=3.00 total=1\n")
            lines = self.watch(host, 1)
            self.assertIn(" swap_kb=2097152 ", lines[0])
            self.assertIn(" psi_full=31.50 ", lines[0])

    def memory_floor(self, host, rounds, **extra):
        host.write_tool("pkill", f'echo "$*" >> "{host.root}/pkill-calls"\n')
        host.write_tool("pgrep", 'echo 4242\n')
        pids = host.root / "runner-pids"
        pids.write_text("")
        settings = dict(WATCH_INTERVAL="0", WATCH_ROUNDS=str(rounds),
                        WATCH_MEM_FLOOR_KB="524288", WATCH_MEM_FLOOR_SECONDS="0",
                        WATCH_STOP="cargo-nextest cargo rustc")
        settings.update(extra)
        result = host.run(WATCH, str(pids), **settings)
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = host.root / "pkill-calls"
        return result.stdout, calls.read_text() if calls.exists() else ""

    def test_memory_held_under_the_floor_with_swap_in_use_stops_the_build_once(self):
        # On a 16 GB runner the instrumented build's rustc peaks can push the
        # host into swap. The runner is shielded from the OOM killer, so
        # nothing dies: its heartbeat stalls and GitHub drops it with no log
        # (18 Nov 2026). Stopping the build first leaves a failed step.
        with tempfile.TemporaryDirectory() as directory:
            host = FakeHost(directory)
            host.meminfo(available=400000, swap_total=3145728, swap_free=1048576)
            output, calls = self.memory_floor(host, 3)
            self.assertEqual(calls, "-TERM -x cargo-nextest\n-TERM -x cargo\n-TERM -x rustc\n")
            floor = [line for line in output.splitlines() if line.startswith("mem_floor: ")]
            self.assertEqual(len(floor), 1, output)
            # The window is 0 s; a second can tick over inside the round.
            self.assertRegex(floor[0], r"^mem_floor: 400000 kB available is under 524288 kB for [01] s "
                                       r"with 2097152 kB swap in use: stopping cargo-nextest cargo rustc "
                                       r"\(3 processes\)$")

    def test_a_dip_shorter_than_the_window_stops_nothing(self):
        with tempfile.TemporaryDirectory() as directory:
            host = FakeHost(directory)
            host.meminfo(available=400000, swap_total=3145728, swap_free=1048576)
            output, calls = self.memory_floor(host, 3, WATCH_MEM_FLOOR_SECONDS="3600")
            self.assertEqual(calls, "")
            self.assertNotIn("mem_floor", output)

    def test_low_memory_without_swap_in_use_is_left_to_the_oom_killer(self):
        # With no swap in use the host can't thrash: the kernel kills the
        # biggest rustc, which fails the step with a log.
        with tempfile.TemporaryDirectory() as directory:
            host = FakeHost(directory)
            host.meminfo(available=400000, swap_total=3145728, swap_free=3145728)
            output, calls = self.memory_floor(host, 3)
            self.assertEqual(calls, "")
            self.assertNotIn("mem_floor", output)


    def memory_stall(self, host, rounds, full, **extra):
        (host.proc / "pressure").mkdir(exist_ok=True)
        (host.proc / "pressure" / "memory").write_text(
            f"some avg10=99.00 avg60=50.00 avg300=5.00 total=1\n"
            f"full avg10={full} avg60=40.00 avg300=3.00 total=1\n")
        settings = dict(WATCH_PSI_FULL_PERCENT="60", WATCH_PSI_SECONDS="0")
        settings.update(extra)
        return self.memory_floor(host, rounds, **settings)

    def test_a_memory_stall_held_over_the_limit_stops_the_build_once_swap_or_not(self):
        # 20 Nov 2026 (run 44648854969): the 26.04 job's build paged, and
        # every task stalled on memory, with 5 GiB still "available" and no
        # memory floor crossed. Passing runs peak at 49% for ten seconds.
        with tempfile.TemporaryDirectory() as directory:
            host = FakeHost(directory)
            host.meminfo(available=5000000, swap_total=0, swap_free=0)
            output, calls = self.memory_stall(host, 3, "72.10")
            self.assertEqual(calls, "-TERM -x cargo-nextest\n-TERM -x cargo\n-TERM -x rustc\n")
            stall = [line for line in output.splitlines() if line.startswith("mem_stall: ")]
            self.assertEqual(len(stall), 1, output)
            self.assertRegex(stall[0], r"^mem_stall: every task waited on memory 72\.10% of the last 10 s, "
                                       r"at or over 60% for [01] s: stopping cargo-nextest cargo rustc "
                                       r"\(3 processes\)$")

    def test_a_stall_shorter_than_its_window_or_under_the_limit_stops_nothing(self):
        for full, window in (("72.10", "3600"), ("49.25", "0")):
            with self.subTest(full=full, window=window), tempfile.TemporaryDirectory() as directory:
                host = FakeHost(directory)
                output, calls = self.memory_stall(host, 3, full, WATCH_PSI_SECONDS=window)
                self.assertEqual(calls, "")
                self.assertNotIn("mem_stall", output)

    def test_without_a_stall_limit_a_stall_stops_nothing(self):
        with tempfile.TemporaryDirectory() as directory:
            host = FakeHost(directory)
            output, calls = self.memory_stall(host, 3, "99.00", WATCH_PSI_FULL_PERCENT="")
            self.assertEqual(calls, "")
            self.assertNotIn("mem_stall", output)

    def test_a_stop_reaches_processes_that_run_as_root(self):
        # The 26.04 job runs its build and tests as root; the watcher runs
        # as the runner's user, which can't signal them by itself.
        with tempfile.TemporaryDirectory() as directory:
            host = FakeHost(directory)
            host.write_tool("sudo", f'echo "$*" >> "{host.root}/sudo-calls"\n'
                                    'if [ "$1" = -n ]; then shift; fi\nexec "$@"\n')
            host.meminfo(available=400000, swap_total=3145728, swap_free=1048576)
            _, calls = self.memory_floor(host, 1, WATCH_STOP="rustc")
            self.assertEqual(calls, "-TERM -x rustc\n")
            self.assertIn("-n pkill -TERM -x rustc\n", (host.root / "sudo-calls").read_text())


class RunnerSnapshotTests(unittest.TestCase):
    """One dump of what a stuck job is doing, while it is still doing it.

    On 20 Nov 2026 three privileged jobs ran into their limits and GitHub
    kept no log (runs 44729365098, 44778442800, 44783868580). A step's own
    timeout kills the hung processes before a report step can look at them,
    so the watcher takes this snapshot a few minutes before that."""

    PS_SELECT = (
        "  1 Ss   /sbin/init\n"
        "  4242 Dl  /home/runner/work/x/target/debug/deps/owned_rootless-0a1b --exact "
        "rootless_published_port_survives_bun_replacement --nocapture\n"
        "  4300 Sl  /usr/bin/cargo-nextest nextest run --profile ci\n"
        "  4400 D   [kworker/u8:3+flush-8:0]\n"
        "  4500 S   bash\n"
    )

    def host(self, directory):
        host = FakeHost(directory)
        host.meminfo(available=812000, swap_total=3145728, swap_free=1048576)
        (host.proc / "pressure").mkdir()
        for name in ("memory", "io", "cpu"):
            (host.proc / "pressure" / name).write_text(
                f"some avg10=1.00 avg60=0.50 avg300=0.10 total=1 {name}\n")
        for pid, comm in ((4242, "owned_rootless-0a1b"), (4400, "kworker/u8:3"), (4500, "bash")):
            task = host.process(pid, comm) / "task" / str(pid)
            task.mkdir(parents=True)
            (task / "comm").write_text(comm + "\n")
            (task / "wchan").write_text("do_wait\n")
            (task / "stack").write_text(f"[<0>] stack-of-{pid}\n")
        select = host.root / "ps-select"
        select.write_text(self.PS_SELECT)
        host.write_tool("ps", f"""
case "$*" in
*pid=,stat=,args=*) cat "{select}" ;;
*etimes=*) cat "{select}" ;;
*) echo "PS FOREST $*" ;;
esac
""")
        host.write_tool("free", 'echo "FREE $*"\n')
        host.write_tool("swapon", 'echo "SWAPON $*"\n')
        host.write_tool("df", 'echo "DF $*"\n')
        host.kernel_log.write_text("".join(f"kernel line {n}\n" for n in range(300)))
        return host

    def test_it_dumps_memory_pressure_the_running_tests_and_the_processes(self):
        with tempfile.TemporaryDirectory() as directory:
            host = self.host(directory)
            result = host.run(SNAPSHOT)
            self.assertEqual(result.returncode, 0, result.stderr)
            out = result.stdout
            self.assertIn("=== runner snapshot at ", out)
            for text in ("MemAvailable: 812000 kB", "SwapFree: 1048576 kB", "FREE ", "SWAPON ",
                         "DF ", "some avg10=1.00 avg60=0.50 avg300=0.10 total=1 memory",
                         "total=1 io", "total=1 cpu", "PS FOREST "):
                self.assertIn(text, out)
            # The running tests are nextest's progress: which ones, for how long.
            tests = out.split("--- running test processes", 1)[1].split("\n--- ", 1)[0]
            self.assertIn("rootless_published_port_survives_bun_replacement", tests)
            self.assertNotIn("bash", tests)

    def test_it_prints_kernel_stacks_of_stuck_and_test_processes_only(self):
        with tempfile.TemporaryDirectory() as directory:
            host = self.host(directory)
            out = host.run(SNAPSHOT).stdout
            self.assertIn("[<0>] stack-of-4242", out)
            self.assertIn("[<0>] stack-of-4400", out)
            self.assertNotIn("stack-of-4500", out)
            self.assertIn("(owned_rootless-0a1b, do_wait)", out)

    def test_it_keeps_the_end_of_the_kernel_log_and_of_each_log_it_is_given(self):
        with tempfile.TemporaryDirectory() as directory:
            host = self.host(directory)
            log = host.root / "tests.log"
            log.write_text("".join(f"test line {n}\n" for n in range(500)))
            out = host.run(SNAPSHOT, str(log), str(host.root / "missing.log")).stdout
            self.assertIn("kernel line 299", out)
            self.assertNotIn("kernel line 99\n", out)
            self.assertIn(f"--- last lines of {log}", out)
            self.assertIn("test line 499", out)
            self.assertNotIn("test line 399\n", out)
            self.assertIn("missing.log: not there", out)

    def test_a_host_without_the_tools_still_gets_a_snapshot_and_a_zero_exit(self):
        with tempfile.TemporaryDirectory() as directory:
            host = FakeHost(directory)
            for tool in ("ps", "free", "swapon", "df", "dmesg", "sudo"):
                host.write_tool(tool, "exit 1\n")
            result = host.run(SNAPSHOT)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("=== runner snapshot at ", result.stdout)


class WatchRunnerSnapshotTests(unittest.TestCase):
    """The watcher takes snapshots from a set age on, each to its own file,
    before a step's own timeout kills what they would show."""

    def run_watch(self, host, **extra):
        pids = host.root / "runner-pids"
        pids.write_text("")
        snapshots = host.root / "snapshots"
        snapshots.mkdir()
        # A stand-in for runner-snapshot beside a copy of the watcher.
        scripts = host.root / "scripts"
        scripts.mkdir()
        watch = scripts / "watch-runner"
        watch.write_text(WATCH.read_text())
        watch.chmod(0o755)
        fake = scripts / "runner-snapshot"
        fake.write_text('#!/bin/sh\necho "snapshot of $*"\n')
        fake.chmod(0o755)
        settings = dict(WATCH_INTERVAL="0", WATCH_SNAPSHOT_DIR=str(snapshots))
        settings.update(extra)
        result = host.run(watch, str(pids), **settings)
        self.assertEqual(result.returncode, 0, result.stderr)
        return result.stdout, snapshots

    def written(self, snapshots, count):
        """The snapshot files, once `count` of them have content (they're written in the background)."""
        for _ in range(50):
            files = sorted(snapshots.iterdir())
            if len(files) >= count and all(path.read_text() for path in files):
                break
            time.sleep(0.1)
        return sorted(snapshots.iterdir())

    def test_past_the_set_age_it_writes_one_snapshot_and_says_where(self):
        with tempfile.TemporaryDirectory() as directory:
            host = FakeHost(directory)
            out, snapshots = self.run_watch(host, WATCH_ROUNDS="3", WATCH_SNAPSHOT_AFTER="0",
                                            WATCH_SNAPSHOT_LOGS="/tmp/a.log /tmp/b.log")
            files = self.written(snapshots, 1)
            self.assertEqual([path.name for path in files], ["snapshot-0s.txt"])
            self.assertEqual(files[0].read_text(), "snapshot of /tmp/a.log /tmp/b.log\n")
            said = [line for line in out.splitlines() if line.startswith("snapshot: ")]
            self.assertEqual(said, [f"snapshot: writing {files[0]}"])

    def test_with_a_period_it_takes_one_per_period_each_in_its_own_file(self):
        # A later snapshot must never overwrite the one taken before a kill.
        with tempfile.TemporaryDirectory() as directory:
            host = FakeHost(directory)
            out, snapshots = self.run_watch(host, WATCH_INTERVAL="1", WATCH_ROUNDS="4",
                                            WATCH_SNAPSHOT_AFTER="0", WATCH_SNAPSHOT_EVERY="2")
            names = [path.name for path in self.written(snapshots, 2)]
            self.assertEqual(names[:2], ["snapshot-0s.txt", "snapshot-2s.txt"])
            self.assertLessEqual(len(names), 3)
            self.assertEqual(len([line for line in out.splitlines() if line.startswith("snapshot: ")]),
                             len(names))

    def test_before_the_set_age_or_without_one_it_takes_none(self):
        for extra in (dict(WATCH_SNAPSHOT_AFTER="3600"), dict()):
            with self.subTest(extra=extra), tempfile.TemporaryDirectory() as directory:
                host = FakeHost(directory)
                out, snapshots = self.run_watch(host, WATCH_ROUNDS="2", **extra)
                time.sleep(0.3)
                self.assertEqual(list(snapshots.iterdir()), [])
                self.assertNotIn("snapshot:", out)

class WatchMacosMemoryTests(unittest.TestCase):
    """watch-macos-memory says whether portable macOS's build paged (#1723)."""

    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.host = FakeHost(self.directory.name)
        self.host.write_tool("sysctl", 'case "$2" in\n'
                             '  vm.loadavg) echo "{ 2.75 2.10 1.50 }" ;;\n'
                             '  vm.swapusage) echo "total = 3072.00M  used = 1203.25M  free = 1868.75M  (encrypted)" ;;\n'
                             'esac\n')
        self.host.write_tool("memory_pressure", 'echo "The system has 7516192768 (1835008 pages with a page size of 4096)."\n'
                             'echo "System-wide memory free percentage: 12%"\n')
        self.host.write_tool("vm_stat", 'echo "Pageouts:                                    10."\n'
                             'echo "Swapouts:                                  4242."\n')
        self.host.write_tool("ps", 'echo "  10240 /usr/bin/python3"\n'
                             'echo "9437184 /Users/runner/.rustup/toolchains/1.98.0/bin/rustc"\n')

    def tearDown(self):
        self.directory.cleanup()

    def test_each_line_says_the_load_free_memory_swap_and_the_biggest_process(self):
        result = self.host.run(WATCH_MACOS, "0", "2")
        self.assertEqual(result.returncode, 0, result.stderr)
        lines = result.stdout.splitlines()
        self.assertEqual(len(lines), 2)
        self.assertRegex(lines[0], r"^\d\d:\d\d:\d\d load=2.75 free=12% swap_used=1203.25M "
                                   r"swapouts=4242 top=rustc:9216M$")

    def test_a_missing_tool_leaves_its_field_empty_and_never_stops_it(self):
        for tool in ("sysctl", "memory_pressure", "vm_stat"):
            self.host.write_tool(tool, "exit 1\n")
        result = self.host.run(WATCH_MACOS, "0", "1")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertRegex(result.stdout, r"load= free= swap_used= swapouts= top=rustc:9216M\n$")

    def test_the_summary_names_the_lines_with_the_most_swap_and_the_biggest_process(self):
        log = Path(self.directory.name) / "macos.log"
        log.write_text("10:00:00 load=1.0 free=40% swap_used=0.00M swapouts=0 top=rustc:4000M\n"
                       "10:00:15 load=2.9 free=3% swap_used=2950.50M swapouts=900 top=rustc:9000M\n"
                       "10:00:30 load=2.1 free=9% swap_used=1500.00M swapouts=950 top=rustc:11000M\n")
        result = self.host.run(WATCH_MACOS, "--summary", str(log))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines(), [
            "samples: 3",
            "most swap: 10:00:15 load=2.9 free=3% swap_used=2950.50M swapouts=900 top=rustc:9000M",
            "biggest process: 10:00:30 load=2.1 free=9% swap_used=1500.00M swapouts=950 top=rustc:11000M",
        ])

    def test_the_summary_of_a_missing_log_reports_nothing_and_passes(self):
        result = self.host.run(WATCH_MACOS, "--summary", str(Path(self.directory.name) / "absent.log"))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines(), ["samples: 0", "most swap: ", "biggest process: "])


class GiveSwapTests(unittest.TestCase):
    """Swap for the build's peak, but never through a device-mapper snapshot."""

    def host(self, directory, source, swap_total):
        host = FakeHost(directory)
        host.meminfo(available=14000000, swap_total=swap_total, swap_free=swap_total)
        host.write_tool("findmnt", f'echo "$*" >> "{host.root}/findmnt-calls"\necho "{source}"\n')
        for tool in ("fallocate", "chmod", "mkswap", "swapon"):
            host.write_tool(tool, f'echo "{tool} $*" >> "{host.root}/swap-calls"\n')
        return host

    def give(self, host, *arguments):
        result = host.run(GIVE_SWAP, *arguments)
        calls = host.root / "swap-calls"
        return result, calls.read_text() if calls.exists() else ""

    def test_it_tops_the_swap_up_with_a_file_in_the_directory(self):
        with tempfile.TemporaryDirectory() as directory:
            host = self.host(directory, "/dev/sdb1", 3145728)
            result, calls = self.give(host, "8388608", "/mnt")
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(calls, "fallocate -l 5242880K /mnt/ci-swapfile\n"
                                    "chmod 600 /mnt/ci-swapfile\n"
                                    "mkswap /mnt/ci-swapfile\n"
                                    "swapon /mnt/ci-swapfile\n")
            self.assertIn("--target /mnt", (host.root / "findmnt-calls").read_text())

    def test_it_keeps_the_image_s_swap_when_that_is_enough(self):
        with tempfile.TemporaryDirectory() as directory:
            host = self.host(directory, "/dev/root", 8388608)
            result, calls = self.give(host, "8388608", "/")
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(calls, "")

    def test_it_never_puts_swap_on_a_device_mapper_disk(self):
        # 20 Nov 2026 (run 44648854969): a 26.04 runner whose / was
        # /dev/mapper/cow-root, a dm-snapshot, got an 8 GiB swap file there.
        # When the build paged, writeback waited on the snapshot's copy-on-
        # write (wait_for_in_progress, kcopyd) and the host froze for 34
        # minutes, the watcher with it.
        for source in ("/dev/mapper/cow-root", "/dev/dm-0"):
            with self.subTest(source=source), tempfile.TemporaryDirectory() as directory:
                host = self.host(directory, source, 0)
                result, calls = self.give(host, "8388608", "/")
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(calls, "")
                self.assertIn(f"no swap file: / is on {source}, a device-mapper device", result.stdout)

    def test_it_refuses_to_run_without_its_arguments(self):
        result = subprocess.run([str(GIVE_SWAP), "8388608"], capture_output=True, text=True)
        self.assertEqual(result.returncode, 2)
        self.assertIn("usage", result.stderr)


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

    def test_a_memory_floor_stop_fails_the_report(self):
        log = self.LOG + ("mem_floor: 400000 kB available is under 524288 kB for 120 s with 2097152 kB "
                          "swap in use: stopping cargo-nextest cargo rustc (5 processes)\n")
        result = self.check(log, 5 * 1024 * 1024)
        self.assertEqual(result.returncode, 1)
        self.assertIn("::error::the watcher stopped the run: mem_floor: 400000 kB available", result.stdout)

    def test_a_memory_stall_stop_fails_the_report(self):
        log = self.LOG + ("mem_stall: every task waited on memory 72.10% of the last 10 s, at or over 60% "
                          "for 30 s: stopping cargo-nextest cargo rustc (5 processes)\n")
        result = self.check(log, 5 * 1024 * 1024)
        self.assertEqual(result.returncode, 1)
        self.assertIn("::error::the watcher stopped the run: mem_stall: every task waited", result.stdout)

    def test_it_reports_the_most_swap_in_use_and_the_worst_memory_stall(self):
        log = (
            "12:04:19 disk_kb=76792508 mem_kb=14801980 load=1.41 swap_kb=0 psi_full=0.00 runners=none\n"
            "12:06:15 disk_kb=75917720 mem_kb=1505772 load=3.68 swap_kb=2621440 psi_full=12.25 runners=none\n"
            "12:07:15 disk_kb=75917720 mem_kb=1605772 load=3.68 swap_kb=1048576 psi_full=40.10 runners=none\n"
        )
        result = self.check(log, 5 * 1024 * 1024)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("most swap in use: 2.5 GiB at 12:06:15", result.stdout)
        self.assertIn("worst full memory stall (avg10): 40.10% at 12:07:15", result.stdout)

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
