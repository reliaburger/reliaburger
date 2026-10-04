"""Validate observed cargo-llvm-cov0.9.1 instrumentation and completion.

Private wrapper fields are opaque tool context, not an instrumentation
implementation. Tool identity, checkout, selectors and actual child completion
must match the owner's approved pre-execution context.
"""
from pathlib import Path
import re
import contracts as c
OPAQUE = (
    '__CARGO_LLVM_COV_RUSTC_WRAPPER',
    '__CARGO_LLVM_COV_RUSTC_WRAPPER_RUSTFLAGS',
    '__CARGO_LLVM_COV_RUSTC_WRAPPER_COVERAGE_TARGET',
    '__CARGO_LLVM_COV_RUSTC_WRAPPER_HOST',
    '__CARGO_LLVM_COV_RUSTC_WRAPPER_CRATE_NAMES',
    '__CARGO_LLVM_COV_RUSTC_WRAPPER_PRE_EXISTING',
)
BASE = (
    'LLVM_COV',
    'LLVM_PROFDATA',
    'LLVM_COV_FLAGS',
    'LLVM_PROFDATA_FLAGS',
    'CARGO_LLVM_COV_FLAGS',
    'CARGO_LLVM_PROFDATA_FLAGS',
    'RUSTC_BOOTSTRAP',
    'CARGO',
    'CARGO_HOME',
    'RUSTDOC',
    'CARGO_BUILD_RUSTC',
    'CARGO_BUILD_RUSTC_WRAPPER',
    'CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER',
    'CARGO_BUILD_RUSTDOC',
    'CARGO_BUILD_TARGET',
    'CARGO_BUILD_TARGET_DIR',
    'CARGO_BUILD_RUSTFLAGS',
    'CARGO_BUILD_RUSTDOCFLAGS',
    'CARGO_BUILD_INCREMENTAL',
    'CARGO_INCREMENTAL',
    'RUSTC_WRAPPER',
    'RUSTC_WORKSPACE_WRAPPER',
    'RUSTC',
    'RUSTFLAGS',
    'CARGO_ENCODED_RUSTFLAGS',
    'RUSTDOCFLAGS',
    'CARGO_ENCODED_RUSTDOCFLAGS',
    'CARGO_TARGET_DIR',
    'CARGO_BUILD_BUILD_DIR',
    'CARGO_LLVM_COV_TARGET_DIR',
    'CARGO_LLVM_COV_BUILD_DIR',
    'CARGO_LLVM_COV',
    'CARGO_LLVM_COV_SHOW_ENV',
    'LLVM_PROFILE_FILE',
    'RUSTUP_TOOLCHAIN',
    'RELIABURGER_GIT_SHA',
)

def dynamic_key(key):
    return re.fullmatch(
        'CARGO_TARGET_[A-Z0-9_]+_(RUSTFLAGS|RUSTDOCFLAGS|LINKER|RUNNER)',
        key,
    ) or re.fullmatch('CARGO_PROFILE_[A-Z0-9_]+_(CODEGEN_UNITS|DEBUG|DEBUG_ASSERTIONS|INCREMENTAL|LTO|OVERFLOW_CHECKS|OPT_LEVEL|PANIC|RPATH|SPLIT_DEBUGINFO|STRIP)', key)

def snapshot_environment(environment):
    """Capture audited compiler context while excluding credentials."""
    unknown = {key for key in environment if key.startswith('__CARGO_LLVM_COV_')} - set(OPAQUE)
    c.require(not unknown, 'unreviewed private instrumentation keys')
    dynamic = {key for key in environment if dynamic_key(key)}
    return {key: environment.get(key) for key in sorted(set(OPAQUE) | set(BASE) | dynamic)}

def capture_tools(commands, run):
    """Record genuine executable versions, command exits and binary digests."""
    observed = {}
    c.require(
        set(commands) == {'cargo', 'rustc', 'nextest', 'coverage'},
        'missing actual tool version command',
    )
    for (name, command) in commands.items():
        c.argv(command)
        path = Path(command[0])
        c.require(path.is_absolute(), 'resolve genuine tool before installing adapter')
        if name == 'coverage':
            allowed = [['llvm-cov', '--version']]
        elif name == 'rustc':
            allowed = [['--version', '--verbose']]
        elif name == 'cargo':
            allowed = [['--version'], ['--version', '--verbose']]
        else:
            allowed = [['--version']]
        c.require(command[1:] in allowed, 'unaudited tool version query')
        result = run(command, capture_output=True, text=True)
        observed[name] = dict(
            path=str(path),
            sha256=c.digest(path),
            version=result.stdout.strip(),
            version_command=command,
            exit_code=result.returncode,
        )
    return observed

