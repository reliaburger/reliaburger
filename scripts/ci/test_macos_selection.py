"""The macOS CI job builds and tests relish and the macOS-only code, nothing else (#1720).

Bun ships for Linux only, so the `portable macOS` job lints the library,
`relish` and `bun` (which compile-checks bun), runs relish's tests and the
tests only macOS compiles, and runs relish's doctests. Bun, grill, cluster
and eBPF tests run on Linux. These tests pin that selection where it lives
(ci.yml, the Makefile and the evidence manifest) and keep the manifest's
`portable-darwin` requirements and the qualification matrix in step with it.
"""
from pathlib import Path
import json
import re
import shlex
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))
import macos_selection as m  # noqa: E402
from test_ci_workflow import jobs, steps  # noqa: E402
import workflow_assembly  # noqa: E402

ROOT = Path(__file__).resolve().parents[2]

# What the macOS job runs. Changing it is a decision about what macOS CI
# proves, so it changes here, in the Makefile and in docs/testing.md together.
TARGETS = ['--lib', '--bin', 'relish', '--test', 'suite', '--test', 'runtime_selection']
FILTER = ("binary_id(reliaburger::bin/relish)"
          " | (binary_id(reliaburger) & (test(/^relish::/) | test(/^grill::apple::/)"
          " | test(/^grill::records::tests::absolute_start_time/)))"
          " | (binary_id(reliaburger::suite) & (test(/^relish_/) | test(/^user_directories::macos_/)))"
          " | binary_id(reliaburger::runtime_selection)")


class FilterEvaluation(unittest.TestCase):
    """The evaluator understands exactly the filterset grammar the job uses."""

    def test_union_intersection_and_parentheses(self):
        selected = m.compile_filter("binary_id(a) | (binary_id(b) & (test(/^x::/) | test(/^y/)))")
        self.assertTrue(selected('a', 'anything'))
        self.assertTrue(selected('b', 'x::one'))
        self.assertTrue(selected('b', 'y_two'))
        self.assertFalse(selected('b', 'z::x::three'))
        self.assertFalse(selected('c', 'x::one'))

    def test_binary_id_is_exact(self):
        selected = m.compile_filter("binary_id(reliaburger)")
        self.assertTrue(selected('reliaburger', 'relish::tests::one'))
        self.assertFalse(selected('reliaburger::suite', 'relish_cli::one'))

    def test_test_regex_searches_like_nextest(self):
        # nextest's /regex/ matches anywhere in the name unless anchored.
        selected = m.compile_filter("test(/apple/)")
        self.assertTrue(selected('reliaburger', 'grill::apple::tests::one'))
        self.assertFalse(selected('reliaburger', 'grill::process::tests::one'))

    def test_unknown_syntax_is_refused(self):
        for text in ("binary(relish)", "test(relish)", "binary_id(a) - binary_id(b)", "not binary_id(a)",
                     "binary_id(a) |", "(binary_id(a)", "binary_id(a))", ""):
            with self.assertRaises(ValueError, msg=text):
                m.compile_filter(text)


