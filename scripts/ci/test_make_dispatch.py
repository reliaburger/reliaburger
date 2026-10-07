"""Real Make/IPC dispatch with Python tool stubs; no Cargo/Rust qualification."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

import contracts as c
import make_owner
import workflow_assembly as w


class MakeDispatch(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name).resolve()
        self.directory = self.root / 'target/contracts/linux/linux-root-storage'
        self.root.joinpath('src').mkdir()
        self.root.joinpath('src/case.rs').write_text('// synthetic source case\n')
        self.root.joinpath('.gitignore').write_text('target/\n')
        self.root.joinpath('.config').mkdir()
        self.root.joinpath('.config/nextest.toml').write_text('synthetic source config\n')
        self.root.joinpath('Makefile').write_text('''CARGO = cargo
NEXTEST_PROFILE ?= default
NEXTEST = $(CARGO) nextest run --profile $(NEXTEST_PROFILE) --no-tests=fail
WITH_TEST_IMAGES = python3 mirror.py run --cache "$(TEST_IMAGE_CACHE)" --listen 127.0.0.1:5099 --
.PHONY: test-linux
test-linux:
\t$(CARGO) build --features ebpf --bin bun
\tRELIABURGER_RUNC_TESTS=1 RELIABURGER_BUN_BINARY="$(CURDIR)/target/debug/bun" $(WITH_TEST_IMAGES) $(NEXTEST) --features ebpf --run-ignored=only -E 'binary(test_storage) $(LINUX_EXCLUDE)'
''')
        self.tools = {}
        scripts = {
            'cargo': '''from pathlib import Path
import sys
p = Path('target'); p.mkdir(exist_ok=True)
(p / 'prerequisite').write_text('built')
raise SystemExit(17 if (p / 'fail-build').exists() else 0)
''',
            'rustc': 'raise SystemExit("Rust must never execute in this fixture")\n',
            'nextest': '''from pathlib import Path
import json, sys
assert Path('target/prerequisite').exists()
assert Path('target/mirror-alive').exists()
with Path('target/nextest-invocations').open('a') as f: f.write(sys.argv[2] + '\\n')
if sys.argv[2] == 'list':
    print(json.dumps({'rust-build-meta': {}, 'test-count': 1, 'rust-suites': {'reliaburger': {'binary-id': 'reliaburger', 'status': 'listed', 'testcases': {'tests::owned': {'ignored': True, 'filter-match': {'status': 'matches'}}}}}}))
else:
    p = Path('target/nextest/ci'); p.mkdir(parents=True, exist_ok=True)
    (p / 'junit.xml').write_text('<testsuites><testsuite name="reliaburger"><testcase classname="reliaburger" name="tests::owned"/></testsuite></testsuites>')
''',
        }
        for name, code in scripts.items():
            path = self.root / ('tool-' + name)
            path.write_text('#!' + sys.executable + '\n' + code)
            path.chmod(0o700)
            self.tools[name] = dict(path=str(path), sha256=c.digest(path))
        self.root.joinpath('mirror.py').write_text('''from pathlib import Path
import os, subprocess, sys
marker = Path('target/mirror-alive')
marker.write_text('active')
try:
    env = dict(os.environ, RELIABURGER_TEST_IMAGE_MIRROR='127.0.0.1:5099')
    result = subprocess.call(sys.argv[sys.argv.index('--') + 1:], env=env)
finally:
    marker.unlink()
raise SystemExit(result)
''')
        for command in [['git', 'init', '-q'], ['git', 'add', '.'],
                        ['git', '-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-qm', 'synthetic Make fixture']]:
            subprocess.run(command, cwd=self.root, check=True, capture_output=True)
        commit = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=self.root, text=True).strip()
        self.context = dict(commit=commit, run_id='123', attempt='2', host='linux')
        selectors, environment = w.make_selection(self.root, 'test-linux')
        self.entry = dict(gate='linux-root-storage', host='linux', owner_command=['make', 'test-linux'],
                          selectors=selectors, environment=environment, source_files=['src/case.rs'],
                          inputs={'.config/nextest.toml': c.digest(self.root / '.config/nextest.toml')},
                          run_options=['--no-tests=fail', '--retries=0'], junit_source='target/nextest/ci/junit.xml')

    def tearDown(self):
        self.tmp.cleanup()

    def execute(self):
        return make_owner.execute(self.root, self.directory, self.context, self.entry, tools=self.tools,
                                  environment={'PATH': os.environ['PATH'],
                                               'PYTHONPYCACHEPREFIX': str(self.root / 'target/python-cache')})

    def test_real_make_build_and_mirror_remain_owned_around_one_test_execution(self):
        self.assertEqual(self.execute(), 0)
        self.assertEqual(self.root.joinpath('target/nextest-invocations').read_text(), 'list\nrun\n')
        self.assertFalse(self.root.joinpath('target/mirror-alive').exists())
        expected = c.read_json(self.directory / 'expected.json')
        self.assertEqual(make_owner.evidence(expected, self.directory, {('reliaburger', 'tests::owned')}),
                         {('reliaburger', 'tests::owned')})

    def test_real_make_prerequisite_failure_never_dispatches_or_claims_completion(self):
        self.root.joinpath('target').mkdir()
        self.root.joinpath('target/fail-build').write_text('injected')
        self.assertNotEqual(self.execute(), 0)
        self.assertFalse(self.root.joinpath('target/nextest-invocations').exists())
        self.assertFalse(self.root.joinpath('target/mirror-alive').exists())
        self.assertEqual(c.read_json(self.directory / 'owner.json')['interception_count'], 0)
