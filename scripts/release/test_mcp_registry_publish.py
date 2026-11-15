"""`make mcp-registry-publish` checks the promoted entry before publishing it (#1695).

`gh` and `mcp-publisher` are fakes on PATH: nothing here reaches GitHub or
the registry.
"""
import base64
import hashlib
import json
import os
from pathlib import Path
import tempfile
import unittest
import unittest.mock

from mcp_registry import entry
from mcp_registry_publish import TOKEN_FILE, check_entry, logged_in, main, promote_run, run_title
from package import PLATFORMS, mcpb_name
from test_mcp_registry import record, server

TAG = "v0.2.8"
REPOSITORY = "reliaburger/reliaburger"


def digests():
    return {mcpb_name(platform): hashlib.sha256(platform.encode()).hexdigest()
            for platform in PLATFORMS["relish"]}


def promoted_entry():
    """What the Promote run's artefact holds for TAG."""
    return entry(record(TAG), server(TAG[1:]))


def jwt(expires):
    payload = base64.urlsafe_b64encode(json.dumps({"exp": expires}).encode()).decode().rstrip("=")
    return f"header.{payload}.signature"


class PromoteRunNameTests(unittest.TestCase):
    def test_promote_runs_are_named_for_their_tag(self):
        # The command finds the run by this name; GitHub doesn't list a
        # dispatched run's inputs.
        workflow = (Path(__file__).resolve().parents[2] / ".github/workflows/promote.yml").read_text()
        self.assertIn("\nrun-name: Promote ${{ inputs.tag }}\n", workflow)
        self.assertEqual(run_title(TAG), "Promote v0.2.8")


class CheckEntryTests(unittest.TestCase):
    def test_the_promoted_entry_for_the_published_bundles_passes(self):
        check_entry(promoted_entry(), TAG, REPOSITORY, digests())

    def test_an_entry_for_another_version_is_refused(self):
        with self.assertRaisesRegex(ValueError, "0.2.7"):
            check_entry(entry(record("v0.2.7"), server("0.2.7")), TAG, REPOSITORY, digests())

    def test_a_bundle_that_differs_from_the_published_one_is_refused(self):
        published = digests()
        published["relish-linux-aarch64.mcpb"] = "0" * 64
        with self.assertRaisesRegex(ValueError, "relish-linux-aarch64.mcpb"):
            check_entry(promoted_entry(), TAG, REPOSITORY, published)

    def test_a_bundle_missing_from_the_release_is_refused(self):
        published = digests()
        del published["relish-macos-aarch64.mcpb"]
        with self.assertRaisesRegex(ValueError, "no relish-macos-aarch64.mcpb"):
            check_entry(promoted_entry(), TAG, REPOSITORY, published)

    def test_a_missing_or_extra_package_is_refused(self):
        document = json.loads(promoted_entry())
        missing = dict(document, packages=document["packages"][1:])
        extra = dict(document, packages=document["packages"] + document["packages"][:1])
        for broken in (missing, extra, dict(document, packages=[])):
            with self.subTest(packages=len(broken["packages"])):
                with self.assertRaisesRegex(ValueError, "one bundle per platform"):
                    check_entry(json.dumps(broken), TAG, REPOSITORY, digests())

    def test_another_server_is_refused(self):
        document = dict(json.loads(promoted_entry()), name="io.github.someone/else")
        with self.assertRaisesRegex(ValueError, "someone"):
            check_entry(json.dumps(document), TAG, REPOSITORY, digests())

    def test_a_pre_release_tag_is_refused(self):
        with self.assertRaisesRegex(ValueError, "release tag"):
            check_entry(promoted_entry(), "v0.2.8-rc.1", REPOSITORY, digests())


class LoginTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.home = Path(self.temp.name)

    def save(self, text):
        path = self.home / TOKEN_FILE
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def test_no_token_file_means_logged_out(self):
        self.assertFalse(logged_in(self.home))

    def test_an_unexpired_token_means_logged_in(self):
        self.save(json.dumps({"method": "github", "token": jwt(2000)}))
        self.assertTrue(logged_in(self.home, now=1000))

    def test_an_expired_or_nearly_expired_token_means_logged_out(self):
        self.save(json.dumps({"method": "github", "token": jwt(1030)}))
        self.assertFalse(logged_in(self.home, now=1000))
        self.assertFalse(logged_in(self.home, now=5000))

    def test_an_unreadable_token_means_logged_out(self):
        for text in ("", "{}", '{"token": "not-a-jwt"}', json.dumps({"token": "a.!!!.c"})):
            with self.subTest(text=text):
                self.save(text)
                self.assertFalse(logged_in(self.home, now=1000))


