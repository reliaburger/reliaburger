"""The power-cut and reboot driver makes the disposable VM right, runs every lane and never touches the development VM.

`qualify-power-cut-reboot.sh` is release gate step 5 in one command. These
tests run it against a fake `limactl` and stub lane scripts in a throwaway Git
repository, so they check the driver's own decisions: which arguments it
refuses, which VM it creates and from what, the lane order, stopping on the
first failure, the summary's rule-of-three bounds and the clean-up. The lanes
themselves only run on a real VM (docs/releasing.md, step 5).
"""
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import textwrap
import unittest

HERE = Path(__file__).resolve().parent
DRIVER = HERE / "qualify-power-cut-reboot.sh"
TEMPLATE = HERE / "power-cut-vm.yaml"
SIX_GIB = 6 * 1024 ** 3

# A fake limactl: it logs every call, keeps the YAML it was asked to create a
# VM from, lists FAKE_VMS, and answers the driver's guest check with facts
# the test can spoil one at a time.
FAKE_LIMACTL = textwrap.dedent("""\
    #!/usr/bin/env python3
    import json, os, shutil, sys
    arguments = sys.argv[1:]
    with open(os.environ["FAKE_LIMA_LOG"], "a") as log:
        log.write(json.dumps(arguments) + "\\n")
    if arguments[:1] == ["--version"]:
        print("limactl version 2.1.0")
    elif arguments[:1] == ["list"]:
        for name in os.environ.get("FAKE_VMS", "").split():
            print(name)
    elif arguments[:1] == ["create"]:
        shutil.copy(arguments[-1], os.environ["FAKE_LIMA_CREATED"])
    elif arguments[:1] == ["shell"]:
        sys.stdin.read()
        worktree = arguments[-1]
        facts = {
            "swap_bytes": os.environ.get("FAKE_SWAP_BYTES", str(6 * 1024 ** 3 - 4096)),
            "writable": os.environ.get("FAKE_WRITABLE", "no"),
            "options": os.environ.get("FAKE_OPTIONS", "ro,relatime"),
            "cpus": os.environ.get("FAKE_CPUS", "4"),
            "source": "yes",
            "cargo": "yes",
            "kernel": "6.8.0-142-generic",
        }
        for key, value in facts.items():
            print(f"{key}={value}")
        for mount in os.environ.get("FAKE_MOUNTS", worktree).split(":"):
            print(f"mount={mount}")
    """)

# Stub lanes: each logs its name and arguments, and fails on the call
# FAKE_FAIL names (`LANE:CALL`, counting from 1).
STUB_PRELUDE = textwrap.dedent("""\
    #!/usr/bin/env bash
    set -euo pipefail
    should_fail() {
        local lane=$1 calls
        shift
        echo "$lane $*" >> "$FAKE_LANE_LOG"
        calls=$(grep -c "^$lane " "$FAKE_LANE_LOG")
        [[ ${FAKE_FAIL:-} == "$lane:$calls" ]]
    }
    """)

STUB_STORAGE = STUB_PRELUDE + textwrap.dedent("""\
    fixture= iterations= record=
    while [[ $# -gt 0 ]]; do
        case $1 in
            --fixture) fixture=$2 ;;
            --iterations) iterations=$2 ;;
            --record) record=$2 ;;
        esac
        shift 2
    done
    printf '# Storage power-cut qualification: %s\\n\\n' "$fixture" > "$record"
    if should_fail "storage-$fixture" "--fixture $fixture --iterations $iterations"; then
        printf '| 1 | 3 | `/var/tmp/x` | FAIL | see `/tmp/e/1` |\\n' >> "$record"
        printf '\\n0 of %s iterations passed before the first failure.\\n' "$iterations" >> "$record"
        echo "FAIL: iteration 1" >&2
        exit 1
    fi
    for ((i = 1; i <= iterations; i++)); do
        result=PASS
        [[ ${FAKE_RETRIED:-} != "$fixture:$i" ]] || result='PASS (VM start retried)'
        printf '| %s | 3 | `/var/tmp/x` | %s | `{}` |\\n' "$i" "$result" >> "$record"
    done
    printf '\\n%s of %s iterations passed.\\n' "$iterations" "$iterations" >> "$record"
    echo "PASS: $iterations power cuts"
    """)

STUB_REBOOT = STUB_PRELUDE + textwrap.dedent("""\
    lane=$(basename "$0" .sh)
    lane=${lane#qualify-}
    if should_fail "$lane" "$*"; then
        echo "thread 'main' panicked: lost a hold"
        exit 1
    fi
    echo "test result: ok. 1 passed"
    echo "verified boot: 1234"
    echo "PASS: $lane"
    """)


def git(*arguments, cwd):
    return subprocess.run(["git", *arguments], cwd=cwd, check=True, capture_output=True, text=True).stdout.strip()


class PowerCutRebootTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        # The driver mounts real paths in the VM, so compare resolved ones.
        self.root = Path(os.path.realpath(temp.name))
        self.repository = self.root / "repository"
        release = self.repository / "scripts" / "release"
        release.mkdir(parents=True)
        shutil.copy(DRIVER, release / DRIVER.name)
        shutil.copy(TEMPLATE, release / TEMPLATE.name)
        for name, body in [("qualify-storage-power-cut.sh", STUB_STORAGE),
                           ("qualify-oci-reboot.sh", STUB_REBOOT),
                           ("qualify-discovery-reboot.sh", STUB_REBOOT)]:
            (release / name).write_text(body)
        for path in release.iterdir():
            path.chmod(0o755)
        self.environment = dict(
            os.environ,
            GIT_AUTHOR_NAME="Test", GIT_AUTHOR_EMAIL="test@example.com",
            GIT_COMMITTER_NAME="Test", GIT_COMMITTER_EMAIL="test@example.com",
            GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM="1")
        git("init", "-q", cwd=self.repository)
        subprocess.run(["git", "add", "."], cwd=self.repository, check=True, env=self.environment)
        subprocess.run(["git", "commit", "-qm", "candidate"], cwd=self.repository, check=True,
                       env=self.environment)
        self.commit = git("rev-parse", "HEAD", cwd=self.repository)
        self.short = self.commit[:8]
        self.vm = f"rb-pcut-{self.short}"
        self.temporary = self.root / "tmp"
        self.temporary.mkdir()
        self.worktree = self.temporary / f"reliaburger-pcut-{self.short}"
        bin_directory = self.root / "bin"
        bin_directory.mkdir()
        (bin_directory / "limactl").write_text(FAKE_LIMACTL)
        (bin_directory / "limactl").chmod(0o755)
        self.lima_log = self.root / "lima.log"
        self.lane_log = self.root / "lanes.log"
        self.created = self.root / "created.yaml"
        self.records = self.root / "records" / "2026-10-17-power-cut-reboot"
        self.records.parent.mkdir()
        self.environment.update(
            PATH=f"{bin_directory}{os.pathsep}{os.environ['PATH']}",
            TMPDIR=str(self.temporary),
            FAKE_LIMA_LOG=str(self.lima_log), FAKE_LIMA_CREATED=str(self.created),
            FAKE_LANE_LOG=str(self.lane_log))

    def run_driver(self, *arguments, **environment):
        command = [str(self.repository / "scripts" / "release" / DRIVER.name), *arguments]
        return subprocess.run(command, cwd=self.root, capture_output=True, text=True,
                              env=dict(self.environment, **environment), timeout=120)

    def standard(self, iterations="2", *extra, **environment):
        return self.run_driver("--commit", self.commit, "--iterations", iterations,
                               "--record-dir", str(self.records), *extra, **environment)

    def lima_calls(self):
        if not self.lima_log.exists():
            return []
        return [json.loads(line) for line in self.lima_log.read_text().splitlines()]

    def lanes(self):
        if not self.lane_log.exists():
            return []
        return [line.split()[0] for line in self.lane_log.read_text().splitlines()]

    def summary(self):
        return Path(f"{self.records}.md").read_text()

    def row(self, lane_command):
        """The summary row whose command names this lane, split into cells."""
        for line in self.summary().splitlines():
            if lane_command in line:
                return [cell.strip() for cell in line.strip("|").split("|")]
        self.fail(f"no summary row for {lane_command}")

    def assert_cleaned_up(self):
        self.assertIn(["delete", "--force", self.vm], self.lima_calls())
        self.assertFalse(self.worktree.exists(), "the worktree outlived the run")

    # Arguments

    def test_missing_or_malformed_arguments_are_usage_errors(self):
        for arguments in [(),
                          ("--commit", "HEAD", "--iterations", "2"),
                          ("--commit", "HEAD", "--record-dir", "x"),
                          ("--iterations", "2", "--record-dir", "x"),
                          ("--commit", "HEAD", "--iterations", "0", "--record-dir", "x"),
                          ("--commit", "HEAD", "--iterations", "ten", "--record-dir", "x"),
                          ("--commit", "HEAD", "--iterations", "2", "--record-dir", "x", "--bogus", "y"),
                          ("--commit", "HEAD", "--iterations", "2", "--record-dir")]:
            with self.subTest(arguments=arguments):
                self.assertEqual(self.run_driver(*arguments).returncode, 2)
        self.assertEqual(self.lima_calls(), [])

    def test_unknown_commit_is_refused_before_any_vm(self):
        result = self.standard("2", "--commit", "0" * 40)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("commit", result.stderr)
        self.assertEqual(self.lima_calls(), [])

    def test_existing_record_directory_is_never_overwritten(self):
        self.records.mkdir()
        self.assertEqual(self.standard().returncode, 2)
        self.assertEqual(self.lima_calls(), [])

    # Never the development VM, never a VM it didn't make

    def test_refuses_the_development_vm_without_calling_limactl(self):
        result = self.standard("2", "--vm", "reliaburger-test")
        self.assertEqual(result.returncode, 2)
        self.assertIn("reliaburger-test", result.stderr)
        self.assertEqual(self.lima_calls(), [])
        self.assertEqual(self.lanes(), [])

    def test_refuses_a_vm_that_already_exists(self):
        result = self.standard(FAKE_VMS=f"reliaburger-test {self.vm}")
        self.assertEqual(result.returncode, 2)
        self.assertIn("already exists", result.stderr)
        self.assertFalse(any(call[0] in ("create", "delete", "stop") for call in self.lima_calls()))
        self.assertEqual(self.lanes(), [])

    # The checkout

    def test_refuses_a_dirty_worktree(self):
        git("worktree", "add", "-q", "--detach", str(self.worktree), self.commit, cwd=self.repository)
        (self.worktree / "stray.txt").write_text("not in the candidate\n")
        result = self.standard("2", "--worktree", str(self.worktree))
        self.assertEqual(result.returncode, 2)
        self.assertIn("dirty", result.stderr)
        self.assertFalse(any(call[0] == "create" for call in self.lima_calls()))
        self.assertTrue((self.worktree / "stray.txt").exists(), "deleted a worktree it didn't make")

    def test_refuses_a_worktree_at_another_commit(self):
        git("worktree", "add", "-q", "--detach", str(self.worktree), self.commit, cwd=self.repository)
        (self.worktree / "next.txt").write_text("next\n")
        subprocess.run(["git", "add", "next.txt"], cwd=self.worktree, check=True, env=self.environment)
        subprocess.run(["git", "commit", "-qm", "next"], cwd=self.worktree, check=True, env=self.environment)
        result = self.standard("2", "--worktree", str(self.worktree))
        self.assertEqual(result.returncode, 2)
        self.assertIn(self.commit, result.stderr)
        self.assertFalse(any(call[0] == "create" for call in self.lima_calls()))

    def test_reuses_a_clean_worktree_at_the_commit_and_keeps_it(self):
        git("worktree", "add", "-q", "--detach", str(self.worktree), self.commit, cwd=self.repository)
        result = self.standard("1", "--worktree", str(self.worktree))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(self.worktree.exists(), "deleted a worktree it didn't make")

    # The VM

    def test_creates_the_vm_from_the_template_with_only_the_checkout_mounted_read_only(self):
        result = self.standard()
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = self.lima_calls()
        create = next(call for call in calls if call[0] == "create")
        self.assertEqual(create[:-1], ["create", "--tty=false", "--name", self.vm])
        self.assertIn(["start", "--tty=false", self.vm], calls)
        created = self.created.read_text()
        self.assertNotIn("__WORKTREE__", created)
        self.assertEqual(re.findall(r'(?m)^- location: "(.*)"\n  writable: (\w+)', created),
                         [(str(self.worktree), "false")])
        self.assertEqual(created, TEMPLATE.read_text().replace("__WORKTREE__", str(self.worktree)))
        # The guest is checked against the same checkout.
        shells = [call for call in calls if call[0] == "shell"]
        self.assertEqual(shells[0][:2], ["shell", self.vm])
        self.assertEqual(shells[0][-1], str(self.worktree))

    def test_template_sizes_the_vm_like_the_release_gate_asks(self):
        template = TEMPLATE.read_text()
        self.assertRegex(template, r"(?m)^cpus: 4$")
        self.assertRegex(template, r'(?m)^memory: "6GiB"$')
        self.assertRegex(template, r'(?m)^disk: "40GiB"$')
        self.assertRegex(template, r"fallocate -l 6G /swapfile")
        self.assertEqual(re.findall(r"(?m)^- location: \"(.*)\"\n  writable: (\w+)", template),
                         [("__WORKTREE__", "false")])
        self.assertNotRegex(template, r"writable: true")

    def test_guest_checks_stop_the_run_before_any_lane(self):
        for name, environment in [("no swap", dict(FAKE_SWAP_BYTES="0")),
                                  ("too little swap", dict(FAKE_SWAP_BYTES=str(2 * 1024 ** 3))),
                                  ("writable checkout", dict(FAKE_WRITABLE="yes")),
                                  ("read-write mount", dict(FAKE_OPTIONS="rw,relatime")),
                                  ("another mount", dict(FAKE_MOUNTS=f"{self.worktree}:/Users")),
                                  ("two vCPUs", dict(FAKE_CPUS="2"))]:
            with self.subTest(name):
                for path in (self.lima_log, self.lane_log):
                    path.unlink(missing_ok=True)
                result = self.standard(**environment)
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertEqual(self.lanes(), [])
                self.assert_cleaned_up()
                self.assertFalse(self.records.exists())

    # The lanes

    def test_runs_every_lane_in_order_with_its_iterations(self):
        result = self.standard("3")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.lanes(),
                         ["storage-exporter", "storage-leases", "storage-backups"]
                         + ["oci-reboot"] * 3 + ["discovery-reboot"] * 3)
        storage = [line for line in self.lane_log.read_text().splitlines() if line.startswith("storage-")]
        for line in storage:
            self.assertIn("--iterations 3", line)
        oci = [line for line in self.lane_log.read_text().splitlines() if line.startswith("oci-reboot")]
        self.assertEqual(oci, [f"oci-reboot --vm {self.vm}"] * 3)
        for name in ["power-cut-exporter.md", "power-cut-leases.md", "power-cut-backups.md",
                     "oci-reboot.md", "discovery-reboot.md"]:
            self.assertTrue((self.records / name).exists(), name)
        reboots = (self.records / "oci-reboot.md").read_text()
        self.assertEqual(re.findall(r"(?m)^## Iteration (\d+)$", reboots), ["1", "2", "3"])
        self.assertIn("PASS: oci-reboot", reboots)
        self.assert_cleaned_up()

    def test_summary_gives_each_lane_its_rule_of_three_bound(self):
        for iterations, bound in [("10", "< 30%"), ("25", "< 12%"), ("7", "< 42.9%"), ("1", "< 100%")]:
            with self.subTest(iterations=iterations):
                shutil.rmtree(self.records, ignore_errors=True)
                Path(f"{self.records}.md").unlink(missing_ok=True)
                self.assertEqual(self.standard(iterations).returncode, 0)
                for lane in ["--fixture exporter", "--fixture leases", "--fixture backups",
                             "qualify-oci-reboot.sh", "qualify-discovery-reboot.sh"]:
                    cells = self.row(lane)
                    self.assertEqual(cells[3:6], [iterations, "0", bound], lane)
                self.assertIn(self.commit, self.summary())
                self.assertIn(self.vm, self.summary())
                self.assertIn("Result: every lane passed", self.summary())
                # Links point into the record directory, next to the summary.
                self.assertIn(f"]({self.records.name}/oci-reboot.md)", self.summary())

    def test_summary_counts_vm_start_retries(self):
        result = self.standard("3", FAKE_RETRIED="leases:2")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("1 iteration needed the VM start retry", self.summary())

    def test_stops_at_the_first_reboot_failure_and_keeps_the_evidence(self):
        result = self.standard("3", FAKE_FAIL="oci-reboot:2")
        self.assertEqual(result.returncode, 1)
        self.assertEqual(self.lanes(),
                         ["storage-exporter", "storage-leases", "storage-backups", "oci-reboot", "oci-reboot"])
        self.assertEqual(self.row("qualify-oci-reboot.sh")[3:6], ["2", "1", "failed"])
        self.assertEqual(self.row("qualify-discovery-reboot.sh")[3:6], ["0", "-", "not run"])
        self.assertIn("Result: FAIL", self.summary())
        self.assertIn("lost a hold", (self.records / "oci-reboot.md").read_text())
        # A failure is a release blocker: the VM and checkout stay for diagnosis.
        self.assertFalse(any(call[0] == "delete" for call in self.lima_calls()))
        self.assertTrue(self.worktree.exists())
        self.assertIn(f"limactl delete --force {self.vm}", result.stderr)

    def test_stops_at_the_first_storage_failure(self):
        result = self.standard("3", FAKE_FAIL="storage-leases:1")
        self.assertEqual(result.returncode, 1)
        self.assertEqual(self.lanes(), ["storage-exporter", "storage-leases"])
        self.assertEqual(self.row("--fixture leases")[3:6], ["1", "1", "failed"])
        self.assertEqual(self.row("--fixture backups")[3:6], ["0", "-", "not run"])

    def test_keep_leaves_the_vm_and_worktree(self):
        result = self.standard("1", "--keep")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(any(call[0] == "delete" for call in self.lima_calls()))
        self.assertTrue(self.worktree.exists())

    def test_never_names_the_development_vm_to_limactl(self):
        self.assertEqual(self.standard("2").returncode, 0)
        shutil.rmtree(self.records)
        Path(f"{self.records}.md").unlink()
        self.lane_log.unlink()
        self.assertEqual(self.standard("2", FAKE_FAIL="discovery-reboot:1").returncode, 1)
        for call in self.lima_calls():
            self.assertNotIn("reliaburger-test", call)


if __name__ == "__main__":
    unittest.main()
