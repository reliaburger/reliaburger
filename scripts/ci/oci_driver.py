"""Build and execute the three original isolated OCI test groups once.

Whole-driver completion accompanies each discovery and execution envelope.
Manual runs retain their operator/session authority and cannot qualify CI gates.
"""
from __future__ import annotations

import argparse
import getpass
import uuid
import json
import os
import re
from pathlib import Path
import shutil
import subprocess

# Evidence guards include ignored source files; imports must not dirty them.
import sys
sys.dont_write_bytecode = True
os.environ['PYTHONDONTWRITEBYTECODE'] = '1'

import completion
import contracts as c
import gates
import workflow_assembly as w
import owner_tools


BINARIES = ['reliaburger::owned_runc', 'reliaburger::owned_network', 'reliaburger::oci_crash']
SOURCE_FILES = {binary: ['tests/' + binary.split('::')[1] + '.rs'] for binary in BINARIES}
SETUP = '''set -eu
mkdir -p /run/netns
mount -t tmpfs tmpfs /run/netns
ip link set lo up
exec "$@"
'''
TOOL_NAMES = ['cargo', 'rustc', 'timeout', 'sudo', 'unshare', 'bash']


def tool_bytes(tools):
    c.require(set(tools) == set(TOOL_NAMES), 'wrong OCI owner tool set')
    for name, observed in tools.items():
        c.require(Path(observed['path']).is_absolute(), 'unresolved actual OCI tool')
        c.require(c.digest(observed['path']) == observed['sha256'],
                  f'actual OCI tool changed: {name}')


def namespace_wrapper(tools):
    return [tools['timeout']['path'], '420s', tools['sudo']['path'],
            tools['unshare']['path'], '--mount', '--net', '--propagation',
            'private', tools['bash']['path'], '-c', SETUP, 'qualification']


def child_plan(context, tools, origin, relative_origin, origin_hash, binary):
    """Construct expected selection from this owner's fixed audited driver."""
    return dict(schema_version=1, gate='oci-interruptions', context=context,
                binary=binary, executable=origin['artifacts'][binary]['executable'],
                wrapper=namespace_wrapper(tools), selectors=w.OCI_SELECTORS,
                run_options=['--nocapture', '--test-threads=1', '--format=pretty', '--color=never'],
                source_files=SOURCE_FILES[binary], build_command=w.OCI_BUILD_COMMAND,
                build_origin=relative_origin, build_origin_sha256=origin_hash)


def retain(directory, row):
    temporary = directory / 'driver.json.tmp'
    temporary.write_text(json.dumps(row, sort_keys=True, indent=2) + '\n')
    temporary.replace(directory / 'driver.json')


def build_environment(context, tools):
    """Validate the shared entry before any artifacts or Cargo child are created."""
    for name in ('RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS', 'RUSTC_WRAPPER',
                 'RUSTC_WORKSPACE_WRAPPER', 'CARGO_TARGET_DIR', 'CARGO_BUILD_TARGET'):
        c.require(name not in os.environ, 'OCI build override requires separate audit: ' + name)
    for name, tool in (('RUSTC', 'rustc'), ('CARGO', 'cargo')):
        c.require(name not in os.environ or os.environ[name] == tools[tool]['path'],
                  'OCI build override requires separate audit: ' + name)
    c.require(os.environ.get('RELIABURGER_GIT_SHA', context['commit']) == context['commit'],
              'OCI build identity differs from actual checkout')
    environment = dict(os.environ, CARGO_INCREMENTAL='0', RUSTC=tools['rustc']['path'],
                       PYTHONDONTWRITEBYTECODE='1')
    environment.setdefault('CARGO_BUILD_JOBS', '1')
    return environment


