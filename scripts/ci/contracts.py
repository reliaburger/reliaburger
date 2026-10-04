#!/usr/bin/env python3
"""Validate a finite contract inventory and gate-scoped completion evidence.

Callers supply the expected checkout and workflow context. Artifact filenames
and self-described receipts cannot establish freshness or owner authority.
"""
from __future__ import annotations
import hashlib
import json
from pathlib import Path
import re
import xml.etree.ElementTree as ET


class Invalid(ValueError):
    pass


def require(condition, message):
    if not condition:
        raise Invalid(message)


def fields(row, required, optional=()):
    require(isinstance(row, dict), 'expected object')
    require(set(required) <= row.keys(), f'missing fields: {set(required) - row.keys()}')
    require(
        row.keys() <= set(required) | set(optional),
        f'unknown fields: {row.keys() - set(required) - set(optional)}',
    )


def string(value, label):
    require(isinstance(value, str) and bool(value.strip()), f'{label} must be nonempty string')
    return value


def strings(value, label):
    require(isinstance(value, list) and bool(value), f'{label} must be nonempty list')
    for item in value:
        string(item, label)
    require(len(set(value)) == len(value), f'duplicate {label}')
    return value


def argv(value):
    require(isinstance(value, list) and bool(value), 'command argv must be nonempty list')
    for item in value:
        string(item, 'command argv')
    require(all('\x00' not in item for item in value), 'NUL in argv')


def read_json(path):
    def object_pairs(pairs):
        row = {}
        for key, value in pairs:
            require(key not in row, f'duplicate JSON key: {key}')
            row[key] = value
        return row
    try:
        return json.loads(Path(path).read_text(), object_pairs_hook=object_pairs)
    except (OSError, ValueError) as error:
        raise Invalid(f'{path}: {error}') from error


def inventory(row, root):
    fields(row, ('schema_version', 'gates', 'contracts'))
    require(type(row['schema_version']) is int and row['schema_version'] == 1, 'unsupported schema_version')
    require(isinstance(row['gates'], dict) and bool(row['gates']), 'empty gates')
    used_gates, case_ids, assignments, contract_ids = set(), set(), set(), set()
    for name, gate in row['gates'].items():
        string(name, 'gate')
        fields(gate, ('mode', 'hosts', 'command'), ('owner', 'required_in'))
        require(gate['mode'] in ('ci', 'manual'), 'invalid gate mode')
        strings(gate['hosts'], 'hosts')
        require(set(gate['hosts']) <= {'linux', 'darwin'}, 'unsupported host')
        if 'required_in' in gate:
            require(
                set(strings(gate['required_in'], 'required_in')) <= {'portable', 'full'},
                'unknown gate-plan mode',
            )
        argv(gate['command'])
        command = gate['command']
        if command[0] == 'make':
            require(
                len(command) >= 2 and re.search(r'^' + re.escape(command[1]) + r':', (Path(root) / 'Makefile').read_text(), re.M),
                'Makefile gate missing',
            )
        elif command[0].startswith('scripts/'):
            require((Path(root) / command[0]).is_file(), 'script gate missing')
        else:
            raise Invalid('gate must name actual make target or repository script')
        if gate['mode'] == 'manual':
            string(gate.get('owner'), 'manual owner')
        else:
            require('owner' not in gate, 'CI gate cannot carry manual owner')
    require(isinstance(row['contracts'], list) and bool(row['contracts']), 'empty contracts')
    for contract in row['contracts']:
        fields(
            contract,
            ('id', 'issue', 'promise', 'supported_paths', 'refused_paths', 'boundaries', 'cases'),
        )
        identifier = string(contract['id'], 'contract id')
        require(identifier not in contract_ids, 'duplicate contract id')
        contract_ids.add(identifier)
        require(type(contract['issue']) is int and contract['issue'] > 0, 'invalid issue')
        string(contract['promise'], 'promise')
        for field in ('supported_paths', 'refused_paths', 'boundaries'):
            strings(contract[field], field)
        require(isinstance(contract['cases'], list) and bool(contract['cases']), 'empty cases')
        for case in contract['cases']:
            fields(case, ('id', 'binary', 'test', 'requires'), ('sources',))
            identifier = string(case['id'], 'case id')
            require(identifier not in case_ids, 'duplicate case id')
            case_ids.add(identifier)
            string(case['binary'], 'binary')
            string(case['test'], 'test')
            if 'sources' in case:
                for source in strings(case['sources'], 'case sources'):
                    path = Path(source)
                    require(
                        not path.is_absolute() and '..' not in path.parts and (Path(root) / path).is_file(),
                        'missing or uncontained case source',
                    )
                    require(
                        (Path(root) / path).resolve().is_relative_to(Path(root).resolve()),
                        'case source symlink escapes checkout',
                    )
                    require(
                        not (Path(root) / path).is_symlink(),
                        'case source must be a tracked regular file',
                    )
            for gate in strings(case['requires'], 'requires'):
                require(gate in row['gates'], 'unknown required gate')
                identity = gate, case['binary'], case['test']
                require(identity not in assignments, 'duplicate binary/test/gate assignment')
                assignments.add(identity)
                used_gates.add(gate)
    require(used_gates == row['gates'].keys(), 'gate has no assigned concrete cases')
    return row


