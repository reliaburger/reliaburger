"""Prepare one verified nextest archive for a job's independent Make owners.

Only preparation uses --archive-file. Discovery and execution reuse its exact
metadata and immutable runtime files; each owner seals the preparation receipt.
"""
from __future__ import annotations

import json
from pathlib import Path
import re
import subprocess

import contracts as c
import gates


RECEIPT = 'archive-extraction.json'
METADATA = ('target/nextest/cargo-metadata.json', 'target/nextest/binaries-metadata.json')
SELECTORS = [
    '--cargo-metadata', METADATA[0], '--binaries-metadata', METADATA[1],
    '--target-dir-remap', 'target', '--workspace-remap', '.', '--profile', 'ci',
]


def command(nextest):
    return [nextest['path'], 'nextest', 'list', '--archive-file', 'tests.tar.zst',
            '--extract-to', '.', '--extract-overwrite', '--workspace-remap', '.',
            '--list-type', 'binaries-only', '--message-format=json']


def payload_name(name):
    """The archive's target tree, excluding reports and later owner evidence."""
    path = Path(name)
    return (isinstance(name, str) and path.as_posix() == name and not path.is_absolute()
            and '..' not in path.parts and len(path.parts) > 1 and path.parts[0] == 'target'
            and not path.is_relative_to('target/contracts')
            and (not path.is_relative_to('target/nextest') or name in METADATA)
            and name != 'target/workflow-needs.json')


def payload_file(root, name):
    c.require(payload_name(name), 'invalid immutable archive payload path')
    path = gates.inside(root, name)
    # Archive reuse never follows an independently replaced symlink, even when
    # its destination happens to contain the old bytes.
    original = root / name
    c.require(not any(parent.is_symlink() for parent in [original, *original.parents]
                      if parent != root and parent.is_relative_to(root)),
              'archive payload must not be a symlink')
    c.require(path.is_file(), 'missing regular archive payload')
    return path


def validate(row, context, origin, nextest):
    """Validate sealed preparation evidence without needing the runner's files."""
    c.fields(row, ('schema_version', 'context', 'archive_origin', 'nextest', 'command', 'exit_code', 'files'))
    c.require(type(row['schema_version']) is int and row['schema_version'] == 1
              and row['context'] == context and row['archive_origin'] == origin
              and row['nextest'] == nextest and row['command'] == command(nextest)
              and type(row['exit_code']) is int and row['exit_code'] == 0,
              'wrong current archive extraction')
    files = row['files']
    c.require(isinstance(files, dict) and set(METADATA) < files.keys(), 'incomplete archive payload inventory')
    for name, digest in files.items():
        c.require(isinstance(name, str) and payload_name(name), 'invalid archive payload inventory path')
        c.require(isinstance(digest, str) and re.fullmatch('[0-9a-f]{64}', digest) is not None,
                  'invalid archive payload digest')


def verify_files(root, row):
    root = Path(root).resolve()
    for name, digest in row['files'].items():
        c.require(c.digest(payload_file(root, name)) == digest, 'extracted archive payload changed: ' + name)


def prepare(root, context, origin, nextest, *, environment=None, run=subprocess.run):
    """Extract a fresh payload once. Failure never publishes a reusable receipt."""
    root = Path(root).resolve()
    receipt = root / RECEIPT
    c.require(not receipt.exists(), 'archive extraction receipt already exists')
    gates.archive_origin(context, {'archive_origin': origin}, root)
    c.require(c.digest(root / 'tests.tar.zst') == origin['builder']['archive_sha256'],
              'archive differs from successful builder')
    target = root / 'target'
    c.require(not target.is_symlink(), 'archive target must not be a symlink')
    # The workflow only writes this trusted needs input before extraction.
    # Refuse other existing files rather than accidentally inventorying a stale
    # executable that was absent from the new archive. Nextest still needs
    # --extract-overwrite because this one input makes target/ exist already.
    before = {p.relative_to(root).as_posix(): c.digest(p) for p in target.rglob('*') if p.is_file()}
    c.require(set(before) <= {'target/workflow-needs.json'}, 'archive extraction needs a fresh target tree')
    c.require(not any(p.is_symlink() for p in target.rglob('*')), 'archive target contains a symlink')
    invocation = command(nextest)
    with (root / 'archive-extraction-binaries.json').open('wb') as output, \
            (root / 'archive-extraction.log').open('wb') as errors:
        result = run(invocation, cwd=root, env=environment, stdout=output, stderr=errors)
    c.require(result.returncode == 0, 'archive extraction failed')
    for name, digest in before.items():
        c.require(c.digest(root / name) == digest, 'extraction changed an existing workflow input')
    files = {}
    for path in sorted(target.rglob('*')):
        name = path.relative_to(root).as_posix()
        c.require(not path.is_symlink(), 'extracted archive contains a symlink')
        if path.is_dir() or name in before:
            continue
        files[name] = c.digest(payload_file(root, name))
    row = dict(schema_version=1, context=context, archive_origin=origin, nextest=nextest,
               command=invocation, exit_code=0, files=files)
    validate(row, context, origin, nextest)
    # No later owner needs to trust directory contents to choose its inputs.
    # Its workflow step supplies the hash of this exact receipt.
    with receipt.open('x') as output:
        output.write(json.dumps(row, sort_keys=True, indent=2) + '\n')
    return row


def load(root, digest, context, origin, nextest):
    root = Path(root).resolve()
    c.require(isinstance(digest, str) and re.fullmatch('[0-9a-f]{64}', digest) is not None,
              'trusted archive extraction digest required')
    c.require(c.digest(root / RECEIPT) == digest, 'archive extraction receipt differs from trusted step output')
    row = c.read_json(root / RECEIPT)
    validate(row, context, origin, nextest)
    verify_files(root, row)
    return row
