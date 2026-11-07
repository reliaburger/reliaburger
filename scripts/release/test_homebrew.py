"""The Homebrew formula pins exactly the published relish bytes, and the tap gets it (#1583)."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import unittest
import unittest.mock

from homebrew import BOTTLE_TAGS, FORMULA_PATH, TOKEN, bottles, formula, main, publish
from package import PLATFORMS


def record(version="v0.2.7", channel=None, platforms=PLATFORMS["relish"]):
    assets = {f"relish-{platform}": {"sha256": hashlib.sha256(platform.encode()).hexdigest(), "size": 1}
              for platform in platforms}
    assets["bun-linux-x86_64"] = {"sha256": "f" * 64, "size": 1}
    document = dict(schema=1, version=version, repository="reliaburger/reliaburger", commit="a" * 40,
                    run_id=1, run_attempt=1, assets=assets)
    if channel:
        document["channel"] = channel
    return (json.dumps(document, indent=2, sort_keys=True) + "\n").encode()


class FormulaTests(unittest.TestCase):
    def test_each_platform_gets_its_own_url_and_digest(self):
        text = formula(record())
        self.assertIn('version "0.2.7"', text)
        for platform in PLATFORMS["relish"]:
            with self.subTest(platform=platform):
                url = f"https://github.com/reliaburger/reliaburger/releases/download/v0.2.7/relish-{platform}"
                digest = hashlib.sha256(platform.encode()).hexdigest()
                self.assertIn(f'url "{url}"\n', text)
                # The digest sits on the line after its own URL.
                lines = text.splitlines()
                at = next(index for index, line in enumerate(lines) if url in line)
                self.assertEqual(lines[at + 1].strip(), f'sha256 "{digest}"')
        self.assertNotIn("bun-", text)
        self.assertNotRegex(text, r"@[A-Z0-9_]+@")

    def test_it_installs_only_the_cli_and_points_clusters_at_relish_upgrade(self):
        text = formula(record())
        self.assertIn('bin.install Dir["relish-*"].fetch(0) => "relish"', text)
        self.assertIn("relish upgrade", text)
        self.assertIn("depends_on arch: :arm64", text)

    def test_a_record_that_differs_from_the_qualified_digest_is_refused(self):
        data = record()
        formula(data, hashlib.sha256(data).hexdigest())
        with self.assertRaisesRegex(ValueError, "qualified digest"):
            formula(data, "0" * 64)

    def test_nightly_builds_and_pre_releases_never_reach_the_tap(self):
        with self.assertRaisesRegex(ValueError, "nightly"):
            formula(record(channel="nightly"))
        with self.assertRaisesRegex(ValueError, "release tag"):
            formula(record(version="v0.2.7-rc.1"))

    def test_a_missing_platform_is_refused(self):
        with self.assertRaisesRegex(ValueError, "relish-macos-aarch64"):
            formula(record(platforms=("linux-aarch64", "linux-x86_64")))

    def test_the_command_line_writes_the_formula_from_a_record_file(self):
        with tempfile.TemporaryDirectory() as scratch:
            path, output = Path(scratch) / "candidate.json", Path(scratch) / "relish.rb"
            path.write_bytes(record())
            main(["formula", "--record", str(path), "--qualified-digest",
                  hashlib.sha256(record()).hexdigest(), "--output", str(output)])
            self.assertEqual(output.read_text(), formula(record()))

    @unittest.skipUnless(shutil.which("ruby"), "needs ruby")
    def test_the_formula_is_valid_ruby(self):
        with tempfile.NamedTemporaryFile("w", suffix=".rb") as output:
            output.write(formula(record()))
            output.flush()
            result = subprocess.run(["ruby", "-c", output.name], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)


def binaries(directory, platforms=PLATFORMS["relish"]):
    """Write each platform's stand-in binary, whose bytes `record()` hashes."""
    for platform in platforms:
        (Path(directory) / f"relish-{platform}").write_bytes(platform.encode())


