#!/usr/bin/env python3
"""Run an existing Make test owner and intercept its single nextest invocation.

The original recipe retains prerequisite builds and the image mirror lifetime.
Context and selectors are approved before execution. Successful late JUnit alone
cannot authorise completion; synthetic subprocess tests prove parser and policy
behaviour, not Rust runtime behaviour.
"""
from __future__ import annotations

import argparse
import copy
import json
import os
from pathlib import Path
import re
import secrets
import shlex
import shutil
import socketserver
import subprocess
import sys
import threading

# Evidence guards include ignored source files; imports must not dirty them.
sys.dont_write_bytecode = True
os.environ['PYTHONDONTWRITEBYTECODE'] = '1'

import contracts as c
import coverage_contract as k
from coverage_owner import Handler, request
import gates
import workflow_assembly as w
import owner_tools


def capture_tools(environment, run):
    """Observe genuine executables before replacing Make's NEXTEST variable."""
    result = {}
    rust = owner_tools.rust_tools(environment, run)
    for name, binary in [('cargo', 'cargo'), ('rustc', 'rustc'), ('nextest', 'cargo-nextest')]:
        path = rust.get(name) or shutil.which(binary, path=environment.get('PATH'))
        c.require(path is not None, 'missing genuine owner tool: ' + binary)
        # Keep rustup proxy argv[0]; realpath is diagnostic, not dispatch argv.
        path = str(Path(path).absolute())
        command = [path, '--version'] + (['--verbose'] if name != 'nextest' else [])
        observed = run(command, env=environment, capture_output=True, text=True)
        c.require(observed.returncode == 0, 'actual owner tool version failed')
        version = observed.stdout.strip()
        pattern = r'cargo-nextest 0\.9\.145(?:\s|$)' if name == 'nextest' else name + r' 1\.98\.[01](?:\s|$)'
        c.require(re.match(pattern, version) is not None, 'owner tool version requires audit')
        result[name] = dict(path=path, realpath=str(Path(path).resolve()), sha256=c.digest(path),
                            version=version, version_command=command, exit_code=0)
    validate_tools(result)
    return result


def validate_tools(tools):
    """Refuse tool/version/schema drift in preobserved owner runtime context."""
    c.require(set(tools) == {'cargo', 'rustc', 'nextest'}, 'wrong actual owner tool set')
    for name, observed in tools.items():
        c.fields(observed, ('path', 'realpath', 'sha256', 'version', 'version_command', 'exit_code'))
        c.require(Path(observed['path']).is_absolute() and Path(observed['realpath']).is_absolute(),
                  'actual owner tool path unresolved')
        c.require(
            type(observed['exit_code']) is int and observed['exit_code'] == 0,
            'actual tool query failed',
        )
        query = [observed['path'], '--version'] + (['--verbose'] if name != 'nextest' else [])
        c.require(observed['version_command'] == query, 'wrong actual tool observation command')
        pattern = r'cargo-nextest 0\.9\.145(?:\s|$)' if name == 'nextest' else name + r' 1\.98\.[01](?:\s|$)'
        c.require(re.match(pattern, observed['version']) is not None, 'actual tool version requires audit')
        c.require(re.fullmatch('[0-9a-f]{64}', observed['sha256']) is not None, 'invalid actual tool hash')
    for name in ('cargo', 'rustc'):
        version = tools[name]['version']
        for field in ('release', 'host', 'commit-hash'):
            c.require(
                re.search(r'^' + field + r': .+$', version, re.M) is not None,
                'incomplete verbose tool identity',
            )
        c.require(
            re.search(r'^commit-hash: [0-9a-f]{40}$', version, re.M) is not None,
            'invalid verbose commit',
        )
        release = re.search(r'^release: (.+)$', version, re.M)[1]
        c.require(version.splitlines()[0].split()[1] == release, 'verbose release mismatch')
    c.require(re.search(r'^LLVM version: .+$', tools['rustc']['version'], re.M) is not None,
              'actual compiler LLVM identity missing')


def tool_bytes(tools):
    c.require(set(tools) == {'cargo', 'rustc', 'nextest'}, 'wrong genuine owner tools')
    for observed in tools.values():
        c.require(c.digest(observed['path']) == observed['sha256'], 'genuine owner tool changed')


