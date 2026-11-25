"""A recipe that builds before its tests builds what they will build (#846, #904).

`cargo build` resolves features without the dev-dependencies (proc-macro2's
span-locations is one), so a recipe that ran `cargo build` and then nextest
compiled the library, every proc macro and the binaries a second time, and the
test build replaced the binaries the first build had just made. A pre-build
has to be `cargo test --no-run` with the build flags of the test build that
follows it.
"""
from pathlib import Path
import re
import shlex
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))
import workflow_assembly  # noqa: E402

ROOT = Path(__file__).resolve().parents[2]
# Flags that change what Cargo compiles. Selection flags (-E, --run-ignored,
# --test) only choose what runs, and nextest builds every test target anyway.
BUILD_FLAGS = ("--release", "--all-features", "--no-default-features")
VALUED_BUILD_FLAGS = ("--features", "-F", "--target", "--manifest-path", "--cargo-profile")


def build_flags(arguments):
    """The flags in `arguments` that change what Cargo compiles, normalised.

    Anything after `--` goes to the test binaries, not to Cargo.
    """
    flags = {}
    features = set()
    if "--" in arguments:
        arguments = arguments[:arguments.index("--")]
    tokens = iter(arguments)
    for token in tokens:
        name, equals, value = token.partition("=")
        if name in BUILD_FLAGS:
            flags[name] = True
        elif name in VALUED_BUILD_FLAGS:
            value = value if equals else next(tokens, "")
            if name in ("--features", "-F"):
                features.update(part for part in re.split(r"[ ,]+", value) if part)
            else:
                flags[name] = value
    if features:
        flags["--features"] = tuple(sorted(features))
    return flags


def targets(text):
    """Each Make target's prerequisites, from `name: prerequisites ## help` lines."""
    found = {}
    for line in text.splitlines():
        match = re.match(r"([A-Za-z0-9_-]+):(?!=)([^#]*)", line)
        if match and match.group(1) != ".PHONY":
            found[match.group(1)] = match.group(2).split()
    return found


def lines_run_by(text, target, known):
    """The recipe lines `make target` runs: its prerequisites' first, then its own."""
    lines = []
    for prerequisite in known.get(target, []):
        if prerequisite in known:
            lines += lines_run_by(text, prerequisite, known)
    return lines + workflow_assembly.recipe(text, target)


def test_build(line):
    """The arguments of a line's test build, or None when it builds no tests.

    `make coverage` builds through cargo-llvm-cov: its `nextest` stage is the
    test build and a `test` stage before it a pre-build.
    """
    if "$(NEXTEST)" in line:
        return shlex.split(line.split("$(NEXTEST)", 1)[1])
    coverage = llvm_cov_build(line)
    if coverage is not None:
        return coverage[1:]
    if not line.startswith("$(CARGO) test"):
        return None
    words = shlex.split(line)
    return None if "--doc" in words else words[2:]


def llvm_cov_build(line):
    """A `$(CARGO) llvm-cov` line's subcommand and arguments, or None when it builds nothing."""
    words = shlex.split(line) if line.startswith("$(CARGO) llvm-cov ") else []
    words = [word for word in words[2:] if word != "--no-report"]
    return words if words[:1] in (["test"], ["nextest"]) else None


def build_only(arguments):
    """Whether a test build's arguments build its tests without running one.

    cargo-llvm-cov takes `--no-run` for itself, so its pre-build lists the
    tests (`-- --list`) instead.
    """
    return "--no-run" in arguments or arguments[-2:] == ["--", "--list"]


def prebuild_problems(text):
    """Each pre-build that would make the test build after it compile again."""
    known = targets(text)
    problems = []
    for target in known:
        lines = lines_run_by(text, target, known)
        builds = [(index, arguments) for index, line in enumerate(lines)
                  if (arguments := test_build(line)) is not None and not build_only(arguments)]
        if not builds:
            continue
        first, tests = builds[0]
        wanted = build_flags(tests)
        for line in lines[:first]:
            words = shlex.split(line) if line.startswith("$(CARGO) ") else []
            coverage = llvm_cov_build(line)
            if coverage is not None:
                words = ["$(CARGO)"] + coverage
            if words[:2] == ["$(CARGO)", "build"]:
                problems.append(f"{target}: `{line}` builds without the dev-dependencies' features")
            elif words[:2] == ["$(CARGO)", "test"] and build_only(words[2:]) and build_flags(words[2:]) != wanted:
                problems.append(f"{target}: `{line}` builds with {build_flags(words[2:])}, "
                                f"its tests with {wanted}")
    return problems


