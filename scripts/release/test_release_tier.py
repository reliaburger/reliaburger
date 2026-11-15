"""A release runs the patch gate unless it's a minor release or changes formats or the agent loop (#1363)."""
from pathlib import Path
import unittest

from release_tier import FULL, PATCH, decide, parse_current, parse_version

REPO = Path(__file__).resolve().parents[2]
SAME = (55, 72)


class DecideTests(unittest.TestCase):
    def test_a_patch_with_the_same_formats_and_loop_runs_the_patch_gate(self):
        tier, reasons = decide("v0.2.3", "v0.2.4", SAME, SAME, ["src/meat/scheduler.rs", "docs/releasing.md"])
        self.assertEqual(tier, PATCH)
        self.assertEqual(len(reasons), 1)

    def test_a_minor_release_runs_the_full_gate(self):
        self.assertEqual(decide("v0.2.4", "v0.3.0", SAME, SAME, []), (FULL, ["a minor or major release"]))

    def test_a_major_release_runs_the_full_gate(self):
        self.assertEqual(decide("v0.9.3", "v1.0.0", SAME, SAME, [])[0], FULL)

    def test_a_protocol_bump_runs_the_full_gate(self):
        tier, reasons = decide("v0.2.3", "v0.2.4", SAME, (56, 72), [])
        self.assertEqual(tier, FULL)
        self.assertEqual(reasons, ["the wire format moved: protocol 55 -> 56"])

    def test_a_state_bump_runs_the_full_gate(self):
        tier, reasons = decide("v0.2.3", "v0.2.4", SAME, (55, 73), [])
        self.assertEqual(tier, FULL)
        self.assertEqual(reasons, ["the durable state moved: state 72 -> 73"])

    def test_an_agent_loop_change_runs_the_full_gate(self):
        for path in ("src/bun/agent.rs", "src/bun/agent/turn.rs"):
            with self.subTest(path=path):
                tier, reasons = decide("v0.2.3", "v0.2.4", SAME, SAME, [path])
                self.assertEqual(tier, FULL)
                self.assertIn(path, reasons[0])

    def test_a_file_that_only_starts_like_the_agent_does_not_count(self):
        self.assertEqual(decide("v0.2.3", "v0.2.4", SAME, SAME, ["src/bun/agent_notes.md"])[0], PATCH)

    def test_every_reason_is_listed(self):
        tier, reasons = decide("v0.2.4", "v0.3.0", SAME, (56, 73), ["src/bun/agent.rs"])
        self.assertEqual(tier, FULL)
        self.assertEqual(len(reasons), 4)

    def test_a_release_older_than_the_previous_one_is_refused(self):
        with self.assertRaises(ValueError):
            decide("v0.2.4", "v0.2.3", SAME, SAME, [])


class ParseTests(unittest.TestCase):
    def test_versions_with_and_without_v_and_suffix(self):
        self.assertEqual(parse_version("v0.2.4"), (0, 2, 4))
        self.assertEqual(parse_version("0.2.4-dev"), (0, 2, 4))
        with self.assertRaises(ValueError):
            parse_version("staging-v0.2.4-1-1")

    def test_current_is_read_from_this_tree_s_compatibility_rs(self):
        protocol, state = parse_current((REPO / "src/compatibility.rs").read_text())
        self.assertGreater(protocol, 0)
        self.assertGreater(state, 0)

    def test_a_source_without_current_is_refused(self):
        with self.assertRaises(ValueError):
            parse_current("pub const OTHER: u32 = 1;")


if __name__ == "__main__":
    unittest.main()
