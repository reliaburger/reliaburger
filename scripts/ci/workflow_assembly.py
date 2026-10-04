"""Build trusted workflow selections and source-derived Make selectors.

Workflow inputs supply checkout and job context before a producer runs.
Artifact self-description cannot choose its own required gates or selectors.
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import re
import shlex

# Evidence guards include ignored source files; imports must not dirty them.
import sys
import os
sys.dont_write_bytecode = True
os.environ['PYTHONDONTWRITEBYTECODE'] = '1'

import contracts as c
import gates


OWNERS = {
    'portable-linux': ['make', 'coverage'],
    'portable-darwin': ['make', 'test'],
    'optimized-cron': ['make', 'test-contract-boundaries'],
    'rootless-linux': ['make', 'test-rootless-runc'],
    'linux-root-storage': ['make', 'test-linux'],
    'oci-interruptions': ['scripts/release/qualify-oci-interruptions.sh'],
    'cluster-tests': ['make', 'test-cluster'],
    'slow-tests': ['make', 'test-slow'],
    'upgrade-node': ['make', 'test-upgrade-node'],
    'upgrade-cluster': ['make', 'test-upgrade-cluster'],
    'standard-clients': ['make', 'test-standard-clients'],
}
ARCHIVE_COMMAND = ['cargo', 'nextest', 'archive', '--locked', '--archive-file', 'tests.tar.zst']
ARCHIVED_SELECTORS = [
    '--archive-file', 'tests.tar.zst', '--extract-to', '.',
    '--extract-overwrite', '--workspace-remap', '.', '--profile', 'ci',
]
OCI_BUILD_COMMAND = [
    'cargo', 'test', '--features', 'ebpf', '--test', 'owned_runc',
    '--test', 'owned_network', '--test', 'oci_crash',
    '--no-run', '--message-format=json',
]
OCI_SELECTORS = [
    '--ignored', '--skip', 'normal_rootless_bun', '--skip',
    'actual_host_reboot', '--skip', 'actual_bun_kernel_discovery_host_reboot',
]
LINUX_EXCLUDE = '& not binary(owned_runc) & not binary(owned_network)'
PORTABLE_GATES = ['portable-linux', 'portable-darwin', 'optimized-cron']
HEAVY_GATES = [name for name in OWNERS if name not in PORTABLE_GATES]


def selected_workflow(expected, *, code, heavy):
    """Compute required jobs from trusted changes-job outputs, never artifacts."""
    c.fields(expected, ('commit', 'run_id', 'attempt'))
    c.require(re.fullmatch('[0-9a-f]{40}', expected['commit']) is not None,
              'actual checkout SHA required')
    for field in ('run_id', 'attempt'):
        c.require(isinstance(expected[field], str) and expected[field].isdigit()
                  and int(expected[field]) > 0, 'current workflow identity required')
    c.require(code in ('true', 'false') and heavy in ('true', 'false'),
              'trusted literal changes-job outputs required')
    c.require(code == 'true' or heavy == 'false', 'heavy work requires code selection')
    if code == 'false':
        return None
    full = heavy == 'true'
    jobs = ['changes', 'portable', 'macos', 'contract-boundaries']
    if full:
        jobs += ['linux', 'build-tests', 'cluster', 'acceptance']
    return dict(schema_version=1, **expected, mode='full' if full else 'portable',
                jobs=jobs, gates=PORTABLE_GATES + (HEAVY_GATES if full else []))


def job_results(selection, needs):
    """Check workflow-provided job conclusions after every selected owner ended."""
    c.require(isinstance(needs, dict), 'trusted workflow needs object required')
    for name in selection['jobs']:
        c.require(name in needs and isinstance(needs[name], dict),
                  f'missing selected owner job: {name}')
        c.require(needs[name].get('result') == 'success',
                  f'selected owner job did not complete successfully: {name}')


def recipe(text, target):
    """Read one literal existing Make recipe without evaluating shell or Make."""
    lines = text.splitlines()
    starts = [i for i, line in enumerate(lines)
              if re.match(re.escape(target) + r':(?:\s|$)', line)]
    c.require(len(starts) == 1, 'missing or duplicate Make owner target')
    result = []
    for line in lines[starts[0] + 1:]:
        if line.startswith('\t'):
            result.append(line[1:])
        elif line.strip() and not line.startswith('#'):
            break
    return result


def make_selection(root, target, *, archived=False):
    """Share the existing recipe suffix for discovery and run.

    Actual Make still owns prerequisite builds and image mirror lifetime. The
    producer must intercept its one NEXTEST invocation, not execute this derived
    selection instead of Make. Unknown expansion shapes refuse for root review.
    """
    root = Path(root).resolve()
    lines = recipe(root.joinpath('Makefile').read_text(), target)
    nextest = [line for line in lines if '$(NEXTEST)' in line]
    c.require(len(nextest) == 1, 'owner must invoke NEXTEST exactly once')
    prefix, suffix = nextest[0].split('$(NEXTEST)', 1)
    c.require('$(NEXTEST)' not in suffix, 'multiple nextest invocations')
    mirror = '$(WITH_TEST_IMAGES)' in prefix
    c.require(prefix.count('$(WITH_TEST_IMAGES)') <= 1, 'duplicate image mirror')
    prefix = prefix.replace('$(WITH_TEST_IMAGES)', '').replace('$(CURDIR)', str(root))
    suffix = suffix.replace('$(LINUX_EXCLUDE)', LINUX_EXCLUDE if target == 'test-linux' else '')
    c.require('$' not in prefix + suffix, 'unknown Make expansion requires review')
    environment = {}
    for token in shlex.split(prefix):
        name, separator, value = token.partition('=')
        c.require(separator and re.fullmatch(r'RELIABURGER_[A-Z0-9_]+', name),
                  'unapproved owner shell prefix')
        c.require(name not in environment, 'duplicate environment prefix')
        environment[name] = value
    if mirror:
        c.require(target == 'test-linux', 'unknown image mirror owner')
        environment['RELIABURGER_TEST_IMAGE_MIRROR'] = '127.0.0.1:5099'
    selectors = (ARCHIVED_SELECTORS[:] if archived else ['--profile', 'ci']) + shlex.split(suffix)
    c.require('--' not in selectors, 'extra test runner arguments require review')
    return selectors, environment


def entry_from_make(root, manifest, gate, host, *, archive_origin=None):
    """Build ordinary nextest selectors from the current committed owner recipe.

    This is a plan, not execution. Original Make must still run once around the
    intercepting producer; do not lose its prerequisites or mirror lifetime.
    Coverage and OCI require their distinct owner producers.
    """
    c.require(gate in OWNERS and gate not in ('portable-linux', 'oci-interruptions'),
              'gate requires its own audited owner producer')
    owner = OWNERS[gate]
    c.require(gate in manifest['gates'] and manifest['gates'][gate]['command'] == owner
              and host in manifest['gates'][gate]['hosts'], 'wrong manifest owner or host')
    archived = gate in ('cluster-tests', 'slow-tests', 'upgrade-node', 'upgrade-cluster', 'standard-clients')
    c.require(archived == (archive_origin is not None), 'archived owner needs trusted builder outputs')
    selectors, environment = make_selection(root, owner[1], archived=archived)
    source_files = sorted({source for family in manifest['contracts'] for case in family['cases']
                           if gate in case['requires'] for source in case['sources']})
    c.require(bool(source_files), 'empty concrete owner source inventory')
    config = 'tests/contracts/cron/nextest.toml' if gate == 'optimized-cron' else '.config/nextest.toml'
    inputs = {config: c.digest(Path(root) / config)}
    if gate == 'optimized-cron':
        for name in ('Cargo.toml', 'Cargo.lock', 'tests/contracts/cron/Cargo.toml', 'tests/contracts/cron/Cargo.lock'):
            inputs[name] = c.digest(Path(root) / name)
    entry = dict(gate=gate, host=host, owner_command=owner, selectors=selectors,
                 run_options=['--no-tests=fail', '--retries=0'], environment=environment,
                 inputs=inputs, source_files=source_files,
                 junit_source=('tests/contracts/cron/' if gate == 'optimized-cron' else '') + 'target/nextest/ci/junit.xml',
                 selector_context=dict(profile='ci', ignored='only' if '--run-ignored=only' in selectors else 'default',
                                       default_filter='honor'))
    if archived:
        c.require(archive_origin['builder']['command'] == ARCHIVE_COMMAND,
                  'wrong trusted actual archive builder command')
        entry['archive_origin'] = archive_origin
        inputs['tests.tar.zst'] = archive_origin['builder']['archive_sha256']
    gates.commands(entry)
    return entry


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('commit', 'run-id', 'attempt', 'code', 'heavy', 'output'):
        parser.add_argument('--' + name, required=True)
    args = parser.parse_args(argv)
    plan = selected_workflow(dict(commit=args.commit, run_id=args.run_id, attempt=args.attempt),
                             code=args.code, heavy=args.heavy)
    c.require(plan is not None, 'no code selected; no qualification plan generated')
    Path(args.output).write_text(json.dumps(plan, indent=2) + '\n')
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
