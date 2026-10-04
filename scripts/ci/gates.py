#!/usr/bin/env python3
"""Execute approved nextest selections and aggregate current gate envelopes.

The caller supplies the trusted plan and workflow context before execution.
Discovery, completion and artifact origins must match that approved selection.
"""
from __future__ import annotations
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
# Evidence guards include ignored source files; imports must not dirty them.
sys.dont_write_bytecode = True
os.environ['PYTHONDONTWRITEBYTECODE'] = '1'

import contracts as c


def plan_bytes(plan):
    return json.dumps(plan, sort_keys=True, separators=(',', ':')).encode()


def plan_digest(plan):
    return hashlib.sha256(plan_bytes(plan)).hexdigest()


def context_check(plan, expected):
    c.fields(expected, ('commit', 'run_id', 'attempt', 'mode'))
    c.require(re.fullmatch('[0-9a-f]{40}', expected['commit']) is not None, 'invalid current commit')
    for key in ('run_id', 'attempt'):
        c.require(
            isinstance(expected[key], str) and expected[key].isdigit() and int(expected[key]) > 0,
            'invalid current run identity',
        )
    c.require(expected['mode'] in ('portable', 'full'), 'invalid expected mode')
    for key in expected:
        c.require(plan[key] == expected[key], f'stale/wrong plan {key}')


def inside(root, name):
    c.string(name, 'relative file')
    path = Path(name)
    c.require(not path.is_absolute() and '..' not in path.parts, 'file must be relative and contained')
    resolved = (root / path).resolve()
    c.require(resolved.is_relative_to(root.resolve()), 'file escapes root')
    return resolved


def selector_values(selectors, name):
    """Read separated or equal-style arguments without changing their spelling."""
    values = []
    index = 0
    while index < len(selectors):
        if selectors[index] == name:
            c.require(index + 1 < len(selectors), 'missing selector value')
            values.append(selectors[index + 1])
            index += 2
        else:
            if selectors[index].startswith(name + '='):
                values.append(selectors[index].partition('=')[2])
            index += 1
    for value in values:
        c.string(value, 'actual selector value')
    return values


def commands(entry):
    # A single selector vector supplies BOTH operations, including feature,
    # archive, profile, ignored, platform, filter and exclusion flags. Runtime
    # options below cannot silently alter the selected set.
    options = entry['run_options']
    c.require(isinstance(options, list), 'run_options must be list')
    allowed = {'--no-fail-fast', '--no-tests=fail', '--test-threads=2', '--retries=0'}
    c.require(set(options) <= allowed and len(options) == len(set(options)), 'unapproved execution option')
    c.require(
        '--retries=0' in options and '--no-tests=fail' in options,
        'zero retries and nonempty selection required',
    )
    selectors = entry['selectors']
    c.argv(selectors)
    c.require(
        '--' not in selectors and all(not x.startswith(('--message-format', '--retries', '--no-tests', '--test-threads', '--no-fail-fast')) for x in selectors),
        'execution-only option in selectors',
    )
    # Preserve real separated/equal spelling. When a policy is implicit in the
    # actual invocation, the externally supplied context declares it; do not
    # silently add --ignore-default-filter to change the owner command.
    profile = selector_values(selectors, '--profile')
    c.require(len(profile) == 1, 'explicit profile required')
    ignored = selector_values(selectors, '--run-ignored')
    c.require(len(ignored) <= 1, 'duplicate ignored selection')
    policy = entry.get('selector_context')
    if policy is None:
        c.require(
            bool(ignored) and ignored[0] in ('default','only','all'),
            'explicit ignored selection required',
        )
        c.require('--ignore-default-filter' in selectors, 'explicit default-filter policy required')
    else:
        c.fields(policy, ('profile','ignored','default_filter'))
        c.require(policy['profile'] == profile[0], 'wrong actual nextest profile')
        c.require(
            policy['ignored'] in ('default','only','all') and policy['ignored'] == (ignored[0] if ignored else 'default'),
            'wrong actual ignored policy',
        )
        c.require(
            policy['default_filter'] == ('ignore' if '--ignore-default-filter' in selectors else 'honor'),
            'wrong actual default-filter policy',
        )
    return (['cargo', 'nextest', 'run'] + selectors + options,
            ['cargo', 'nextest', 'list'] + selectors + ['--message-format=json'])