class PublishTests(unittest.TestCase):
    """The whole command, with `gh` and `mcp-publisher` faked on PATH."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.home = self.root / "home"
        self.home.mkdir()
        self.log = self.root / "calls.log"
        self.runs = [
            {"databaseId": 1, "displayTitle": run_title("v0.2.7"), "createdAt": "2026-11-01T00:00:00Z"},
            {"databaseId": 2, "displayTitle": run_title(TAG), "createdAt": "2026-11-10T00:00:00Z"},
            {"databaseId": 3, "displayTitle": run_title(TAG), "createdAt": "2026-11-11T00:00:00Z"},
        ]
        self.release = {"tag_name": TAG, "draft": False, "prerelease": False,
                        "assets": [{"name": name, "digest": "sha256:" + digest}
                                   for name, digest in digests().items()]}
        self.artefact = self.root / "artefact.json"
        self.artefact.write_text(promoted_entry())
        self.fake_tools()

    def fake_tools(self):
        """A `gh` that answers from files, and an `mcp-publisher` that logs where it ran."""
        directory = self.root / "fake-bin"
        directory.mkdir()
        (self.root / "runs.json").write_text(json.dumps(self.runs))
        (self.root / "release.json").write_text(json.dumps(self.release))
        gh = directory / "gh"
        gh.write_text(
            "#!/bin/sh\n"
            f'echo "gh $*" >> "{self.log}"\n'
            'case "$1 $2" in\n'
            f'  "run list") cat "{self.root}/runs.json" ;;\n'
            '  "run download")\n'
            '    while [ "$#" -gt 0 ]; do [ "$1" = "--dir" ] && dir="$2"; shift; done\n'
            f'    mkdir -p "$dir" && cp "{self.artefact}" "$dir/server.json" ;;\n'
            f'  api*) cat "{self.root}/release.json" ;;\n'
            '  *) exit 2 ;;\n'
            'esac\n')
        publisher = directory / "mcp-publisher"
        publisher.write_text(
            "#!/bin/sh\n"
            f'echo "mcp-publisher $* in $(pwd)" >> "{self.log}"\n'
            f'if [ "$1" = publish ]; then cp server.json "{self.root}/published.json"; fi\n')
        for script in (gh, publisher):
            script.chmod(0o755)
        self.environment = {**os.environ, "HOME": str(self.home),
                            "PATH": f"{directory}{os.pathsep}{os.environ['PATH']}"}

    def run_main(self, *extra):
        with unittest.mock.patch.dict(os.environ, self.environment, clear=True):
            return main(["--tag", TAG, *extra])

    def calls(self):
        return self.log.read_text().splitlines()

    def publisher_calls(self):
        return [call for call in self.calls() if call.startswith("mcp-publisher")]

    def test_it_logs_in_then_publishes_the_newest_promote_runs_entry(self):
        self.assertEqual(self.run_main(), 0)
        self.assertIn(f"gh run download 3 --repo {REPOSITORY} --name mcp-registry-server-json", self.calls()[1])
        login, publish = self.publisher_calls()
        self.assertTrue(login.startswith("mcp-publisher login github in"), login)
        self.assertTrue(publish.startswith("mcp-publisher publish in"), publish)
        self.assertEqual((self.root / "published.json").read_text(), promoted_entry())

    def test_a_saved_token_skips_the_login(self):
        token = self.home / TOKEN_FILE
        token.parent.mkdir(parents=True)
        token.write_text(json.dumps({"method": "github", "token": jwt(4_000_000_000)}))
        self.assertEqual(self.run_main(), 0)
        self.assertEqual([call.split(" in ")[0] for call in self.publisher_calls()],
                         ["mcp-publisher publish"])

    def test_a_bundle_digest_that_differs_publishes_nothing(self):
        self.release["assets"][0]["digest"] = "sha256:" + "0" * 64
        (self.root / "release.json").write_text(json.dumps(self.release))
        self.assertEqual(self.run_main(), 1)
        self.assertEqual(self.publisher_calls(), [])
        self.assertFalse((self.root / "published.json").exists())

    def test_a_draft_release_publishes_nothing(self):
        self.release["draft"] = True
        (self.root / "release.json").write_text(json.dumps(self.release))
        self.assertEqual(self.run_main(), 1)
        self.assertEqual(self.publisher_calls(), [])

    def test_an_explicit_run_is_used_as_given(self):
        self.assertEqual(self.run_main("--run", "42"), 0)
        self.assertFalse(any(call.startswith("gh run list") for call in self.calls()))
        self.assertIn("gh run download 42 ", self.calls()[0])

    def test_no_promote_run_for_the_tag_publishes_nothing(self):
        with unittest.mock.patch.dict(os.environ, self.environment, clear=True):
            with self.assertRaisesRegex(ValueError, "Promote v0.2.9"):
                promote_run(REPOSITORY, "v0.2.9")
            self.assertEqual(main(["--tag", "v0.2.9"]), 1)
        self.assertEqual(self.publisher_calls(), [])


if __name__ == "__main__":
    unittest.main()
