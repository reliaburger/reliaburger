"""The fuzzing workflow runs every target nightly and on decoder changes (#1398)."""
from fnmatch import fnmatch
from pathlib import Path
import os
import re
import subprocess
import tempfile
import tomllib
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
        self.assertRegex(makefile, r"(?m)^FUZZ_TARGET \?= \$\(shell \$\(CARGO\) \+\$\(FUZZ_TOOLCHAIN\) -vV.*host")
        calls = re.findall(r"\$\(FUZZ\) (build|run)\b([^\n]*)", makefile)
        self.assertEqual(sorted({verb for verb, _ in calls}), ["build", "run"])
        for verb, rest in calls:
            with self.subTest(verb=verb):
                self.assertIn("--target $(FUZZ_TARGET)", rest)

    def test_the_sanitizer_build_fits_a_hosted_runners_memory(self):
        # cargo-fuzz's default of one codegen unit compiled reliaburger as a
        # single sanitized LLVM module: one rustc peaked at 31 GB, and the
        # 16 GB runner was reclaimed mid-build (#1839). `run` builds too, so
        # it must ask for the same units, or it rebuilds with the default.
        makefile = (REPO / "Makefile").read_text()
        units = re.search(r"(?m)^FUZZ_CODEGEN_UNITS \?= (\d+)$", makefile)
        self.assertIsNotNone(units, "the Makefile sets FUZZ_CODEGEN_UNITS")
        self.assertGreater(int(units.group(1)), 1)
        calls = re.findall(r"\$\(FUZZ\) (build|run)\b([^\n]*)", makefile)
        self.assertEqual(sorted({verb for verb, _ in calls}), ["build", "run"])
        for verb, rest in calls:
            with self.subTest(verb=verb):
                self.assertIn("--codegen-units $(FUZZ_CODEGEN_UNITS)", rest)

    def test_the_sanitizer_build_is_cached_and_the_job_has_headroom(self):
        # The uncached nightly is about 33 minutes of build, five minutes of
        # fuzzing per target and 3 of set-up: the limit leaves at least half
        # again on top.
        limit = re.search(r"(?m)^    timeout-minutes: (\d+)$", self.text)
        self.assertIsNotNone(limit)
        targets = len(list((REPO / "fuzz" / "fuzz_targets").glob("*.rs")))
        self.assertGreaterEqual(int(limit.group(1)), (33 + 5 * targets + 3) * 1.5)
        cache = self.text[self.text.index("uses: Swatinem/rust-cache@"):]
        cache = cache[:cache.index("\n      - ")]
        self.assertIn("workspaces: fuzz -> target", cache)
        # One key for the nightly and the pull request runs.
        self.assertIn("shared-key: fuzz", cache)
        self.assertNotIn("${{", cache.split("shared-key:")[1].splitlines()[0])
        # The cache restores before the build it saves.
        self.assertLess(self.text.index("uses: Swatinem/rust-cache@"),
                        self.text.index("run: make fuzz-smoke"))

    def test_the_cache_key_only_changes_with_the_pinned_nightly(self):
        # rust-cache hashes every installed toolchain into its key. A floating
        # nightly changes daily, and the runner image's own stable moved from
        # 1.99.0 to 1.99.1 between two attempts of run 40684652389, so the
        # re-run missed the cache it had just saved.
        pin = re.search(r"(?m)^  FUZZ_TOOLCHAIN: (nightly-\d{4}-\d{2}-\d{2})$", self.text)
        self.assertIsNotNone(pin, "fuzz.yml pins a dated nightly in FUZZ_TOOLCHAIN")
        self.assertIn("toolchain: ${{ env.FUZZ_TOOLCHAIN }}", self.text)
        keep = self.text.index("rustup toolchain uninstall")
        self.assertLess(keep, self.text.index("uses: Swatinem/rust-cache@"))
        makefile = (REPO / "Makefile").read_text()
        self.assertIn("FUZZ_TOOLCHAIN ?= nightly", makefile)
        fuzz_block = makefile[makefile.index("# --- Fuzzing"):makefile.index("fuzz-corpus-check:")]
        self.assertNotIn("+nightly", fuzz_block, "every fuzz command uses $(FUZZ_TOOLCHAIN)")

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

    def test_only_the_decoders_and_the_fuzzing_set_up_start_a_pull_request_run(self):
        # The job takes 14 to 33 minutes; a lockfile, Makefile or any config
        # change shouldn't buy one. The nightly covers those (#1442).
        paths = pull_request_paths(self.text)
        for broad in ("Cargo.lock", "Makefile", "src/config/**"):
            self.assertNotIn(broad, paths)
        self.assertIn("src/config/mod.rs", paths)
        self.assertIn("src/config/node.rs", paths)

    def test_pull_request_runs_never_save_the_cache(self):
        # ci.yml keeps the 10 GB cache quota for main (#1442).
        cache = self.text[self.text.index("uses: Swatinem/rust-cache@"):]
        cache = cache[:cache.index("\n      - ")]
        self.assertIn("save-if: ${{ github.event_name != 'pull_request' }}", cache)

    def test_the_summary_reports_each_targets_coverage_counters(self):
        # The counter count is how a reader of the job sees that only the
        # decoder crates are instrumented (#1953).
        report = self.text[self.text.index("name: Report executions and coverage"):]
        report = report[:report.index("\n      - ")]
        self.assertIn("inline 8-bit counters", report)

    def test_the_fuzz_crate_is_outside_the_root_workspace(self):
        manifest = (REPO / "fuzz" / "Cargo.toml").read_text()
        self.assertRegex(manifest, r"(?m)^\[workspace\]$")
        self.assertIn('features = ["fuzzing"]', manifest)
        root = (REPO / "Cargo.toml").read_text()
        self.assertNotRegex(root, r'(?m)^members = .*"fuzz"')