class MacosJob(unittest.TestCase):
    """ci.yml's macOS job lints, compile-checks bun, and runs only its selection."""

    @classmethod
    def setUpClass(cls):
        cls.ci = (ROOT / '.github/workflows/ci.yml').read_text()
        cls.macos = jobs(cls.ci)['macos']
        cls.makefile = (ROOT / 'Makefile').read_text()
        cls.manifest = json.loads((ROOT / 'tests/contracts/manifest.json').read_text())

    def step(self, command):
        found = [block for block in steps(self.macos) if command in block]
        self.assertEqual(len(found), 1, command)
        self.assertNotIn('if:', found[0])
        return found[0]

    def test_the_lint_compile_checks_bun_and_lints_relish(self):
        self.step('run: make lint-macos')
        recipe = workflow_assembly.recipe(self.makefile, 'lint-macos')
        self.assertEqual(len(recipe), 1)
        words = shlex.split(recipe[0])
        self.assertEqual(words[:2], ['$(CARGO)', 'clippy'])
        for flag in (['--lib'], ['--bin', 'relish'], ['--bin', 'bun'], ['--all-features']):
            self.assertTrue(any(words[i:i + len(flag)] == flag for i in range(len(words))), flag)
        # Every target would compile every Linux test binary again.
        self.assertNotIn('--all-targets', words)
        self.assertEqual(words[-2:], ['-D', 'warnings'])

    def test_the_portable_darwin_owner_is_the_macos_make_target(self):
        self.step('--gate portable-darwin ')
        self.assertEqual(workflow_assembly.OWNERS['portable-darwin'], ['make', m.TARGET])
        self.assertEqual(self.manifest['gates']['portable-darwin']['command'], ['make', m.TARGET])

    def test_the_macos_target_runs_the_pinned_filterset(self):
        targets, expression = m.selection(self.makefile)
        self.assertEqual(targets, TARGETS)
        self.assertEqual(expression, FILTER)
        # It builds what it runs and warms those binaries first, so macOS's
        # first-exec assessment never lands inside a timed test (#722).
        recipe = workflow_assembly.recipe(self.makefile, m.TARGET)
        self.assertEqual(recipe[0], '$(CARGO) test --no-run ' + ' '.join(TARGETS))
        self.assertEqual(recipe[1], 'python3 scripts/ci/warm_executables.py')
        self.assertEqual(len(recipe), 3)

    def test_no_bun_or_linux_suite_runs_on_macos(self):
        selected = m.compile_filter(FILTER)
        for binary, test in [('reliaburger::bin/bun', 'tests::anything'),
                             ('reliaburger', 'bun::agent::tests::one'),
                             ('reliaburger', 'grill::process::tests::one'),
                             ('reliaburger', 'council::state_machine::tests::one'),
                             ('reliaburger', 'onion::ebpf::maps::tests::one'),
                             ('reliaburger::suite', 'batch::one'),
                             ('reliaburger::documentation_first_run', 'one')]:
            self.assertFalse(selected(binary, test), (binary, test))
        for binary, test in [('reliaburger::bin/relish', 'tests::one'),
                             ('reliaburger', 'relish::quickstart::preflight::tests::macos_reports_a_memory_pressure_level'),
                             ('reliaburger', 'relish::dashboard::tests::one'),
                             ('reliaburger', 'relish::manual::web::tests::one'),
                             ('reliaburger', 'grill::apple::tests::one'),
                             ('reliaburger::suite', 'relish_cli::one'),
                             ('reliaburger::suite', 'user_directories::macos_ignores_xdg_overrides'),
                             ('reliaburger::runtime_selection', 'automatic_selection_uses_process_even_with_apple_cli_installed')]:
            self.assertTrue(selected(binary, test), (binary, test))

    def test_doctests_are_relishs_only(self):
        block = self.step('cargo test --locked --doc')
        self.assertIn("run: 'cargo test --locked --doc -- relish::'", block)
        self.assertNotIn('make test-doc', self.macos)