class PrebuildChecker(unittest.TestCase):
    """The check finds both ways a pre-build can differ from its test build."""

    def test_a_cargo_build_before_nextest_is_reported(self):
        text = ("test-linux: ## Linux\n"
                "\t$(CARGO) build --features ebpf --bin bun\n"
                "\tRELIABURGER_BUN_BINARY=x $(NEXTEST) --features ebpf --run-ignored=only\n")
        self.assertEqual(len(prebuild_problems(text)), 1)
        self.assertIn("test-linux", prebuild_problems(text)[0])

    def test_a_no_run_build_with_other_features_is_reported(self):
        text = ("test-linux: ## Linux\n"
                "\t$(CARGO) test --no-run\n"
                "\t$(NEXTEST) --features ebpf --run-ignored=only\n")
        self.assertEqual(len(prebuild_problems(text)), 1)

    def test_a_prerequisites_build_counts_for_the_target_that_runs_the_tests(self):
        text = ("test: warm ## Suite\n"
                "\t$(NEXTEST)\n"
                "\n"
                "warm: ## Warm\n"
                "\t$(CARGO) build --bins\n")
        self.assertEqual(prebuild_problems(text), ["test: `$(CARGO) build --bins` builds "
                                                   "without the dev-dependencies' features"])

    def test_a_no_run_build_with_the_same_features_in_any_order_passes(self):
        text = ("test-linux: ## Linux\n"
                "\t$(CARGO) test --no-run --features 'ebpf,extra'\n"
                "\t$(NEXTEST) --features extra,ebpf -E 'binary(ebpf)'\n")
        self.assertEqual(prebuild_problems(text), [])

    def test_a_coverage_prebuild_with_other_features_is_reported(self):
        text = ("coverage: ## Coverage\n"
                "\t$(CARGO) llvm-cov --no-report test --features ebpf --lib -- --list\n"
                "\t$(CARGO) llvm-cov --no-report nextest --profile ci\n")
        self.assertEqual(len(prebuild_problems(text)), 1)
        self.assertIn("coverage", prebuild_problems(text)[0])

    def test_a_coverage_prebuild_with_the_runs_features_passes(self):
        text = ("coverage: ## Coverage\n"
                "\t$(CARGO) llvm-cov --no-report test --lib -- --list\n"
                "\t$(CARGO) llvm-cov --no-report nextest --profile ci\n")
        self.assertEqual(prebuild_problems(text), [])

    def test_a_build_with_no_tests_after_it_is_not_a_prebuild(self):
        text = ("build: ## Build\n"
                "\t$(CARGO) build\n"
                "\n"
                "demo: build ## Demo\n"
                "\t./scripts/demo.sh\n")
        self.assertEqual(prebuild_problems(text), [])