def execute(root, directory, context, tools, run=subprocess.run, on_plan=None):
    """Perform the original build/loop once and retain its actual overall result."""
    completion.context(context)
    c.require(context['host'] == 'linux', 'provisioned Linux OCI owner required')
    environment = build_environment(context, tools)
    root, directory = Path(root).resolve(), Path(directory).resolve()
    tool_bytes(tools)
    c.require(directory.is_relative_to(root / 'target'),
              'OCI evidence must be under checkout target before the build')
    try:
        directory.mkdir(parents=True, exist_ok=False)
    except FileExistsError as error:
        raise c.Invalid('OCI owner evidence directory must be exclusive') from error
    source_files = [source for binary in BINARIES for source in SOURCE_FILES[binary]]
    gates.candidate_sources(root, context['commit'], run, source_files)
    row = dict(schema_version=1, context=context, tools=tools,
               owner_command=w.OWNERS['oci-interruptions'], build_command=w.OCI_BUILD_COMMAND,
               build_origin_sha256=None, children=[], exit_code=1)

    def invoke(command, **kwargs):
        # Bind the actual Cargo invocation to its preobserved executable. Keep
        # the builder's approved logical argv separately from actual delegation.
        tool_bytes(tools)
        actual = [tools['cargo']['path']] + command[1:] if command[0] == 'cargo' else command
        kwargs.setdefault('env', environment)
        return run(actual, **kwargs)

    try:
        status = completion.produce_build(context, w.OCI_BUILD_COMMAND, root,
                                          directory / 'build', BINARIES, invoke)
        origin_path = directory / 'build/build-origin.json'
        if origin_path.is_file():
            row['build_origin_sha256'] = c.digest(origin_path)
        if status:
            row['exit_code'] = status
            return status
        origin = c.read_json(origin_path)
        c.require(set(origin['artifacts']) == set(BINARIES), 'wrong actual OCI binary set')
        # completion's source/executable containment model currently requires
        # evidence under checkout. Keep its origin copy there for per-case plans;
        # the whole-driver directory remains independently exclusive.
        relative_origin = origin_path.relative_to(root).as_posix()
        for binary in BINARIES:
            plan = child_plan(context, tools, origin, relative_origin,
                              row['build_origin_sha256'], binary)
            if on_plan is not None:
                on_plan(plan)
            child = directory / binary.split('::')[1]
            status = completion.produce(plan, root, child, invoke)
            row['children'].append(dict(binary=binary, plan=plan, exit_code=status,
                                        receipt_sha256=c.digest(child / 'receipt.json')
                                        if (child / 'receipt.json').is_file() else None))
            if status:
                row['exit_code'] = status
                return status
        row['exit_code'] = 0
        return 0
    finally:
        retain(directory, row)


def driver_evidence(directory, expected_context, expected_tools, expected_origin, allow_manual=False):
    """Require whole-owner success before accepting any child case evidence.

    The caller must additionally run completion.evidence for every exact current
    source-bound required case. This guard alone is deliberately not a release
    or per-case qualification API.
    """
    completion.context(expected_context)
    c.require(expected_context['authority'] == 'ci' or allow_manual,
              'manual OCI evidence cannot qualify CI')
    directory = Path(directory)
    row = c.read_json(directory / 'driver.json')
    c.fields(row, ('schema_version', 'context', 'tools', 'owner_command', 'build_command',
                   'build_origin_sha256', 'children', 'exit_code'))
    c.require(type(row['schema_version']) is int and row['schema_version'] == 1,
              'unreviewed OCI owner schema')
    c.require(row['context'] == expected_context and row['tools'] == expected_tools,
              'stale OCI owner context or tools')
    c.require(row['owner_command'] == w.OWNERS['oci-interruptions']
              and row['build_command'] == w.OCI_BUILD_COMMAND, 'wrong actual OCI owner command')
    c.require(type(row['exit_code']) is int and row['exit_code'] == 0,
              'whole OCI owner did not complete successfully')
    c.require(c.digest(directory / 'build/build-origin.json') == row['build_origin_sha256'],
              'OCI build origin changed')
    origin = c.read_json(directory / 'build/build-origin.json')
    c.fields(origin, ('schema_version', 'context', 'command', 'exit_code', 'artifacts'))
    c.require(origin['context'] == expected_context
              and origin['command'] == w.OCI_BUILD_COMMAND
              and type(origin['exit_code']) is int and origin['exit_code'] == 0
              and set(origin['artifacts']) == set(BINARIES),
              'wrong or failed actual OCI artifact builder')
    c.require([child['binary'] for child in row['children']] == BINARIES,
              'OCI driver lacks all original binary completions')
    for child in row['children']:
        c.fields(child, ('binary', 'plan', 'exit_code', 'receipt_sha256'))
        # The source/selection/context must come from the externally expected
        # owner, never the child's advertised plan in a downloaded artifact.
        advertised = child['plan']
        expected = child_plan(expected_context, expected_tools, origin,
                              expected_origin, row['build_origin_sha256'], child['binary'])
        c.require(advertised == expected, 'OCI child plan changed from driver policy')
        c.require(type(child['exit_code']) is int and child['exit_code'] == 0,
                  'OCI child failed')
        path = directory / child['binary'].split('::')[1] / 'receipt.json'
        c.require(c.digest(path) == child['receipt_sha256'], 'OCI child receipt changed')
    return row


