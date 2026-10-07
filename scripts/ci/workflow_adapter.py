#!/usr/bin/env python3
"""Actual workflow front doors for current-run producers and strict aggregation.

Expected checkout/run/attempt and selection come from workflow inputs. A producer
publishes an output digest only after its invocation and exact-case validation
succeed. Aggregation receives those digests from trusted job outputs, not from
artifact self-description. Synthetic tests do not qualify real coverage or Rust.
"""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys

# Evidence guards include ignored source files; imports must not dirty them.
sys.dont_write_bytecode = True
os.environ['PYTHONDONTWRITEBYTECODE'] = '1'

import completion
import contracts as c
import coverage_contract as coverage
import coverage_owner
import gates
import make_owner
import oci_driver
import workflow_assembly as w


JOBS = {
    'portable-linux': 'portable', 'portable-darwin': 'macos',
    'optimized-cron': 'contract-boundaries', 'rootless-linux': 'linux',
    'linux-root-storage': 'linux', 'oci-interruptions': 'linux',
    'cluster-tests': 'cluster', 'slow-tests': 'acceptance',
    'upgrade-node': 'acceptance', 'upgrade-cluster': 'acceptance',
    'standard-clients': 'acceptance',
}


def required_cases(manifest, gate):
    cases = {(case['binary'], case['test']) for family in manifest['contracts']
             for case in family['cases'] if gate in case['requires']}
    c.require(bool(cases), 'owner has no concrete required cases')
    return cases


def source_files(manifest, gate):
    return sorted({source for family in manifest['contracts'] for case in family['cases']
                   if gate in case['requires'] for source in case['sources']})


def output_name(gate):
    c.require(gate in JOBS, 'unknown workflow owner gate')
    return gate.replace('-', '_') + '_sha256'


def archive_origin(context, needs):
    """Use current successful build-job outputs before opening the archive."""
    builder = needs.get('build-tests', {})
    c.require(builder.get('result') == 'success', 'archive build job did not succeed')
    outputs = builder.get('outputs', {})
    for name in ('archive_sha256', 'archive_origin_sha256'):
        c.require(isinstance(outputs.get(name), str) and len(outputs[name]) == 64,
                  'missing trusted archive builder digest')
    expected = dict(schema_version=1, **{key: context[key] for key in ('commit', 'run_id', 'attempt')},
                    host='linux', command=w.ARCHIVE_COMMAND, exit_code=0,
                    archive_sha256=outputs['archive_sha256'])
    return dict(path='archive-origin.json', sha256=outputs['archive_origin_sha256'], builder=expected)


def verify_archive_runtime(root, context, needs):
    """Bind downloaded archive tool/runtime bytes to successful builder outputs."""
    outputs = needs.get('build-tests', {}).get('outputs', {})
    digest = outputs.get('archive_runtime_sha256')
    c.require(isinstance(digest, str) and len(digest) == 64, 'missing trusted archive runtime digest')
    path = Path(root) / 'archive-runtime.json'
    c.require(c.digest(path) == digest, 'archive runtime differs from successful builder output')
    runtime = c.read_json(path)
    c.fields(runtime, ('context', 'tools', 'command', 'exit_code', 'origin_sha256'))
    c.require(runtime['context'] == {key: context[key] for key in ('commit', 'run_id', 'attempt')}
              and runtime['exit_code'] == 0 and runtime['origin_sha256'] == outputs.get('archive_origin_sha256'),
              'wrong current archive runtime')
    make_owner.validate_tools(runtime['tools'])
    c.require(runtime['command'] == [runtime['tools']['nextest']['path'], 'nextest', 'archive'] + w.ARCHIVE_COMMAND[3:],
              'wrong actual archive executable or command')
    return runtime


def validate_expected(manifest, gate, context, expected):
    """Recheck static identity around runtime values approved by the producer."""
    c.require(expected['context'] == context, 'wrong current producer context')
    if gate == 'portable-linux':
        c.require(expected['owner_command'] == w.OWNERS[gate], 'wrong coverage owner')
        coverage.tool_context(expected['tools'], expected['tools'])
    elif gate == 'oci-interruptions':
        c.require(expected['origin_path'] == 'target/contracts/linux/oci-interruptions/build/build-origin.json',
                  'wrong trusted OCI origin location')
        c.require(set(expected['plans']) == set(oci_driver.BINARIES), 'missing actual OCI plans')
        for binary, plan in expected['plans'].items():
            c.require(plan['context'] == dict(context, authority='ci', owner=None)
                      and plan['binary'] == binary and plan['selectors'] == w.OCI_SELECTORS
                      and plan['wrapper'] == oci_driver.namespace_wrapper(expected['tools']),
                      'wrong approved OCI context or selection')
    else:
        make_owner.validate_tools(expected['tools'])
        c.require(expected['entry']['gate'] == gate
                  and expected['entry']['host'] == context['host']
                  and expected['entry']['owner_command'] == w.OWNERS[gate]
                  and expected['entry']['source_files'] == source_files(manifest, gate),
                  'wrong source-bound Make owner entry')


