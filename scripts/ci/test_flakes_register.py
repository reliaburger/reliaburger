"""The known-flakes register only works if every open row points at an open issue.

These tests stay offline: the issue lookup is a stub. The live lookup, which
asks GitHub for each issue's state, runs nightly in ci.yml.
"""
from pathlib import Path
import unittest

import flakes_register

REPO = Path(__file__).resolve().parents[2]

ISSUE = "https://github.com/reliaburger/reliaburger/issues/{}"

REGISTER = f"""\
# Known flakes

Some prose with a table-looking line: | not | a | row |

| Test | First seen | Cause | Status |
|---|---|---|---|
| `a::tracked` | 1 Oct | Unknown | Open ([#10]({ISSUE.format(10)}), 0.2.1): retain receipts |
| `b::untracked` | 2 Oct | Unknown | Open: nobody owns this yet |
| `c::closed_tracker` | 3 Oct | Unknown | Open ([#11]({ISSUE.format(11)})): still flaky |
| `d::fixed` | 4 Oct | Harness | Fixed ([#11]({ISSUE.format(11)})): the fixture waits |
| `e::pull_request_only` | 5 Oct | Unknown | Open: see [#12](https://github.com/reliaburger/reliaburger/pull/12) |
"""

STATES = {10: "OPEN", 11: "CLOSED"}


def stub_state(number):
    return STATES[number]


class FlakesRegisterTests(unittest.TestCase):
    def setUp(self):
        self.rows = flakes_register.parse(REGISTER)

    def test_only_the_table_rows_are_parsed(self):
        self.assertEqual([row.test for row in self.rows],
                         ["`a::tracked`", "`b::untracked`", "`c::closed_tracker`",
                          "`d::fixed`", "`e::pull_request_only`"])
        self.assertEqual(self.rows[0].line, 7)

    def test_open_rows_are_the_ones_whose_status_says_open(self):
        self.assertEqual([row.test for row in self.rows if row.is_open()],
                         ["`a::tracked`", "`b::untracked`", "`c::closed_tracker`",
                          "`e::pull_request_only`"])

    def test_every_open_row_links_an_issue(self):
        problems = flakes_register.missing_issues(self.rows)
        self.assertEqual(len(problems), 2, problems)
        self.assertIn("`b::untracked`", problems[0])
        # A pull request is not an issue: it can't stay open as the tracker.
        self.assertIn("`e::pull_request_only`", problems[1])

    def test_a_row_linking_a_closed_issue_is_reported(self):
        problems = flakes_register.closed_issues(self.rows, stub_state)
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("`c::closed_tracker`", problems[0])
        self.assertIn("#11", problems[0])

    def test_a_fixed_row_may_link_a_closed_issue(self):
        problems = flakes_register.closed_issues(self.rows, stub_state)
        self.assertFalse(any("`d::fixed`" in problem for problem in problems))

    def test_each_issue_is_looked_up_once(self):
        asked = []

        def counting_state(number):
            asked.append(number)
            return STATES[number]

        rows = flakes_register.parse(REGISTER + REGISTER.splitlines()[6] + "\n")
        flakes_register.closed_issues(rows, counting_state)
        self.assertEqual(sorted(asked), [10, 11])

    def test_an_escaped_pipe_stays_inside_its_cell(self):
        text = (f"| Test | First seen | Cause | Status |\n|---|---|---|---|\n"
                f"| `x` | 1 Oct | `a \\| b` | Open ([#10]({ISSUE.format(10)})) |\n")
        rows = flakes_register.parse(text)
        self.assertEqual(rows[0].cause, "`a \\| b`")
        self.assertEqual(rows[0].issues(), [10])


class RealRegisterTests(unittest.TestCase):
    def test_every_open_row_in_the_real_register_links_an_issue(self):
        rows = flakes_register.parse((REPO / "docs" / "flakes.md").read_text())
        self.assertTrue(rows)
        self.assertEqual(flakes_register.missing_issues(rows), [])

    def test_ci_checks_the_live_issue_states_nightly(self):
        # Its own workflow: ci.yml is also called from ci-retarget.yml and
        # build.yml, which grant only `contents: read`, and a called job may
        # not ask for more than its caller grants.
        workflow = (REPO / ".github" / "workflows" / "flakes.yml").read_text()
        self.assertRegex(workflow, r"(?m)^  schedule:\n    - cron: ")
        self.assertRegex(workflow, r"(?m)^  issues: read$")
        self.assertIn("GH_TOKEN: ${{ github.token }}", workflow)
        self.assertIn("python3 scripts/ci/flakes_register.py check", workflow)
        # A pull request that edits the register is checked before it lands.
        self.assertIn("docs/flakes.md", workflow)
        ci = (REPO / ".github" / "workflows" / "ci.yml").read_text()
        self.assertNotIn("issues: read", ci)


if __name__ == "__main__":
    unittest.main()