def candidate_sources(root, commit, run, source_files=()):
    head = run(['git', 'rev-parse', 'HEAD'], cwd=root, capture_output=True, text=True)
    c.require(head.returncode == 0 and head.stdout.strip() == commit, 'checkout is not expected candidate')
    clean = run(['git', 'diff', '--quiet', 'HEAD', '--'], cwd=root)
    c.require(clean.returncode == 0, 'tracked source differs from expected candidate')
    if source_files:
        tracked = run(
            ['git', 'ls-files', '--error-unmatch', '--'] + list(source_files),
            cwd=root,
            capture_output=True,
            text=True,
        )
        c.require(
            tracked.returncode == 0 and set(tracked.stdout.splitlines()) == set(source_files),
            'finite case source not tracked at candidate HEAD',
        )
    # Exact repository inputs inspected in RustEmbed/include_*! and build.rs,
    # plus test/helper/workflow source. Do not require unrelated docs clean.
    roots = ['src', 'tests', 'benches', 'scripts', '.github', 'docs/manual', 'examples', 'brioche/dist', 'ebpf']
    files = {'build.rs', 'Cargo.toml', 'Cargo.lock', '.cargo/config', '.cargo/config.toml', 'rust-toolchain', 'rust-toolchain.toml'}
    for ignored in (False, True):
        command = ['git', 'ls-files', '--others', '--exclude-standard']
        if ignored:
            command.append('--ignored')
        unknown = run(
            command + ['--', '.', ':(exclude)target/**', ':(exclude)tests/contracts/cron/target/**'],
            cwd=root,
            capture_output=True,
            text=True,
        )
        c.require(unknown.returncode == 0, 'cannot inventory untracked source')
        for name in unknown.stdout.splitlines():
            path = Path(name)
            # Generated Rust build output is allowed only at known target roots.
            if path.is_relative_to('target') or path.is_relative_to('tests/contracts/cron/target'):
                continue
            if name in files or any(path.is_relative_to(directory) for directory in roots):
                raise c.Invalid(f'untracked source/helper cannot qualify candidate: {name}')


def archive_origin(plan, entry, root):
    origin = entry.get('archive_origin')
    if origin is None:
        return
    path = inside(root, origin['path'])
    c.require(c.digest(path) == origin['sha256'], 'archive build-origin receipt changed')
    builder = c.read_json(path)
    c.require(builder == origin['builder'], 'wrong archive build-origin receipt')
    for key in ('commit', 'run_id', 'attempt'):
        c.require(builder[key] == plan[key], f'stale archive build-origin {key}')
    c.require(builder['exit_code'] == 0, 'archive builder failed')


def archive_build(command, root, archive, receipt, expected, run=subprocess.run):
    """Record the actual builder invocation, before any consuming gate exists."""
    c.fields(expected, ('commit', 'run_id', 'attempt', 'host'))
    c.require(re.fullmatch('[0-9a-f]{40}', expected['commit']) is not None, 'invalid builder commit')
    c.require(expected['host'] in ('linux', 'darwin'), 'invalid builder host')
    for name in ('run_id', 'attempt'):
        c.require(
            isinstance(expected[name], str) and expected[name].isdigit() and int(expected[name]) > 0,
            'invalid builder run identity',
        )
    c.require(command[:3] == ['cargo', 'nextest', 'archive'], 'unapproved archive builder')
    c.argv(command)
    root = Path(root).resolve()
    archive = inside(root, archive)
    receipt = inside(root, receipt)
    receipt.unlink(missing_ok=True)
    archive.unlink(missing_ok=True)
    candidate_sources(root, expected['commit'], run)
    finished = run(command, cwd=root)
    row = dict(expected, schema_version=1, command=command, exit_code=finished.returncode,
               archive_sha256=c.digest(archive) if archive.is_file() else None)
    receipt.parent.mkdir(parents=True, exist_ok=True)
    temporary = receipt.with_suffix(receipt.suffix + '.tmp')
    temporary.write_text(json.dumps(row, sort_keys=True, indent=2) + '\n')
    temporary.replace(receipt)
    return finished.returncode or (0 if row['archive_sha256'] is not None else 1)


