"""Source-policy controls only; no Rust, archive or actual CI qualification."""
from pathlib import Path
import re
import unittest

from test_ci_workflow import jobs, steps

REPO = Path(__file__).resolve().parents[2]
PINNED_TOOLCHAIN = "dtolnay/rust-toolchain@1.98.0"


class ArchiveConsumerSelectionTests(unittest.TestCase):
    def setUp(self):
        self.workflow = jobs((REPO / ".github/workflows/ci.yml").read_text())

    def assert_selected_before_producer(self, consumer):
        builder_steps = steps(self.workflow["build-tests"])
        builder = [step for step in builder_steps if "uses: dtolnay/rust-toolchain@" in step]
        self.assertEqual(len(builder), 1)
        self.assertRegex(builder[0], r"(?m)^\s*- uses: " + re.escape(PINNED_TOOLCHAIN) + r"\s*$")
        consumer_steps = steps(consumer)
        selected = [index for index, step in enumerate(consumer_steps)
                    if "uses: dtolnay/rust-toolchain@" in step]
        self.assertEqual(len(selected), 1,
                         "archive consumers must select the pinned compiler before tool observation")
        self.assertRegex(consumer_steps[selected[0]],
                         r"(?m)^\s*- uses: " + re.escape(PINNED_TOOLCHAIN) + r"\s*$")
        producers = [index for index, step in enumerate(consumer_steps)
                     if "workflow_adapter.py produce" in step]
        self.assertTrue(producers, "an actual archive-consumer producer is required")
        self.assertLess(selected[0], min(producers),
                        "late selection cannot qualify the producer's observed compiler")

    def test_cluster_selects_the_archive_builders_pinned_compiler_before_production(self):
        self.assert_selected_before_producer(self.workflow["cluster"])

    def test_acceptance_selects_the_archive_builders_pinned_compiler_before_production(self):
        self.assert_selected_before_producer(self.workflow["acceptance"])

    def test_missing_different_and_late_selection_are_refused(self):
        pin = "      - uses: " + PINNED_TOOLCHAIN
        for name in ("cluster", "acceptance"):
            original = self.workflow[name]
            # Synthetic controls test this policy helper; they do not run an archive.
            without = original.replace(pin + "\n", "")
            first_producer = next(step for step in steps(without)
                                  if "workflow_adapter.py produce" in step)
            valid = without.replace(first_producer, pin + "\n" + first_producer, 1)
            self.assert_selected_before_producer(valid)
            for changed in (without, valid.replace(PINNED_TOOLCHAIN, "dtolnay/rust-toolchain@1.98.1"),
                            without + "\n" + pin):
                with self.subTest(job=name, changed=changed), self.assertRaises(AssertionError):
                    self.assert_selected_before_producer(changed)


if __name__ == "__main__":
    unittest.main()