def verify_owner(manifest, gate, context, expected, directory):
    validate_expected(manifest, gate, context, expected)
    required = required_cases(manifest, gate)
    if gate == 'portable-linux':
        return coverage.evidence(expected, directory, required)
    if gate == 'oci-interruptions':
        oci_context = dict(context, authority='ci', owner=None)
        oci_driver.driver_evidence(directory, oci_context, expected['tools'], expected['origin_path'])
        found = set()
        for binary, plan in expected['plans'].items():
            wanted = {name for candidate, name in required if candidate == binary}
            c.require(bool(wanted), 'OCI binary lacks concrete required cases')
            found |= completion.evidence(plan, Path(directory) / binary.split('::')[1], wanted)
        c.require(found == required, 'OCI current cases not all completed')
        return found
    return make_owner.evidence(expected, directory, required)


def seal(manifest_path, manifest, gate, context, expected, directory):
    """Seal only a successful current producer, then expose its approved digest."""
    directory = Path(directory)
    verify_owner(manifest, gate, context, expected, directory)
    approved = directory / 'approved.json'
    approved.write_text(json.dumps(expected, sort_keys=True, indent=2) + '\n')
    payloads = {}
    for path in directory.rglob('*'):
        if path.is_file() and path.name != 'gate-seal.json':
            payloads[path.relative_to(directory).as_posix()] = c.digest(path)
    row = dict(schema_version=1, gate=gate, context=context,
               manifest_sha256=c.digest(manifest_path), files=payloads)
    destination = directory / 'gate-seal.json'
    destination.write_text(json.dumps(row, sort_keys=True, indent=2) + '\n')
    return c.digest(destination)


def write_output(path, name, digest):
    """Publish a trusted step output after completion checks, never beforehand."""
    c.require(len(digest) == 64 and all(char in '0123456789abcdef' for char in digest),
              'invalid successful producer digest')
    with Path(path).open('a') as output:
        output.write(name + '=' + digest + '\n')


def cron_dependencies(root, directory=None, run=subprocess.run, tools=None):
    """Bind actual tiny/main resolved dependency features before optimized tests."""
    roots = [[], ['--manifest-path', 'tests/contracts/cron/Cargo.toml']]
    observations = []
    for suffix in roots:
        command = [tools['cargo']['path'] if tools else 'cargo', 'metadata', '--locked', '--format-version=1'] + suffix
        result = run(command, cwd=root, capture_output=True, text=True)
        c.require(result.returncode == 0, 'actual cron dependency metadata failed')
        data = json.loads(result.stdout)
        c.require(type(data.get('version')) is int and data['version'] == 1, 'unknown Cargo metadata schema')
        resolution = data.get('resolve')
        c.require(isinstance(resolution, dict), 'missing actual cron dependency resolution')
        nodes = resolution.get('nodes')
        c.require(isinstance(nodes, list), 'missing actual cron resolved nodes')
        root_id = resolution.get('root')
        c.require(isinstance(root_id, str), 'missing actual cron package root')
        roots = [node for node in nodes if node.get('id') == root_id]
        c.require(len(roots) == 1, 'ambiguous or missing actual cron root node')
        dependencies = roots[0].get('deps')
        c.require(isinstance(dependencies, list), 'missing actual cron direct dependencies')
        packages = data.get('packages')
        c.require(isinstance(packages, list), 'missing actual cron packages')
        observation = {}
        for name in ('time', 'thiserror'):
            direct = [dependency for dependency in dependencies
                      if dependency.get('name') == name
                      and any(kind.get('kind') is None and kind.get('target') is None
                              for kind in dependency.get('dep_kinds', []))]
            c.require(len(direct) == 1, 'ambiguous or missing direct production cron dependency')
            package_id = direct[0].get('pkg')
            c.require(isinstance(package_id, str), 'missing direct cron dependency package ID')
            selected = [package for package in packages if package.get('id') == package_id]
            c.require(len(selected) == 1 and selected[0].get('name') == name,
                      'direct cron dependency package differs from its edge')
            resolved = [node for node in nodes if node.get('id') == package_id]
            c.require(len(resolved) == 1, 'ambiguous or missing direct cron resolved node')
            features = resolved[0].get('features')
            c.require(isinstance(features, list) and all(isinstance(feature, str) for feature in features),
                      'invalid direct cron resolved features')
            version = selected[0].get('version')
            c.require(isinstance(version, str), 'missing direct cron dependency version')
            observation[name] = dict(version=version, features=sorted(features))
        observations.append(observation)
    c.require(observations[0] == observations[1], 'optimized cron dependency graph differs from main')
    if directory is not None:
        (Path(directory) / 'dependency-comparison.json').write_text(json.dumps(observations, indent=2) + '\n')
    return observations


