"""Validate complete libtest commands for manual and OCI owners.

The externally approved plan supplies authority. A receipt describes an actual
wrapped command; it cannot authorise itself or make a manual owner a CI gate.
"""
from pathlib import Path
import json
import re
import subprocess
import contracts as c
import gates


def context(row):
    """Validate externally supplied CI or explicitly owned manual invocation identity."""
    c.fields(row, ('commit', 'run_id', 'attempt', 'host', 'authority', 'owner'))
    c.require(re.fullmatch('[0-9a-f]{40}', row['commit']) is not None, 'invalid checkout commit')
    c.require(row['host'] in ('linux', 'darwin'), 'invalid host')
    c.require(row['authority'] in ('ci', 'manual'), 'invalid authority')
    c.string(row['run_id'], 'current run/session')
    c.require(
        isinstance(row['attempt'], str) and row['attempt'].isdigit() and (int(row['attempt']) > 0),
        'invalid current attempt',
    )
    if row['authority'] == 'ci':
        c.require(row['run_id'].isdigit() and int(row['run_id']) > 0, 'invalid CI run')
        c.require(row['owner'] is None, 'CI receipt cannot assign manual owner')
    else:
        c.string(row['owner'], 'manual owner')
        c.require(row['run_id'].startswith('manual:'), 'manual invocation requires explicit session identity')


def libtest_discovery(text):
    """Parse the audited stable pretty formatter and its exact test/benchmark count."""
    names = []
    summary = None
    for line in text.splitlines():
        if not line.strip():
            continue
        count = re.fullmatch('(\\d+) tests?, (\\d+) benchmarks?', line)
        if count:
            c.require(summary is None, 'duplicate libtest discovery summary')
            summary = (int(count[1]), int(count[2]))
            continue
        c.require(summary is None, 'libtest discovery data after summary')
        match = re.fullmatch('([^\\s]+): test', line)
        c.require(match is not None, 'invalid libtest discovery line')
        c.require(match[1] not in names, 'duplicate libtest discovery name')
        names.append(match[1])
    c.require(bool(names), 'empty libtest discovery')
    c.require(summary == (len(names), 0), 'missing or inconsistent libtest discovery summary')
    return set(names)


def libtest_completion(text, selected, exit_code):
    """Require sequential completion, the final footer and a successful real process exit.

    OCI uses nocapture with one test thread. Output may follow a start line,
    but a started case without its final completion can never qualify."""
    c.require(type(exit_code) is int and exit_code == 0, 'libtest command failed')
    started = set()
    passed = set()
    pending = None
    header = None
    footer = None
    for line in text.splitlines():
        h = re.fullmatch('running (\\d+) tests?', line)
        if h:
            c.require(header is None, 'duplicate libtest header')
            header = int(h[1])
            continue
        f = re.fullmatch(
            'test result: (ok|FAILED)\\. (\\d+) passed; (\\d+) failed; (\\d+) ignored; (\\d+) measured; (\\d+) filtered out; finished in .+',
            line,
        )
        if f:
            c.require(footer is None and pending is None, 'duplicate or premature libtest footer')
            footer = f
            continue
        m = re.fullmatch('test ([^\\s]+)\\s+\\.\\.\\. ?(.*)', line)
        if m:
            c.require(
                footer is None and pending is None,
                'test started before previous completion or after footer',
            )
            name = m[1]
            c.require(name in selected and name not in started, 'wrong or duplicate libtest case')
            started.add(name)
            if m[2] == 'ok':
                passed.add(name)
            elif m[2] in ('FAILED', 'ignored') or m[2].startswith('ignored,'):
                raise c.Invalid('libtest case failed or skipped')
            else:
                pending = name
            continue
        if pending is not None and line == 'ok':
            passed.add(pending)
            pending = None
        elif pending is not None and line in ('FAILED', 'ignored'):
            raise c.Invalid('libtest case failed or skipped')
    c.require(header is not None and footer is not None, 'missing libtest completion')
    c.require(
        footer[1] == 'ok' and int(footer[2]) == header == len(selected),
        'libtest passed count differs from discovery',
    )
    c.require(all((int(footer[i]) == 0 for i in (3, 4, 5))), 'libtest failure/skip/benchmark cannot qualify')
    c.require(started == passed == selected, 'libtest selected cases did not all complete')
    return passed


