"""Candidate qualification binds every published byte to a source/run identity."""
import hashlib
import json
from pathlib import Path
import tempfile
import unittest

from candidate import (agreed_source_digest, bundle_digest, candidate_names, record_candidate, staging_notes,
                       verify_candidate, verify_run, verify_uploaded_assets)
from package import GUEST_METADATA, PLATFORMS

# What `relish agent source version` puts in the envelope's `relish` block.
SOURCE_DIGEST = "sha256:" + "c" * 64


class CandidateTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)
        source = {"file": "ubuntu.img", "url": "https://images.example/ubuntu.img", "sha256": "5" * 64}
        self.pins = {"packages": ["runc"], "images": {"fixture": {"asset": "guest.qcow2", "source": source}}}
        metadata = {"schema": 1, "version": "v0.1.0", "images": {"fixture": {
            "asset": "guest.qcow2", "sha256": hashlib.sha256(b"guest").hexdigest(), "size": 5,
            "signature": "c2ln", "source": {"url": source["url"], "sha256": source["sha256"]}}}}
        self.identity = dict(version="v0.1.0", repository="reliaburger/reliaburger",
                             commit="a" * 40, run_id=123, run_attempt=2)
        contents = {"guest.qcow2": b"guest", GUEST_METADATA: json.dumps(metadata).encode()}
        for name in candidate_names(self.pins):
            (self.directory / name).write_bytes(contents.get(name, name.encode()))
        self.run = dict(id=123, run_attempt=2, head_sha="a" * 40, head_branch="main",
                        event="workflow_dispatch", path=".github/workflows/build.yml",
                        status="completed", conclusion="success",
                        repository={"full_name": "reliaburger/reliaburger"},
                        head_repository={"full_name": "reliaburger/reliaburger"})

    def record(self):
        return record_candidate(self.directory, self.pins, source_digest=SOURCE_DIGEST, **self.identity)

    def verify(self, digest):
        return verify_candidate(self.directory, self.pins, digest, **self.identity)

    def test_complete_candidate_round_trips_without_changing_any_assets(self):
        before = {p.name: p.read_bytes() for p in self.directory.iterdir()}
        digest = self.record()
        record = self.verify(digest)
        self.assertEqual(record["commit"], "a" * 40)
        self.assertEqual(before, {p.name: p.read_bytes() for p in self.directory.iterdir()
                                  if p.name != "candidate.json"})
        self.assertEqual(set(record["assets"]), set(before))
        verify_run(self.run, self.identity["repository"], self.identity["commit"], 123)

    def test_candidate_has_no_intel_macos_build(self):
        # 0.2.1 is the last release with `relish-macos-x86_64` (#1018), so
        # staging and promotion must not wait for one.
        names = candidate_names(self.pins)
        self.assertIn("relish-macos-aarch64", names)
        self.assertNotIn("relish-macos-x86_64", names)
        self.assertNotIn("relish-macos-x86_64.sig", names)

    def test_each_bundle_and_its_signature_are_candidate_assets(self):
        # Like every binary, each MCP Bundle ships with its release-key `.sig`
        # (#1695), so staging and promotion refuse a release without one.
        names = candidate_names(self.pins)
        for platform in PLATFORMS["relish"]:
            self.assertIn(f"relish-{platform}.mcpb", names)
            self.assertIn(f"relish-{platform}.mcpb.sig", names)

    def test_an_upload_without_a_bundle_signature_is_refused(self):
        digest = self.record()
        document = json.loads((self.directory / "candidate.json").read_text())
        assets = [{"name": path.name, "size": path.stat().st_size,
                   "digest": "sha256:" + hashlib.sha256(path.read_bytes()).hexdigest(),
                   "state": "uploaded"} for path in self.directory.iterdir()
                  if path.name != "relish-linux-x86_64.mcpb.sig"]
        release = dict(tag_name="v0.1.0", draft=True, prerelease=False, assets=assets)
        with self.assertRaisesRegex(ValueError, "inventory"):
            verify_uploaded_assets(release, document, digest, (self.directory / "candidate.json").stat().st_size)

    def test_staging_notes_install_the_candidate_as_a_laptop_cluster(self):
        # Without a mode the installer only explains the choices (#670).
        notes = staging_notes(self.identity, "v0.1.0-staging-123-2", "b" * 64)
        self.assertIn("RELIABURGER_RELEASE_BASE_URL=https://github.com/reliaburger/reliaburger/releases/download/"
                      "v0.1.0-staging-123-2 sh -s -- --laptop\n", notes)

    def test_any_changed_missing_or_extra_asset_refuses_promotion(self):
        digest = self.record()
        for name in sorted(candidate_names(self.pins)):
            with self.subTest(name=name):
                path = self.directory / name
                original = path.read_bytes()
                path.write_bytes(original + b"changed")
                with self.assertRaises(ValueError):
                    self.verify(digest)
                path.unlink()
                with self.assertRaises(ValueError):
                    self.verify(digest)
                path.write_bytes(original)
        (self.directory / "unexpected").write_bytes(b"new asset")
        with self.assertRaises(ValueError):
            self.verify(digest)

    def test_a_modified_record_cannot_supply_its_own_qualification_digest(self):
        digest = self.record()
        path = self.directory / "candidate.json"
        path.write_text(path.read_text() + "\n")
        with self.assertRaises(ValueError):
            self.verify(digest)

    def test_source_version_repository_and_run_must_match_qualification(self):
        digest = self.record()
        for key, value in [("commit", "b" * 40), ("version", "v0.2.0"),
                           ("repository", "other/project"), ("run_id", 124), ("run_attempt", 3)]:
            with self.subTest(key=key):
                expected = dict(self.identity, **{key: value})
                with self.assertRaises(ValueError):
                    verify_candidate(self.directory, self.pins, digest, **expected)

    def test_incomplete_or_unpinned_candidate_is_not_recorded(self):
        path = self.directory / "guest.qcow2"
        path.write_bytes(b"wrong image")
        with self.assertRaises(ValueError):
            self.record()
        self.assertFalse((self.directory / "candidate.json").exists())
        path.write_bytes(b"guest")
        (self.directory / "metadata.json").unlink()
        with self.assertRaises(ValueError):
            self.record()
        self.assertFalse((self.directory / "candidate.json").exists())

    def test_guest_image_built_from_another_source_is_not_recorded(self):
        path = self.directory / GUEST_METADATA
        metadata = json.loads(path.read_text())
        metadata["images"]["fixture"]["source"]["sha256"] = "6" * 64
        path.write_text(json.dumps(metadata))
        with self.assertRaises(ValueError):
            self.record()
        del metadata["images"]["fixture"]
        path.write_text(json.dumps(metadata))
        with self.assertRaises(ValueError):
            self.record()
        self.assertFalse((self.directory / "candidate.json").exists())

    def test_candidate_creation_refuses_overwrite(self):
        digest = self.record()
        with self.assertRaises(FileExistsError):
            self.record()
        self.verify(digest)

    def test_symlinks_and_directories_are_not_candidate_assets(self):
        digest = self.record()
        path = self.directory / "bun-linux-aarch64"
        path.unlink()
        path.symlink_to(self.directory / "bun-linux-x86_64")
        with self.assertRaises(ValueError):
            self.verify(digest)
        path.unlink()
        path.mkdir()
        with self.assertRaises(ValueError):
            self.verify(digest)

    def test_pr_failed_foreign_or_different_source_runs_are_not_candidates(self):
        for key, value in [("event", "pull_request"), ("conclusion", "failure"),
                           ("status", "in_progress"), ("head_branch", "unreviewed"),
                           ("head_sha", "b" * 40), ("id", 124), ("run_attempt", 0),
                           ("path", ".github/workflows/other.yml"),
                           ("repository", {"full_name": "other/project"}),
                           ("head_repository", {"full_name": "other/project"})]:
            with self.subTest(key=key), self.assertRaises(ValueError):
                verify_run(dict(self.run, **{key: value}), self.identity["repository"],
                           self.identity["commit"], 123)

    def test_uploaded_draft_must_contain_every_qualified_byte(self):
        digest = self.record()
        document = self.verify(digest)
        assets = [{"name": path.name, "size": path.stat().st_size,
                   "digest": "sha256:" + hashlib.sha256(path.read_bytes()).hexdigest(),
                   "state": "uploaded"} for path in self.directory.iterdir()]
        release = dict(tag_name="v0.1.0", draft=True, prerelease=False, assets=assets)
        verify_uploaded_assets(release, document, digest, (self.directory / "candidate.json").stat().st_size)
        for change in [dict(tag_name="v0.2.0"), dict(draft=False), dict(prerelease=True),
                       dict(assets=assets[:-1]),
                       dict(assets=assets + assets[:1]),
                       dict(assets=[dict(assets[0], digest="sha256:" + "0" * 64)] + assets[1:]),
                       dict(assets=[dict(assets[0], state="new")] + assets[1:]),
                       dict(assets=[dict(assets[0], size=0)] + assets[1:])]:
            with self.subTest(change=change), self.assertRaises(ValueError):
                verify_uploaded_assets(dict(release, **change), document, digest,
                                       (self.directory / "candidate.json").stat().st_size)

    def test_invalid_identity_values_refuse_before_writing(self):
        for key, value in [("version", "../main"), ("commit", "main"),
                           ("repository", "../project"), ("run_id", 0), ("run_attempt", True)]:
            with self.subTest(key=key), self.assertRaises(ValueError):
                record_candidate(self.directory, self.pins, source_digest=SOURCE_DIGEST,
                                 **dict(self.identity, **{key: value}))
        self.assertFalse((self.directory / "candidate.json").exists())

    def test_the_record_names_the_embedded_source_bundle(self):
        # relish embeds the tree it was built from (#1570); the record says
        # which tree, so `relish.source_digest` can be checked against it (#1694).
        digest = self.record()
        self.assertEqual(json.loads((self.directory / "candidate.json").read_text())["source_digest"],
                         SOURCE_DIGEST)
        self.assertEqual(self.verify(digest)["source_digest"], SOURCE_DIGEST)
        document = verify_candidate(self.directory, self.pins, digest, source_digest=SOURCE_DIGEST,
                                    **self.identity)
        self.assertEqual(document["source_digest"], SOURCE_DIGEST)

    def test_verification_refuses_another_source_digest(self):
        digest = self.record()
        with self.assertRaises(ValueError):
            verify_candidate(self.directory, self.pins, digest, source_digest="sha256:" + "d" * 64,
                             **self.identity)

    def test_a_record_without_a_source_digest_does_not_verify(self):
        digest = self.record()
        path = self.directory / "candidate.json"
        document = json.loads(path.read_text())
        del document["source_digest"]
        encoded = (json.dumps(document, indent=2, sort_keys=True) + "\n").encode()
        path.write_bytes(encoded)
        self.assertNotEqual(digest, hashlib.sha256(encoded).hexdigest())
        with self.assertRaises(ValueError):
            self.verify(hashlib.sha256(encoded).hexdigest())

    def test_a_malformed_source_digest_is_not_recorded(self):
        for value in (None, "", "c" * 64, "sha256:" + "C" * 64, "sha256:" + "c" * 63, "sha512:" + "c" * 64):
            with self.subTest(value=value), self.assertRaises(ValueError):
                record_candidate(self.directory, self.pins, source_digest=value, **self.identity)
        self.assertFalse((self.directory / "candidate.json").exists())