def produce(root, manifest_path, gate, context, directory, needs=None):
    root, directory = Path(root).resolve(), Path(directory).resolve()
    manifest = c.inventory(c.read_json(manifest_path), root)
    c.require(directory == root / 'target/contracts' / context['host'] / gate,
              'producer artifact directory differs from the predeclared workflow path')
    c.require(gate in JOBS and gate in manifest['gates'], 'unknown required workflow gate')
    declaration = manifest['gates'][gate]
    c.require(declaration['mode'] == 'ci' and context['host'] in declaration['hosts']
              and declaration['command'] == w.OWNERS[gate], 'wrong CI owner declaration')
    gates.candidate_sources(root, context['commit'], subprocess.run, source_files(manifest, gate))
    if gate == 'portable-linux':
        expected = coverage_owner.execute(root, directory, context, source_files(manifest, gate))
    elif gate == 'oci-interruptions':
        approved = {}
        tools = oci_driver.resolve_tools()
        status = oci_driver.execute(root, directory, dict(context, authority='ci', owner=None), tools,
                                    on_plan=lambda plan: approved.update({plan['binary']: plan}))
        if status:
            return status, None
        expected = dict(context=context, tools=tools, plans=approved,
                        origin_path='target/contracts/linux/oci-interruptions/build/build-origin.json')
    else:
        origin = archive_origin(context, needs or {}) if JOBS[gate] in ('cluster', 'acceptance') else None
        runtime = verify_archive_runtime(root, context, needs) if origin else None
        entry = w.entry_from_make(root, manifest, gate, context['host'], archive_origin=origin)
        if runtime is not None:
            entry['archive_runtime'] = runtime
        approved = []
        tools = make_owner.capture_tools(dict(os.environ), subprocess.run)
        dependencies = cron_dependencies(root, tools=tools) if gate == 'optimized-cron' else None
        status = make_owner.execute(root, directory, context, entry, tools=tools, on_expected=approved.append)
        if status:
            return status, None
        c.require(len(approved) == 1, 'missing pre-execution Make plan')
        expected = approved[0]
        if dependencies is not None:
            (directory / 'dependency-comparison.json').write_text(json.dumps(dependencies, indent=2) + '\n')
    return 0, seal(manifest_path, manifest, gate, context, expected, directory)


