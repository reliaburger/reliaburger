"""Tests for queue_conflicts.py: which queued PR a branch conflicts with.

The fixture is the shape of 24 November 2026's ejections (#2264): two PRs
each add a row at the top of `docs/flakes.md`'s table. The second merges
cleanly onto the train, but not onto the queue entry built from the first.
"""
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))
import queue_conflicts  # noqa: E402

TABLE = "| Test | Status |\n|---|---|\n| old | Open |\n"


class Repository:
    """A throwaway git repository with one helper per kind of commit."""

    def __init__(self, path):
        self.path = path
        self.git("init", "--quiet", "--initial-branch=train")
        self.git("config", "user.email", "test@example.com")
        self.git("config", "user.name", "Test")

    def git(self, *arguments):
        result = subprocess.run(["git", *arguments], cwd=self.path, capture_output=True,
                                text=True, check=True)
        return result.stdout.strip()

    def commit(self, files, message, parent=None):
        """Commit `files` on a branch from `parent` (default: HEAD); return its sha."""
        if parent:
            self.git("checkout", "--quiet", "--detach", parent)
        for name, text in files.items():
            path = Path(self.path, name)
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text)
            self.git("add", name)
        self.git("commit", "--quiet", "-m", message)
        return self.git("rev-parse", "HEAD")

    def merge(self, base, head, message):
        """The queue's merge commit of `head` onto `base`."""
        self.git("checkout", "--quiet", "--detach", base)
        self.git("merge", "--quiet", "--no-ff", "-m", message, head)
        return self.git("rev-parse", "HEAD")


def entry(position, number, head=None, state="AWAITING_CHECKS"):
    return {"position": position, "state": state, "pullRequest": {"number": number},
            "headCommit": {"oid": head} if head else None}


class QueueConflictTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.repo = Repository(self.tmp.name)
        self.train = self.repo.commit({"docs/flakes.md": TABLE, "src/a.rs": "a\n"}, "train")
        first = self.repo.commit({"docs/flakes.md": TABLE + "| first | Open |\n"}, "first", self.train)
        code = self.repo.commit({"src/a.rs": "a\nb\n"}, "code", self.train)
        self.group_first = self.repo.merge(self.train, first, "queue: first")
        self.group_code = self.repo.merge(self.group_first, code, "queue: code")
        self.second = self.repo.commit({"docs/flakes.md": TABLE + "| second | Open |\n"}, "second", self.train)
        self.unrelated = self.repo.commit({"src/b.rs": "b\n"}, "unrelated", self.train)

    def tearDown(self):
        self.tmp.cleanup()

    def conflicts(self, base, head):
        return queue_conflicts.conflicting_paths(base, head, cwd=self.tmp.name)

    def test_a_branch_clean_on_the_train_conflicts_with_the_entry_ahead(self):
        self.assertEqual(self.conflicts(self.train, self.second), [])
        ahead = [entry(1, 101, self.group_first), entry(2, 102, self.group_code)]
        self.assertEqual(queue_conflicts.find_blocker(self.second, self.train, ahead, self.conflicts),
                         (101, ["docs/flakes.md"]))

    def test_a_branch_that_touches_nothing_queued_merges_cleanly(self):
        ahead = [entry(1, 101, self.group_first), entry(2, 102, self.group_code)]
        self.assertIsNone(queue_conflicts.find_blocker(self.unrelated, self.train, ahead, self.conflicts))

    def test_a_conflict_with_the_train_itself_names_no_pull_request(self):
        moved = self.repo.commit({"docs/flakes.md": TABLE + "| landed | Fixed |\n"}, "landed", self.train)
        self.assertEqual(queue_conflicts.find_blocker(self.second, moved, [], self.conflicts),
                         (None, ["docs/flakes.md"]))

    def test_unbuilt_entries_add_no_base(self):
        ahead = [entry(1, 103, state="UNMERGEABLE")]
        self.assertIsNone(queue_conflicts.find_blocker(self.second, self.train, ahead, self.conflicts))

    def test_a_queued_pull_request_is_checked_against_the_entries_ahead_of_it(self):
        entries = [entry(3, 103), entry(1, 101), entry(2, 102)]
        self.assertEqual([e["pullRequest"]["number"] for e in queue_conflicts.queue_ahead(entries, 102)], [101])
        self.assertEqual([e["pullRequest"]["number"] for e in queue_conflicts.queue_ahead(entries, 999)],
                         [101, 102, 103])

    def test_a_commit_git_does_not_have_is_an_error_not_a_clean_merge(self):
        with self.assertRaises(RuntimeError):
            self.conflicts(self.train, "0" * 40)


class UnionMergeTests(unittest.TestCase):
    """Why `merge=union` stays off `docs/flakes.md` and `CHANGELOG.md`.

    Each case is one `git merge-file --union` over a base, the queue's side
    and a PR's side, and each shows union producing a file that a plain merge
    would have stopped on and a reviewer would have fixed. The first is the
    real 0.3.3 pair, #2249 against #2234, cut down to its two rows.
    """

    def union(self, ours, base, theirs):
        with tempfile.TemporaryDirectory() as directory:
            paths = []
            for name, text in (("ours", ours), ("base", base), ("theirs", theirs)):
                path = Path(directory, name)
                path.write_text(text)
                paths.append(str(path))
            result = subprocess.run(["git", "merge-file", "--union", "-p", *paths],
                                    capture_output=True, text=True)
            return result.stdout

    def test_union_keeps_a_stale_flake_row_beside_its_fixed_copy(self):
        header = "| Test | Status |\n|---|---|\n"
        base = header + "| other | Open |\n"
        ours = header + "| job_recovery | Fixed (#2236) |\n| other | Open |\n"
        theirs = header + "| six timeouts | Open (#2234) |\n| job_recovery | Open (#2236) |\n| other | Open |\n"
        merged = self.union(ours, base, theirs)
        self.assertIn("| job_recovery | Fixed (#2236) |", merged)
        self.assertIn("| job_recovery | Open (#2236) |", merged)

    def test_union_files_a_bullet_under_the_wrong_changelog_heading(self):
        base = "### Changed\n\n- c\n"
        ours = "### Changed\n\n- c\n\n### Fixed\n\n- f\n"
        theirs = "### Changed\n\n- c\n- d\n"
        merged = self.union(ours, base, theirs)
        self.assertTrue(merged.endswith("### Fixed\n\n- f\n- d\n"), merged)

    def test_union_keeps_both_wordings_of_an_edited_bullet(self):
        base = "### Changed\n\n- c\n"
        ours = "### Changed\n\n- c, reworded\n"
        theirs = "### Changed\n\n- c\n- d\n"
        self.assertEqual(self.union(ours, base, theirs), "### Changed\n\n- c, reworded\n- c\n- d\n")

    def test_neither_file_has_a_union_driver(self):
        root = Path(__file__).resolve().parents[2]
        result = subprocess.run(["git", "check-attr", "merge", "--", "docs/flakes.md", "CHANGELOG.md"],
                                cwd=root, capture_output=True, text=True, check=True)
        self.assertNotIn("union", result.stdout)


if __name__ == "__main__":
    unittest.main()