def tool_context(observed, expected):
    """Bind actual tool observations to their independently expected context.

    Public Rust/Cargo1.98.0 and1.98.1 interfaces are audited. The private
    coverage wrapper remains limited to cargo-llvm-cov0.9.1 and nextest0.9.145.
    """
    c.require(
        set(observed) == set(expected) == {'cargo', 'rustc', 'nextest', 'coverage'},
        'incomplete toolchain context',
    )
    for name in expected:
        c.fields(observed[name], ('path', 'sha256', 'version', 'version_command', 'exit_code'))
        c.require(observed[name] == expected[name], 'wrong actual ' + name + ' toolchain context')
        tool = observed[name]
        c.require(
            type(tool['exit_code']) is int and tool['exit_code'] == 0,
            'tool version command failed',
        )
        c.require(Path(tool['path']).is_absolute(), 'tool executable must be resolved')
        c.require(re.fullmatch('[0-9a-f]{64}', tool['sha256']) is not None, 'invalid tool digest')
        c.argv(tool['version_command'])
        c.string(tool['version'], 'actual version')
        c.require(tool['version_command'][0] == tool['path'], 'version command used another executable')
        if name == 'coverage':
            c.require(
                tool['version_command'][1:] == ['llvm-cov', '--version'],
                'unaudited coverage version query',
            )
    c.require(
        re.match('cargo-llvm-cov 0\\.9\\.1(?:\\s|$)', observed['coverage']['version']) is not None,
        'unaudited cargo-llvm-cov version',
    )
    c.require(
        re.match('cargo-nextest 0\\.9\\.145(?:\\s|$)', observed['nextest']['version']) is not None,
        'unaudited nextest version',
    )
    rustc = observed['rustc']
    c.require(
        rustc['version_command'][1:] == ['--version', '--verbose'],
        'rustc verbose identity missing',
    )
    c.require(
        re.match('rustc 1\\.98\\.[01](?:\\s|$)', rustc['version']) is not None,
        'unaudited coverage Rust version',
    )
    for key in ('commit-hash', 'host', 'release', 'LLVM version'):
        c.require(
            re.search('^' + re.escape(key) + ': .+$', rustc['version'], re.M) is not None,
            'incomplete actual rustc identity',
        )
    c.require(
        re.match('cargo 1\\.98\\.[01](?:\\s|$)', observed['cargo']['version']) is not None,
        'unaudited coverage Cargo version',
    )
    release = re.match('rustc (1\\.98\\.[01])(?:\\s|$)', rustc['version'])[1]
    c.require(
        re.search('^release: ' + re.escape(release) + '$', rustc['version'], re.M) is not None,
        'Rust verbose release differs from actual version',
    )
    c.require(
        re.search('^commit-hash: [0-9a-f]{40}$', rustc['version'], re.M) is not None,
        'invalid actual Rust compiler commit',
    )
    if observed['cargo']['version_command'][1:] == ['--version', '--verbose']:
        for key in ('commit-hash', 'host', 'release'):
            c.require(
                re.search('^' + key + ': .+$', observed['cargo']['version'], re.M) is not None,
                'incomplete actual Cargo identity',
            )
        release = re.match('cargo (1\\.98\\.[01])(?:\\s|$)', observed['cargo']['version'])[1]
        c.require(
            re.search('^release: ' + re.escape(release) + '$', observed['cargo']['version'], re.M) is not None,
            'Cargo verbose release differs from actual version',
        )
        c.require(
            re.search('^commit-hash: [0-9a-f]{40}$', observed['cargo']['version'], re.M) is not None,
            'invalid actual Cargo commit',
        )
    return observed

