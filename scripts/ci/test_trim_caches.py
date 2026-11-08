"""Which Actions caches trim_caches.py deletes, and the workflow that runs it (#1724)."""
from pathlib import Path
import unittest
from unittest.mock import patch

import trim_caches

REPO = Path(__file__).resolve().parents[2]
WORKFLOW = REPO / ".github" / "workflows" / "cache-trim.yml"


def cache(key, ref="refs/heads/main", created="2026-11-07T01:00:00Z", size=1_000_000, number=1):
    return dict(id=number, key=key, ref=ref, sizeInBytes=size, createdAt=created)


def reasons(caches, open_pulls=frozenset({490}), branches=frozenset({"main", "release-0-2-8"})):
    return [(row["key"], row["ref"], reason) for row, reason in trim_caches.stale(caches, open_pulls, branches)]


class Family(unittest.TestCase):
    def test_trailing_hashes_and_run_ids_are_not_part_of_the_family(self):
        self.assertEqual(trim_caches.family("v0-rust-portable-Linux-x64-5dedfa29-e5bd4a76"), "v0-rust-portable-Linux-x64")
        self.assertEqual(trim_caches.family("task-array-bench-42404660217"), "task-array-bench")
        self.assertEqual(trim_caches.family(
            "test-images-X64-5aa5e019963dff1184cb8da1592a40612981138817d2be1501676b8a55b1d4c8"), "test-images-X64")
        self.assertEqual(trim_caches.family("v0-rust-macos-Darwin-arm64-3dd9360d-e5bd4a76"), "v0-rust-macos-Darwin-arm64")

    def test_different_jobs_are_different_families(self):
        self.assertNotEqual(trim_caches.family("v0-rust-lint-Linux-x64-5dedfa29-e5bd4a76"),
                            trim_caches.family("v0-rust-portable-Linux-x64-5dedfa29-e5bd4a76"))


class Stale(unittest.TestCase):
    def test_current_caches_on_main_and_open_pull_requests_stay(self):
        caches = [cache("v0-rust-portable-Linux-x64-5dedfa29-e5bd4a76"),
                  cache("v0-rust-lint-Linux-x64-5dedfa29-e5bd4a76"),
                  cache("v0-rust-appliance-x86_64-Linux-x64-fce45c49-72e4bacb", ref="refs/pull/490/merge")]
        self.assertEqual(reasons(caches), [])

    def test_a_closed_pull_requests_caches_go(self):
        caches = [cache("v0-rust-appliance-Linux-x64-fce45c49-72e4bacb", ref="refs/pull/1700/merge")]
        self.assertEqual(reasons(caches), [("v0-rust-appliance-Linux-x64-fce45c49-72e4bacb",
                                            "refs/pull/1700/merge", "pull request #1700 is closed")])

    def test_a_deleted_branchs_caches_go(self):
        ref = "refs/heads/gh-readonly-queue/release-0-2-8/pr-1744-abc"
        self.assertEqual(reasons([cache("v0-rust-portable-Linux-x64-1-2", ref=ref)]),
                         [("v0-rust-portable-Linux-x64-1-2", ref,
                           "branch gh-readonly-queue/release-0-2-8/pr-1744-abc is gone")])
        self.assertEqual(reasons([cache("v0-rust-portable-Linux-x64-1-2", ref="refs/heads/release-0-2-8")]), [])

    def test_an_older_cache_of_the_same_family_on_the_same_ref_is_superseded(self):
        old = cache("v0-rust-portable-Linux-x64-5dedfa29-aaaaaaaa", created="2026-11-06T01:00:00Z")
        new = cache("v0-rust-portable-Linux-x64-5dedfa29-e5bd4a76", created="2026-11-07T01:00:00Z")
        for order in ([old, new], [new, old]):
            with self.subTest(first=order[0]["key"]):
                self.assertEqual(reasons(order), [("v0-rust-portable-Linux-x64-5dedfa29-aaaaaaaa", "refs/heads/main",
                                                   "superseded by v0-rust-portable-Linux-x64-5dedfa29-e5bd4a76")])

    def test_the_same_family_on_another_ref_is_not_superseded(self):
        caches = [cache("v0-rust-appliance-Linux-x64-1-2", ref="refs/pull/490/merge", created="2026-11-06T01:00:00Z"),
                  cache("v0-rust-appliance-Linux-x64-3-4", created="2026-11-07T01:00:00Z")]
        self.assertEqual(reasons(caches), [])

    def test_only_the_newest_of_three_stays(self):
        caches = [cache(f"task-array-bench-{run}", created=f"2026-11-0{day}T01:00:00Z")
                  for run, day in [(100, 5), (300, 7), (200, 6)]]
        self.assertEqual(sorted(key for key, _, _ in reasons(caches)), ["task-array-bench-100", "task-array-bench-200"])


class Command(unittest.TestCase):
    def run_main(self, arguments):
        listed = [cache("task-array-bench-1", created="2026-11-06T01:00:00Z", number=11),
                  cache("task-array-bench-2", created="2026-11-07T01:00:00Z", number=12)]
        calls = []

        def gh(command, **_):
            calls.append(command)
            return listed if command[:2] == ["cache", "list"] else []

        with patch.object(trim_caches, "gh_json", side_effect=gh), \
             patch.object(trim_caches, "branch_names", return_value={"main"}), \
             patch.object(trim_caches, "delete") as delete:
            self.assertEqual(trim_caches.main(["--repository", "reliaburger/reliaburger", *arguments]), 0)
        return delete

    def test_without_apply_nothing_is_deleted(self):
        self.run_main([]).assert_not_called()

    def test_apply_deletes_exactly_the_stale_caches(self):
        delete = self.run_main(["--apply"])
        delete.assert_called_once_with("reliaburger/reliaburger", 11)


class Workflow(unittest.TestCase):
    def test_a_scheduled_workflow_applies_the_rule(self):
        text = WORKFLOW.read_text()
        self.assertIn("  schedule:\n", text)
        self.assertIn("  workflow_dispatch:\n", text)
        # Deleting a cache needs actions: write, and nothing else.
        self.assertIn("permissions:\n  actions: write\n  contents: read\n  pull-requests: read\n", text)
        self.assertIn("python3 scripts/ci/trim_caches.py --repository \"$GITHUB_REPOSITORY\" --apply", text)


if __name__ == "__main__":
    unittest.main()
