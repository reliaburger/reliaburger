#!/usr/bin/env python3
"""Own the existing make coverage stages and one approved instrumented run.

The controller fixes context, nonce, tool observations and selectors before
execution. A child handshake must precede the run; late report metadata cannot
supply approval. All original report stages and the coverage floor must succeed.
"""
import argparse
import copy
import json
import os
import re
import secrets
import shlex
import shutil
import socket
import socketserver
import subprocess
import sys
import threading
from pathlib import Path
# Evidence guards include ignored source files; imports must not dirty them.
sys.dont_write_bytecode = True
os.environ['PYTHONDONTWRITEBYTECODE'] = '1'

import contracts as c
import gates
import coverage_contract as k
import owner_tools
MAKE_RECIPE = """COVERAGE_MIN_LINES ?= 78.65
COVERAGE_REPORT = $(CARGO) llvm-cov report --failure-mode all
coverage: ## Run the portable suite once under line coverage and enforce the floor
	$(CARGO) llvm-cov clean --workspace
	$(CARGO) llvm-cov --no-report nextest --profile $(NEXTEST_PROFILE) --no-tests=fail
	mkdir -p target/coverage
	$(COVERAGE_REPORT) --lcov --output-path target/coverage/lcov.info
	$(COVERAGE_REPORT) --html --output-dir target/coverage/html
	$(COVERAGE_REPORT) --fail-under-lines $(COVERAGE_MIN_LINES)
"""

def recipe_stages(profile, floor):
    """Return the five existing Makefile operations in their original order."""
    c.require(profile == 'ci' and floor == '78.65', 'unaudited coverage selection/floor')
    return [
        ['clean', '--workspace'],
        ['--no-report', 'nextest', '--profile', profile, '--no-tests=fail'],
        ['report', '--failure-mode', 'all', '--lcov', '--output-path', 'target/coverage/lcov.info'],
        ['report', '--failure-mode', 'all', '--html', '--output-dir', 'target/coverage/html'],
        ['report', '--failure-mode', 'all', '--fail-under-lines', floor],
    ]

def verify_recipe(root):
    """Refuse recipe drift before executing any tool or installing the overlay."""
    source = (root / 'Makefile').read_text()
    for line in MAKE_RECIPE.splitlines()[:2]:
        c.require(line in source.splitlines(), 'coverage recipe/header changed')
    start = source.find('coverage:')
    c.require(start >= 0, 'coverage recipe absent')
    block = source[start:].split('\n\n', 1)[0].rstrip('\n')
    c.require(
        block == ('coverage:' + MAKE_RECIPE.split('coverage:', 1)[1]).rstrip('\n'),
        'coverage recipe changed; review owner plan',
    )

def request(configuration, row):
    """Exchange one nonce-bound message with the live parent controller."""
    row = dict(row, nonce=configuration['nonce']) if 'nonce' not in row else row
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as connection:
        connection.settimeout(30)
        connection.connect(tuple(configuration['endpoint']))
        connection.sendall(json.dumps(row, separators=(',', ':')).encode() + b'\n')
        with connection.makefile('rb') as stream:
            data = stream.readline(262145)
    c.require(bool(data) and len(data) <= 262144, 'missing/oversized owner response')
    response = json.loads(data)
    c.require(response.get('ok') is True, response.get('error', 'owner refused request'))
    return response['result']

def adapter_configuration(configuration):
    """Supply the child with parent-owned context and the handshake endpoint."""
    return dict(
        root=configuration['root'],
        genuine_nextest=configuration['expected']['tools']['nextest']['path'],
        adapter_path=configuration['adapter_path'],
        expected=configuration['expected'],
        directory=configuration['directory'],
        junit_source=configuration['junit_source'],
        source_files=configuration['source_files'],
        handshake=configuration,
    )

def stage(arguments, configuration, run=subprocess.run):
    """Wrap one original Makefile Cargo call and preserve its genuine process exit.

    Make exports the CARGO command override. Reset that variable to the
    resolved genuine Cargo before invoking the genuine coverage executable.
    """
    c.require(arguments[:1] == ['llvm-cov'], 'only make coverage Cargo subcommand is approved')
    arguments = arguments[1:]
    approved = request(configuration, dict(kind='stage_start', arguments=arguments))
    command = approved['command']
    environment = os.environ.copy()
    environment.update(configuration['base_tool_environment'])
    environment['PATH'] = configuration['overlay_path']
    environment['RELIABURGER_COVERAGE_OWNER_CONFIG'] = configuration['config_path']
    done = run(command, cwd=configuration['root'], env=environment)
    request(configuration, dict(kind='stage_finish', index=approved['index'], exit_code=done.returncode))
    return done.returncode