class BottleTests(unittest.TestCase):
    """Bottles hold the verified published binary, so `brew install` compiles nothing."""

    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.binaries = self.root / "published"
        self.binaries.mkdir()
        binaries(self.binaries)
        self.output = self.root / "bottles"

    def test_every_platform_gets_a_bottle_for_each_of_its_tags(self):
        digests = bottles(record(), self.binaries, self.output)
        self.assertEqual(sorted(digests), sorted(tag for tags in BOTTLE_TAGS.values() for tag in tags))
        self.assertEqual(set(BOTTLE_TAGS), set(PLATFORMS["relish"]))
        self.assertIn("arm64_tahoe", BOTTLE_TAGS["macos-aarch64"])
        self.assertEqual(BOTTLE_TAGS["linux-aarch64"], ("arm64_linux",))
        self.assertEqual(BOTTLE_TAGS["linux-x86_64"], ("x86_64_linux",))
        for tag, digest in digests.items():
            with self.subTest(tag=tag):
                path = self.output / f"relish-0.2.7.{tag}.bottle.tar.gz"
                self.assertEqual(hashlib.sha256(path.read_bytes()).hexdigest(), digest)

    def test_a_bottle_holds_exactly_the_verified_binary_in_the_keg_layout(self):
        bottles(record(), self.binaries, self.output)
        for platform, tags in BOTTLE_TAGS.items():
            for tag in tags:
                with self.subTest(tag=tag), tarfile.open(self.output / f"relish-0.2.7.{tag}.bottle.tar.gz") as tar:
                    files = {member.name: member for member in tar.getmembers() if member.isfile()}
                    self.assertEqual(sorted(files), ["relish/0.2.7/INSTALL_RECEIPT.json", "relish/0.2.7/bin/relish"])
                    binary = files["relish/0.2.7/bin/relish"]
                    self.assertEqual(binary.mode, 0o755)
                    self.assertEqual(tar.extractfile(binary).read(), platform.encode())
                    receipt = json.loads(tar.extractfile(files["relish/0.2.7/INSTALL_RECEIPT.json"]).read())
                    self.assertEqual(receipt["changed_files"], [])
                    self.assertEqual(receipt["source"]["versions"]["stable"], "0.2.7")
                    for member in tar.getmembers():
                        self.assertFalse(member.issym() or member.islnk(), member.name)
                        self.assertTrue(member.name == "relish" or member.name.startswith("relish/0.2.7"),
                                        member.name)

    def test_bottles_are_byte_for_byte_reproducible(self):
        first = bottles(record(), self.binaries, self.output)
        second = bottles(record(), self.binaries, self.root / "again")
        self.assertEqual(first, second)

    def test_a_binary_that_differs_from_the_record_is_refused(self):
        (self.binaries / "relish-linux-x86_64").write_bytes(b"tampered")
        with self.assertRaisesRegex(ValueError, "relish-linux-x86_64"):
            bottles(record(), self.binaries, self.output)

    def test_a_missing_binary_is_refused(self):
        (self.binaries / "relish-macos-aarch64").unlink()
        with self.assertRaisesRegex(ValueError, "relish-macos-aarch64"):
            bottles(record(), self.binaries, self.output)

    def test_bottles_check_the_record_against_the_qualified_digest(self):
        with self.assertRaisesRegex(ValueError, "qualified digest"):
            bottles(record(), self.binaries, self.output, qualified_digest="0" * 64)

    def test_the_formula_carries_root_url_and_each_bottle_digest(self):
        digests = bottles(record(), self.binaries, self.output)
        text = formula(record(), bottles=digests)
        # The tap's own release holds the bottles, so the published reliaburger
        # release never changes after promotion.
        self.assertIn('root_url "https://github.com/reliaburger/homebrew-tap/releases/download/v0.2.7"', text)
        for tag, digest in digests.items():
            with self.subTest(tag=tag):
                self.assertIn(f'sha256 cellar: :any_skip_relocation, {tag}: "{digest}"\n', text)
        self.assertNotRegex(text, r"@[A-Z0-9_]+@")

    def test_a_dry_run_can_point_root_url_at_a_local_directory(self):
        digests = bottles(record(), self.binaries, self.output)
        text = formula(record(), bottles=digests, root_url=self.output.as_uri())
        self.assertIn(f'root_url "{self.output.as_uri()}"', text)

    def test_without_bottles_the_formula_has_no_bottle_block(self):
        self.assertNotIn("bottle do", formula(record()))

    def test_the_command_line_writes_bottles_and_a_formula_that_names_them(self):
        digest = hashlib.sha256(record()).hexdigest()
        candidate = self.root / "candidate.json"
        candidate.write_bytes(record())
        main(["bottles", "--record", str(candidate), "--qualified-digest", digest,
              "--binaries", str(self.binaries), "--output", str(self.output)])
        digests = json.loads((self.output / "bottles.json").read_text())
        output = self.root / "relish.rb"
        main(["formula", "--record", str(candidate), "--qualified-digest", digest,
              "--bottles", str(self.output / "bottles.json"), "--output", str(output)])
        self.assertEqual(output.read_text(), formula(record(), bottles=digests))

    @unittest.skipUnless(shutil.which("ruby"), "needs ruby")
    def test_the_formula_with_bottles_is_valid_ruby(self):
        with tempfile.NamedTemporaryFile("w", suffix=".rb") as output:
            output.write(formula(record(), bottles=bottles(record(), self.binaries, self.output)))
            output.flush()
            result = subprocess.run(["ruby", "-c", output.name], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)