def executable_path(root, name):
    """Resolve a Cargo executable while requiring containment in the actual checkout."""
    path = Path(name)
    if not path.is_absolute():
        return gates.inside(root, name)
    resolved = path.resolve()
    c.require(resolved.is_relative_to(root.resolve()), 'Cargo executable escapes checkout')
    return resolved


def produce_build(expected, command, root, directory, binaries, run=subprocess.run):
    """Run the approved Cargo artifact build once and retain exact executable origins."""
    context(expected)
    c.argv(command)
    c.require(
        command[:2] == ['cargo', 'test'] and '--no-run' in command and ('--message-format=json' in command),
        'unapproved actual Cargo artifact builder',
    )
    c.strings(binaries, 'expected artifact targets')
    root = Path(root).resolve()
    directory = Path(directory)
    directory.mkdir(parents=True, exist_ok=True)
    for name in ('build-origin.json', 'artifacts.json', 'build.stderr'):
        (directory / name).unlink(missing_ok=True)
    gates.candidate_sources(root, expected['commit'], run)
    with (directory / 'artifacts.json').open('wb') as out, (directory / 'build.stderr').open('wb') as err:
        built = run(command, cwd=root, stdout=out, stderr=err)
    artifacts = {}
    if built.returncode == 0:
        for line in (directory / 'artifacts.json').read_text().splitlines():
            try:
                row = json.loads(line)
            except ValueError as error:
                raise c.Invalid('invalid Cargo artifact JSON') from error
            if row.get('reason') != 'compiler-artifact' or not row.get('executable'):
                continue
            name = row.get('target', {}).get('name')
            binary = 'reliaburger::' + str(name)
            if binary not in binaries:
                continue
            c.require(row.get('profile', {}).get('test') is True, 'artifact is not an actual test executable')
            c.require(binary not in artifacts, 'duplicate Cargo test executable')
            executable = row['executable']
            artifacts[binary] = dict(
                executable=executable,
                sha256=c.digest(executable_path(root, executable)),
            )
    result = dict(
        schema_version=1,
        context=expected,
        command=command,
        exit_code=built.returncode,
        artifacts=artifacts,
    )
    (directory / 'build-origin.json').write_text(json.dumps(result, sort_keys=True, indent=2) + '\n')
    return built.returncode or (0 if set(artifacts) == set(binaries) else 1)


def build_origin(origin, expected, binary, executable, root):
    """Check successful current build identity and executable bytes before dispatch."""
    c.fields(origin, ('schema_version', 'context', 'command', 'exit_code', 'artifacts'))
    c.require(
        origin['schema_version'] == 1 and type(origin['schema_version']) is int,
        'wrong binary origin schema',
    )
    c.require(origin['context'] == expected['context'], 'stale binary build origin')
    c.require(type(origin['exit_code']) is int and origin['exit_code'] == 0, 'binary builder failed')
    c.require(origin['command'] == expected['build_command'], 'wrong binary builder command')
    c.require(
        origin['command'][:2] == ['cargo', 'test'] and '--no-run' in origin['command'] and ('--message-format=json' in origin['command']),
        'builder must produce actual Cargo test artifacts',
    )
    c.require(
        isinstance(origin['artifacts'], dict) and binary in origin['artifacts'],
        'missing exact Cargo binary artifact',
    )
    artifact = origin['artifacts'][binary]
    c.fields(artifact, ('executable', 'sha256'))
    c.require(artifact['executable'] == executable, 'wrong artifact executable')
    c.require(
        c.digest(executable_path(root, executable)) == artifact['sha256'],
        'executable differs from current build origin',
    )
    return artifact


def validate_plan(plan):
    """Validate the finite audited OCI selector subset shared by discovery and execution."""
    c.fields(
        plan,
        ('schema_version', 'gate', 'context', 'binary', 'executable', 'wrapper', 'selectors', 'run_options', 'source_files', 'build_command', 'build_origin', 'build_origin_sha256'),
    )
    c.require(
        type(plan['schema_version']) is int and plan['schema_version'] == 1,
        'wrong completion plan schema',
    )
    context(plan['context'])
    c.string(plan['gate'], 'gate')
    c.string(plan['binary'], 'binary')
    c.string(plan['executable'], 'executable')
    c.require(plan['binary'].startswith('reliaburger::'), 'libtest requires exact integration binary ID')
    c.require(isinstance(plan['wrapper'], list), 'invalid command wrapper')
    if plan['wrapper']:
        c.argv(plan['wrapper'])
    c.strings(plan['source_files'], 'finite source paths')
    c.argv(plan['build_command'])
    selectors = plan['selectors']
    c.require(isinstance(selectors, list), 'invalid selectors')
    i = 0
    while i < len(selectors):
        if selectors[i] == '--ignored':
            i += 1
            continue
        c.require(selectors[i] == '--skip' and i + 1 < len(selectors), 'unknown libtest selector')
        c.string(selectors[i + 1], 'skipped filter')
        i += 2
    c.require(
        plan['run_options'] == ['--nocapture', '--test-threads=1', '--format=pretty', '--color=never'],
        'audited sequential libtest options required',
    )
    c.require(re.fullmatch('[0-9a-f]{64}', plan['build_origin_sha256']) is not None, 'invalid origin digest')
    return plan