def resolve_tools():
    tools = {}
    rust = owner_tools.rust_tools(dict(os.environ))
    for name in TOOL_NAMES:
        path = rust.get(name) or shutil.which(name)
        c.require(path is not None, f'provisioned OCI tool missing: {name}')
        path = str(Path(path).absolute())
        # Rust/Cargo are the actual selected sysroot executables. Other tools
        # preserve their resolved invocation path and are hashed before each run.
        query = [path, '--version'] + (['--verbose'] if name in ('cargo', 'rustc') else [])
        version = subprocess.run(query, capture_output=True, text=True)
        c.require(version.returncode == 0, f'actual OCI tool version query failed: {name}')
        tools[name] = dict(
            path=path,
            realpath=str(Path(path).resolve()),
            sha256=c.digest(path),
            version=version.stdout.strip(),
            exit_code=version.returncode,
        )
    for name in ('cargo', 'rustc'):
        c.require(re.match(name + r' 1\.98\.[01](?:\s|$)', tools[name]['version']) is not None,
                  'OCI formatter/toolchain version requires audit')
    c.require(re.search(r'^host: [^\n]*-linux-[^\n]+$', tools['rustc']['version'], re.M) is not None,
              'actual OCI compiler must target Linux host')
    return tools


def manual_main(argv):
    """Observe a disposable-host invocation without creating CI authority."""
    parser = argparse.ArgumentParser(description='Manual disposable-host OCI qualification')
    parser.add_argument('--root', default='.')
    parser.add_argument('--manual-session')
    parser.add_argument('--owner')
    args = parser.parse_args(argv)
    c.require(os.environ.get('GITHUB_ACTIONS') != 'true',
              'GitHub Actions must use the current CI owner entry')
    root = Path(args.root).resolve()
    if args.owner is None:
        try:
            owner = getpass.getuser()
        except (OSError, KeyError) as error:
            raise c.Invalid('operator identity unavailable; supply --owner') from error
        c.require(isinstance(owner, str) and bool(owner.strip()),
                  'operator identity unavailable; supply --owner')
    else:
        owner = args.owner
    session = args.manual_session or ('manual:' + str(uuid.uuid4()))
    commit = subprocess.run(['git', 'rev-parse', 'HEAD'], cwd=root,
                            capture_output=True, text=True, check=True).stdout.strip()
    context = dict(commit=commit, run_id=session, attempt='1', host='linux',
                   authority='manual', owner=owner)
    completion.context(context)
    # The session is a receipt identity, not a pathname supplied by the operator.
    directory = root / 'target' / 'contracts' / 'manual-oci' / str(uuid.uuid4())
    print('Manual OCI owner: ' + json.dumps(context, sort_keys=True), flush=True)
    print('Manual OCI evidence: ' + str(directory), flush=True)
    return execute(root, directory, context, resolve_tools())


def main(argv=None):
    argv = list(sys.argv[1:] if argv is None else argv)
    if argv[:1] == ['manual']:
        return manual_main(argv[1:])
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('root', 'directory', 'commit', 'run-id', 'attempt'):
        parser.add_argument('--' + name, required=True)
    args = parser.parse_args(argv)
    context = dict(commit=args.commit, run_id=args.run_id, attempt=args.attempt,
                   host='linux', authority='ci', owner=None)
    return execute(args.root, args.directory, context, resolve_tools())


if __name__ == '__main__':
    try:
        raise SystemExit(main())
    except (c.Invalid, OSError, ValueError, subprocess.CalledProcessError) as error:
        print('OCI owner refused: ' + str(error), file=sys.stderr)
        raise SystemExit(1)
