"""mutants_plan.py sizes a cargo-mutants run to its budget and reports it (#1584)."""
from pathlib import Path
from urllib.parse import parse_qs, urlparse
import re
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))
import mutants_plan  # noqa: E402

LISTING = """\
src/council/raft.rs:12:5: replace Raft::is_leader -> bool with true
src/council/raft.rs:12:5: replace Raft::is_leader -> bool with false
src/onion/dns.rs:40:17: replace > with >= in decode_query
src/redact.rs:7:9: replace redact -> String with String::new()
"""


class PlanTests(unittest.TestCase):
    def test_a_listing_under_the_budget_is_one_shard_tested_whole(self):
        planned = mutants_plan.plan(LISTING, 20)
        self.assertEqual(planned["count"], 4)
        self.assertEqual(planned["shards"], 1)
        self.assertEqual(planned["tested"], 4)

    def test_a_listing_over_the_budget_tests_one_round_robin_shard_within_it(self):
        listing = "".join(f"src/council/raft.rs:{n}:1: replace f -> bool with true\n" for n in range(1, 46))
        planned = mutants_plan.plan(listing, 20)
        self.assertEqual(planned["shards"], 3)
        self.assertEqual(planned["tested"], 15)
        self.assertLessEqual(planned["tested"], 20)

    def test_an_empty_listing_plans_nothing(self):
        planned = mutants_plan.plan("Found 0 mutants to test\n", 20)
        self.assertEqual((planned["count"], planned["shards"], planned["filter"]), (0, 1, ""))

    def test_the_filter_runs_the_unit_tests_of_each_touched_module(self):
        self.assertEqual(mutants_plan.plan(LISTING, 20)["filter"], "test(/^(council|onion|redact)::/)")

    def test_a_mutant_outside_any_module_runs_every_library_test(self):
        listing = LISTING + "src/lib.rs:3:1: replace f with ()\n"
        self.assertEqual(mutants_plan.plan(listing, 20)["filter"], "")

    def test_the_command_line_appends_the_plan_to_the_github_output(self):
        with tempfile.TemporaryDirectory() as directory:
            listing = Path(directory) / "mutants.list"
            listing.write_text(LISTING)
            output = Path(directory) / "output"
            output.write_text("earlier=1\n")
            subprocess.run([sys.executable, mutants_plan.__file__, "plan", "--list", str(listing), "--max", "3",
                            "--github-output", str(output)], check=True, capture_output=True)
            self.assertEqual(output.read_text(), "earlier=1\ncount=4\nshards=2\ntested=2\n"
                             "filter=test(/^(council|onion|redact)::/)\n")

    def test_a_budget_below_one_is_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            listing = Path(directory) / "mutants.list"
            listing.write_text(LISTING)
            result = subprocess.run([sys.executable, mutants_plan.__file__, "plan", "--list", str(listing),
                                     "--max", "0"], capture_output=True, text=True)
            self.assertEqual(result.returncode, 2)
            self.assertIn("--max must be at least 1", result.stderr)


class SummaryTests(unittest.TestCase):
    def summary(self, missed, listing=LISTING, maximum=20):
        return mutants_plan.summary(missed, mutants_plan.plan(listing, maximum), "owner/repo", "abc123")

    def test_no_mutants_says_so(self):
        self.assertIn("No mutants in this change.", self.summary("", listing=""))

    def test_it_says_how_many_were_tested_of_how_many_and_which_tests_ran(self):
        text = self.summary("")
        self.assertIn("Tested 4 of 4 mutants (shard 1 of 1, round-robin); 0 missed.", text)
        self.assertIn("`test(/^(council|onion|redact)::/)`", text)
        self.assertIn("only an integration test catches shows as missed", text)

    def test_each_missed_mutant_links_a_prefilled_missed_mutant_issue(self):
        text = self.summary("src/onion/dns.rs:40:17: replace > with >= in decode_query\n")
        link = re.search(r"\[file an issue\]\(([^)]+)\)", text).group(1)
        url = urlparse(link)
        self.assertEqual(url.path, "/owner/repo/issues/new")
        query = parse_qs(url.query)
        self.assertEqual(query["labels"], ["missed-mutant"])
        self.assertEqual(query["title"], ["Missed mutant: replace > with >= in decode_query (src/onion/dns.rs)"])
        body = query["body"][0]
        self.assertIn("https://github.com/owner/repo/blob/abc123/src/onion/dns.rs#L40", body)
        self.assertIn("cargo mutants --file src/onion/dns.rs --re 'replace > with >= in decode_query'", body)

    def test_the_reproduce_command_escapes_regex_syntax_and_quotes(self):
        what = "replace Raft::term -> Option<u64> with Some(0) in it's"
        self.assertEqual(mutants_plan.shell_regex(what),
                         "'replace Raft::term -> Option<u64> with Some\\(0\\) in it'\\''s'")
        # The escaped text, unquoted, is a regex that matches the original.
        pattern = mutants_plan.shell_regex("Some(0) [x] a.b")[1:-1]
        self.assertRegex("Some(0) [x] a.b", "^" + pattern + "$")

    def test_a_long_list_is_cut_and_points_at_the_artefact(self):
        missed = "".join(f"src/council/raft.rs:{n}:1: replace f -> bool with true\n" for n in range(1, 61))
        text = self.summary(missed, listing=missed, maximum=100)
        self.assertEqual(text.count("[file an issue]"), mutants_plan.SUMMARY_ROWS)
        self.assertIn("And 10 more in the `mutants-out` artefact's `missed.txt`.", text)


if __name__ == "__main__":
    unittest.main()
