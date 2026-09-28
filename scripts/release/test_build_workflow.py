"""The candidate build tells qualification its digest without a 2 GB download."""
from pathlib import Path
import re
import unittest

WORKFLOW = Path(__file__).resolve().parents[2] / ".github" / "workflows" / "build.yml"


def steps(text):
    """Return each step of every job as the block of lines that belongs to it."""
    blocks, current, indent = [], None, None
    for line in text.splitlines():
        match = re.match(r"(\s*)- (name|uses):", line)
        if match and (indent is None or len(match.group(1)) == indent):
            if current is not None:
                blocks.append("\n".join(current))
            current, indent = [line], len(match.group(1))
        elif current is not None:
            if line.strip() and len(line) - len(line.lstrip()) < indent:
                blocks.append("\n".join(current))
                current, indent = None, None
            else:
                current.append(line)
    if current is not None:
        blocks.append("\n".join(current))
    return blocks


def step_named(text, name):
    for block in steps(text):
        if re.search(rf"- name: {re.escape(name)}\s*$", block.splitlines()[0]):
            return block
    raise AssertionError(f"build.yml has no step named {name!r}")


class CandidateManifestTests(unittest.TestCase):
    def setUp(self):
        self.text = WORKFLOW.read_text()

    def test_the_digest_reaches_the_job_log(self):
        # The job summary alone can't be read without a browser; the log can
        # (`gh run view --log`).
        record = step_named(self.text, "Record exact candidate identity and all asset digests")
        self.assertIn("Qualification manifest SHA-256: ", record)
        log_lines = [line for line in record.splitlines()
                     if "Qualification manifest SHA-256" in line and "GITHUB_STEP_SUMMARY" not in line]
        self.assertTrue(any(line.lstrip().startswith("echo ") for line in log_lines), record)

    def test_the_manifest_is_uploaded_on_its_own_for_a_day(self):
        manifest = step_named(self.text, "Preserve the qualification manifest on its own")
        self.assertIn("actions/upload-artifact@", manifest)
        self.assertIn("name: candidate-manifest-${{ github.sha }}-${{ github.run_attempt }}", manifest)
        self.assertIn("path: dist/candidate.json", manifest)
        self.assertIn("retention-days: 1", manifest)
        self.assertIn("if-no-files-found: error", manifest)

    def test_the_whole_candidate_is_still_preserved(self):
        # Staging and promotion verify every byte of this artefact against
        # the digest; the manifest artefact is only a shortcut to the digest.
        candidate = step_named(self.text, "Preserve signed candidate without publishing a release")
        self.assertIn("name: candidate-${{ github.sha }}-${{ github.run_attempt }}", candidate)
        self.assertIn("path: dist/*", candidate)
        self.assertIn("retention-days: 90", candidate)


if __name__ == "__main__":
    unittest.main()