def fixture_names(root):
    names = set(re.findall(r'\b(RELIABURGER_[A-Z0-9_]+)=', (root / 'Makefile').read_text()))
    return sorted(names | {'RELIABURGER_TEST_IMAGE_MIRROR'})


def child_commands(expected):
    prefix = [expected['tools']['nextest']['path'], 'nextest']
    selectors = expected['entry']['selectors']
    return (prefix + ['list'] + selectors + ['--message-format=json'],
            prefix + ['run'] + selectors + expected['entry']['run_options'])


class Controller:
    """Approve one source-derived invocation before any list or run command."""

    def __init__(self, expected):
        self.expected = expected
        self.count = 0
        self.finished = None
        self.errors = []
        self.lock = threading.Lock()

    def handle(self, row):
        with self.lock:
            try:
                c.require(row.get('nonce') == self.expected['nonce'], 'wrong owner nonce')
                if row.get('kind') == 'start':
                    c.fields(row, ('kind', 'nonce', 'arguments', 'compiler', 'fixtures'))
                    c.require(self.count == 0, 'duplicate Make nextest interception')
                    # Recipe appends its suffix; the approved selector vector is
                    # shared with discovery rather than rebuilt by the child.
                    c.require(row['arguments'] == self.expected['entry']['selectors'],
                              'actual Make selectors differ from source plan')
                    c.require(row['compiler'] == self.expected['compiler'],
                              'Make child compiler context changed')
                    c.require(row['fixtures'] == self.expected['fixtures'],
                              'Make child fixture context changed')
                    tool_bytes(self.expected['tools'])
                    self.count = 1
                    return copy.deepcopy(self.expected)
                c.fields(row, ('kind', 'nonce', 'exit_code'))
                c.require(row['kind'] == 'finish' and self.count == 1 and self.finished is None,
                          'unexpected Make child completion')
                c.require(type(row['exit_code']) is int, 'invalid actual child exit')
                self.finished = row['exit_code']
                return None
            except c.Invalid as error:
                self.errors.append(str(error))
                raise


def child(configuration, arguments, environment, run=subprocess.run):
    """Run list and the original test execution inside Make's owned environment."""
    expected = configuration['expected']
    approved = request(configuration, dict(kind='start', arguments=arguments,
                       compiler=k.snapshot_environment(environment),
                       fixtures={name: environment.get(name) for name in expected['fixture_names']}))
    c.require(approved == expected, 'owner approval changed current plan')
    root = Path(configuration['root'])
    directory = Path(configuration['directory'])
    marker = directory / 'interception.claim'
    try:
        with marker.open('x') as claim:
            claim.write(expected['nonce'] + '\n')
    except FileExistsError as error:
        raise c.Invalid('duplicate child execution') from error
    listed, executed = child_commands(expected)
    source = gates.inside(root, expected['entry']['junit_source'])
    source.unlink(missing_ok=True)
    status = 1
    discovery_exit = None
    try:
        tool_bytes(expected['tools'])
        gates.candidate_sources(root, expected['context']['commit'], run,
                                expected['entry']['source_files'])
        for name, digest in expected['entry']['inputs'].items():
            c.require(c.digest(gates.inside(root, name)) == digest, 'owner input changed')
        with (directory / 'discovery.json').open('wb') as output, (directory / 'discovery.stderr').open('wb') as errors:
            discovered = run(listed, cwd=root, env=environment, stdout=output, stderr=errors)
        discovery_exit = discovered.returncode
        c.require(discovery_exit == 0, 'actual Make discovery failed')
        c.selected(c.read_json(directory / 'discovery.json'))
        with (directory / 'execution.log').open('wb') as output:
            completed = run(executed, cwd=root, env=environment, stdout=output, stderr=subprocess.STDOUT)
        status = completed.returncode
        if source.is_file():
            shutil.copyfile(source, directory / 'junit.xml')
        if status == 0:
            c.passed_junit(directory / 'junit.xml')
        return status
    finally:
        row = dict(schema_version=1, context=expected['context'], nonce=expected['nonce'],
                   command=executed, discovery_command=listed, discovery_exit=discovery_exit,
                   exit_code=status, compiler=k.snapshot_environment(environment),
                   fixtures={name: environment.get(name) for name in expected['fixture_names']},
                   discovery_sha256=c.digest(directory / 'discovery.json')
                   if (directory / 'discovery.json').is_file() else None,
                   junit_sha256=c.digest(directory / 'junit.xml')
                   if (directory / 'junit.xml').is_file() else None)
        (directory / 'child.json').write_text(json.dumps(row, sort_keys=True, indent=2) + '\n')
        request(configuration, dict(kind='finish', exit_code=status))


