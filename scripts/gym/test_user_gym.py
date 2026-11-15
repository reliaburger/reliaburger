"""Tests for user_gym.py: the checks the gym runs over `relish gym run`'s
report and the cluster it leaves behind (#1683)."""
import unittest

import user_gym


def run(scenario="crash-loop", outcome="scored", verdict="correct", verified=True):
    return {
        "scenario": scenario, "run": 1, "outcome": outcome, "reason": None,
        "score": {"verdict": verdict} if outcome == "scored" else None,
        "cleanup": {"verified": verified},
    }


def report(*runs, cancelled=False, findings=(), verified=True):
    return {
        "schema": "reliaburger.gym.report/v1",
        "runs": list(runs),
        "cancelled": cancelled,
        "safety_findings": list(findings),
        "cleanup_verified": verified,
    }


class Report(unittest.TestCase):
    def test_a_correct_safe_clean_report_has_no_problems(self):
        self.assertEqual(user_gym.check_report(report(run(), run("paused-app"))), [])

    def test_a_wrong_verdict_an_unsafe_action_or_a_dirty_cleanup_is_a_problem(self):
        problems = user_gym.check_report(report(
            run(verdict="partial"), run("paused-app", verified=False),
            findings=[{"kind": "mutating_call"}], verified=False,
        ))
        text = "\n".join(problems)
        self.assertIn("scored partial", text)
        self.assertIn("unsafe or refused", text)
        self.assertIn("cleanup wasn't verified", text)
        self.assertIn("paused-app run 1: cleanup not verified", text)

    def test_a_skipped_run_is_a_problem_for_the_baseline(self):
        problems = user_gym.check_report(report(run(outcome="skipped")))
        self.assertTrue(any("skipped" in problem for problem in problems), problems)

    def test_a_cancelled_run_must_say_so_and_be_clean(self):
        self.assertEqual(
            user_gym.check_report(report(run(outcome="cancelled"), cancelled=True),
                                  expect_cancelled=True),
            [],
        )
        problems = user_gym.check_report(report(run()), expect_cancelled=True)
        self.assertTrue(any("cancelled is False" in p for p in problems), problems)

    def test_an_empty_report_is_a_problem(self):
        self.assertIn("no runs", user_gym.check_report(report()))


class Cluster(unittest.TestCase):
    def test_nothing_left_is_clean(self):
        self.assertEqual(
            user_gym.check_cluster_clean(
                faults=[{"namespace": "other", "id": 1}],
                instances=[{"app_name": "gym-web", "namespace": "other"},
                           {"app_name": "checkout", "namespace": "gym-staging"}],
                desired=[],
                tokens=[{"name": "ci"}],
            ),
            [],
        )

    def test_a_fault_a_gym_app_or_a_solver_token_left_is_a_problem(self):
        problems = user_gym.check_cluster_clean(
            faults=[{"namespace": "gym-staging", "id": 4}],
            instances=[{"app_name": "gym-api", "namespace": "gym-staging"}],
            desired=[],
            tokens=[{"name": "gym-solver-1234abcd-1"}],
        )
        self.assertEqual(len(problems), 3, problems)


if __name__ == "__main__":
    unittest.main()