def selected(discovery):
    """Parse pinned nextest0.9.145 lists; fail closed on missing selections."""
    require(
        isinstance(discovery, dict) and isinstance(discovery.get('rust-build-meta'), dict),
        'missing discovery build metadata',
    )
    require(
        type(discovery.get('test-count')) is int and discovery['test-count'] > 0,
        'empty/invalid discovery test count',
    )
    suites = discovery.get('rust-suites')
    require(isinstance(suites, dict) and bool(suites), 'empty discovery suites')
    found = set()
    for binary, suite in suites.items():
        require(isinstance(suite, dict) and suite.get('binary-id') == binary, 'wrong discovery binary id')
        require(suite.get('status') in ('listed', 'skipped'), 'unknown discovery suite status')
        if suite['status'] == 'skipped':
            continue
        cases = suite.get('testcases')
        require(isinstance(cases, dict), 'missing testcases')
        for name, case in cases.items():
            require(isinstance(case, dict), 'bad discovery testcase')
            match = case.get('filter-match')
            require(
                isinstance(match, dict) and match.get('status') in ('matches', 'mismatch'),
                'unknown filter status',
            )
            if match['status'] == 'matches':
                identity = binary, string(name, 'discovery name')
                require(identity not in found, 'duplicate discovery identity')
                found.add(identity)
    require(bool(found), 'empty selected discovery')
    return found


def passed_junit(path):
    """Exact binary/full name; a testcase element alone is not execution proof."""
    try:
        root = ET.parse(path).getroot()
    except (OSError, ET.ParseError) as error:
        raise Invalid(f'malformed JUnit: {error}') from error
    require(root.tag == 'testsuites', 'invalid JUnit root')
    found = set()
    suites = list(root.findall('testsuite'))
    require(bool(suites), 'empty JUnit suites')
    for suite in suites:
        binary = string(suite.get('name'), 'JUnit suite name')
        require(not suite.findall('testsuite'), 'nested suite unsupported')
        for case in suite.findall('testcase'):
            require(case.get('classname') == binary, 'JUnit binary/classname mismatch')
            identity = binary, string(case.get('name'), 'JUnit test name')
            require(identity not in found, 'duplicate JUnit testcase')
            require(
                not any(case.findall(tag) for tag in ('skipped', 'failure', 'error', 'rerunFailure', 'flakyFailure')),
                f'case did not pass exactly once: {identity}',
            )
            found.add(identity)
    require(bool(found), 'empty JUnit execution')
    return found


def digest(path):
    try:
        result = hashlib.sha256()
        with Path(path).open('rb') as source:
            for chunk in iter(lambda: source.read(1024 * 1024), b''):
                result.update(chunk)
        return result.hexdigest()
    except OSError as error:
        raise Invalid(str(error)) from error


def gate_evidence(manifest, gate_name, directory, expected, execution=None):
    """CI artifact envelope is bound to current head/run/attempt, gate and bytes.

    Artifact collection must retain distinct platform/gate directories. A merged
    directory cannot let a similarly named report overwrite another gate.
    """
    gate = manifest['gates'][gate_name]
    fields(expected, ('commit', 'run_id', 'attempt', 'host'))
    require(expected['host'] in gate['hosts'], 'gate is not applicable to current host')
    directory = Path(directory)
    receipt = read_json(directory / 'receipt.json')
    base_fields = ('schema_version', 'gate', 'commit', 'run_id', 'attempt', 'host', 'command', 'exit_code', 'discovery_sha256', 'junit_sha256')
    extended = ('owner_command', 'discovery_command', 'selectors', 'environment', 'inputs', 'source_files', 'archive_origin', 'plan_sha256', 'selector_context') if execution is not None else ()
    fields(receipt, base_fields + extended)
    if execution is not None:
        for key in extended:
            require(receipt[key] == execution[key], f'wrong execution {key}')
    require(
        type(receipt['schema_version']) is int and receipt['schema_version'] == 1,
        'unsupported receipt schema',
    )
    for key in ('commit', 'run_id', 'attempt', 'host'):
        require(receipt[key] == expected[key], f'stale/wrong {key}')
    require(receipt['gate'] == gate_name, 'wrong gate receipt')
    require(
        receipt['command'] == (execution['command'] if execution is not None else gate['command']),
        'wrong gate command',
    )
    require(type(receipt['exit_code']) is int and receipt['exit_code'] == 0, 'gate command failed')
    require(digest(directory / 'discovery.json') == receipt['discovery_sha256'], 'discovery hash mismatch')
    require(digest(directory / 'junit.xml') == receipt['junit_sha256'], 'JUnit hash mismatch')
    discovered = selected(read_json(directory / 'discovery.json'))
    executed = passed_junit(directory / 'junit.xml')
    require(executed <= discovered, 'executed cases absent from assigned discovery')
    required = {(case['binary'], case['test']) for contract in manifest['contracts'] for case in contract['cases'] if gate_name in case['requires']}
    require(bool(required), 'empty applicable gate contract selection')
    require(required <= discovered, f'contract cases absent/mismatched in discovery: {required - discovered}')
    require(required <= executed, f'contract cases not successfully executed: {required - executed}')
    return required
