"""The nightly runs the gate's long tiers on the train tip, and code freeze checks the candidate against it (#1363).

Only the pure parts are tested: which tiers a night runs and with what, what
happens after a failure, the record, and the freeze rule. The tiers themselves
need Lima and hours (docs/releasing.md, "Nightly qualification").
"""
import hashlib
import json
from pathlib import Path
import plistlib
import subprocess
import tempfile
import unittest

from nightly import (FAIL, GATE_TIERS, JOBS_FAST, JOBS_FINAL, NOT_RUN, PASS, POWER_CUT, V02_FAST,
                     freeze_problems, green, load_records, plan_steps, render_readme, run_tiers,
                     staged_problems)

HERE = Path(__file__).resolve().parent
COMMIT = "c" * 40
TREE = "t" * 40
STAGED = ("https://example.test/staging-v0.2.4-1-1", "d" * 64)


def steps(gate="patch", staged=STAGED, soak_bun="/soak/bun-v0.2.4-soak.1"):
    return plan_steps(gate, COMMIT, "/records/night", "/tmp/checkout", 10, staged, soak_bun)


def record(date, statuses, commit=COMMIT, tree=TREE):
    return dict(schema=1, train="release-0-2-4", commit=commit, tree=tree, date=date,
                started=f"{date}T18:00:00+01:00", finished=f"{date}T23:00:00+01:00", host="studio",
                gate="full", staged=None,
                tiers=[dict(name=name, status=status, seconds=60) for name, status in statuses.items()])


PATCH_GREEN = {POWER_CUT: PASS, V02_FAST: PASS, JOBS_FAST: PASS}
FULL_GREEN = dict(PATCH_GREEN, **{JOBS_FINAL: PASS})


class PlanTests(unittest.TestCase):
    def test_the_patch_gate_runs_power_cut_then_both_fast_soaks(self):
        self.assertEqual([name for name, _, _ in steps()], [POWER_CUT, V02_FAST, JOBS_FAST])

    def test_the_full_gate_adds_the_job_final_last(self):
        self.assertEqual([name for name, _, _ in steps("full")], [POWER_CUT, V02_FAST, JOBS_FAST, JOBS_FINAL])

    def test_every_tier_runs_the_tip_s_own_scripts_against_the_tip(self):
        for name, argv, _ in steps("full"):
            with self.subTest(name=name):
                self.assertTrue(argv[0].startswith("/tmp/checkout/scripts/release/"))
        power_cut = steps()[0][1]
        self.assertEqual(power_cut[1:], ["--commit", COMMIT, "--iterations", "10",
                                         "--record-dir", "/records/night/power-cut-reboot"])

    def test_the_soaks_use_the_staged_build_and_their_tiers(self):
        by_name = {name: argv for name, argv, _ in steps("full")}
        self.assertEqual(Path(by_name[V02_FAST][0]).name, "qualify-sustained.sh")
        self.assertEqual(by_name[V02_FAST][1:3], ["--tier", "fast"])
        self.assertEqual(Path(by_name[JOBS_FAST][0]).name, "qualify-jobs-sustained.sh")
        self.assertEqual(by_name[JOBS_FAST][1:3], ["--tier", "fast"])
        self.assertEqual(by_name[JOBS_FINAL][1:3], ["--tier", "final"])
        for name in (V02_FAST, JOBS_FAST, JOBS_FINAL):
            argv = by_name[name]
            self.assertEqual(argv[argv.index("--base-url") + 1], STAGED[0])
            self.assertEqual(argv[argv.index("--qualified-digest") + 1], STAGED[1])
            self.assertEqual(argv[argv.index("--record") + 1], f"/records/night/{name}.md")

    def test_without_a_staged_build_only_the_power_cut_runs(self):
        planned = steps("full", staged=None)
        self.assertIsNotNone(planned[0][1])
        for name, argv, note in planned[1:]:
            with self.subTest(name=name):
                self.assertIsNone(argv)
                self.assertIn("no staged build", note)

    def test_the_final_needs_a_soak_bun(self):
        planned = {name: (argv, note) for name, argv, note in steps("full", soak_bun=None)}
        self.assertIsNone(planned[JOBS_FINAL][0])
        self.assertIn("--soak-bun", planned[JOBS_FINAL][1])
        self.assertIsNotNone(planned[JOBS_FAST][0])
        self.assertNotIn("--soak-bun", planned[JOBS_FAST][0])


