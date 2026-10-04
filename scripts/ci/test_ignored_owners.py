"""Every ignored test needs an owner, and CI's reports must show the owners ran them."""
from pathlib import Path
import tempfile
import textwrap
import unittest

import ignored_owners

REPO = Path(__file__).resolve().parents[2]

MAKEFILE = """\
NEXTEST = cargo nextest run
test: ## portable
\t$(NEXTEST)

test-linux: ## privileged
\t$(NEXTEST) --run-ignored=only -E 'test(/runc_/)'

test-apple: ## manual
\t$(NEXTEST) --run-ignored=only -E 'test(/apple/)'
"""

WORKFLOW = """\
jobs:
  linux:
    steps:
      - run: sudo -E make test-linux
      - run: scripts/release/qualify-oci.sh
"""


class Tree:
    """A fake checkout: a Makefile, a CI workflow, scripts and Rust sources."""

    def __init__(self, root):
        self.root = Path(root)
        self.write("Makefile", MAKEFILE)
        self.write(".github/workflows/ci.yml", WORKFLOW)
        self.write("scripts/release/qualify-oci.sh", "#!/bin/sh\n")
        self.write("scripts/release/qualify-reboot.sh", "#!/bin/sh\n")

    def write(self, path, text):
        target = self.root / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(textwrap.dedent(text))

    def test(self, path, name, attribute, body=""):
        self.write(path, f"""\
            #[test]
            {attribute}
            fn {name}() {{
                {body}
            }}
            """)

    def problems(self):
        return ignored_owners.owner_problems(self.root)

    def evidence(self, reports):
        # Synthetic owner/source tests mock ALREADY verified gate consumers.
        # Production must never infer owner/context from an artifact filename.
        import contracts
        targets, scripts = ignored_owners.ci_owners(self.root)
        bindings = []
        for test in ignored_owners.find_ignored(self.root):
            if not any((kind == "make" and owner in targets) or
                       (kind == "script" and owner in scripts)
                       for kind, owner in ignored_owners.owners(test)):
                continue
            full_name = test.name
            if test.path.startswith("src/"):
                # These toy source fixtures declare one ordinary tests module.
                full_name = test.path[4:-3].replace("/", "::") + "::tests::" + test.name
            bindings.append(dict(source=test.path, function=test.name,
                                 binary=test.binary(), test=full_name))
        verified, errors = {}, []
        paths = list(Path(reports).rglob("*.xml"))
        if not paths:
            errors.append("no JUnit reports")
        for path in paths:
            try:
                passed = contracts.passed_junit(path)
                # This is the fake tree's explicit mock consumer result only.
                verified[("make", path.stem)] = passed
            except contracts.Invalid as error:
                errors.append(f"{path.name}: {error}")
        problems = ignored_owners.evidence_problems(self.root, reports, bindings, verified)
        return errors + problems


def junit(suites):
    """A nextest-shaped JUnit report: {binary id: [test names]}."""
    body = "".join(
        f'<testsuite name="{suite}">'
        + "".join(f'<testcase name="{case}" classname="{suite}"/>' for case in cases)
        + "</testsuite>"
        for suite, cases in suites.items())
    return f'<?xml version="1.0"?><testsuites name="nextest-run">{body}</testsuites>'


class OwnerTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.tree = Tree(self.tmp.name)

    def tearDown(self):
        self.tmp.cleanup()

    def test_a_reason_naming_a_make_gate_is_an_owner(self):
        self.tree.test("tests/owned_runc.rs", "runc_starts",
                       '#[ignore = "requires root; run with make test-linux"]')
        self.assertEqual(self.tree.problems(), [])

    def test_a_bare_ignore_has_no_owner(self):
        self.tree.test("tests/owned_runc.rs", "runc_starts", "#[ignore]")
        [problem] = self.tree.problems()
        self.assertIn("tests/owned_runc.rs:2 runc_starts", problem)
        self.assertIn("no reason", problem)

    def test_a_reason_that_names_no_owner_fails(self):
        # The crane test said only how to select it, not who does.
        self.tree.test("tests/suite/registry.rs", "crane_pushes",
                       '#[ignore = "requires crane on PATH; run with --run-ignored=only"]')
        [problem] = self.tree.problems()
        self.assertIn("crane_pushes", problem)
        self.assertIn("names no owner", problem)

    def test_a_make_target_that_does_not_exist_is_not_an_owner(self):
        self.tree.test("tests/owned_runc.rs", "runc_starts",
                       '#[ignore = "run with make test-nowhere"]')
        [problem] = self.tree.problems()
        self.assertIn("test-nowhere", problem)

    def test_a_make_target_that_runs_no_ignored_tests_is_not_an_owner(self):
        self.tree.test("tests/owned_runc.rs", "runc_starts",
                       '#[ignore = "run with make test"]')
        [problem] = self.tree.problems()
        self.assertIn("make test", problem)

    def test_an_existing_release_script_is_an_owner(self):
        self.tree.test("tests/owned_runc.rs", "actual_host_reboot",
                       '#[ignore = "run only through scripts/release/qualify-reboot.sh"]')
        self.assertEqual(self.tree.problems(), [])

    def test_a_missing_script_is_not_an_owner(self):
        self.tree.test("tests/owned_runc.rs", "actual_host_reboot",
                       '#[ignore = "run only through scripts/release/gone.sh"]')
        [problem] = self.tree.problems()
        self.assertIn("scripts/release/gone.sh", problem)

    def test_a_subprocess_fixture_is_owned_by_the_test_that_starts_it(self):
        self.tree.write("tests/owned_runc.rs", """\
            #[test]
            #[ignore = "requires root; run with make test-linux"]
            fn runc_parent() {
                spawn(["--exact", "runc_fixture", "--ignored"]);
            }

            #[test]
            #[ignore = "subprocess fixture for runc caller death"]
            fn runc_fixture() {}
            """)
        self.assertEqual(self.tree.problems(), [])

    def test_a_subprocess_fixture_nobody_starts_has_no_owner(self):
        self.tree.test("tests/owned_runc.rs", "runc_fixture",
                       '#[ignore = "subprocess fixture for runc caller death"]')
        [problem] = self.tree.problems()
        self.assertIn("nothing in tests/owned_runc.rs starts it", problem)

    def test_a_tracking_issue_is_an_owner(self):
        self.tree.test("src/bun/agent.rs", "slow_council_write",
                       '#[ignore = "stage 3 of #351"]')
        self.assertEqual(self.tree.problems(), [])

    def test_attributes_and_comments_between_ignore_and_fn_are_skipped(self):
        self.tree.write("src/grill/runc.rs", """\
            mod tests {
                #[ignore = "requires root; run with make test-linux"]
                // Needs a pinned image.
                #[tokio::test(flavor = "multi_thread")]
                async fn runc_pulls() {}
            }
            """)
        [ignored] = ignored_owners.find_ignored(self.tree.root)
        self.assertEqual(ignored.name, "runc_pulls")

    def test_a_doc_comment_mentioning_ignore_is_not_an_attribute(self):
        self.tree.write("src/bun/gpu.rs", "/// `#[ignore]` hardware test.\nfn helper() {}\n")
        self.assertEqual(ignored_owners.find_ignored(self.tree.root), [])

    def test_every_ignored_test_in_this_repository_has_an_owner(self):
        self.assertEqual(ignored_owners.owner_problems(REPO), [])


class EvidenceTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.tree = Tree(Path(self.tmp.name) / "repo")
        self.reports = Path(self.tmp.name) / "reports"
        self.reports.mkdir()
        self.tree.test("tests/owned_runc.rs", "runc_starts",
                       '#[ignore = "requires root; run with make test-linux"]')

    def tearDown(self):
        self.tmp.cleanup()

    def report(self, name, suites):
        (self.reports / name).write_text(junit(suites))

    def test_a_ci_gated_test_in_its_report_passes(self):
        self.report("test-linux.xml", {"reliaburger::owned_runc": ["runc_starts"]})
        self.assertEqual(self.tree.evidence(self.reports), [])

    def test_renaming_a_gated_test_out_of_its_filter_fails(self):
        # The filter selected runc_starts; renamed, nextest silently drops it.
        self.report("test-linux.xml", {"reliaburger::owned_runc": ["runc_starts"]})
        self.tree.test("tests/owned_runc.rs", "starts",
                       '#[ignore = "requires root; run with make test-linux"]')
        [problem] = self.tree.evidence(self.reports)
        self.assertIn("tests/owned_runc.rs:2 starts", problem)
        self.assertIn("make test-linux", problem)

    def test_the_same_name_in_another_binary_does_not_count(self):
        self.report("test-linux.xml", {"reliaburger::ebpf": ["runc_starts"]})
        self.assertEqual(len(self.tree.evidence(self.reports)), 1)

    def test_unit_tests_match_by_module_path_in_the_library_binary(self):
        self.tree.write("tests/owned_runc.rs", "")
        self.tree.write("src/grill/netns.rs", """\
            mod tests {
                #[test]
                #[ignore = "requires root; run with make test-linux"]
                fn port_mapping() {}
            }
            """)
        self.report("test-linux.xml",
                    {"reliaburger": ["grill::netns::tests::port_mapping"]})
        self.assertEqual(self.tree.evidence(self.reports), [])

    def test_a_raw_libtest_log_cannot_prove_completed_ci_script(self):
        self.tree.test("tests/oci_crash.rs", "bun_sigkill",
                       '#[ignore = "run through scripts/release/qualify-oci.sh"]')
        self.report("test-linux.xml", {"reliaburger::owned_runc": ["runc_starts"]})
        (self.reports / "oci").mkdir()
        (self.reports / "oci" / "oci_crash-0123abcd.log").write_text(
            "running 1 test\ntest bun_sigkill ... ok\n")
        # Even an apparent per-case result lacks the actual completed script's
        # final footer, exit status, current build and assigned owner envelope.
        problems = self.tree.evidence(self.reports)
        self.assertTrue(problems)
        self.assertIn("bun_sigkill", "\n".join(problems))

    def test_a_manual_gate_needs_no_ci_evidence(self):
        self.tree.test("src/grill/apple.rs", "apple_runs",
                       '#[ignore = "requires Apple silicon; run with make test-apple"]')
        self.report("test-linux.xml", {"reliaburger::owned_runc": ["runc_starts"]})
        self.assertEqual(self.tree.evidence(self.reports), [])

    def test_no_reports_at_all_fails(self):
        problems = self.tree.evidence(self.reports)
        self.assertIn("no JUnit reports", problems[0])

    def test_an_empty_report_fails(self):
        self.report("test-linux.xml", {"reliaburger::owned_runc": ["runc_starts"]})
        (self.reports / "test-cluster.xml").write_text("")
        [problem] = self.tree.evidence(self.reports)
        self.assertIn("test-cluster.xml", problem)


if __name__ == "__main__":
    unittest.main()
