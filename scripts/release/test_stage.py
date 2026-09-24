"""Staging publishes the qualified bytes as a pre-release that can never pass for the release."""
import hashlib
import json
from pathlib import Path
import re
import tempfile
import unittest
from unittest import mock

import candidate
from candidate import (candidate_names, identity, record_candidate, staging_step, staging_tag,
                       verify_downloaded, verify_uploaded_assets)
from package import GUEST_METADATA

WORKFLOWS = Path(__file__).resolve().parents[2] / ".github" / "workflows"


def run_blocks(text):
    """Every `run: |` script in a workflow, as text."""
    blocks, current, indent = [], None, 0
    for line in text.splitlines():
        stripped = line.lstrip()
        if current is not None and stripped and len(line) - len(stripped) <= indent:
            blocks.append("\n".join(current))
            current = None
        if stripped.startswith("run:"):
            current, indent = [line], len(line) - len(stripped)
        elif current is not None:
            current.append(line)
    if current is not None:
        blocks.append("\n".join(current))
    return blocks


class StagingTagTests(unittest.TestCase):
    def test_tag_names_the_version_run_and_attempt(self):
        self.assertEqual(staging_tag("v0.1.0", 123, 2), "staging-v0.1.0-123-2")

    def test_tag_is_never_a_release_tag(self):
        tag = staging_tag("v0.1.0", 123, 2)
        self.assertFalse(tag.startswith("v"), "a v* tag matches release tag rules")
        # Promotion's own shell pattern, read from promote.yml.
        promote = (WORKFLOWS / "promote.yml").read_text()
        pattern = re.search(r'\[\[ "\$RELEASE_TAG" =~ (\S+) \]\]', promote).group(1)
        self.assertIsNone(re.search(pattern, tag))
        self.assertIn('[[ "$RELEASE_TAG" != *staging* ]]', promote)
        # candidate.py refuses it as a version too, so fetch/verify-upload can't take it.
        with self.assertRaises(ValueError):
            identity(tag, "reliaburger/reliaburger", "a" * 40, 123, 2)

    def test_invalid_parts_refuse_a_tag(self):
        for arguments in [("0.1.0", 1, 1), ("v0.1.0/../x", 1, 1), ("v0.1.0", 0, 1), ("v0.1.0", 1, True)]:
            with self.subTest(arguments=arguments), self.assertRaises(ValueError):
                staging_tag(*arguments)

    def test_rerun_creates_resumes_or_only_verifies(self):
        tag = "staging-v0.1.0-123-2"
        self.assertEqual(staging_step(None, tag), "create")
        self.assertEqual(staging_step(dict(tag_name=tag, draft=True, prerelease=True), tag), "resume")
        self.assertEqual(staging_step(dict(tag_name=tag, draft=False, prerelease=True), tag), "verify")
        for release in [dict(tag_name=tag, draft=False, prerelease=False),
                        dict(tag_name="v0.1.0", draft=False, prerelease=True)]:
            with self.subTest(release=release), self.assertRaises(ValueError):
                staging_step(release, tag)


class StageTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)
        source = {"file": "ubuntu.img", "url": "https://images.example/ubuntu.img", "sha256": "5" * 64}
        self.pins = {"packages": ["runc"], "images": {"fixture": {"asset": "guest.qcow2", "source": source}}}
        metadata = {"schema": 1, "version": "v0.1.0", "images": {"fixture": {
            "asset": "guest.qcow2", "sha256": hashlib.sha256(b"guest").hexdigest(), "size": 5,
            "signature": "c2ln", "source": {"url": source["url"], "sha256": source["sha256"]}}}}
        contents = {"guest.qcow2": b"guest", GUEST_METADATA: json.dumps(metadata).encode()}
        for name in candidate_names(self.pins):
            (self.directory / name).write_bytes(contents.get(name, name.encode()))
        self.run = dict(id=123, run_attempt=2, head_sha="a" * 40)
        self.digest = record_candidate(self.directory, self.pins, version="v0.1.0",
                                       repository="reliaburger/reliaburger", commit="a" * 40,
                                       run_id=123, run_attempt=2)
        self.document = verify_downloaded(self.directory, self.pins, "reliaburger/reliaburger",
                                          self.run, self.digest)
        self.tag = "staging-v0.1.0-123-2"
        assets = [{"name": path.name, "size": path.stat().st_size, "state": "uploaded",
                   "digest": "sha256:" + hashlib.sha256(path.read_bytes()).hexdigest()}
                  for path in self.directory.iterdir()]
        self.published = dict(tag_name=self.tag, draft=False, prerelease=True, assets=assets)

    def test_version_comes_from_the_digest_pinned_record(self):
        self.assertEqual(self.document["version"], "v0.1.0")
        path = self.directory / "candidate.json"
        path.write_text(path.read_text().replace("v0.1.0", "v0.1.1"))
        with self.assertRaises(ValueError):
            verify_downloaded(self.directory, self.pins, "reliaburger/reliaburger", self.run, self.digest)

    def test_run_must_match_the_record(self):
        for change in [dict(head_sha="b" * 40), dict(id=124), dict(run_attempt=3)]:
            with self.subTest(change=change), self.assertRaises(ValueError):
                verify_downloaded(self.directory, self.pins, "reliaburger/reliaburger",
                                  dict(self.run, **change), self.digest)

    def test_staged_upload_is_a_pre_release_with_every_qualified_byte(self):
        size = (self.directory / "candidate.json").stat().st_size
        verify_uploaded_assets(self.published, self.document, self.digest, size,
                               tag=self.tag, draft=False, prerelease=True)
        for change in [dict(prerelease=False), dict(tag_name="v0.1.0"), dict(draft=True),
                       dict(assets=self.published["assets"][1:])]:
            with self.subTest(change=change), self.assertRaises(ValueError):
                verify_uploaded_assets(dict(self.published, **change), self.document, self.digest,
                                       size, tag=self.tag, draft=False, prerelease=True)

    def stage_with(self, releases):
        """Run stage() against a fake GitHub whose release lookups return `releases` in turn."""
        calls = []
        with mock.patch.object(candidate, "find_release", side_effect=releases), \
                mock.patch.object(candidate, "gh", side_effect=lambda *a: calls.append(a)), \
                mock.patch.object(candidate, "gh_json", return_value={"sha": "a" * 40}):
            tag = candidate.stage(self.directory, self.document, self.digest)
        return tag, calls

    def test_first_run_uploads_a_draft_pre_release_then_publishes_it_never_as_latest(self):
        draft = dict(self.published, draft=True)
        tag, calls = self.stage_with([None, draft, self.published])
        self.assertEqual(tag, self.tag)
        self.assertEqual([call[:2] for call in calls], [("release", "create"), ("release", "edit")])
        create, edit = calls
        self.assertIn("--prerelease", create)
        self.assertIn("--draft", create)
        self.assertEqual(create[create.index("--target") + 1], "a" * 40)
        self.assertIn(str(self.directory / "candidate.json"), create)
        notes = create[create.index("--notes") + 1]
        self.assertIn("not a release", notes)
        self.assertIn(self.digest, notes)
        for call in calls:
            self.assertNotIn("--latest", call)
            self.assertIn("--latest=false", call)
        self.assertIn("--draft=false", edit)
        self.assertIn("--prerelease", edit)

    def test_rerun_after_publication_only_verifies(self):
        _, calls = self.stage_with([self.published, self.published])
        self.assertEqual(calls, [])

    def test_rerun_restarts_an_interrupted_draft(self):
        draft = dict(self.published, draft=True)
        _, calls = self.stage_with([draft, draft, self.published])
        self.assertEqual([call[:2] for call in calls],
                         [("release", "delete"), ("release", "create"), ("release", "edit")])

    def test_rerun_never_touches_a_published_staging_release_with_other_bytes(self):
        changed = dict(self.published, assets=self.published["assets"][1:])
        with self.assertRaises(ValueError):
            self.stage_with([changed, changed])

    def test_staging_tag_pointing_elsewhere_fails(self):
        with mock.patch.object(candidate, "find_release", side_effect=[self.published, self.published]), \
                mock.patch.object(candidate, "gh"), \
                mock.patch.object(candidate, "gh_json", return_value={"sha": "b" * 40}), \
                self.assertRaises(ValueError):
            candidate.stage(self.directory, self.document, self.digest)


class StageWorkflowTests(unittest.TestCase):
    def setUp(self):
        self.text = (WORKFLOWS / "stage.yml").read_text()

    def test_runs_only_from_main_with_main_scripts(self):
        self.assertIn("if: github.ref == 'refs/heads/main'", self.text)
        self.assertNotRegex(self.text, r"\bref:")

    def test_write_token_is_scoped_to_the_job(self):
        top = self.text.split("\njobs:")[0]
        self.assertRegex(top, r"permissions:\n  contents: read\n  actions: read\n")
        self.assertIn("    permissions:\n      contents: write\n      actions: read\n", self.text)

    def test_uses_the_shared_verification_and_never_marks_latest(self):
        scripts = "\n".join(run_blocks(self.text))
        self.assertIn("candidate.py stage-fetch", scripts)
        self.assertIn("candidate.py stage ", scripts)
        self.assertNotIn("gh release", scripts)
        self.assertNotIn("--latest", self.text)

    def test_untrusted_inputs_never_expand_inside_shell_scripts(self):
        for block in run_blocks(self.text):
            self.assertNotIn("${{ inputs.", block)


if __name__ == "__main__":
    unittest.main()