class RunTests(unittest.TestCase):
    def run_with(self, planned, failing=None):
        calls = []

        def runner(argv):
            calls.append(argv)
            return 1 if failing is not None and failing in argv else 0
        return run_tiers(planned, runner, clock=iter(range(0, 1000, 60)).__next__, load=lambda: (1.5, 0, 0)), calls

    def test_a_clean_night_passes_every_tier(self):
        results, calls = self.run_with(steps("full"))
        self.assertEqual([result["status"] for result in results], [PASS] * 4)
        self.assertEqual(len(calls), 4)
        self.assertEqual(results[0]["seconds"], 60)
        self.assertEqual(results[0]["load_before"], 1.5)

    def test_a_failed_fast_tier_still_runs_the_other_fast_tiers_but_not_the_final(self):
        planned = steps("full")
        results, calls = self.run_with(planned, failing="/records/night/v02-fast.md")
        self.assertEqual([result["status"] for result in results], [PASS, FAIL, PASS, NOT_RUN])
        self.assertEqual(len(calls), 3)
        self.assertIn("earlier tier", results[3]["note"])

    def test_a_tier_that_cannot_run_is_recorded_not_run(self):
        results, calls = self.run_with(steps(staged=None))
        self.assertEqual([result["status"] for result in results], [PASS, NOT_RUN, NOT_RUN])
        self.assertEqual(len(calls), 1)


class GreenTests(unittest.TestCase):
    def test_a_night_with_every_patch_tier_passed_is_green_for_the_patch_gate_only(self):
        night = record("2026-11-03", PATCH_GREEN)
        self.assertTrue(green(night, "patch"))
        self.assertFalse(green(night, "full"))

    def test_a_full_night_is_green_for_both(self):
        night = record("2026-11-03", FULL_GREEN)
        self.assertTrue(green(night, "patch"))
        self.assertTrue(green(night, "full"))

    def test_a_tier_not_run_is_not_green(self):
        self.assertFalse(green(record("2026-11-03", dict(PATCH_GREEN, **{V02_FAST: NOT_RUN})), "patch"))

    def test_the_readme_names_the_commit_and_the_verdict(self):
        readme = render_readme(record("2026-11-03", PATCH_GREEN))
        self.assertIn(COMMIT, readme)
        self.assertIn("green for the patch gate", readme)
        self.assertIn("| `jobs-fast` | pass |", readme)
        self.assertIn("red", render_readme(record("2026-11-03", dict(PATCH_GREEN, **{POWER_CUT: FAIL}))))