def validate_plan(plan, manifest, expected):
    c.fields(plan, ('schema_version', 'commit', 'run_id', 'attempt', 'mode', 'entries'))
    c.require(type(plan['schema_version']) is int and plan['schema_version'] == 1, 'unsupported plan schema')
    context_check(plan, expected)
    c.require(isinstance(plan['entries'], list) and bool(plan['entries']), 'empty execution plan')
    for contract in manifest['contracts']:
        for case in contract['cases']:
            if any(manifest['gates'][gate]['mode'] == 'ci' for gate in case['requires']):
                c.strings(case.get('sources'), 'concrete CI case sources')
    selected = set()
    for entry in plan['entries']:
        c.fields(
            entry,
            ('gate', 'host', 'owner_command', 'selectors', 'run_options', 'environment', 'inputs', 'source_files', 'junit_source'),
            ('archive_origin','selector_context'),
        )
        key = entry['gate'], entry['host']
        c.require(key not in selected, 'duplicate gate/host plan')
        selected.add(key)
        c.require(entry['gate'] in manifest['gates'], 'unknown selected gate')
        gate = manifest['gates'][entry['gate']]
        c.require(gate['mode'] == 'ci', 'manual gate cannot satisfy CI plan')
        c.require(entry['host'] in gate['hosts'], 'wrong gate host')
        c.require(entry['owner_command'] == gate['command'], 'wrong owner command')
        expected_sources = {source for contract in manifest['contracts'] for case in contract['cases'] if entry['gate'] in case['requires'] for source in case['sources']}
        c.require(
            set(c.strings(entry['source_files'], 'gate source files')) == expected_sources,
            'wrong finite gate source inventory',
        )
        commands(entry)
        env = entry['environment']
        c.require(isinstance(env, dict), 'invalid execution environment')
        for name, value in env.items():
            c.require(
                re.fullmatch(r'(RELIABURGER_[A-Z0-9_]+|CARGO_NET_OFFLINE|CARGO_BUILD_JOBS)', name) is not None,
                'unapproved environment override',
            )
            c.string(value, 'environment value')
        c.string(entry['junit_source'], 'JUnit source')
        c.require(isinstance(entry['inputs'], dict), 'invalid input hashes')
        for path, sha in entry['inputs'].items():
            c.string(path, 'input path')
            c.require(
                isinstance(sha, str) and re.fullmatch('[0-9a-f]{64}', sha) is not None,
                'invalid input hash',
            )
        for option in ('--archive-file','--config-file','--user-config-file'):
            values = selector_values(entry['selectors'],option)
            c.require(len(values)<=1,'duplicate archive/config selector')
            for value in values:c.require(
                value in entry['inputs'],
                'archive/config selector lacks expected input hash',
            )
        c.require(
            not any(option == '--tool-config-file' or option.startswith('--tool-config-file=') for option in entry['selectors']),
            'tool-config path requires separately audited NAME:path binding',
        )
        archives = selector_values(entry['selectors'],'--archive-file')
        c.require(len(archives) <= 1, 'multiple archive selectors')
        c.require(
            bool(archives) == ('archive_origin' in entry),
            'archive requires current build-origin receipt',
        )
        if archives:
            origin = entry['archive_origin']
            c.fields(origin, ('path', 'sha256', 'builder'))
            c.string(origin['path'], 'build-origin path')
            c.require(
                re.fullmatch('[0-9a-f]{64}', origin['sha256']) is not None,
                'invalid origin receipt hash',
            )
            builder = origin['builder']
            c.fields(
                builder,
                ('schema_version', 'commit', 'run_id', 'attempt', 'host', 'command', 'exit_code', 'archive_sha256'),
            )
            c.require(
                type(builder['schema_version']) is int and builder['schema_version'] == 1,
                'invalid origin schema',
            )
            for key in ('commit', 'run_id', 'attempt'):
                c.require(builder[key] == plan[key], f'stale archive build-origin {key}')
            c.require(builder['host'] in ('linux', 'darwin'), 'invalid builder host')
            c.argv(builder['command'])
            c.require(builder['command'][:3] == ['cargo', 'nextest', 'archive'], 'wrong builder command')
            c.require(
                type(builder['exit_code']) is int and builder['exit_code'] == 0,
                'archive builder failed',
            )
            c.require(
                builder['archive_sha256'] == entry['inputs'][archives[0]],
                'archive hash differs from builder receipt',
            )
    required = {(name, host) for name, gate in manifest['gates'].items()
                if gate['mode'] == 'ci' and expected['mode'] in gate.get('required_in', ['portable', 'full'])
                for host in gate['hosts']}
    c.require(
        selected == required,
        f'wrong selected gate plan: missing={required-selected}, unexpected={selected-required}',
    )
    return plan


def execution(plan, entry):
    run, discovery = commands(entry)
    return dict(owner_command=entry['owner_command'], command=run,
                discovery_command=discovery, selectors=entry['selectors'],
                environment=entry['environment'], inputs=entry['inputs'],
                source_files=entry['source_files'], selector_context=entry.get('selector_context'),
                archive_origin=entry.get('archive_origin'), plan_sha256=plan_digest(plan))


