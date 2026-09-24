"""Candidate qualification binds every published byte to a source/run identity."""
import hashlib
import json
from pathlib import Path
import tempfile
import unittest

from candidate import candidate_names, record_candidate, verify_candidate, verify_run, verify_uploaded_assets
from package import GUEST_METADATA


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
        return record_candidate(self.directory, self.pins, **self.identity)

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
                record_candidate(self.directory, self.pins, **dict(self.identity, **{key: value}))
        self.assertFalse((self.directory / "candidate.json").exists())


if __name__ == "__main__":
    unittest.main()