class MakefilePrebuilds(unittest.TestCase):
    """The real Makefile and demo scripts compile nothing twice."""

    def test_no_make_recipe_builds_before_its_tests_with_other_features(self):
        self.assertEqual(prebuild_problems((ROOT / "Makefile").read_text()), [])

    def test_test_linux_builds_bun_as_its_ebpf_suite_does(self):
        # RELIABURGER_BUN_BINARY points at target/debug/bun; a `cargo build`
        # of it was replaced by nextest's own build of the same file.
        # The lib tests build on their own first, so they never compile
        # beside the lib (#1943); both builds are nextest's own.
        lines = workflow_assembly.recipe((ROOT / "Makefile").read_text(), "test-linux")
        self.assertEqual(lines[:2], ["$(CARGO) test --no-run --features ebpf --lib",
                                     "$(CARGO) test --no-run --features ebpf"])
        self.assertIn("--features ebpf", lines[2].split("$(NEXTEST)", 1)[1])

    def test_test_rootless_runc_builds_the_lib_tests_on_their_own_first(self):
        # It is the 24.04 privileged job's first, cold build. On 15 and 20
        # Nov that job lost its runner three times in this step and nowhere
        # else (runs 43715576493, 44729365098, 44783868580): one parallel
        # build compiles the lib (about 7 GB) beside its lib tests (10 to
        # 12 GB) on a 16 GB runner, #1943's mechanism. test-linux's split
        # build keeps the two apart.
        lines = workflow_assembly.recipe((ROOT / "Makefile").read_text(), "test-rootless-runc")
        self.assertEqual(lines[:2], ["$(CARGO) test --no-run --features ebpf --lib",
                                     "$(CARGO) test --no-run --features ebpf"])
        self.assertIn("--features ebpf", lines[2].split("$(NEXTEST)", 1)[1])

    def test_test_macos_builds_the_lib_tests_on_their_own_first(self):
        # The macos-15 runner has 7 GB. One build of the lib tests beside the
        # lib and the suite took 21 to 93 minutes of portable macOS's 42 to
        # 102 on the same cache (#1723), #1943's mechanism on a smaller host.
        lines = workflow_assembly.recipe((ROOT / "Makefile").read_text(), "test-macos")
        self.assertEqual(lines[0], "$(CARGO) test --no-run --lib")
        self.assertTrue(lines[1].startswith("$(CARGO) test --no-run --lib --bin relish "))

    def test_coverage_builds_the_lib_tests_on_their_own_before_the_run(self):
        # #1723: one build job (#1943) made the instrumented build about 13
        # of portable Linux's 25 to 31 minutes. The lib tests (10 to 12 GB
        # of rustc) now build alone at full parallelism, then the run builds
        # the rest, the lib (about 7 GB) first, since every other target
        # links it. The two big rustc processes never overlap. cargo-llvm-cov
        # keeps --no-run for itself, so the build lists the tests instead of
        # running them, and the profile that leaves is deleted before the run.
        lines = workflow_assembly.recipe((ROOT / "Makefile").read_text(), "coverage")
        self.assertEqual(lines[:4], [
            "$(CARGO) llvm-cov clean --workspace",
            "$(CARGO) llvm-cov --no-report test --lib -- --list",
            "$(CARGO) llvm-cov clean --profraw-only",
            "$(CARGO) llvm-cov --no-report nextest --profile $(NEXTEST_PROFILE) --no-tests=fail --test-threads=8",
        ])

    def test_test_linux_names_bun_in_cargos_target_directory(self):
        # target/ in the checkout doesn't exist under an out-of-tree
        # CARGO_TARGET_DIR, and every runtime test then failed in 30 ms (#1859).
        text = (ROOT / "Makefile").read_text()
        self.assertIn("TARGET_DIR = $(or $(CARGO_TARGET_DIR),$(CURDIR)/target)", text.splitlines())
        lines = workflow_assembly.recipe(text, "test-linux")
        self.assertIn('RELIABURGER_BUN_BINARY="$(TARGET_DIR)/debug/bun"', lines[-1])
        self.assertFalse([line for line in text.splitlines() if "$(CURDIR)/target/" in line])

    def test_no_script_names_a_binary_in_the_checkouts_target_directory(self):
        # The demos run what they build, from cargo's target directory (#1859).
        hardcoded = [str(path.relative_to(ROOT)) for path in sorted((ROOT / "scripts").rglob("*.sh"))
                     if "REPO_DIR}/target/debug" in path.read_text()]
        self.assertEqual(hardcoded, [])

    def test_the_identity_demo_builds_the_test_it_then_runs(self):
        script = (ROOT / "scripts" / "workload-identity-demo.sh").read_text()
        commands = [line.strip() for line in script.splitlines() if line.strip().startswith("cargo ")]
        self.assertFalse([line for line in commands if line.startswith("cargo build")])
        self.assertTrue(any("--no-run" in line and "--test identity_demo" in line for line in commands))


if __name__ == "__main__":
    unittest.main()
