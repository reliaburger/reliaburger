#!/usr/bin/env python3
"""Keep the evidence manifest in step with what the macOS CI job runs (#1720).

Bun ships for Linux only, so the `portable macOS` job runs `make test-macos`:
relish's tests and the tests only macOS compiles, chosen by one nextest
filterset in the Makefile. A finite contract case may require the
`portable-darwin` gate only when that filterset selects it, or the evidence
gate would wait for a case the job never runs.

    macos_selection.py apply   rewrite tests/contracts/manifest.json and
                               docs/testing/qualification-matrix.md
    macos_selection.py check   fail if either differs from what apply writes

`apply` is deterministic: a case that requires `portable-linux` keeps (or
gets) `portable-darwin` exactly when the filterset selects its binary and
test, and every other gate stays as it is.

Fixtures: scripts/ci/test_macos_selection.py.
"""
from __future__ import annotations

import argparse
import copy
import json
from pathlib import Path
import re
import shlex
import sys

sys.dont_write_bytecode = True

import workflow_assembly

ROOT = Path(__file__).resolve().parents[2]
# The Make target the portable-darwin owner runs.
TARGET = 'test-macos'
MANIFEST = 'tests/contracts/manifest.json'
MATRIX = 'docs/testing/qualification-matrix.md'
TOKEN = re.compile(r'\s*(?:(?P<binary>binary_id\((?P<id>[A-Za-z0-9_:/-]+)\))'
                   r'|(?P<test>test\(/(?P<regex>[^/]+)/\))|(?P<op>[|&()]))')
# A matrix row: | contract | `case id` | `binary` / `test` | gates | boundary |
ROW = re.compile(r'^(\| [^|]+ \| `(?P<id>[^`]+)` \| `(?P<binary>[^`]+)` / `(?P<test>[^`]+)` \| )'
                 r'(?P<gates>[^|]+?)( \| .*)$')


def selection(makefile):
    """The test targets and the filterset `make test-macos` hands nextest."""
    lines = [line for line in workflow_assembly.recipe(makefile, TARGET) if '$(NEXTEST)' in line]
    if len(lines) != 1:
        raise ValueError(f'{TARGET} must run nextest exactly once')
    words = shlex.split(lines[0].split('$(NEXTEST)', 1)[1])
    if words.count('-E') != 1 or words.index('-E') != len(words) - 2:
        raise ValueError(f'{TARGET} must end with one -E filterset')
    return words[:-2], words[-1]


def compile_filter(text):
    """A predicate (binary_id, test name) -> bool for a nextest filterset.

    Only the operators the macOS job uses are understood: `binary_id(ID)`,
    `test(/REGEX/)`, `&`, `|` and parentheses. Anything else is refused, so a
    filter that outgrows this evaluator fails its tests instead of being
    misread.
    """
    tokens, position = [], 0
    while position < len(text.rstrip()):
        match = TOKEN.match(text, position)
        if match is None:
            raise ValueError(f'unsupported filterset syntax at {position}: {text[position:]!r}')
        tokens.append(match)
        position = match.end()
    if not tokens:
        raise ValueError('empty filterset')
    index = 0

    def peek():
        return tokens[index].group('op') if index < len(tokens) else None

    def factor():
        nonlocal index
        if index >= len(tokens):
            raise ValueError('filterset ends early')
        token = tokens[index]
        index += 1
        if token.group('binary'):
            wanted = token.group('id')
            return lambda binary, test: binary == wanted
        if token.group('test'):
            pattern = re.compile(token.group('regex'))
            return lambda binary, test: pattern.search(test) is not None
        if token.group('op') == '(':
            inner = union()
            if peek() != ')':
                raise ValueError('unclosed parenthesis in filterset')
            index += 1
            return inner
        raise ValueError(f'unexpected {token.group(0).strip()!r} in filterset')

    def intersection():
        nonlocal index
        parts = [factor()]
        while peek() == '&':
            index += 1
            parts.append(factor())
        return lambda binary, test: all(part(binary, test) for part in parts)

    def union():
        nonlocal index
        parts = [intersection()]
        while peek() == '|':
            index += 1
            parts.append(intersection())
        return lambda binary, test: any(part(binary, test) for part in parts)

    predicate = union()
    if index != len(tokens):
        raise ValueError(f'unexpected {tokens[index].group(0).strip()!r} in filterset')
    return predicate


def updated_manifest(manifest, makefile):
    """The manifest with each portable case's macOS requirement set by the filterset."""
    selected = compile_filter(selection(makefile)[1])
    result = copy.deepcopy(manifest)
    for contract in result['contracts']:
        for case in contract['cases']:
            requires = [gate for gate in case['requires'] if gate != 'portable-darwin']
            if 'portable-linux' in requires and selected(case['binary'], case['test']):
                requires.insert(requires.index('portable-linux') + 1, 'portable-darwin')
            case['requires'] = requires
    return result


def matrix_rows(text):
    """{case id: (binary, test, [gates])} for every case row of the qualification matrix."""
    rows = {}
    for line in text.splitlines():
        match = ROW.match(line)
        if match:
            rows[match.group('id')] = (match.group('binary'), match.group('test'),
                                       [gate.strip() for gate in match.group('gates').split(',')])
    return rows


def updated_matrix(text, manifest, selected):
    """The matrix with each case's gate column in step with the macOS job.

    A case the manifest lists gets the manifest's gates. A row for a case the
    manifest no longer lists keeps its gates, less `portable-darwin` when the
    filterset doesn't select it, so no row claims a macOS run that can't happen.
    """
    requires = {case['id']: case['requires'] for contract in manifest['contracts']
                for case in contract['cases']}
    lines = []
    for line in text.splitlines(keepends=True):
        body = line.rstrip('\n')
        match = ROW.match(body)
        if match:
            gates = requires.get(match.group('id'))
            if gates is None:
                gates = [gate.strip() for gate in match.group('gates').split(',')]
                if not selected(match.group('binary'), match.group('test')):
                    gates = [gate for gate in gates if gate != 'portable-darwin']
            body = match.group(1) + ', '.join(gates) + match.group(6)
            line = body + line[len(line.rstrip('\n')):]
        lines.append(line)
    return ''.join(lines)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument('operation', choices=['apply', 'check'])
    parser.add_argument('--root', default=str(ROOT))
    args = parser.parse_args(argv)
    root = Path(args.root)
    makefile = (root / 'Makefile').read_text()
    manifest_text = (root / MANIFEST).read_text()
    matrix_text = (root / MATRIX).read_text()
    manifest = updated_manifest(json.loads(manifest_text), makefile)
    new_manifest = json.dumps(manifest, indent=2, ensure_ascii=False) + '\n'
    new_matrix = updated_matrix(matrix_text, manifest, compile_filter(selection(makefile)[1]))
    if args.operation == 'check':
        stale = [name for name, old, new in [(MANIFEST, manifest_text, new_manifest),
                                             (MATRIX, matrix_text, new_matrix)] if old != new]
        for name in stale:
            print(f'{name} differs from the macOS selection; run scripts/ci/macos_selection.py apply',
                  file=sys.stderr)
        return 1 if stale else 0
    (root / MANIFEST).write_text(new_manifest)
    (root / MATRIX).write_text(new_matrix)
    before = sum('portable-darwin' in case['requires'] for contract in json.loads(manifest_text)['contracts']
                 for case in contract['cases'])
    after = sum('portable-darwin' in case['requires'] for contract in manifest['contracts']
                for case in contract['cases'])
    print(f'portable-darwin cases: {before} -> {after}')
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
