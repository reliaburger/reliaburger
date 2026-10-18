"""The feature demos in scripts/demo/ fail loudly and share one start/stop path.

tests/demos.rs runs each of them for real under `make test-cluster`; this file
checks what can be checked without a cluster, on every CI run.
"""
import pathlib
import re
import subprocess
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]
DEMO = ROOT / 'scripts/demo'
LIBRARY = DEMO / 'lib.sh'
# The feature demos built on lib.sh. tour.sh drives an existing quickstart
# cluster from the homepage and has its own check (tests/suite/website.rs).
FEATURE_DEMOS = ['rollout.sh', 'gitops.sh']


def code_lines(path):
    """The script's lines with comments and blank lines removed."""
    lines = []
    for line in path.read_text().splitlines():
        stripped = line.strip()
        if stripped and not stripped.startswith('#'):
            lines.append(stripped)
    return lines


class DemoScripts(unittest.TestCase):
    def test_every_feature_demo_is_valid_bash_that_stops_on_the_first_error(self):
        for name in FEATURE_DEMOS + ['lib.sh']:
            path = DEMO / name
            with self.subTest(script=name):
                self.assertTrue(path.is_file(), f'{name} is missing')
                subprocess.run(['bash', '-n', str(path)], check=True)
                if name != 'lib.sh':
                    self.assertIn('set -euo pipefail', code_lines(path))

    def test_no_demo_swallows_a_failure(self):
        # `|| true` and `|| :` turn a broken step into a passing demo. A step
        # that may legitimately fail says so with an explicit `if`.
        for name in FEATURE_DEMOS + ['lib.sh']:
            with self.subTest(script=name):
                for line in code_lines(DEMO / name):
                    self.assertNotRegex(line, r'\|\|\s*(true|:)\s*($|;|\))', line)
                    self.assertNotIn('2>/dev/null || ', line)

    def test_demos_start_and_stop_their_cluster_only_through_the_library(self):
        # One place starts and stops a demo's node, so later work (resource
        # use during demos, #671) changes lib.sh alone.
        library = LIBRARY.read_text()
        self.assertRegex(library, r'(?m)^demo_start\(\)')
        self.assertRegex(library, r'(?m)^demo_stop\(\)')
        self.assertIn('trap demo_stop EXIT', library)
        for name in FEATURE_DEMOS:
            with self.subTest(script=name):
                text = '\n'.join(code_lines(DEMO / name))
                self.assertIn('. "${DEMO_DIR}/lib.sh"', text)
                self.assertEqual(len(re.findall(r'(?m)^demo_start\b', text)), 1)
                self.assertNotRegex(text, r'\$\{?BUN\}?"?\s', 'only lib.sh runs bun')
                self.assertIn('demo complete', text)

    def test_demos_apply_the_shipped_examples(self):
        for name in FEATURE_DEMOS:
            with self.subTest(script=name):
                text = (DEMO / name).read_text()
                examples = re.findall(r'examples/[\w./-]+', text)
                self.assertTrue(examples, f'{name} names no shipped example')
                for example in examples:
                    self.assertTrue((ROOT / example).exists(), f'{name}: {example} is missing')

    def test_the_live_runs_and_docs_cover_every_demo(self):
        live = (ROOT / 'tests/demos.rs').read_text()
        testing = (ROOT / 'docs/testing.md').read_text()
        makefile = (ROOT / 'Makefile').read_text()
        self.assertIn('binary(demos)', makefile)
        for name in FEATURE_DEMOS:
            with self.subTest(script=name):
                self.assertIn(f'run_demo("{name}")', live)
                self.assertIn(f'scripts/demo/{name}', testing)


if __name__ == '__main__':
    unittest.main()