# Coverage instrumentation only on the crates the decoders run through
# (#1953). libFuzzer scans every counter after every input: with the whole
# dependency tree instrumented that was about 15.3 million counters.
WRAPPER = REPO / "fuzz" / "rustc-coverage.sh"

# The crates each decoder hands bytes to, as rustc names them. Losing one of
# these would blind libFuzzer inside the parser it's meant to be exploring.
DECODER_CRATES = {
    "reliaburger",
    "serde",
    "serde_json",
    "postcard",
    "toml",
    "hickory_proto",
    "x509_parser",
    "rcgen",
    "oci_client",
    "oci_spec",
}

# What cargo-fuzz 0.13 puts in RUSTFLAGS, among what cargo adds per crate.
CARGO_FUZZ_FLAGS = [
    "-Cpasses=sancov-module",
    "-Cllvm-args=-sanitizer-coverage-level=4",
    "-Cllvm-args=-sanitizer-coverage-inline-8bit-counters",
    "-Cllvm-args=-sanitizer-coverage-pc-table",
    "-Cllvm-args=-sanitizer-coverage-trace-compares",
    "-Zsanitizer=address",
    "--cfg", "fuzzing",
]

# A stand-in rustc that prints each argument it gets on its own line.
FAKE_RUSTC = '#!/bin/sh\nfor arg in "$@"; do printf \'%s\\n\' "$arg"; done\n'


def kept_crates():
    """The crate names in the wrapper's COVERAGE_CRATES list."""
    block = re.search(r'(?ms)^COVERAGE_CRATES="\n(.*?)^"', WRAPPER.read_text())
    return block.group(1).split() if block else []


def locked_crates():
    """Every package in fuzz/Cargo.lock, as rustc names its crate."""
    with open(REPO / "fuzz" / "Cargo.lock", "rb") as lock:
        return {package["name"].replace("-", "_") for package in tomllib.load(lock)["package"]}