def execute(root, directory, context, entry, *, tools=None, environment=None, run=subprocess.run, on_expected=None):
    """Execute the original Make target once, retaining all whole-owner failures."""
    root, directory = Path(root).resolve(), Path(directory).resolve()
    c.fields(context, ('commit', 'run_id', 'attempt', 'host'))
    c.require(re.fullmatch('[0-9a-f]{40}', context['commit']) is not None, 'actual checkout SHA required')
    c.require(context['host'] in ('linux', 'darwin'), 'unsupported Make owner host')
    for name in ('run_id', 'attempt'):
        c.require(isinstance(context[name], str) and context[name].isdigit() and int(context[name]) > 0,
                  'current workflow identity required')
    c.require(entry['host'] == context['host'], 'wrong Make owner host')
    c.require(entry['owner_command'] == w.OWNERS[entry['gate']], 'wrong Make owner command')
    c.require(entry['gate'] not in ('portable-linux', 'oci-interruptions'),
              'gate needs its dedicated owner')
    gates.candidate_sources(root, context['commit'], run, entry['source_files'])
    try:
        directory.mkdir(mode=0o700, parents=True, exist_ok=False)
    except FileExistsError as error:
        raise c.Invalid('Make owner directory must be exclusive') from error
    base = dict(os.environ if environment is None else environment)
    for name in ('RUSTC_WRAPPER', 'RUSTC_WORKSPACE_WRAPPER', 'RUSTFLAGS',
                 'CARGO_ENCODED_RUSTFLAGS', 'CARGO_BUILD_TARGET'):
        c.require(name not in base, 'Make build override requires separate audit: ' + name)
    gates.archive_origin(context, entry, root)
    for name in list(base):
        if name.startswith('NEXTEST_'):
            del base[name]
    tools = tools or capture_tools(base, run)
    tool_bytes(tools)
    names = fixture_names(root)
    for name in names:
        base.pop(name, None)
    base.update(CARGO=tools['cargo']['path'], RUSTC=tools['rustc']['path'])
    c.require(base.get('RELIABURGER_GIT_SHA', context['commit']) == context['commit'],
              'Make build identity differs from checkout')
    expected = dict(context=context, entry=entry, nonce=secrets.token_hex(32), tools=tools,
                    compiler=k.snapshot_environment(base), fixture_names=names,
                    fixtures={name: entry['environment'].get(name) for name in names},
                    makefile_sha256=c.digest(root / 'Makefile'))
    # The helper base contains the common preapproved profile; Make appends only
    # the original recipe suffix. Archived owners keep their original options.
    archived = entry.get('archive_origin') is not None
    prefix = w.ARCHIVED_SELECTORS if archived else ['--profile', 'ci']
    suffix, _ = w.make_selection(root, entry['owner_command'][1], archived=archived)
    c.require(suffix == entry['selectors'], 'current Make selection changed')
    configuration = dict(root=str(root), directory=str(directory), expected=expected,
                         nonce=expected['nonce'])
    controller = Controller(expected)
    server = socketserver.ThreadingTCPServer(('127.0.0.1', 0), Handler)
    server.daemon_threads = True
    server.controller = controller
    configuration['endpoint'] = list(server.server_address)
    config_path = directory / 'config.json'
    config_path.write_text(json.dumps(configuration, sort_keys=True, indent=2) + '\n')
    helper = [sys.executable, str(Path(__file__).resolve()), 'child', str(config_path)] + prefix
    command = entry['owner_command'] + ['NEXTEST_PROFILE=ci', 'CARGO=' + tools['cargo']['path'],
                                        'NEXTEST=' + shlex.join(helper)]
    if entry['gate'] == 'linux-root-storage':
        command += ['LINUX_EXCLUDE=' + w.LINUX_EXCLUDE,
                    'TEST_IMAGE_CACHE=' + base.get('RUNNER_TEMP', str(root / 'target')) + '/test-images']
    expected['make_command'] = command
    configuration['expected'] = expected
    config_path.write_text(json.dumps(configuration, sort_keys=True, indent=2) + '\n')
    (directory / 'expected.json').write_text(json.dumps(expected, sort_keys=True, indent=2) + '\n')
    if on_expected is not None:
        on_expected(copy.deepcopy(expected))
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    status = 1
    try:
        with (directory / 'owner.log').open('wb') as output:
            status = run(command, cwd=root, env=base, stdout=output, stderr=subprocess.STDOUT).returncode
    finally:
        server.shutdown()
        server.server_close()
        thread.join()
        row = dict(schema_version=1, context=context, nonce=expected['nonce'],
                   command=command, exit_code=status, interception_count=controller.count,
                   child_exit=controller.finished, control_errors=controller.errors,
                   child_sha256=c.digest(directory / 'child.json')
                   if (directory / 'child.json').is_file() else None,
                   expected_sha256=c.digest(directory / 'expected.json'))
        (directory / 'owner.json').write_text(json.dumps(row, sort_keys=True, indent=2) + '\n')
    return status or (0 if controller.count == 1 and controller.finished == 0 and not controller.errors else 1)


