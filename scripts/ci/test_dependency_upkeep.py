"""Dependencies get refreshed, and the lag gets reported without failing (#1026).

The 0.2.1 gate found a redb crash that upstream had fixed two majors earlier,
because nothing in the repository ever moved Cargo.lock. These tests hold the
pieces that now move it: Dependabot's weekly lockfile-only refresh, the
`make outdated` report that never fails the build, and the cron contract pins
that must follow the main lockfile.
"""
from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[2]


def recipe(target):
    """The recipe lines of a Makefile target."""
    lines = (ROOT / "Makefile").read_text().splitlines()
    for index, line in enumerate(lines):
        if re.match(rf"^{re.escape(target)}:", line):
            body = []
            for following in lines[index + 1:]:
                if not following.startswith("\t"):
                    break
                body.append(following[1:])
            return body
    raise AssertionError(f"no {target} target in the Makefile")


def ecosystem_blocks(text):
    """Each `- package-ecosystem:` entry of dependabot.yml, as raw text."""
    return re.split(r"\n  - package-ecosystem: ", text)[1:]


class DependabotRefreshesWeekly(unittest.TestCase):
    def setUp(self):
        path = ROOT / ".github/dependabot.yml"
        self.assertTrue(path.exists(), "no Dependabot configuration")
        self.blocks = {block.split("\n", 1)[0].strip(): block
                       for block in ecosystem_blocks(path.read_text())}

    def test_cargo_and_actions_are_both_watched_weekly_with_grouped_minor_and_patch(self):
        self.assertEqual(set(self.blocks), {"cargo", "github-actions"})
        for block in self.blocks.values():
            self.assertIn("interval: weekly", block)
            self.assertRegex(block, r"update-types:\s*\n\s*- minor\s*\n\s*- patch")

    def test_cargo_refresh_never_rewrites_a_version_requirement(self):
        # Dependabot calls 0.13 -> 0.14 "minor"; Cargo calls it breaking.
        self.assertIn("versioning-strategy: lockfile-only", self.blocks["cargo"])

    def test_actions_refresh_leaves_the_compiler_pins_alone(self):
        self.assertIn("dependency-name: dtolnay/rust-toolchain", self.blocks["github-actions"])


class LagReportNeverFails(unittest.TestCase):
    def test_outdated_runs_a_dry_run_and_ignores_its_status(self):
        body = recipe("outdated")
        self.assertEqual(len(body), 1)
        self.assertTrue(body[0].startswith("-"), "make must ignore the report's exit status")
        self.assertIn("update --dry-run --verbose", body[0])

    def test_the_weekly_security_run_prints_the_report(self):
        workflow = (ROOT / ".github/workflows/security.yml").read_text()
        self.assertIn("make outdated", workflow)
        self.assertIn("GITHUB_STEP_SUMMARY", workflow)


class CronContractFollowsTheMainLockfile(unittest.TestCase):
    def test_exact_pins_match_the_versions_in_the_main_lockfile(self):
        manifest = (ROOT / "tests/contracts/cron/Cargo.toml").read_text()
        lockfile = (ROOT / "Cargo.lock").read_text()
        locked = {}
        for name, version in re.findall(r'^name = "([^"]+)"\nversion = "([^"]+)"', lockfile, re.MULTILINE):
            locked.setdefault(name, set()).add(version)
        for name in ("time", "thiserror"):
            pin = re.search(rf'^{name} = {{ version = "=([^"]+)"', manifest, re.MULTILINE)
            self.assertIsNotNone(pin, f"no exact {name} pin in the cron contract")
            self.assertIn(pin.group(1), locked.get(name, set()))


if __name__ == "__main__":
    unittest.main()
