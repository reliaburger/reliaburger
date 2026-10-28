"""The fuzzing workflow runs every target nightly and on decoder changes (#1398)."""
from fnmatch import fnmatch
from pathlib import Path
import re
import unittest

REPO = Path(__file__).resolve().parents[2]
WORKFLOW = REPO / ".github" / "workflows" / "fuzz.yml"


def decoders():
    """The `decoder:` module paths in src/fuzz/mod.rs's target table."""
    return re.findall(r'decoder: "([^"]+)"', (REPO / "src" / "fuzz" / "mod.rs").read_text())


def source_file(decoder):
    """The longest module prefix of `decoder` that is a file under src/."""
    segments = decoder.split("::")
    for end in range(len(segments), 0, -1):
        base = REPO / "src" / Path(*segments[:end])
        for candidate in (base.with_suffix(".rs"), base / "mod.rs"):
            if candidate.is_file():
                return candidate.relative_to(REPO).as_posix()
    return None


def pull_request_paths(text):
    block = re.search(r"(?m)^  pull_request:\n(?:    .*\n)*?    paths:\n((?:      - .*\n)+)", text)
    return [line.strip()[2:] for line in block.group(1).splitlines()] if block else []


class FuzzWorkflowTests(unittest.TestCase):
    def setUp(self):
        self.text = WORKFLOW.read_text()

    def test_it_runs_nightly_and_by_hand(self):
        self.assertRegex(self.text, r"(?m)^  schedule:\n    - cron: ")
        self.assertRegex(self.text, r"(?m)^  workflow_dispatch:")

    def test_every_target_runs_through_make_fuzz_smoke(self):
        self.assertRegex(self.text, r"(?m)run: make fuzz-smoke$")
        makefile = (REPO / "Makefile").read_text()
        # The target list is the directory, so a new target can't be missed.
        self.assertIn("$(wildcard fuzz/fuzz_targets/*.rs)", makefile)
        self.assertIn("-max_total_time=$(FUZZ_SECONDS)", makefile)

    def test_every_fuzz_build_and_run_names_the_toolchains_host_target(self):
        # cargo-fuzz defaults --target to the triple it was itself built for.
        # The prebuilt cargo-fuzz CI installs is a musl binary, so it asked for
        # x86_64-unknown-linux-musl: a static CRT ASan refuses, and no std
        # (run 40675683581). The nightly toolchain's own host is always right.
        makefile = (REPO / "Makefile").read_text()
        self.assertRegex(makefile, r"(?m)^FUZZ_TARGET \?= \$\(shell \$\(CARGO\) \+nightly -vV.*host")
        calls = re.findall(r"\$\(FUZZ\) (build|run)\b([^\n]*)", makefile)
        self.assertEqual(sorted({verb for verb, _ in calls}), ["build", "run"])
        for verb, rest in calls:
            with self.subTest(verb=verb):
                self.assertIn("--target $(FUZZ_TARGET)", rest)

    def test_the_sanitizer_build_is_cached_and_the_job_has_headroom(self):
        # The uncached nightly is about 31 minutes of build plus 45 of
        # fuzzing: the limit leaves at least half again on top.
        limit = re.search(r"(?m)^    timeout-minutes: (\d+)$", self.text)
        self.assertIsNotNone(limit)
        self.assertGreaterEqual(int(limit.group(1)), 120)
        cache = self.text[self.text.index("uses: Swatinem/rust-cache@"):]
        cache = cache[:cache.index("\n      - ")]
        self.assertIn("workspaces: fuzz -> target", cache)
        # One key for the nightly and the pull request runs.
        self.assertIn("shared-key: fuzz", cache)
        self.assertNotIn("${{", cache.split("shared-key:")[1].splitlines()[0])
        # The cache restores before the build it saves.
        self.assertLess(self.text.index("uses: Swatinem/rust-cache@"),
                        self.text.index("run: make fuzz-smoke"))

    def test_a_crash_fails_the_job_and_keeps_its_input(self):
        upload = self.text[self.text.index("uses: actions/upload-artifact@") - 200:]
        self.assertIn("if: failure()", upload)
        self.assertIn("fuzz/artifacts/", upload)
        self.assertNotIn("continue-on-error", self.text)

    def test_results_are_written_to_the_job_summary(self):
        self.assertIn("GITHUB_STEP_SUMMARY", self.text)

    def test_a_pull_request_touching_any_decoder_runs_the_smoke(self):
        paths = pull_request_paths(self.text)
        self.assertIn("src/fuzz/**", paths)
        self.assertIn("fuzz/**", paths)
        found = decoders()
        self.assertGreaterEqual(len(found), 9)
        for decoder in found:
            with self.subTest(decoder=decoder):
                path = source_file(decoder)
                self.assertIsNotNone(path, f"{decoder} names no file under src/")
                self.assertTrue(any(fnmatch(path, pattern.replace("**", "*")) for pattern in paths),
                                f"{path} is missing from fuzz.yml's pull_request paths")

    def test_the_fuzz_crate_is_outside_the_root_workspace(self):
        manifest = (REPO / "fuzz" / "Cargo.toml").read_text()
        self.assertRegex(manifest, r"(?m)^\[workspace\]$")
        self.assertIn('features = ["fuzzing"]', manifest)
        root = (REPO / "Cargo.toml").read_text()
        self.assertNotRegex(root, r'(?m)^members = .*"fuzz"')


if __name__ == "__main__":
    unittest.main()
