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
        # ARTEFACT is `candidate` on main and `nightly` on a train.
        self.assertIn("name: ${{ env.ARTEFACT }}-manifest-${{ github.sha }}-${{ github.run_attempt }}", manifest)
        self.assertIn("path: dist/candidate.json", manifest)
        self.assertIn("retention-days: 1", manifest)
        self.assertIn("if-no-files-found: error", manifest)

    def test_the_whole_candidate_is_still_preserved(self):
        # Staging and promotion verify every byte of this artefact against
        # the digest; the manifest artefact is only a shortcut to the digest.
        candidate = step_named(self.text, "Preserve signed candidate without publishing a release")
        self.assertIn("name: ${{ env.ARTEFACT }}-${{ github.sha }}-${{ github.run_attempt }}", candidate)
        self.assertIn("path: dist/*", candidate)
        self.assertIn("retention-days: ${{ env.CHANNEL == 'nightly' && 7 || 90 }}", candidate)

    def test_the_artefact_names_are_the_ones_candidate_py_downloads(self):
        from candidate import ARTEFACTS

        choose = step_named(self.text, "Choose the release or nightly channel")
        for channel, artefact in ARTEFACTS.items():
            self.assertIn(f"CHANNEL={channel}\\nARTEFACT={artefact}\\n", choose)


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
        self.assertIn("CI_RUN_ATTEMPT: ${{ needs.ci-evidence.outputs.ci_run_attempt }}", record)
        self.assertIn('source_ci="reused ci.yml run $CI_RUN, attempt $CI_RUN_ATTEMPT"', record)
        self.assertIn('echo "Source CI: $source_ci"', record)


def run_script(block):
    """The shell script of a step's `run: |`, dedented."""
    lines = block.splitlines()
    start = next(index for index, line in enumerate(lines) if line.strip() == "run: |")
    body = [line for line in lines[start + 1:]]
    indent = min(len(line) - len(line.lstrip()) for line in body if line.strip())
    return "\n".join(line[indent:] for line in body) + "\n"


class NightlySigningTests(unittest.TestCase):
    """A release train's nightly build is signed with the nightly key, and only main's with the release key (#1363)."""

    def setUp(self):
        self.text = WORKFLOW.read_text()

    def run_step(self, name, ref):
        import os
        import subprocess
        import tempfile

        with tempfile.NamedTemporaryFile("w+") as environment:
            result = subprocess.run(["bash", "-eo", "pipefail", "-c", run_script(step_named(self.text, name))],
                                    capture_output=True, text=True,
                                    env=dict(os.environ, CANDIDATE_REF=ref, GITHUB_ENV=environment.name))
            environment.seek(0)
            return result.returncode, environment.read()

    def test_the_release_key_is_used_once_and_only_on_main(self):
        self.assertEqual(self.text.count("secrets.RELIABURGER_RELEASE_KEY"), 1)
        sign = step_named(self.text, "Sign complete matrix and generate release metadata")
        self.assertIn("RELIABURGER_RELEASE_KEY: ${{ secrets.RELIABURGER_RELEASE_KEY }}", sign)
        self.assertIn("if: github.ref == 'refs/heads/main'", sign)
        self.assertNotIn("--channel nightly", sign)

    def test_the_nightly_key_is_used_once_and_only_on_a_train(self):
        self.assertEqual(self.text.count("secrets.RELIABURGER_NIGHTLY_KEY"), 1)
        sign = step_named(self.text, "Sign a train's nightly build with the nightly key")
        self.assertIn("RELIABURGER_NIGHTLY_KEY: ${{ secrets.RELIABURGER_NIGHTLY_KEY }}", sign)
        self.assertIn("if: startsWith(github.ref, 'refs/heads/release-')", sign)
        self.assertIn("--channel nightly", sign)
        self.assertNotIn("RELIABURGER_RELEASE_KEY", sign)

    def test_only_main_and_trains_are_ever_signed(self):
        for ref, allowed in [("refs/heads/main", True), ("refs/heads/release-0-2-4", True),
                             ("refs/heads/feature", False), ("refs/pull/1/merge", False),
                             ("refs/tags/v0.2.4", False), ("refs/heads/mainline", False)]:
            with self.subTest(ref=ref):
                status, _ = self.run_step("Require main or a release train for a signed build", ref)
                self.assertEqual(status == 0, allowed)

    def test_the_channel_follows_the_branch(self):
        for ref, channel, artefact in [("refs/heads/main", "release", "candidate"),
                                       ("refs/heads/release-0-2-4", "nightly", "nightly")]:
            with self.subTest(ref=ref):
                status, environment = self.run_step("Choose the release or nightly channel", ref)
                self.assertEqual(status, 0)
                self.assertIn(f"CHANNEL={channel}\n", environment)
                self.assertIn(f"ARTEFACT={artefact}\n", environment)
        status, _ = self.run_step("Choose the release or nightly channel", "refs/heads/feature")
        self.assertNotEqual(status, 0)

    def test_a_train_build_compiles_in_the_nightly_trust_and_main_never_does(self):
        binaries = job(self.text, "build-binaries")
        self.assertIn("NIGHTLY_FEATURES: ${{ startsWith(github.ref, 'refs/heads/release-') "
                      "&& '--features nightly-signing' || '' }}", binaries)
        self.assertIn("${{ matrix.arguments }} $NIGHTLY_FEATURES", binaries)
        self.assertEqual(self.text.count("--features nightly-signing"), 1)

    def test_the_candidate_job_runs_on_main_and_trains_only(self):
        candidate = job(self.text, "candidate")
        condition = " ".join(re.search(r"if: >-\n(.*?)\n    needs:", candidate, re.S).group(1).split())
        self.assertIn("(github.ref == 'refs/heads/main' || startsWith(github.ref, 'refs/heads/release-'))",
                      condition)

    def test_the_record_names_its_channel(self):
        record = step_named(self.text, "Record exact candidate identity and all asset digests")
        self.assertIn('--channel "$CHANNEL"', record)


if __name__ == "__main__":
    unittest.main()
