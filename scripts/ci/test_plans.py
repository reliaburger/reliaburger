"""Checks that keep the plans index honest.

`docs/plans/README.md` is where contributors look for current work. Its own
rule is that a dated plan is listed under Live while its work is open, and
moves to `archive/` unchanged once it has shipped or been superseded. A plan
nobody indexed is invisible, and an index entry for a moved file is a dead
link, so both are checked here. Test counts go stale with every commit, so
plans don't quote them.
"""
from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[2]
PLANS = ROOT / "docs" / "plans"
INDEX = PLANS / "README.md"

LINK = re.compile(r"(?<!!)\[[^\]]*\]\(([^)\s#]+)(?:#[^)\s]*)?\)")
TEST_COUNT = re.compile(r"(?<![\w.])\d[\d,]*\s+(?:[a-z-]+\s+)?tests\b|bringing the total to", re.IGNORECASE)


def section(text, heading):
    """The text under a `## heading`, up to the next `## `."""
    match = re.search(rf"^## {re.escape(heading)}\s*$(.*?)(?=^## |\Z)", text, re.MULTILINE | re.DOTALL)
    return match.group(1) if match else ""


def local_links(text):
    return [target for target in LINK.findall(text) if not re.match(r"^[a-z][a-z0-9+.-]*:", target)]


class PlansIndexTests(unittest.TestCase):
    def setUp(self):
        self.index = INDEX.read_text(encoding="utf-8")

    def test_plans_index_lists_every_live_plan(self):
        live = {Path(target).name for target in local_links(section(self.index, "Live"))
                if "/" not in target}
        on_disk = {path.name for path in PLANS.glob("*.md") if path.name != "README.md"}
        self.assertEqual(sorted(on_disk - live), [], "plans in docs/plans/ missing from the Live table")
        self.assertEqual(sorted(live - on_disk), [], "Live entries with no plan in docs/plans/")

    def test_every_plans_index_link_resolves(self):
        broken = [target for target in local_links(self.index) if not (PLANS / target).exists()]
        self.assertEqual(broken, [])

    def test_history_links_point_into_the_archive(self):
        history = local_links(section(self.index, "History"))
        outside = [target for target in history
                   if target.endswith(".md") and not target.startswith("archive/")
                   and not target.startswith("../")]
        self.assertEqual(outside, [], "a History entry links to a plan outside archive/")

    def test_plans_quote_no_test_counts(self):
        counts = []
        for path in sorted(PLANS.glob("*.md")):
            for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
                for match in TEST_COUNT.finditer(line):
                    counts.append(f"{path.relative_to(ROOT)}:{number}: {match.group(0)}")
        self.assertEqual(counts, [])

    def test_test_count_pattern_spares_item_numbers(self):
        self.assertIsNone(TEST_COUNT.search("**C6.8 eBPF tests that write**"))
        self.assertIsNotNone(TEST_COUNT.search("All 54 Linux tests pass"))

    def test_section_reads_up_to_the_next_heading(self):
        text = "## Live\n\nA\n\n## History\n\nB\n"
        self.assertIn("A", section(text, "Live"))
        self.assertNotIn("B", section(text, "Live"))


if __name__ == "__main__":
    unittest.main()