def instrumented_context(observed, expected, tools, checkout, target_dir):
    """Validate approved inherited compiler context for both child operations."""
    c.require(observed == expected, 'discovery/run instrumentation differs from trusted context')
    c.require(set(OPAQUE) | set(BASE) <= set(observed), 'missing explicit instrumentation context')
    for (key, value) in observed.items():
        c.require(key in OPAQUE or key in BASE or dynamic_key(key), 'unreviewed instrumentation context')
        c.require(value is None or isinstance(value, str), 'invalid environment value')
    c.require(
        observed['__CARGO_LLVM_COV_RUSTC_WRAPPER'] == '1' and observed['CARGO_LLVM_COV'] == '1',
        'coverage wrapper is not active',
    )
    c.require(
        all((observed[name] is None for name in ('LLVM_COV', 'LLVM_PROFDATA', 'LLVM_COV_FLAGS', 'LLVM_PROFDATA_FLAGS', 'CARGO_LLVM_COV_FLAGS', 'CARGO_LLVM_PROFDATA_FLAGS', 'RUSTC_BOOTSTRAP'))),
        'default coverage contract refuses unaudited report-tool/bootstrap overrides',
    )
    c.require(
        observed['CARGO_LLVM_COV_SHOW_ENV'] is None,
        'show-env context cannot stand in for default coverage owner',
    )
    c.require(observed['RUSTC_WRAPPER'] == tools['coverage']['path'], 'wrong coverage compiler wrapper')
    flags = observed['__CARGO_LLVM_COV_RUSTC_WRAPPER_RUSTFLAGS']
    c.string(flags, 'opaque wrapper flags')
    c.require('instrument-coverage' in flags.split('\x1f'), 'missing instrumented compiler context')
    crates = observed['__CARGO_LLVM_COV_RUSTC_WRAPPER_CRATE_NAMES']
    c.string(crates, 'opaque workspace crate context')
    c.require('reliaburger' in crates.split(','), 'main crate is not instrumented')
    profile = observed['LLVM_PROFILE_FILE']
    c.string(profile, 'coverage profile output')
    c.require(
        Path(profile).is_absolute() and Path(profile).parent == Path(target_dir) and profile.endswith('.profraw'),
        'wrong instrumented profile directory',
    )
    c.require(
        observed['RELIABURGER_GIT_SHA'] in (None, checkout),
        'build identity differs from actual checkout',
    )
    return observed

def list_argv(run_argv, prefix):
    """Preserve actual selection spelling and remove only audited execution options."""
    c.argv(run_argv)
    c.argv(prefix)
    c.require(run_argv[:len(prefix)] == prefix, 'wrong actual nextest dispatch prefix')
    c.require(run_argv[len(prefix):len(prefix) + 1] == ['run'], 'adapter must intercept an actual run')
    selectors = []
    tail = run_argv[len(prefix) + 1:]
    i = 0
    values = {
        '--profile',
        '--features',
        '--manifest-path',
        '--target-dir',
        '--target',
        '--config-file',
        '--user-config-file',
        '--archive-file',
        '--workspace-remap',
        '--target-dir-remap',
        '--run-ignored',
        '-E',
        '--filter-expr',
    }
    flags = {
        '--locked',
        '--frozen',
        '--offline',
        '--all-features',
        '--no-default-features',
        '--ignore-default-filter',
        '--workspace',
        '--all-targets',
        '--release',
    }
    runtime_values = {
        '--no-tests',
        '--test-threads',
        '--retries',
        '--failure-output',
        '--success-output',
        '--status-level',
        '--final-status-level',
    }
    runtime_flags = {'--no-fail-fast', '--fail-fast'}
    while i < len(tail):
        token = tail[i]
        (key, equals, value) = token.partition('=')
        if key in values | runtime_values:
            if not equals:
                c.require(i + 1 < len(tail), 'missing nextest option value')
                value = tail[i + 1]
                segment = tail[i:i + 2]
                i += 2
            else:
                segment = [token]
                i += 1
            c.string(value, 'nextest option value')
            if key == '--retries':
                c.require(value == '0', 'coverage retries are forbidden')
            if key == '--no-tests':
                c.require(value == 'fail', 'empty coverage selection cannot pass')
            if key in values:
                selectors += segment
        elif token in flags:
            selectors.append(token)
            i += 1
        elif token in runtime_flags:
            i += 1
        else:
            raise c.Invalid('unreviewed actual nextest argument ' + token)
    return prefix + ['list'] + selectors + ['--message-format=json']

def workflow_context(row):
    c.fields(row, ('commit', 'run_id', 'attempt', 'host'))
    c.require(re.fullmatch('[0-9a-f]{40}', row['commit']) is not None, 'invalid actual checkout')
    c.require(row['host'] == 'linux', 'coverage owner belongs to Linux gate')
    for key in ('run_id', 'attempt'):
        c.require(
            isinstance(row[key], str) and row[key].isdigit() and (int(row[key]) > 0),
            'invalid current workflow identity',
        )