def git(*args, cwd):
    return subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True, text=True).stdout


class PublishTests(unittest.TestCase):
    """Against a local bare repository standing in for reliaburger/homebrew-tap."""

    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.tap = self.root / "homebrew-tap.git"
        subprocess.run(["git", "init", "--quiet", "--bare", "--initial-branch=main", str(self.tap)], check=True)
        seed = self.root / "seed"
        subprocess.run(["git", "init", "--quiet", "--initial-branch=main", str(seed)], check=True)
        (seed / "README.md").write_text("tap\n")
        git("add", "README.md", cwd=seed)
        git("-c", "user.name=t", "-c", "user.email=t@example.com", "commit", "--quiet", "-m", "seed", cwd=seed)
        git("push", "--quiet", str(self.tap), "main", cwd=seed)
        self.remote = self.tap.as_uri()

    def published(self):
        return git("show", f"main:{FORMULA_PATH}", cwd=self.tap)

    def test_it_commits_the_formula_and_pushes(self):
        text = formula(record())
        self.assertTrue(publish(text, "v0.2.7", self.remote))
        self.assertEqual(self.published(), text)
        self.assertEqual(git("log", "-1", "--format=%s", "main", cwd=self.tap).strip(), "relish 0.2.7")

    def test_publishing_the_same_formula_twice_changes_nothing(self):
        text = formula(record())
        publish(text, "v0.2.7", self.remote)
        head = git("rev-parse", "main", cwd=self.tap)
        self.assertFalse(publish(text, "v0.2.7", self.remote))
        self.assertEqual(git("rev-parse", "main", cwd=self.tap), head)

    def fake_gh(self, release_exists):
        """A `gh` on PATH that logs its arguments and token, and knows one release or none."""
        directory = self.root / "fake-bin"
        directory.mkdir()
        self.gh_log = self.root / "gh.log"
        script = directory / "gh"
        script.write_text(
            "#!/bin/sh\n"
            f'echo "$GH_TOKEN $*" >> "{self.gh_log}"\n'
            f'if [ "$1 $2" = "release view" ]; then exit {0 if release_exists else 1}; fi\n'
            "exit 0\n")
        script.chmod(0o755)
        return {**os.environ, "PATH": f"{directory}{os.pathsep}{os.environ['PATH']}"}

    def bottle_files(self):
        directory = self.root / "bottles"
        directory.mkdir()
        for tag in ("arm64_tahoe", "x86_64_linux"):
            (directory / f"relish-0.2.7.{tag}.bottle.tar.gz").write_bytes(tag.encode())
        (directory / "bottles.json").write_text("{}")
        return directory

    def test_bottles_go_to_a_new_tap_release_named_for_the_tag(self):
        directory = self.bottle_files()
        environment = self.fake_gh(release_exists=False)
        with unittest.mock.patch.dict(os.environ, environment, clear=True):
            self.assertTrue(publish(formula(record()), "v0.2.7", self.remote, token="secret",
                                    bottles=directory))
        calls = self.gh_log.read_text().splitlines()
        self.assertEqual(calls[0], "secret release view v0.2.7 --repo reliaburger/homebrew-tap")
        self.assertTrue(calls[1].startswith("secret release create v0.2.7 --repo reliaburger/homebrew-tap"),
                        calls[1])
        for tag in ("arm64_tahoe", "x86_64_linux"):
            self.assertIn(str(directory / f"relish-0.2.7.{tag}.bottle.tar.gz"), calls[1])
        self.assertNotIn("bottles.json", calls[1])
        self.assertEqual(self.published(), formula(record()))

    def test_bottles_replace_those_on_an_existing_tap_release(self):
        directory = self.bottle_files()
        environment = self.fake_gh(release_exists=True)
        with unittest.mock.patch.dict(os.environ, environment, clear=True):
            publish(formula(record()), "v0.2.7", self.remote, token="secret", bottles=directory)
        upload = self.gh_log.read_text().splitlines()[1]
        self.assertTrue(upload.startswith("secret release upload v0.2.7 --repo reliaburger/homebrew-tap --clobber"),
                        upload)

    def test_without_a_token_it_says_so_and_pushes_nothing(self):
        formula_file = self.root / "relish.rb"
        formula_file.write_text(formula(record()))
        environment = {key: value for key, value in os.environ.items() if key != TOKEN}
        result = subprocess.run(["python3", str(Path(__file__).with_name("homebrew.py")), "publish",
                                 "--formula", str(formula_file), "--version", "v0.2.7"],
                                env=environment, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(f"{TOKEN} is not set", result.stdout)
        self.assertIn("::notice", result.stdout)


if __name__ == "__main__":
    unittest.main()