class Controller:

    def __init__(self, configuration, base_environment, crates):
        self.configuration = configuration
        self.expected = configuration['expected']
        self.baseline = k.snapshot_environment(base_environment)
        self.crates = crates
        self.index = 0
        self.active = None
        self.count = 0
        self.stages = []
        self.errors = []
        self.lock = threading.Lock()

    def handle(self, row):
        """Serialize all stage and child admissions through this current-run controller."""
        with self.lock:
            try:
                return self._handle(row)
            except (c.Invalid, KeyError, TypeError, ValueError) as error:
                self.errors.append(str(error))
                raise c.Invalid(str(error)) from error

    def _handle(self, row):
        """Validate the nonce, stage order, exact selectors and inherited compiler context.

        The parent approves the generated environment before discovery starts.
        Only audited coverage changes may differ from its initial environment;
        workspace crate names come from the genuine Cargo metadata query. The
        approved environment supplies both child operations, never a late report.
        """
        c.require(row.get('nonce') == self.expected['nonce'], 'wrong owner nonce')
        kind = row.get('kind')
        if kind == 'stage_start':
            c.fields(row, ('kind', 'nonce', 'arguments'))
            c.require(
                self.active is None and self.index < len(self.expected['stages']),
                'overlapping/extra coverage stage',
            )
            command = self.expected['stages'][self.index]
            c.require(row['arguments'] == command[2:], 'make coverage stage differs from trusted recipe')
            for tool in list(self.expected['tools'].values()) + list(self.expected.get('runtime', {}).get('report_tools', {}).values()):
                c.require(
                    c.digest(tool['path']) == tool['sha256'],
                    'genuine coverage tool changed before stage',
                )
            self.active = self.index
            return dict(index=self.index, command=command)
        if kind == 'handshake':
            c.fields(row, ('kind', 'nonce', 'argv', 'environment'))
            c.require(self.active == 1, 'child handshake is outside genuine instrumented stage')
            c.require(self.count == 0, 'duplicate instrumented child handshake')
            c.require(
                row['argv'] == self.expected['run_argv'],
                'actual finite child selectors differ from owner plan',
            )
            observed = row['environment']
            c.require(isinstance(observed, dict), 'invalid generated environment')
            c.require(set(observed) == set(self.baseline), 'generated compiler context keys changed')
            changed = set(k.OPAQUE) | {'RUSTC_WRAPPER', 'CARGO_LLVM_COV', 'LLVM_PROFILE_FILE'}
            for key in set(observed) - changed:
                c.require(
                    observed[key] == self.baseline[key],
                    'generated compiler context changed ' + key,
                )
            c.require(
                observed['__CARGO_LLVM_COV_RUSTC_WRAPPER_RUSTFLAGS'] == '-C\x1finstrument-coverage\x1f--cfg=coverage',
                'unaudited generated coverage flags',
            )
            c.require(
                observed['__CARGO_LLVM_COV_RUSTC_WRAPPER_CRATE_NAMES'] == self.crates,
                'wrong generated workspace instrumentation',
            )
            for key in (
                '__CARGO_LLVM_COV_RUSTC_WRAPPER_COVERAGE_TARGET',
                '__CARGO_LLVM_COV_RUSTC_WRAPPER_HOST',
                '__CARGO_LLVM_COV_RUSTC_WRAPPER_PRE_EXISTING',
            ):
                c.require(observed[key] is None, 'unaudited restricted/pre-existing compiler wrapper')
            pattern = re.escape(Path(self.configuration['root']).name) + '-%p-%[1-9][0-9]*m\\.profraw'
            c.require(
                re.fullmatch(pattern, Path(observed.get('LLVM_PROFILE_FILE') or '').name) is not None,
                'wrong generated profiling filename',
            )
            k.instrumented_context(
                observed,
                observed,
                self.expected['tools'],
                self.expected['context']['commit'],
                self.expected['target_dir'],
            )
            self.expected['instrumentation'] = copy.deepcopy(observed)
            self.count = 1
            handshake = dict(
                context=self.expected['context'],
                nonce=self.expected['nonce'],
                command=row['argv'],
                environment=observed,
            )
            directory = Path(self.configuration['directory'])
            (directory / 'handshake.json').write_text(json.dumps(handshake, sort_keys=True, indent=2) + '\n')
            (directory / 'expected.json').write_text(
                json.dumps(self.expected, sort_keys=True, indent=2) + '\n',
            )
            return copy.deepcopy(self.expected)
        if kind == 'stage_finish':
            c.fields(row, ('kind', 'nonce', 'index', 'exit_code'))
            c.require(
                type(row['index']) is int and row['index'] == self.active,
                'wrong stage completion',
            )
            c.require(type(row['exit_code']) is int, 'invalid actual stage exit')
            self.stages.append(dict(command=self.expected['stages'][self.index], exit_code=row['exit_code']))
            c.require(
                self.index != 1 or self.count == 1,
                'instrumented stage completed without actual child handshake',
            )
            self.active = None
            self.index += 1
            return None
        raise c.Invalid('unknown owner operation')

