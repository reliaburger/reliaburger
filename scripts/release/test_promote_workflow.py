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


def job(text, name):
    match = re.search(rf"^  {re.escape(name)}:\n(.*?)(?=^  [a-z][a-z-]*:\n|\Z)", text, re.M | re.S)
    if not match:
        raise AssertionError(f"promote.yml has no job {name!r}")
    return match.group(1)


class StrangerChecksTests(unittest.TestCase):
    """Promotion checks attestations and feeds the Homebrew tap without changing a byte (#1583)."""

    def setUp(self):
        self.text = WORKFLOW.read_text()
        self.promote = job(self.text, "promote")
        self.tap = job(self.text, "homebrew-tap")

    def test_every_asset_is_attested_before_a_draft_exists(self):
        check = self.promote.index("- name: Check every asset carries the candidate build's attestation")
        fetch = self.promote.index("- name: Download and verify the qualified candidate")
        draft = self.promote.index("- name: Upload unchanged assets to a new draft")
        self.assertLess(fetch, check)
        self.assertLess(check, draft)
        self.assertIn("attestations: read", self.promote)
        step = self.promote[check:draft]
        self.assertIn('gh attestation verify "$asset" --repo "$RELEASE_REPOSITORY"', step)
        self.assertIn('--signer-workflow "$RELEASE_REPOSITORY/.github/workflows/build.yml"', step)
        self.assertIn("--source-ref refs/heads/main", step)
        self.assertIn("for asset in dist/*; do", step)

    def test_the_tap_updates_only_after_publication_in_a_job_of_its_own(self):
        self.assertIn("needs: promote", self.tap)
        self.assertIn("if: github.ref == 'refs/heads/main'", self.tap)
        self.assertIn("environment: release", self.tap)
        self.assertNotIn("contents: write", self.tap)
        self.assertNotIn("HOMEBREW_TAP_TOKEN", self.promote)

    def test_the_formula_comes_from_the_published_record_at_the_qualified_digest(self):
        self.assertIn("--pattern candidate.json", self.tap)
        self.assertIn('--qualified-digest "$QUALIFIED_DIGEST"', self.tap)
        self.assertIn("python3 scripts/release/homebrew.py formula --record published/candidate.json", self.tap)
        self.assertIn("python3 scripts/release/homebrew.py publish --formula relish.rb", self.tap)
        self.assertEqual(self.text.count("secrets.HOMEBREW_TAP_TOKEN"), 1)

    def test_bottles_come_from_the_published_binaries_and_reach_the_tap_first(self):
        # Without bottles Homebrew demands current Command Line Tools to "build"
        # a formula that only copies a binary.
        self.assertIn("--pattern 'relish-*'", self.tap)
        self.assertIn("python3 scripts/release/homebrew.py bottles --record published/candidate.json", self.tap)
        self.assertIn("--binaries published --output bottles", self.tap)
        self.assertEqual(self.tap.count('--qualified-digest "$QUALIFIED_DIGEST"'), 2)
        self.assertIn("--bottles bottles/bottles.json", self.tap)
        self.assertIn("--bottles bottles", self.tap.split("homebrew.py publish", 1)[1])


class RegistryEntryJobTests(unittest.TestCase):
    """Promotion writes the MCP Registry entry with the bundles' digests, and publishes nothing (#1695)."""

    def setUp(self):
        self.text = WORKFLOW.read_text()
        self.promote = job(self.text, "promote")
        self.registry = job(self.text, "mcp-registry-entry")

    def test_it_runs_only_after_publication_without_write_access_or_secrets(self):
        self.assertIn("needs: promote", self.registry)
        self.assertIn("if: github.ref == 'refs/heads/main'", self.registry)
        self.assertNotIn("contents: write", self.registry)
        self.assertNotIn("secrets.", self.registry)
        self.assertNotIn("environment:", self.registry)

    def test_it_reads_the_published_record_and_the_tag_s_entry(self):
        self.assertIn("--pattern candidate.json", self.registry)
        self.assertIn('git show "refs/tags/$RELEASE_TAG:server.json"', self.registry)
        self.assertIn("python3 scripts/release/mcp_registry.py --record published/candidate.json", self.registry)
        self.assertIn('--qualified-digest "$QUALIFIED_DIGEST"', self.registry)

    def test_the_entry_is_kept_for_the_maintainer_and_never_published(self):
        # Publishing needs a GitHub login to the organisation (#1695).
        self.assertIn("actions/upload-artifact@", self.registry)
        self.assertNotIn("mcp-publisher publish", self.registry.replace("`mcp-publisher publish`", ""))


if __name__ == "__main__":
    unittest.main()