def produce(plan, entry, root, destination, run=subprocess.run):
    root, destination = Path(root).resolve(), Path(destination)
    destination.mkdir(parents=True, exist_ok=True)
    for name in ('receipt.json', 'discovery.json', 'junit.xml', 'discovery.stderr', 'execution.log'):
        (destination / name).unlink(missing_ok=True)
    source = inside(root, entry['junit_source'])
    source.unlink(missing_ok=True)  # A previous invocation can never stand in.
    for path, sha in entry['inputs'].items():
        c.require(c.digest(inside(root, path)) == sha, 'execution input changed')
    archive_origin(plan, entry, root)
    candidate_sources(root, plan['commit'], run, entry['source_files'])
    env = os.environ.copy()
    for name in list(env):
        if name.startswith(('NEXTEST_', 'RELIABURGER_')):
            del env[name]
    env.update(entry['environment'])
    details = execution(plan, entry)
    with (destination / 'discovery.json').open('wb') as output, (destination / 'discovery.stderr').open('wb') as errors:
        listed = run(details['discovery_command'], cwd=root, env=env, stdout=output, stderr=errors)
    c.require(listed.returncode == 0, 'discovery command failed')
    c.selected(c.read_json(destination / 'discovery.json'))
    with (destination / 'execution.log').open('wb') as output:
        completed = run(details['command'], cwd=root, env=env, stdout=output, stderr=subprocess.STDOUT)
    if source.is_file():
        shutil.copyfile(source, destination / 'junit.xml')
    receipt = dict(schema_version=1, gate=entry['gate'], host=entry['host'],
                   commit=plan['commit'], run_id=plan['run_id'], attempt=plan['attempt'],
                   exit_code=completed.returncode, **details)
    receipt.update(discovery_sha256=c.digest(destination / 'discovery.json'),
                   junit_sha256=c.digest(destination / 'junit.xml') if (destination / 'junit.xml').is_file() else None)
    temporary = destination / 'receipt.json.tmp'
    temporary.write_text(json.dumps(receipt, sort_keys=True, indent=2) + '\n')
    temporary.replace(destination / 'receipt.json')
    # Preserve the actual failed exit status; the aggregate also rejects it.
    if completed.returncode:
        return completed.returncode
    try:
        c.passed_junit(destination / 'junit.xml')
    except c.Invalid:
        return 1
    return 0


def aggregate(plan, manifest, directory):
    checked = {}
    for entry in plan['entries']:
        key = entry['gate'], entry['host']
        expected = {name: plan[name] for name in ('commit', 'run_id', 'attempt')}
        expected['host'] = entry['host']
        checked[key] = c.gate_evidence(
            manifest,
            entry['gate'],
            Path(directory) / entry['host'] / entry['gate'],
            expected,
            execution(plan, entry),
        )
    return checked


def main(argv=None):
    argv = sys.argv[1:] if argv is None else argv
    if argv and argv[0] == 'archive':
        parser = argparse.ArgumentParser(description='Keep a current actual archive-build result')
        for name in ('root', 'archive', 'receipt', 'commit', 'run-id', 'attempt', 'host'):
            parser.add_argument('--' + name, required=True)
        parser.add_argument('command', nargs=argparse.REMAINDER)
        args = parser.parse_args(argv[1:])
        command = args.command[1:] if args.command[:1] == ['--'] else args.command
        return archive_build(command, args.root, args.archive, args.receipt,
                             dict(commit=args.commit, run_id=args.run_id, attempt=args.attempt, host=args.host))
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('operation', choices=['produce', 'aggregate'])
    for name in ('manifest', 'plan', 'root', 'artifacts', 'commit', 'run-id', 'attempt', 'mode'):
        parser.add_argument('--' + name, required=True)
    parser.add_argument('--gate')
    parser.add_argument('--host')
    args = parser.parse_args(argv)
    manifest = c.inventory(c.read_json(args.manifest), args.root)
    expected = dict(commit=args.commit, run_id=args.run_id, attempt=args.attempt, mode=args.mode)
    plan = validate_plan(c.read_json(args.plan), manifest, expected)
    if args.operation == 'aggregate':
        aggregate(plan, manifest, args.artifacts)
        return 0
    choices = [entry for entry in plan['entries'] if (entry['gate'], entry['host']) == (args.gate, args.host)]
    c.require(len(choices) == 1, 'requested gate/host absent from current plan')
    return produce(plan, choices[0], args.root, Path(args.artifacts) / args.host / args.gate)


if __name__ == '__main__':
    try:
        sys.exit(main())
    except (c.Invalid, OSError) as error:
        print(f'contract evidence refused: {error}', file=sys.stderr)
        sys.exit(1)
