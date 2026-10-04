#!/usr/bin/env python3
"""Intercept the approved cargo-nextest child of the coverage owner.

The owner resolves genuine tools and installs its one-use approval before
execution. The adapter preserves inherited instrumentation, discovers the same
selection and delegates one run without inventing coverage wrapper flags.
"""
import json
import os
import shutil
import subprocess
import sys
from pathlib import Path
# Evidence guards include ignored source files; imports must not dirty them.
sys.dont_write_bytecode = True
os.environ['PYTHONDONTWRITEBYTECODE'] = '1'

import contracts as c
import coverage_contract as k
import gates

def invoke(argv, configuration, environment, run=subprocess.run):
    """Approve one actual nextest child, then list and run in its inherited context.

    Cargo includes the subcommand name in argv. Help/version are genuine
    passthroughs and do not count as test execution. With a live owner, its
    pre-execution approval must preserve all parent-owned plan fields.
    """
    c.fields(
        configuration,
        ('root', 'genuine_nextest', 'adapter_path', 'expected', 'directory', 'junit_source', 'source_files'),
        ('handshake',),
    )
    genuine = Path(configuration['genuine_nextest'])
    adapter = Path(configuration['adapter_path'])
    c.require(
        genuine.is_absolute() and genuine.resolve() != adapter.resolve(),
        'adapter recursion/unresolved genuine tool',
    )
    expected = configuration['expected']
    c.require(
        c.digest(genuine) == expected['tools']['nextest']['sha256'],
        'genuine nextest binary changed',
    )
    actual = [str(genuine)] + argv
    if argv[:2] != ['nextest', 'run']:
        return run(actual, env=environment).returncode
    if configuration.get('handshake') is not None:
        from coverage_owner import request
        approved = request(
            configuration['handshake'],
            dict(kind='handshake', argv=actual, environment=k.snapshot_environment(environment)),
        )
        for name in (
            'context',
            'nonce',
            'tools',
            'run_argv',
            'nextest_prefix',
            'target_dir',
            'stages',
            'owner_command',
        ):
            c.require(approved[name] == expected[name], 'owner approval changed trusted ' + name)
        expected = approved
    root = Path(configuration['root']).resolve()
    directory = Path(configuration['directory'])
    directory.mkdir(parents=True, exist_ok=True)
    marker = directory / 'interception.claim'
    try:
        with marker.open('x') as out:
            out.write(json.dumps(dict(nonce=expected['nonce'], pid=os.getpid())) + '\n')
    except FileExistsError:
        raise c.Invalid('duplicate actual nextest run interception')
    c.require(actual == expected['run_argv'], 'actual instrumented argv differs from trusted owner plan')
    captured = k.snapshot_environment(environment)
    k.instrumented_context(
        captured,
        expected['instrumentation'],
        expected['tools'],
        expected['context']['commit'],
        expected['target_dir'],
    )
    c.strings(configuration['source_files'], 'current finite source inputs')
    gates.candidate_sources(root, expected['context']['commit'], run, configuration['source_files'])
    listed = k.list_argv(actual, expected['nextest_prefix'])
    for name in ('child.json', 'discovery.json', 'junit.xml', 'discovery.stderr', 'execution.log'):
        (directory / name).unlink(missing_ok=True)
    source = Path(configuration['junit_source'])
    source = source if source.is_absolute() else root / source
    c.require(source.resolve().is_relative_to(root), 'JUnit source escapes actual workspace')
    source.unlink(missing_ok=True)
    with (directory / 'discovery.json').open('wb') as out, (directory / 'discovery.stderr').open('wb') as err:
        discovery = run(listed, cwd=root, env=environment, stdout=out, stderr=err)
    c.require(discovery.returncode == 0, 'actual instrumented discovery failed')
    c.selected(c.read_json(directory / 'discovery.json'))
    with (directory / 'execution.log').open('wb') as out:
        completed = run(actual, cwd=root, env=environment, stdout=out, stderr=subprocess.STDOUT)
    if source.is_file():
        shutil.copyfile(source, directory / 'junit.xml')
    child = dict(
        schema_version=1,
        context=expected['context'],
        nonce=expected['nonce'],
        tools=expected['tools'],
        run_environment=captured,
        list_environment=captured,
        target_dir=expected['target_dir'],
        command=actual,
        discovery_command=listed,
        exit_code=completed.returncode,
        discovery_sha256=c.digest(directory / 'discovery.json'),
        junit_sha256=c.digest(directory / 'junit.xml') if (directory / 'junit.xml').is_file() else None,
    )
    tmp = directory / 'child.json.tmp'
    tmp.write_text(json.dumps(child, sort_keys=True, indent=2) + '\n')
    tmp.replace(directory / 'child.json')
    if completed.returncode:
        return completed.returncode
    try:
        c.passed_junit(directory / 'junit.xml')
    except c.Invalid:
        return 1
    return 0

def overlay_path(original, cargo_home, overlay):
    """Prepend the overlay while keeping resolved Cargo home-bin explicitly in PATH.

    Cargo otherwise gives its home-bin external command extra precedence.
    This dispatch behavior has a separate actual parent proof; instrumented
    Rust execution through the adapter remains unqualified.
    """
    home_bin = str(Path(cargo_home) / 'bin')
    parts = original.split(os.pathsep)
    parts = [str(Path(overlay))] + [entry for entry in parts if entry != str(Path(overlay))]
    if home_bin not in parts:
        parts.append(home_bin)
    return os.pathsep.join(parts)

def main():
    """Dispatch the standalone adapter using its externally controlled config."""
    path = os.environ.get('RELIABURGER_COVERAGE_ADAPTER_CONFIG')
    c.require(path is not None, 'current coverage owner config is required')
    return invoke(sys.argv[1:], c.read_json(path), os.environ.copy())
if __name__ == '__main__':
    try:
        sys.exit(main())
    except c.Invalid as error:
        print(error, file=sys.stderr)
        sys.exit(1)
