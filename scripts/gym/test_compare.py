"""The comparison record generator and the check that keeps its page honest (#1679)."""
import json
from pathlib import Path
import re
import tempfile
import unittest

import compare

GYM_DIR = Path(__file__).resolve().parent
REPOSITORY = GYM_DIR.parent.parent
PAGE = REPOSITORY / "docs" / "website" / "compare" / "ai-agents.html"


def run(scenario, index, verdict, seconds, solver, model="claude-test", cost=0.5, calls=3):
    return {
        "scenario": scenario, "run": f"run-{index}", "seconds": seconds,
        "score": {"verdict": verdict, "calls": calls, "checks": {}},
        "solver_meta": {"solver": solver, "model": model, "cost_usd": cost},
    }


def results(directory, platform, solver, runs, kind="command"):
    directory = Path(directory)
    directory.mkdir(parents=True, exist_ok=True)
    (directory / "results.json").write_text(json.dumps({
        "context": {"date": "2026-11-20", "platform": platform, "solver": solver, "solver_kind": kind,
                    "runs_per_scenario": 5, "host": "arm64, Darwin", "relish_version": "relish 0.2.7"},
        "summary": {},
        "runs": runs,
    }))
    return directory


class Record(unittest.TestCase):
    def setUp(self):
        self.root = Path(tempfile.mkdtemp())
        ours = [run("crash-loop", i, "correct", 30 + i, "claude-code") for i in range(1, 6)]
        ours += [run("planted-logs-since", i, "partial", 60, "claude-code") for i in range(1, 6)]
        theirs = [run("crash-loop", i, "correct", 20 + i, "claude-code") for i in range(1, 6)]
        kai = [run("crash-loop", i, "no_answer", 10, "kubectl-ai", "bedrock-claude", None)
               for i in range(1, 4)]
        self.paths = [
            results(self.root / "rb", "reliaburger", "claude_code.py", ours),
            results(self.root / "k8s-claude", "k8s", "claude_code.py", theirs),
            results(self.root / "k8s-kai", "k8s", "kubectl_ai.py", kai),
        ]
        self.out = self.root / "record"
        self.out.mkdir()
        self.text = compare.render(self.paths, self.out)

    def test_each_scenario_gets_one_table_with_every_cell(self):
        crash = self.text.split("## crash-loop")[1].split("##")[0]
        rows = [line for line in crash.splitlines() if line.startswith("| ") and "---" not in line][1:]
        self.assertEqual(len(rows), 3)
        self.assertTrue(rows[0].startswith("| Reliaburger | Claude Code with `relish`"))
        self.assertIn("| Kubernetes (k3s) | kubectl-ai | bedrock-claude | 3 |", self.text)

    def test_a_cell_with_fewer_than_five_runs_is_flagged(self):
        self.assertIn("Cells without 5 runs:", self.text)
        self.assertIn("crash-loop (kubectl-ai, Kubernetes (k3s)): 3 run(s)", self.text)

    def test_cost_the_tool_didnt_report_says_so(self):
        self.assertIn("not reported (3 of 3 runs)", self.text)
        self.assertIn("$2.50 total, $0.50 median", self.text)

    def test_the_planted_bug_says_kubernetes_has_no_equivalent(self):
        planted = self.text.split("## planted-logs-since")[1].split("##")[0]
        self.assertIn("Kubernetes has no equivalent loop", planted)
        self.assertNotIn("| Kubernetes (k3s) |", planted)

    def test_where_kubernetes_did_better_is_listed(self):
        better = self.text.split("## Where Kubernetes did better")[1]
        self.assertIn("crash-loop, Claude Code with `kubectl` and the Kubernetes MCP server: "
                      "a shorter median time (23 s against 33 s)", better)

    def test_every_run_links_its_transcript_relative_to_the_record(self):
        self.assertIn("[run-1](../rb/crash-loop/run-1/solver.stdout)", self.text)


class Page(unittest.TestCase):
    def write(self, text):
        path = Path(tempfile.mkdtemp()) / "page.html"
        path.write_text(text)
        return path

    def page(self, record="", body="<p>No results yet.</p>"):
        return (f'<html><head><meta name="gym-record" content="{record}"></head>'
                f"<body><main>{body}</main><footer>Updated 6 November 2026</footer></body></html>")

    def test_the_published_page_claims_nothing_the_record_doesnt_hold(self):
        self.assertEqual(compare.check_page(PAGE), [])

    def test_a_number_with_no_record_behind_it_fails(self):
        problems = compare.check_page(self.write(self.page(body="<p>No results yet. 4 of 5 correct.</p>")))
        self.assertEqual(len(problems), 1)
        self.assertIn("4", problems[0])

    def test_a_number_the_record_holds_passes(self):
        directory = Path(tempfile.mkdtemp())
        (directory / "record.md").write_text("| crash-loop | 4 | 5 |\n")
        page = self.write(self.page("record.md", "<p>4 of 5 correct.</p>"))
        self.assertEqual(compare.check_page(page, repository=directory), [])
        page = self.write(self.page("record.md", "<p>3 of 5 correct.</p>"))
        self.assertIn("3", compare.check_page(page, repository=directory)[0])

    def test_a_record_that_doesnt_exist_fails(self):
        problems = compare.check_page(self.write(self.page("docs/qualification/nope.md", "")))
        self.assertIn("doesn't exist", problems[0])

    def test_a_page_without_a_record_must_say_so(self):
        problems = compare.check_page(self.write(self.page(body="<p>Soon.</p>")))
        self.assertIn("No results yet", problems[0])

    def test_pinned_versions_and_issue_numbers_may_appear(self):
        body = "<p>No results yet. k3s v1.37.2+k3s1, see #1679.</p>"
        self.assertEqual(compare.check_page(self.write(self.page(body=body))), [])

    def test_only_main_is_checked(self):
        # The footer's date is outside <main>.
        self.assertEqual(compare.check_page(self.write(self.page())), [])


class Wiring(unittest.TestCase):
    def test_the_page_names_its_markdown_copy(self):
        self.assertIn('<link rel="alternate" type="text/markdown" href="ai-agents.md">', PAGE.read_text())

    def test_llms_txt_lists_the_copy_once_it_lists_the_website(self):
        # #1582 adds a Website section to llms.txt that lists every page's
        # Markdown copy; until it lands there's nothing to list it in.
        llms = (REPOSITORY / "docs" / "website" / "llms.txt").read_text()
        if re.search(r"^## Website", llms, re.M):
            self.assertIn("compare/ai-agents.md", llms)

    def test_the_draft_stays_out_of_search_until_it_has_a_record(self):
        page = PAGE.read_text()
        named = compare.RECORD_META.search(page).group(1)
        self.assertEqual('name="robots" content="noindex"' in page, not named)


if __name__ == "__main__":
    unittest.main()