class Handler(socketserver.StreamRequestHandler):

    def handle(self):
        """Serialize all stage and child admissions through this current-run controller."""
        self.request.settimeout(30)
        try:
            data = self.rfile.readline(262145)
            c.require(bool(data) and len(data) <= 262144, 'oversized owner request')
            result = self.server.controller.handle(json.loads(data))
            answer = dict(ok=True, result=result)
        except (c.Invalid, ValueError, KeyError, TypeError) as error:
            answer = dict(ok=False, error=str(error))
        self.wfile.write(json.dumps(answer, separators=(',', ':')).encode() + b'\n')

def metadata_crates(data, root):
    """Use version1 workspace metadata without interpreting opaque package IDs."""
    c.require(
        isinstance(data, dict) and type(data.get('version')) is int and (data['version'] == 1),
        'unreviewed Cargo metadata schema',
    )
    c.require(
        isinstance(data.get('packages'), list) and isinstance(data.get('workspace_members'), list) and bool(data['workspace_members']),
        'invalid Cargo metadata packages/members',
    )
    c.require(Path(data['target_directory']).is_absolute(), 'metadata target path must be absolute')
    c.require(
        len(data['workspace_members']) == len(set(data['workspace_members'])),
        'duplicate metadata member identity',
    )
    c.require(
        len(data['packages']) == len({package['id'] for package in data['packages']}),
        'duplicate metadata package identity',
    )
    c.require(data['workspace_root'] == str(root), 'metadata describes another workspace')
    by_id = {package['id']: package for package in data['packages']}
    names = []
    for identifier in data['workspace_members']:
        package = by_id[identifier]
        name = package['name'].replace('-', '_')
        names.extend([name, name + '_tests'])
        names.extend((target['name'].replace('-', '_') for target in package['targets']))
    c.require('reliaburger' in names, 'main workspace crate absent')
    return ','.join(names)

def report_tools(tools, run):
    """Observe the actual compiler-sysroot LLVM tools and bind versions and bytes."""
    compiler = tools['rustc']
    sysroot = run([compiler['path'], '--print', 'sysroot'], capture_output=True, text=True)
    c.require(sysroot.returncode == 0, 'actual compiler sysroot query failed')
    host = re.search('^host: (.+)$', compiler['version'], re.M)[1]
    binary = Path(sysroot.stdout.strip()) / 'lib/rustlib' / host / 'bin'
    result = {}
    for name in ('llvm-cov', 'llvm-profdata'):
        path = binary / name
        version = run([str(path), '--version'], capture_output=True, text=True)
        c.require(version.returncode == 0, 'actual LLVM report tool query failed')
        major = re.search('^LLVM version: (\\d+)', compiler['version'], re.M)[1]
        c.require(
            re.search('\\bLLVM version:? ' + major + '(?:\\.|\\b)', version.stdout) is not None,
            'report LLVM version differs from actual compiler',
        )
        result[name] = dict(
            path=str(path),
            sha256=c.digest(path),
            version=version.stdout.strip(),
            exit_code=version.returncode,
        )
    return result