def owner_completion(receipt, expected, child_path):
    """Require one successful instrumented child and all original coverage stages.

    Validate the exact child bytes and parent-bound runtime when present.
    Discovery/JUnit bytes and required identities are checked by evidence().
    """
    child = c.read_json(child_path)
    c.fields(
        expected,
        ('context', 'owner_command', 'nonce', 'tools', 'instrumentation', 'target_dir', 'nextest_prefix', 'run_argv', 'stages'),
        ('runtime',),
    )
    workflow_context(expected['context'])
    c.string(expected['nonce'], 'current interception nonce')
    c.fields(
        receipt,
        ('schema_version', 'context', 'owner_command', 'nonce', 'exit_code', 'interception_count', 'child_sha256', 'stages'),
        ('runtime_sha256',),
    )
    c.require(
        type(receipt['schema_version']) is int and receipt['schema_version'] == 1,
        'wrong owner schema',
    )
    for key in ('context', 'owner_command', 'nonce'):
        c.require(receipt[key] == expected[key], 'wrong/stale coverage owner ' + key)
    c.require(type(receipt['exit_code']) is int and receipt['exit_code'] == 0, 'coverage owner failed')
    c.require(
        type(receipt['interception_count']) is int and receipt['interception_count'] == 1,
        'actual instrumented run interception missing or duplicated',
    )
    c.fields(
        child,
        ('schema_version', 'context', 'nonce', 'tools', 'run_environment', 'list_environment', 'target_dir', 'command', 'discovery_command', 'exit_code', 'discovery_sha256', 'junit_sha256'),
    )
    for key in ('context', 'nonce'):
        c.require(child[key] == expected[key], 'wrong/stale coverage child ' + key)
    c.require(
        type(child['schema_version']) is int and child['schema_version'] == 1,
        'wrong child schema',
    )
    c.require(type(child['exit_code']) is int and child['exit_code'] == 0, 'instrumented nextest failed')
    tool_context(child['tools'], expected['tools'])
    c.require(
        child['target_dir'] == expected['target_dir'],
        'discovery/run used another target directory',
    )
    for key in ('run_environment', 'list_environment'):
        instrumented_context(
            child[key],
            expected['instrumentation'],
            child['tools'],
            expected['context']['commit'],
            expected['target_dir'],
        )
    c.require(child['command'] == expected['run_argv'], 'wrong actual instrumented execution')
    c.require(
        child['discovery_command'] == list_argv(child['command'], expected['nextest_prefix']),
        'discovery does not preserve actual instrumented selectors',
    )
    c.require(receipt['child_sha256'] == c.digest(child_path), 'coverage child receipt changed')
    c.require(
        isinstance(receipt['stages'], list) and len(receipt['stages']) == len(expected['stages']),
        'missing owner stages',
    )
    for (actual, command) in zip(receipt['stages'], expected['stages']):
        c.fields(actual, ('command', 'exit_code'))
        c.require(actual['command'] == command, 'wrong owner stage')
        c.require(
            type(actual['exit_code']) is int and actual['exit_code'] == 0,
            'coverage clean/report/floor stage failed',
        )
    c.require(
        sum((command[:4] in (['cargo', 'llvm-cov', '--no-report', 'nextest'], [expected['tools']['coverage']['path'], 'llvm-cov', '--no-report', 'nextest']) for command in expected['stages'])) == 1,
        'coverage must have exactly one instrumented portable execution',
    )
    c.require(
        any(('--fail-under-lines=78.65' in command or ('--fail-under-lines' in command and command[command.index('--fail-under-lines') + 1:command.index('--fail-under-lines') + 2] == ['78.65']) for command in expected['stages'])),
        'existing line floor missing or changed',
    )
    if 'runtime' in expected:
        runtime_path = Path(child_path).parent / 'owner-runtime.json'
        c.require(
            receipt.get('runtime_sha256') == c.digest(runtime_path),
            'actual owner runtime changed',
        )
        runtime = c.read_json(runtime_path)
        c.fields(runtime, tuple(expected['runtime']) + ('exit_code', 'control_errors'))
        for (key, value) in expected['runtime'].items():
            c.require(runtime[key] == value, 'wrong owner runtime ' + key)
        c.require(
            runtime['exit_code'] == 0 and runtime['control_errors'] == [],
            'actual owner controller/stage failure',
        )
    return True

def evidence(expected, directory, required):
    """Validate current completion, report bytes and exact required case identities."""
    directory = Path(directory)
    owner = c.read_json(directory / 'owner.json')
    child_path = directory / 'child.json'
    owner_completion(owner, expected, child_path)
    child = c.read_json(child_path)
    for (name, key) in [('discovery.json', 'discovery_sha256'), ('junit.xml', 'junit_sha256')]:
        c.require(c.digest(directory / name) == child[key], 'coverage report hash mismatch')
    discovered = c.selected(c.read_json(directory / 'discovery.json'))
    passed = c.passed_junit(directory / 'junit.xml')
    c.require(passed <= discovered, 'coverage executed case absent from discovery')
    c.require(
        bool(required) and set(required) <= discovered and (set(required) <= passed),
        'coverage required cases did not execute successfully',
    )
    return set(required)
