"""The candidate build tells qualification its digest without a 2 GB download."""
import fnmatch
import json
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


def build_matrix(text):
    """The full build matrix the `changes` job selects for a candidate."""
    match = re.search(r"all='(\[.*\])'", text)
    if not match:
        raise AssertionError("build.yml has no full build matrix")
    return json.loads(match.group(1))


class BuildMatrixTests(unittest.TestCase):
    def setUp(self):
        self.matrix = build_matrix(WORKFLOW.read_text())

    def test_no_intel_macos_build_since_0_2_2(self):
        # Maintainer decision, 17 October 2026 (#1018): 0.2.1 is the last
        # release with `relish-macos-x86_64`.
        self.assertNotIn("x86_64-apple-darwin", [entry["target"] for entry in self.matrix])
        self.assertNotIn("macos-15-intel", [entry["runner"] for entry in self.matrix])

    def test_matrix_builds_exactly_what_packaging_signs(self):
        from package import PLATFORMS

        built = set()
        for entry in self.matrix:
            for binary in re.findall(r"--bin (\S+)", entry["arguments"]):
                built.add(f"{binary}-{entry['suffix']}")
        expected = {f"{binary}-{platform}" for binary, platforms in PLATFORMS.items() for platform in platforms}
        self.assertEqual(built, expected)


def job(text, name):
    """The lines of one top-level job, from its `  name:` line to the next job."""
    match = re.search(rf"^  {re.escape(name)}:\n(.*?)(?=^  [a-z][a-z-]*:\n|\Z)", text, re.M | re.S)
    if not match:
        raise AssertionError(f"build.yml has no job {name!r}")
    return match.group(1)


class ProfilingBuildTests(unittest.TestCase):
    """The release profile's attribution build (#1358) is built beside the candidate, never shipped."""

    def setUp(self):
        self.text = WORKFLOW.read_text()
        self.job = job(self.text, "build-profiling")

    def test_it_adds_frame_pointers_and_line_tables_to_the_release_profile(self):
        self.assertIn("RUSTFLAGS: -C force-frame-pointers=yes", self.job)
        self.assertIn("CARGO_PROFILE_RELEASE_DEBUG: line-tables-only", self.job)
        self.assertIn('CARGO_PROFILE_RELEASE_STRIP: "false"', self.job)
        self.assertIn("dtolnay/rust-toolchain@1.98.0", self.job)

    def test_it_builds_what_the_shipped_linux_aarch64_build_builds(self):
        shipped = next(entry for entry in build_matrix(self.text) if entry["suffix"] == "linux-aarch64")
        self.assertIn(f"cargo build --locked --release --target {shipped['target']} {shipped['arguments']}",
                      self.job)
        self.assertIn(f"runs-on: {shipped['runner']}", self.job)

    def test_its_artefact_is_one_staging_and_promotion_never_collect(self):
        upload = step_named(self.job, "Upload the profiling build, never shipped")
        name = re.search(r"^\s+name: (.+)$", upload, re.M).group(1)
        self.assertEqual(name, "profiling-build-${{ github.sha }}-${{ github.run_attempt }}")
        # The candidate job gathers `binaries-*` and candidate.py downloads
        # `candidate-<commit>-<attempt>`; neither may match.
        self.assertFalse(fnmatch.fnmatch(name, "binaries-*"))
        self.assertFalse(name.startswith("candidate-"))

    def test_the_candidate_does_not_wait_for_or_collect_it(self):
        candidate = job(self.text, "candidate")
        self.assertNotIn("build-profiling", candidate)
        self.assertNotIn("profiling", candidate)

    def test_the_profile_step_downloads_the_same_name(self):
        script = (Path(__file__).resolve().parent / "qualify-profile.sh").read_text()
        self.assertIn('--name "profiling-build-$commit-$attempt"', script)


class SourceCiReuseTests(unittest.TestCase):
    """A candidate reuses the CI run that passed on its commit, and runs the suite only without one (#1363)."""

    def setUp(self):
        self.text = WORKFLOW.read_text()

    def test_the_search_runs_on_manual_builds_with_read_access_to_actions(self):
        evidence = job(self.text, "ci-evidence")
        self.assertIn("if: github.event_name == 'workflow_dispatch'", evidence)
        self.assertIn("actions: read", evidence)
        self.assertIn("python3 scripts/release/ci_reuse.py", evidence)
        self.assertIn('--commit "$CANDIDATE_COMMIT"', evidence)
        self.assertIn("CANDIDATE_COMMIT: ${{ github.sha }}", evidence)

    def test_the_suite_runs_unless_a_passing_run_was_found(self):
        validate = job(self.text, "validate-release")
        self.assertIn("uses: ./.github/workflows/ci.yml", validate)
        self.assertIn("needs: ci-evidence", validate)
        # `!= 'true'`, not `== 'false'`: a failed search leaves the output
        # empty, and that has to run the suite too.
        self.assertIn("needs.ci-evidence.outputs.reuse != 'true'", validate)
        self.assertIn("!cancelled()", validate)

    def test_the_candidate_is_signed_only_after_a_full_pass(self):
        candidate = job(self.text, "candidate")
        condition = " ".join(re.search(r"if: >-\n(.*?)\n    needs:", candidate, re.S).group(1).split())
        for build in ("build-pdf", "build-binaries", "build-guest-images", "test-packaging"):
            self.assertIn(f"needs.{build}.result == 'success'", condition)
        self.assertIn("needs.validate-release.result == 'success' || "
                      "(needs.validate-release.result == 'skipped' && needs.ci-evidence.result == 'success' "
                      "&& needs.ci-evidence.outputs.reuse == 'true')", condition)
        self.assertIn("github.ref == 'refs/heads/main'", condition)
        needs = re.search(r"^    needs: \[(.*)\]$", candidate, re.M).group(1).split(", ")
        self.assertEqual(set(needs), {"ci-evidence", "validate-release", "build-pdf", "build-binaries",
                                      "build-guest-images", "test-packaging"})

    def test_the_summary_says_which_ci_run_the_candidate_rests_on(self):
        record = step_named(self.text, "Record exact candidate identity and all asset digests")
        self.assertIn("CI_RUN: ${{ needs.ci-evidence.outputs.ci_run }}", record)
        self.assertIn('echo "Source CI: $source_ci"', record)


if __name__ == "__main__":
    unittest.main()