class FreezeTests(unittest.TestCase):
    def test_the_candidate_on_the_latest_green_nightly_passes(self):
        self.assertEqual(freeze_problems([record("2026-11-03", PATCH_GREEN)], COMMIT, TREE, "patch"), [])

    def test_the_gate_pr_merge_with_the_same_tree_passes(self):
        merge = "m" * 40
        self.assertEqual(freeze_problems([record("2026-11-03", PATCH_GREEN)], merge, TREE, "patch"), [])

    def test_another_commit_fails(self):
        problems = freeze_problems([record("2026-11-03", PATCH_GREEN)], "e" * 40, "u" * 40, "patch")
        self.assertIn("not the candidate", problems[0])

    def test_only_the_latest_green_nightly_counts(self):
        older = record("2026-11-02", PATCH_GREEN)
        newer = record("2026-11-03", PATCH_GREEN, commit="e" * 40, tree="u" * 40)
        self.assertTrue(freeze_problems([older, newer], COMMIT, TREE, "patch"))

    def test_a_full_gate_needs_a_nightly_with_the_job_final(self):
        nights = [record("2026-11-03", PATCH_GREEN)]
        self.assertEqual(freeze_problems(nights, COMMIT, TREE, "full"), ["no nightly is green for the full gate"])
        self.assertEqual(freeze_problems([record("2026-11-04", FULL_GREEN)], COMMIT, TREE, "full"), [])

    def test_no_nightly_fails(self):
        self.assertEqual(freeze_problems([], COMMIT, TREE, "patch"), ["no nightly is green for the patch gate"])

    def test_a_later_red_night_on_the_same_code_fails_until_triaged(self):
        nights = [record("2026-11-03", PATCH_GREEN), record("2026-11-04", dict(PATCH_GREEN, **{JOBS_FAST: FAIL}))]
        problems = freeze_problems(nights, COMMIT, TREE, "patch")
        self.assertEqual(len(problems), 1)
        self.assertIn("2026-11-04", problems[0])

    def test_a_newer_green_night_after_a_triaged_red_one_clears_it(self):
        nights = [record("2026-11-03", PATCH_GREEN), record("2026-11-04", dict(PATCH_GREEN, **{JOBS_FAST: FAIL})),
                  record("2026-11-05", PATCH_GREEN)]
        self.assertEqual(freeze_problems(nights, COMMIT, TREE, "patch"), [])

    def test_records_load_oldest_first(self):
        with tempfile.TemporaryDirectory() as directory:
            for night in (record("2026-11-04", PATCH_GREEN), record("2026-11-03", PATCH_GREEN)):
                path = Path(directory) / night["date"]
                path.mkdir()
                (path / "nightly.json").write_text(json.dumps(night))
            self.assertEqual([night["date"] for night in load_records(directory)], ["2026-11-03", "2026-11-04"])


class StagedTests(unittest.TestCase):
    def manifest(self, commit=COMMIT):
        data = json.dumps(dict(schema=1, commit=commit)).encode()
        return data, hashlib.sha256(data).hexdigest()

    def test_a_staged_build_of_the_tip_is_accepted(self):
        data, digest = self.manifest()
        self.assertEqual(staged_problems(data, digest, COMMIT, lambda a, b: False), [])

    def test_a_staged_build_of_the_same_tree_is_accepted(self):
        data, digest = self.manifest("m" * 40)
        self.assertEqual(staged_problems(data, digest, COMMIT, lambda a, b: True), [])

    def test_a_staged_build_of_other_code_is_refused(self):
        data, digest = self.manifest("e" * 40)
        self.assertIn("not the tip", staged_problems(data, digest, COMMIT, lambda a, b: False)[0])

    def test_a_manifest_that_does_not_hash_to_the_digest_is_refused(self):
        data, _ = self.manifest()
        self.assertIn("doesn't hash", staged_problems(data, "0" * 64, COMMIT, lambda a, b: True)[0])


class WrapperTests(unittest.TestCase):
    def test_the_gates_name_the_same_tiers_as_releasing_md(self):
        self.assertEqual(GATE_TIERS["full"][:3], GATE_TIERS["patch"])

    def test_the_wrapper_prints_its_usage(self):
        result = subprocess.run([str(HERE / "nightly-qualify.sh"), "--help"], capture_output=True, text=True,
                                check=True)
        self.assertIn("--train NAME", result.stdout)
        self.assertIn("--plan, --dry-run", result.stdout)

    def test_the_launchd_example_is_a_valid_plist_that_runs_the_wrapper(self):
        plist = plistlib.loads((HERE / "nightly-qualify.plist.example").read_bytes())
        self.assertEqual(plist["Label"], "com.reliaburger.nightly-qualify")
        self.assertIn("scripts/release/nightly-qualify.sh --train ", plist["ProgramArguments"][-1])


if __name__ == "__main__":
    unittest.main()