def source_version(**relish):
    """The envelope `relish agent source version --offline` prints."""
    block = dict(version="0.2.8", commit="a" * 40, dirty=False, source_digest=SOURCE_DIGEST)
    block.update(relish)
    return json.dumps(dict(schema="relish.agent/v1", command="source version", ok=True, relish=block,
                           data=dict(digest=block["source_digest"]), truncated=False, next=[],
                           warnings=[]))


class BundleDigestTests(unittest.TestCase):
    """Each build job reads the digest from the relish it built (#1694)."""

    def test_a_clean_release_build_gives_its_digest(self):
        self.assertEqual(bundle_digest(source_version()), SOURCE_DIGEST)

    def test_a_dirty_or_unknown_tree_is_refused(self):
        # build.rs already refuses a dirty tree when the build job asks it
        # to; this catches a build that stopped asking.
        for dirty in (True, None):
            with self.subTest(dirty=dirty), self.assertRaises(ValueError):
                bundle_digest(source_version(dirty=dirty))

    def test_a_missing_or_malformed_digest_is_refused(self):
        for value in (None, "", "c" * 64, "sha256:xyz"):
            with self.subTest(value=value), self.assertRaises(ValueError):
                bundle_digest(source_version(source_digest=value))

    def test_a_failed_answer_or_one_whose_data_disagrees_is_refused(self):
        answer = json.loads(source_version())
        for change in (dict(ok=False), dict(command="version"), dict(data=dict(digest="sha256:" + "d" * 64))):
            with self.subTest(change=change), self.assertRaises(ValueError):
                bundle_digest(json.dumps(dict(answer, **change)))
        with self.assertRaises(ValueError):
            bundle_digest("not json")