def aggregate(root, manifest_path, incoming, context, needs, bindings_path=None):
    """Waited job results and trusted output hashes authorize artifact consumption."""
    root, incoming = Path(root), Path(incoming)
    manifest = c.inventory(c.read_json(manifest_path), root)
    changes = needs.get('changes', {}).get('outputs', {})
    selection = w.selected_workflow(context, code=changes.get('code'), heavy=changes.get('heavy'))
    c.require(selection is not None, 'no selected code qualification')
    w.job_results(selection, needs)
    expected_gates = {(name, host) for name, gate in manifest['gates'].items()
                      if gate['mode'] == 'ci' and selection['mode'] in gate.get('required_in', ['portable', 'full'])
                      for host in gate['hosts']}
    c.require({name for name, _ in expected_gates} == set(selection['gates']),
              'manifest differs from the finite actual workflow gate schedule')
    gates.candidate_sources(root, context['commit'], subprocess.run)
    verified = {}
    for gate, host in sorted(expected_gates):
        c.require(gate in JOBS, 'unknown actual workflow owner')
        outputs = needs[JOBS[gate]].get('outputs', {})
        digest = outputs.get(output_name(gate))
        c.require(isinstance(digest, str) and len(digest) == 64, 'missing successful owner step output')
        directory = incoming / ('contract-' + host + '-' + gate)
        c.require(
            c.digest(directory / 'gate-seal.json') == digest,
            'owner artifact differs from trusted step output',
        )
        row = c.read_json(directory / 'gate-seal.json')
        c.fields(row, ('schema_version', 'gate', 'context', 'manifest_sha256', 'files'))
        current = dict(context, host=host)
        c.require(type(row['schema_version']) is int and row['schema_version'] == 1
                  and row['gate'] == gate and row['context'] == current
                  and row['manifest_sha256'] == c.digest(manifest_path), 'wrong current sealed owner')
        c.require(
            isinstance(row['files'], dict) and 'approved.json' in row['files'],
            'missing approved owner plan',
        )
        for name, expected_digest in row['files'].items():
            c.require(
                c.digest(gates.inside(directory, name)) == expected_digest,
                'sealed owner payload changed',
            )
        approved = c.read_json(directory / 'approved.json')
        verified[(gate, host)] = verify_owner(manifest, gate, current, approved, directory)
    if selection['mode'] == 'full':
        c.require(bindings_path is not None, 'complete legacy ignored-owner bindings required')
        import ignored_owners
        owned = {}
        for (gate, _), cases in verified.items():
            command = w.OWNERS[gate]
            key = ('make', command[1]) if command[0] == 'make' else ('script', command[0])
            owned.setdefault(key, set()).update(cases)
        # The existing test-linux recipe excludes these two groups because
        # the original OCI driver owns their isolated execution in this job.
        # Expand only the committed finite alias table, after exact current
        # whole-driver and per-case success; original reason strings stay put.
        import oci_legacy_owner
        aliases_path = root / 'scripts/ci/legacy-oci-aliases.json'
        bindings = c.read_json(bindings_path)
        gates.candidate_sources(root, context['commit'], subprocess.run,
                                ['scripts/ci/legacy-oci-aliases.json'])
        aliases = oci_legacy_owner.qualify(
            root, c.read_json(aliases_path), bindings, manifest, manifest_path,
            dict(context, host='linux'), needs['linux'],
            incoming / 'contract-linux-oci-interruptions',
            verified[('oci-interruptions', 'linux')],
        )
        owned.setdefault(oci_legacy_owner.DECLARED_OWNER, set()).update(aliases)
        problems = ignored_owners.evidence_problems(root, incoming, bindings, owned)
        c.require(not problems, 'legacy ignored owner evidence refused: ' + '; '.join(problems))
    return verified


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('operation', choices=['produce', 'aggregate', 'archive'])
    for name in ('root', 'commit', 'run-id', 'attempt'):
        parser.add_argument('--' + name, required=True)
    for name in ('manifest', 'gate', 'host', 'directory', 'needs', 'github-output', 'ignored-bindings'):
        parser.add_argument('--' + name)
    args = parser.parse_args(argv)
    context = dict(commit=args.commit, run_id=args.run_id, attempt=args.attempt)
    if args.operation == 'archive':
        c.require(args.host == 'linux' and args.github_output, 'current Linux archive outputs required')
        base = dict(os.environ)
        tools = make_owner.capture_tools(base, subprocess.run)
        base.update(CARGO=tools['cargo']['path'], RUSTC=tools['rustc']['path'])

        def actual_builder(command, **kwargs):
            make_owner.tool_bytes(tools)
            if command[:3] == ['cargo', 'nextest', 'archive']:
                command = [tools['nextest']['path'], 'nextest', 'archive'] + command[3:]
                kwargs['env'] = base
            return subprocess.run(command, **kwargs)

        status = gates.archive_build(w.ARCHIVE_COMMAND, args.root, 'tests.tar.zst', 'archive-origin.json',
                                     dict(context, host='linux'), actual_builder)
        runtime = dict(context=context, tools=tools, command=[tools['nextest']['path'], 'nextest', 'archive']
                       + w.ARCHIVE_COMMAND[3:], exit_code=status,
                       origin_sha256=c.digest(Path(args.root) / 'archive-origin.json'))
        (Path(args.root) / 'archive-runtime.json').write_text(json.dumps(runtime, indent=2) + '\n')
        if status:
            return status
        write_output(args.github_output, 'archive_sha256', c.digest(Path(args.root) / 'tests.tar.zst'))
        write_output(
            args.github_output,
            'archive_origin_sha256',
            c.digest(Path(args.root) / 'archive-origin.json'),
        )
        write_output(
            args.github_output,
            'archive_runtime_sha256',
            c.digest(Path(args.root) / 'archive-runtime.json'),
        )
        return 0
    c.require(args.manifest and args.directory, 'manifest and owner artifact directory required')
    if args.operation == 'aggregate':
        c.require(args.needs, 'trusted workflow needs object required')
        aggregate(
            args.root,
            args.manifest,
            args.directory,
            context,
            c.read_json(args.needs),
            args.ignored_bindings,
        )
        return 0
    c.require(args.gate and args.host and args.github_output, 'current owner gate/host/output required')
    context['host'] = args.host
    needs = c.read_json(args.needs) if args.needs else None
    status, digest = produce(args.root, args.manifest, args.gate, context, args.directory, needs)
    if status:
        return status
    write_output(args.github_output, output_name(args.gate), digest)
    return 0


if __name__ == '__main__':
    try:
        raise SystemExit(main())
    except (c.Invalid, OSError, ValueError) as error:
        print('workflow contract evidence refused: ' + str(error), file=sys.stderr)
        raise SystemExit(1)
