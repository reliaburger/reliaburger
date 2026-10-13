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
    """The flags in `arguments` that change what Cargo compiles, normalised."""
    flags = {}
    features = set()
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
    """The arguments of a line's test build, or None when it builds no tests."""
    if "$(NEXTEST)" in line:
        return shlex.split(line.split("$(NEXTEST)", 1)[1])
    if not line.startswith("$(CARGO) test"):
        return None
    words = shlex.split(line)
    return None if "--doc" in words else words[2:]


def prebuild_problems(text):
    """Each pre-build that would make the test build after it compile again."""
    known = targets(text)
    problems = []
    for target in known:
        lines = lines_run_by(text, target, known)
        builds = [(index, arguments) for index, line in enumerate(lines)
                  if (arguments := test_build(line)) is not None and "--no-run" not in arguments]
        if not builds:
            continue
        first, tests = builds[0]
        wanted = build_flags(tests)
        for line in lines[:first]:
            words = shlex.split(line) if line.startswith("$(CARGO) ") else []
            if words[:2] == ["$(CARGO)", "build"]:
                problems.append(f"{target}: `{line}` builds without the dev-dependencies' features")
            elif words[:2] == ["$(CARGO)", "test"] and "--no-run" in words and build_flags(words[2:]) != wanted:
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
        lines = workflow_assembly.recipe((ROOT / "Makefile").read_text(), "test-linux")
        self.assertEqual(lines[0], "$(CARGO) test --no-run --features ebpf")
        self.assertIn("--features ebpf", lines[1].split("$(NEXTEST)", 1)[1])

    def test_the_identity_demo_builds_the_test_it_then_runs(self):
        script = (ROOT / "scripts" / "workload-identity-demo.sh").read_text()
        commands = [line.strip() for line in script.splitlines() if line.strip().startswith("cargo ")]
        self.assertFalse([line for line in commands if line.startswith("cargo build")])
        self.assertTrue(any("--no-run" in line and "--test identity_demo" in line for line in commands))


if __name__ == "__main__":
    unittest.main()
