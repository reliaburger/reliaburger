"""Promotion runs trusted main-branch scripts, never code from the tag it promotes."""
from pathlib import Path
import re
import unittest

WORKFLOW = Path(__file__).resolve().parents[2] / ".github" / "workflows" / "promote.yml"


def checkout_steps(text):
    """Return each `actions/checkout` step as the block of lines that belongs to it."""
    steps, current = [], None
    for line in text.splitlines():
        if re.match(r"\s*- ", line):
            if current is not None:
                steps.append("\n".join(current))
            current = [line] if "actions/checkout@" in line else None
        elif current is not None:
            current.append(line)
    if current is not None:
        steps.append("\n".join(current))
    return steps


class PromoteWorkflowTests(unittest.TestCase):
    def setUp(self):
        self.text = WORKFLOW.read_text()

    def test_checkout_uses_the_workflow_ref_not_the_tag(self):
        # The job holds `contents: write`. Checking out the tag would run
        # whatever scripts/release/candidate.py the tag's author wrote.
        steps = checkout_steps(self.text)
        self.assertTrue(steps, "promote.yml must check out the repository")
        for step in steps:
            self.assertNotIn("inputs.tag", step)
            self.assertNotIn("refs/tags", step)
            self.assertNotRegex(step, r"\bref:")

    def test_job_only_runs_from_main(self):
        self.assertIn("if: github.ref == 'refs/heads/main'", self.text)

    def test_tag_version_and_commit_come_from_git_objects(self):
        self.assertIn('git show "refs/tags/$RELEASE_TAG:Cargo.toml"', self.text)
        self.assertIn('git rev-parse "refs/tags/$RELEASE_TAG^{commit}"', self.text)
        self.assertNotIn("git rev-parse HEAD", self.text)

    def test_untrusted_inputs_never_expand_inside_shell_scripts(self):
        # `${{ inputs.* }}` is pasted into the script text before bash runs;
        # the inputs travel through `env:` instead.
        in_run, indent = False, 0
        for line in self.text.splitlines():
            stripped = line.lstrip()
            if in_run and stripped and len(line) - len(stripped) <= indent:
                in_run = False
            if stripped.startswith("run:"):
                in_run, indent = True, len(line) - len(stripped)
            if in_run:
                self.assertNotIn("${{ inputs.", line)


if __name__ == "__main__":
    unittest.main()