def execute(root, directory, context, source_files, run=subprocess.run, commands=None, environment=None):
    """Invoke the original make coverage once under a fresh parent controller.

    Resolve and observe genuine tools before creating the overlay. The current
    checkout/run/attempt is supplied by trusted workflow context, not reports.
    Retain the actual make and stage failures even when test JUnit is healthy.
    """
    root = Path(root).resolve()
    directory = Path(directory).resolve()
    verify_recipe(root)
    k.workflow_context(context)
    nextest_config = (root / '.config/nextest.toml').read_text()
    c.require(
        re.findall('^retries\\s*=\\s*(.+)$', nextest_config, re.M) and all((value == '0' for value in re.findall('^retries\\s*=\\s*(.+)$', nextest_config, re.M))),
        'nextest retry policy differs from zero retries',
    )
    ci = re.search('^\\[profile\\.ci\\]\\n(.*?)(?=^\\[|\\Z)', nextest_config, re.S | re.M)
    store = re.search('^\\[store\\]\\n(.*?)(?=^\\[|\\Z)', nextest_config, re.S | re.M)
    c.require(
        ci is not None and 'retries = 0' in ci[1] and ('junit = { path = "junit.xml" }' in ci[1]) and (store is not None) and ('dir = "target/nextest"' in store[1]),
        'nextest current profile/JUnit policy changed',
    )
    c.strings(source_files, 'finite source files')
    try:
        directory.mkdir(mode=448, parents=True, exist_ok=False)
    except FileExistsError:
        raise c.Invalid('coverage run directory must be exclusive')
    gates.candidate_sources(root, context['commit'], run, source_files)
    base = dict(os.environ if environment is None else environment)
    if commands is None:
        rust = owner_tools.rust_tools(base, run)
        commands = {
            name: [rust.get(name) or shutil.which(binary, path=base.get('PATH'))]
            + (['llvm-cov', '--version'] if name == 'coverage' else ['--version'])
            + (['--verbose'] if name in ('rustc', 'cargo') else [])
            for name, binary in [('cargo', 'cargo'), ('rustc', 'rustc'),
                                 ('nextest', 'cargo-nextest'), ('coverage', 'cargo-llvm-cov')]
        }
    c.require(all((command[0] for command in commands.values())), 'missing genuine coverage tool')
    version_run = lambda command, **kw: run(command, env=base, **kw)
    tools = k.capture_tools(commands, version_run)
    k.tool_context(tools, copy.deepcopy(tools))
    c.require(
        re.search('^host: [^\\n]*-linux-[^\\n]+$', tools['rustc']['version'], re.M) is not None,
        'coverage owner/compiler host differs from Linux gate',
    )
    llvm = report_tools(tools, version_run)
    before = k.snapshot_environment(base)
    override = (
        'CARGO_LLVM_COV_TARGET_DIR',
        'CARGO_LLVM_COV_BUILD_DIR',
        'CARGO_TARGET_DIR',
        'CARGO_BUILD_BUILD_DIR',
        'RUSTC_WRAPPER',
        'RUSTC_WORKSPACE_WRAPPER',
        'CARGO_BUILD_RUSTC',
        'CARGO_BUILD_RUSTC_WRAPPER',
        'CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER',
        'CARGO_BUILD_TARGET',
        'CARGO',
        'RUSTC',
    )
    c.require(
        all((before[key] is None for key in override)),
        'default owner refuses compiler/target overrides',
    )
    base.update(CARGO=tools['cargo']['path'], RUSTC=tools['rustc']['path'])
    metadata_command = [tools['cargo']['path'], 'metadata', '--format-version=1', '--no-deps']
    metadata = run(metadata_command, cwd=root, env=base, capture_output=True, text=True)
    c.require(metadata.returncode == 0, 'actual workspace metadata failed')
    data = json.loads(metadata.stdout)
    crates = metadata_crates(data, root)
    target = str(Path(data['target_directory']) / 'llvm-cov-target')
    nonce = secrets.token_hex(32)
    profile = 'ci'
    stages = [[tools['coverage']['path'], 'llvm-cov'] + args for args in recipe_stages(profile, '78.65')]
    actual = [
        tools['nextest']['path'],
        'nextest',
        'run',
        '--manifest-path',
        str(root / 'Cargo.toml'),
        '--target-dir',
        target,
        '--profile',
        profile,
        '--no-tests=fail',
    ]
    expected = dict(
        context=copy.deepcopy(context),
        owner_command=['make', 'coverage'],
        nonce=nonce,
        tools=tools,
        instrumentation=None,
        target_dir=target,
        nextest_prefix=[tools['nextest']['path'], 'nextest'],
        run_argv=actual,
        stages=stages,
    )
    overlay = directory / 'overlay'
    overlay.mkdir()
    adapter = overlay / 'cargo-nextest'
    script = Path(__file__).with_name('coverage_adapter.py').resolve()
    adapter.write_text(
        f"#!{sys.executable}\n"
        "import sys\n"
        f"sys.path.insert(0, {str(script.parent)!r})\n"
        "from coverage_owner import adapter_main\n"
        "raise SystemExit(adapter_main())\n"
    )
    adapter.chmod(448)
    home = base.get('CARGO_HOME') or str(Path.home() / '.cargo')
    from coverage_adapter import overlay_path
    configured = dict(
        root=str(root),
        directory=str(directory),
        config_path=str(directory / 'config.json'),
        nonce=nonce,
        expected=expected,
        adapter_path=str(adapter),
        junit_source='target/nextest/ci/junit.xml',
        source_files=source_files,
        base_tool_environment={key: base[key] for key in ('CARGO', 'RUSTC')},
        overlay_path=overlay_path(base.get('PATH', ''), home, str(overlay)),
    )
    controller = Controller(configured, base, crates)
    server = socketserver.ThreadingTCPServer(('127.0.0.1', 0), Handler)
    server.daemon_threads = True
    server.controller = controller
    configured['endpoint'] = list(server.server_address)
    (directory / 'config.json').write_text(json.dumps(configured, sort_keys=True, indent=2) + '\n')
    make_command = [
        'make',
        'coverage',
        'NEXTEST_PROFILE=ci',
        'COVERAGE_MIN_LINES=78.65',
        'CARGO=' + shlex.join([sys.executable, str(Path(__file__).resolve()), 'stage', configured['config_path']]),
    ]
    base['RELIABURGER_COVERAGE_OWNER_CONFIG'] = configured['config_path']
    base['PATH'] = configured['overlay_path']
    runtime = dict(
        context=context,
        nonce=nonce,
        make_command=make_command,
        makefile_sha256=c.digest(root / 'Makefile'),
        nextest_config_sha256=c.digest(root / '.config/nextest.toml'),
        metadata_command=metadata_command,
        metadata=data,
        report_tools=llvm,
    )
    controller.expected['runtime'] = copy.deepcopy(runtime)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    status = 1
    try:
        with (directory / 'owner.log').open('wb') as out:
            status = run(
                make_command,
                cwd=root,
                env=base,
                stdout=out,
                stderr=subprocess.STDOUT,
            ).returncode
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)
        child = directory / 'child.json'
        owner = dict(
            schema_version=1,
            context=context,
            owner_command=['make', 'coverage'],
            nonce=nonce,
            exit_code=status,
            interception_count=controller.count,
            child_sha256=c.digest(child) if child.is_file() else None,
            stages=controller.stages,
        )
        runtime.update(exit_code=status, control_errors=controller.errors)
        (directory / 'owner-runtime.json').write_text(json.dumps(runtime, sort_keys=True, indent=2) + '\n')
        owner['runtime_sha256'] = c.digest(directory / 'owner-runtime.json')
        (directory / 'owner.json').write_text(json.dumps(owner, sort_keys=True, indent=2) + '\n')
    c.require(
        status == 0 and (not controller.errors),
        'coverage owner failed: ' + '; '.join(controller.errors),
    )
    c.require(
        controller.active is None and controller.index == 5 and (controller.count == 1),
        'coverage stages/handshake incomplete',
    )
    k.owner_completion(owner, controller.expected, directory / 'child.json')
    return controller.expected

def adapter_main():
    """Run the narrow generated nextest overlay without reconstructing compiler flags."""
    from coverage_adapter import invoke
    configuration = c.read_json(os.environ['RELIABURGER_COVERAGE_OWNER_CONFIG'])
    return invoke(sys.argv[1:], adapter_configuration(configuration), os.environ.copy())


def main():
    """Run the owner front door or a Makefile stage wrapper."""
    if sys.argv[1:2] == ['stage']:
        c.require(len(sys.argv) >= 4, 'stage requires config and actual argv')
        return stage(sys.argv[3:], c.read_json(sys.argv[2]))
    parser = argparse.ArgumentParser()
    parser.add_argument('--root', required=True)
    parser.add_argument('--directory', required=True)
    parser.add_argument('--context', required=True)
    parser.add_argument('--source-files', required=True)
    args = parser.parse_args()
    execute(args.root, args.directory, c.read_json(args.context), c.read_json(args.source_files))
    return 0
if __name__ == '__main__':
    try:
        sys.exit(main())
    except (c.Invalid, OSError, ValueError) as error:
        print(error, file=sys.stderr)
        sys.exit(1)