def evidence(expected, directory, required):
    """Require actual Make completion and exact selected successful case IDs."""
    directory = Path(directory)
    owner = c.read_json(directory / 'owner.json')
    c.fields(owner, ('schema_version', 'context', 'nonce', 'command', 'exit_code',
                     'interception_count', 'child_exit', 'control_errors', 'child_sha256', 'expected_sha256'))
    c.require(owner['schema_version'] == 1 and owner['context'] == expected['context']
              and owner['nonce'] == expected['nonce'] and owner['command'] == expected['make_command'],
              'wrong current Make owner')
    c.require(type(owner['exit_code']) is int and owner['exit_code'] == 0
              and owner['interception_count'] == 1 and owner['child_exit'] == 0
              and owner['control_errors'] == [], 'whole Make owner did not complete')
    c.require(c.digest(directory / 'expected.json') == owner['expected_sha256'], 'Make expected plan changed')
    c.require(c.read_json(directory / 'expected.json') == expected, 'wrong pre-execution Make plan')
    c.require(c.digest(directory / 'child.json') == owner['child_sha256'], 'Make child changed')
    child_row = c.read_json(directory / 'child.json')
    listed, executed = child_commands(expected)
    c.require(child_row['context'] == expected['context'] and child_row['nonce'] == expected['nonce']
              and child_row['command'] == executed and child_row['discovery_command'] == listed
              and child_row['compiler'] == expected['compiler'] and child_row['fixtures'] == expected['fixtures'],
              'wrong Make child context or selectors')
    c.require(child_row['discovery_exit'] == 0 and child_row['exit_code'] == 0, 'Make child failed')
    c.require(c.digest(directory / 'discovery.json') == child_row['discovery_sha256']
              and c.digest(directory / 'junit.xml') == child_row['junit_sha256'], 'Make payload changed')
    selected = c.selected(c.read_json(directory / 'discovery.json'))
    passed = c.passed_junit(directory / 'junit.xml')
    c.require(passed <= selected and bool(required) and set(required) <= selected
              and set(required) <= passed, 'required Make cases absent or incomplete')
    return set(required)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('operation', choices=['child'])
    parser.add_argument('configuration')
    parser.add_argument('arguments', nargs=argparse.REMAINDER)
    args = parser.parse_args()
    return child(c.read_json(args.configuration), args.arguments, dict(os.environ))


if __name__ == '__main__':
    try:
        raise SystemExit(main())
    except (c.Invalid, OSError) as error:
        print('Make owner refused: ' + str(error), file=sys.stderr)
        raise SystemExit(1)