class AgreedSourceDigestTests(unittest.TestCase):
    """Every platform's relish must carry the same source (#1694)."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)
        for platform in PLATFORMS["relish"]:
            (self.directory / f"relish-{platform}").write_text(SOURCE_DIGEST + "\n")

    def test_one_digest_when_every_platform_agrees(self):
        self.assertEqual(agreed_source_digest(self.directory), SOURCE_DIGEST)

    def test_a_platform_with_other_source_is_refused(self):
        (self.directory / "relish-macos-aarch64").write_text("sha256:" + "d" * 64 + "\n")
        with self.assertRaises(ValueError):
            agreed_source_digest(self.directory)

    def test_a_missing_or_extra_platform_is_refused(self):
        (self.directory / "relish-linux-aarch64").unlink()
        with self.assertRaises(ValueError):
            agreed_source_digest(self.directory)
        (self.directory / "relish-linux-aarch64").write_text(SOURCE_DIGEST)
        (self.directory / "relish-macos-x86_64").write_text(SOURCE_DIGEST)
        with self.assertRaises(ValueError):
            agreed_source_digest(self.directory)

    def test_a_malformed_digest_is_refused(self):
        for platform in PLATFORMS["relish"]:
            (self.directory / f"relish-{platform}").write_text("c" * 64)
        with self.assertRaises(ValueError):
            agreed_source_digest(self.directory)


if __name__ == "__main__":
    unittest.main()
