"""The Homebrew formula pins exactly the published relish bytes, and the tap gets it (#1583)."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

from homebrew import FORMULA_PATH, TOKEN, formula, main, publish
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