def commands(plan):
    """Use identical wrappers and selectors for the actual discovery and execution."""
    prefix = plan['wrapper'] + [plan['executable']]
    return (prefix + plan['selectors'] + ['--list', '--format=pretty', '--color=never'], prefix + plan['selectors'] + plan['run_options'])


def produce(plan, root, directory, run=subprocess.run):
    """Retain discovery, completion and the real child exit for one approved binary."""
    validate_plan(plan)
    root = Path(root).resolve()
    directory = Path(directory)
    directory.mkdir(parents=True, exist_ok=True)
    for name in ('receipt.json', 'discovery.log', 'execution.log'):
        (directory / name).unlink(missing_ok=True)
    gates.candidate_sources(root, plan['context']['commit'], run, plan['source_files'])
    origin = gates.inside(root, plan['build_origin'])
    c.require(c.digest(origin) == plan['build_origin_sha256'], 'binary build-origin receipt changed')
    build_origin(c.read_json(origin), plan, plan['binary'], plan['executable'], root)
    (listed, executed) = commands(plan)
    with (directory / 'discovery.log').open('wb') as out:
        discovery = run(listed, cwd=root, stdout=out, stderr=subprocess.PIPE)
    c.require(discovery.returncode == 0, 'libtest discovery failed')
    selected = libtest_discovery((directory / 'discovery.log').read_text())
    with (directory / 'execution.log').open('wb') as out:
        completed = run(executed, cwd=root, stdout=out, stderr=subprocess.STDOUT)
    receipt = dict(
        schema_version=1,
        gate=plan['gate'],
        context=plan['context'],
        binary=plan['binary'],
        command=executed,
        discovery_command=listed,
        exit_code=completed.returncode,
        plan_sha256=gates.plan_digest(plan),
        build_origin_sha256=plan['build_origin_sha256'],
        discovery_sha256=c.digest(directory / 'discovery.log'),
        execution_sha256=c.digest(directory / 'execution.log'),
    )
    temp = directory / 'receipt.json.tmp'
    temp.write_text(json.dumps(receipt, sort_keys=True, indent=2) + '\n')
    temp.replace(directory / 'receipt.json')
    try:
        libtest_completion((directory / 'execution.log').read_text(), selected, completed.returncode)
    except c.Invalid:
        return completed.returncode or 1
    return 0


def evidence(plan, directory, required, allow_manual=False):
    """Consume successful exact completion under externally expected owner context."""
    validate_plan(plan)
    c.require(
        plan['context']['authority'] == 'ci' or allow_manual,
        'manual receipt cannot satisfy CI completion',
    )
    directory = Path(directory)
    row = c.read_json(directory / 'receipt.json')
    c.fields(
        row,
        ('schema_version', 'gate', 'context', 'binary', 'command', 'discovery_command', 'exit_code', 'plan_sha256', 'build_origin_sha256', 'discovery_sha256', 'execution_sha256'),
    )
    (listed, executed) = commands(plan)
    for (field, value) in [('schema_version', 1), ('gate', plan['gate']), ('context', plan['context']), ('binary', plan['binary']), ('command', executed), ('discovery_command', listed), ('plan_sha256', gates.plan_digest(plan)), ('build_origin_sha256', plan['build_origin_sha256'])]:
        c.require(row[field] == value, 'wrong or stale completion ' + field)
    c.require(c.digest(directory / 'discovery.log') == row['discovery_sha256'], 'discovery changed')
    c.require(c.digest(directory / 'execution.log') == row['execution_sha256'], 'execution log changed')
    discovered = libtest_discovery((directory / 'discovery.log').read_text())
    passed = libtest_completion((directory / 'execution.log').read_text(), discovered, row['exit_code'])
    c.require(bool(required) and set(required) <= passed, 'required cases absent from completed gate')
    return {(plan['binary'], name) for name in required}