class FuzzInstrumentationTests(unittest.TestCase):
    def rustc_sees(self, *args):
        """Run the wrapper over the stand-in rustc; return what rustc got."""
        with tempfile.TemporaryDirectory() as scratch:
            fake = Path(scratch) / "rustc"
            fake.write_text(FAKE_RUSTC)
            fake.chmod(0o755)
            out = subprocess.run([str(WRAPPER), str(fake), *args],
                                 capture_output=True, text=True, check=True)
        return out.stdout.splitlines()

    def test_a_dependency_is_built_without_the_coverage_pass(self):
        args = ["--crate-name", "tokio", "--edition=2021", "src/lib.rs", *CARGO_FUZZ_FLAGS]
        seen = self.rustc_sees(*args)
        self.assertNotIn("-Cpasses=sancov-module", seen)
        # AddressSanitizer stays on, and nothing else changes.
        self.assertIn("-Zsanitizer=address", seen)
        self.assertEqual(seen, [arg for arg in args if arg != "-Cpasses=sancov-module"])

    def test_the_decoder_crates_keep_every_flag(self):
        for crate in sorted(DECODER_CRATES):
            with self.subTest(crate=crate):
                args = ["--crate-name", crate, "src/lib.rs", *CARGO_FUZZ_FLAGS]
                self.assertEqual(self.rustc_sees(*args), args)

    def test_a_call_that_names_no_crate_passes_through(self):
        self.assertEqual(self.rustc_sees("-vV"), ["-vV"])

    def test_an_argument_with_spaces_survives(self):
        args = ["--crate-name", "tokio", "--cfg", 'feature="a b"', "-Cpasses=sancov-module"]
        self.assertEqual(self.rustc_sees(*args), args[:-1])

    def test_the_decoder_crates_are_on_the_list(self):
        missing = DECODER_CRATES - set(kept_crates())
        self.assertFalse(missing, f"not instrumented: {sorted(missing)}")

    def test_every_kept_crate_is_one_the_fuzz_build_uses(self):
        # A misspelt or since-dropped name would silently instrument nothing.
        locked = locked_crates()
        for crate in kept_crates():
            with self.subTest(crate=crate):
                self.assertIn(crate, locked)

    def test_make_builds_and_runs_through_the_wrapper(self):
        makefile = (REPO / "Makefile").read_text()
        self.assertRegex(makefile, r"(?m)^FUZZ_RUSTC_WRAPPER \?= \$\(CURDIR\)/fuzz/rustc-coverage\.sh$")
        self.assertRegex(makefile, r"(?m)^FUZZ = RUSTC_WRAPPER=\$\(FUZZ_RUSTC_WRAPPER\) ")
        self.assertTrue(os.access(WRAPPER, os.X_OK), "the wrapper is executable")

    def test_a_fuzz_run_never_rebuilds_the_crate(self):
        # A release build embeds the source bundle and watches its allowlist,
        # fuzz/ included, by modification time. Every log and corpus file a
        # run wrote under fuzz/target/ rebuilt reliaburger, twice a target:
        # the 16 Nov nightly spent 25 minutes a target and hit its timeout.
        build = (REPO / "build.rs").read_text()
        embed = build[build.index("fn embed_source()"):]
        embed = embed[:embed.index("\n}\n")]
        skip = embed.find('std::env::var_os("CARGO_FEATURE_FUZZING").is_some()')
        self.assertNotEqual(skip, -1, "embed_source skips the fuzzing build")
        self.assertLess(skip, embed.index("rerun-if-changed"))

    def test_a_change_to_the_list_rebuilds_every_crate(self):
        # Cargo doesn't fingerprint a RUSTC_WRAPPER: a cached build, or one
        # from before the wrapper, kept its old instrumentation (the first
        # narrowed build here reused all 600 dependencies as they were). A
        # cfg named after the wrapper's checksum puts it in RUSTFLAGS, which
        # Cargo does fingerprint.
        makefile = (REPO / "Makefile").read_text()
        self.assertRegex(makefile, r"(?m)^FUZZ_COVERAGE_ID = .*cksum < \$\(FUZZ_RUSTC_WRAPPER\)")
        fuzz = re.search(r"(?ms)^FUZZ = (.*?)fuzz$", makefile).group(1)
        self.assertIn('RUSTFLAGS="$$RUSTFLAGS --cfg=$(FUZZ_COVERAGE_CFG) '
                      '--check-cfg=cfg($(FUZZ_COVERAGE_CFG))"', fuzz)
        self.assertIn("$(FUZZ_COVERAGE_ID)", makefile[makefile.index("FUZZ_COVERAGE_CFG ="):])


if __name__ == "__main__":
    unittest.main()