class Contract(unittest.TestCase):
    """portable-darwin requires exactly the relish and macOS cases the job runs."""

    @classmethod
    def setUpClass(cls):
        cls.makefile = (ROOT / 'Makefile').read_text()
        cls.manifest = json.loads((ROOT / 'tests/contracts/manifest.json').read_text())
        cls.selected = staticmethod(m.compile_filter(m.selection(cls.makefile)[1]))
        cls.cases = [case for contract in cls.manifest['contracts'] for case in contract['cases']]

    def test_every_portable_darwin_case_is_selected_by_the_macos_filterset(self):
        required = [case for case in self.cases if 'portable-darwin' in case['requires']]
        self.assertTrue(required)
        for case in required:
            self.assertTrue(self.selected(case['binary'], case['test']), case['id'])

    def test_every_selected_portable_case_still_requires_macos(self):
        # The rule `macos_selection.py apply` applies, so it can't drift:
        # a portable case keeps macOS exactly when the macOS job runs it.
        for case in self.cases:
            expected = 'portable-linux' in case['requires'] and self.selected(case['binary'], case['test'])
            self.assertEqual('portable-darwin' in case['requires'], expected, case['id'])

    def test_the_manifest_is_already_what_apply_would_write(self):
        self.assertEqual(m.updated_manifest(self.manifest, self.makefile), self.manifest)

    def test_the_matrix_lists_each_case_with_the_manifests_gates(self):
        text = (ROOT / 'docs/testing/qualification-matrix.md').read_text()
        self.assertEqual(m.updated_matrix(text, self.manifest, self.selected), text)
        requires = {case['id']: case['requires'] for case in self.cases}
        rows = m.matrix_rows(text)
        self.assertTrue(rows)
        for identifier, (binary, test, gates) in rows.items():
            if identifier in requires:
                self.assertEqual(gates, requires[identifier], identifier)
            elif 'portable-darwin' in gates:
                self.assertTrue(self.selected(binary, test), identifier)

    def test_apply_drops_only_portable_darwin_from_unselected_cases(self):
        manifest = {'contracts': [{'cases': [
            {'id': 'a', 'binary': 'reliaburger', 'test': 'relish::tests::one',
             'requires': ['portable-linux', 'portable-darwin']},
            {'id': 'b', 'binary': 'reliaburger', 'test': 'bun::tests::one',
             'requires': ['portable-linux', 'portable-darwin']},
            {'id': 'c', 'binary': 'reliaburger', 'test': 'bun::tests::two',
             'requires': ['portable-darwin', 'portable-linux']},
            {'id': 'd', 'binary': 'reliaburger::cluster_failover', 'test': 'one',
             'requires': ['cluster-tests']},
        ]}]}
        cases = m.updated_manifest(manifest, self.makefile)['contracts'][0]['cases']
        self.assertEqual([case['requires'] for case in cases],
                         [['portable-linux', 'portable-darwin'], ['portable-linux'],
                          ['portable-linux'], ['cluster-tests']])
        # The input is left alone.
        self.assertEqual(manifest['contracts'][0]['cases'][1]['requires'],
                         ['portable-linux', 'portable-darwin'])

    def test_apply_rewrites_a_matrix_rows_gate_column(self):
        text = ('| Contract / issue | Case ID | Binary and full test | Required gate(s) | Evidence boundary |\n'
                '|---|---|---|---|---|\n'
                '| 1 / x | `b` | `reliaburger` / `bun::tests::one` | portable-linux, portable-darwin | Boundary. |\n'
                '| 1 / x | `a` | `reliaburger` / `relish::tests::one` | portable-linux, portable-darwin | Boundary. |\n'
                '| 1 / x | `gone` | `reliaburger` / `bun::tests::two` | portable-linux, portable-darwin | Boundary. |\n'
                '| 1 / x | `kept` | `reliaburger` / `relish::tests::two` | portable-linux, portable-darwin | Boundary. |\n')
        manifest = {'contracts': [{'cases': [
            {'id': 'a', 'requires': ['portable-linux', 'portable-darwin']},
            {'id': 'b', 'requires': ['portable-linux']}]}]}
        updated = m.updated_matrix(text, manifest, m.compile_filter(FILTER))
        self.assertIn('| `b` | `reliaburger` / `bun::tests::one` | portable-linux | Boundary. |', updated)
        self.assertIn('| `a` | `reliaburger` / `relish::tests::one` | portable-linux, portable-darwin |', updated)
        # Rows the manifest no longer lists follow the filterset.
        self.assertIn('| `gone` | `reliaburger` / `bun::tests::two` | portable-linux | Boundary. |', updated)
        self.assertIn('| `kept` | `reliaburger` / `relish::tests::two` | portable-linux, portable-darwin |', updated)


if __name__ == '__main__':
    unittest.main()
