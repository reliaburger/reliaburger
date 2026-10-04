"""Finite reviewed migration of existing Linux-owner cases to their OCI driver.

Call only from full workflow aggregation with current workflow inputs and waited
job outputs. This preserves ignored-test reasons and does not substitute a gate
for any case absent from the committed mapping. Synthetic tests are not runtime
qualification.
"""
from pathlib import Path

import completion
import contracts as c
import gates
import ignored_owners
import oci_driver


DECLARED_OWNER = ('make', 'test-linux')
GATE = 'oci-interruptions'
GROUPS = {
    'reliaburger::owned_runc': ('owned_runc', 'runc'),
    'reliaburger::owned_network': ('owned_network', 'linux-network-namespaces'),
}
ROW_FIELDS = (
    'source', 'function', 'binary', 'test', 'declared_owner', 'gate', 'group',
    'runtime', 'source_sha256', 'case_id',
)


def reviewed_rows(root, rows, bindings, manifest):
    """Bind every finite alias to current source, original owner and case ID."""
    root = Path(root)
    c.require(isinstance(rows, list) and bool(rows), 'empty OCI legacy mapping')
    current = {
        (case.path, case.name): case
        for case in ignored_owners.find_ignored(root)
        if case.binary() in GROUPS and DECLARED_OWNER in ignored_owners.owners(case)
    }
    bound = {}
    for row in bindings:
        key = row['source'], row['function']
        c.require(key not in bound, 'duplicate current legacy binding')
        bound[key] = row
    cases = {
        case['id']: case
        for family in manifest['contracts']
        for case in family['cases']
    }
    seen = set()
    for row in rows:
        c.fields(row, ROW_FIELDS)
        key = row['source'], row['function']
        c.require(key not in seen and key in current, 'duplicate or unreviewed OCI legacy alias')
        seen.add(key)
        original = current[key]
        identity = row['binary'], row['test']
        c.require(row['declared_owner'] == list(DECLARED_OWNER), 'changed declared legacy owner')
        c.require(row['binary'] == original.binary(), 'changed legacy binary identity')
        c.require(row['gate'] == GATE, 'unreviewed substitute gate')
        group, runtime = GROUPS[row['binary']]
        c.require((row['group'], row['runtime']) == (group, runtime), 'wrong OCI group or runtime')
        binding = bound.get(key)
        c.require(binding is not None and (binding['binary'], binding['test']) == identity,
                  'changed exact legacy full name or source binding')
        c.require(c.digest(gates.inside(root, row['source'])) == row['source_sha256'],
                  'reviewed OCI legacy source changed')
        case = cases.get(row['case_id'])
        c.require(case is not None and (case['binary'], case['test']) == identity
                  and GATE in case['requires'] and row['source'] in case['sources'],
                  'legacy OCI case lacks its exact committed manifest identity')
    c.require(seen == set(current), 'incomplete finite OCI legacy mapping')
    return rows


def qualify(root, rows, bindings, manifest, manifest_path, context, job, directory, verified_cases):
    """Return only exact aliases completed by the current successful OCI owner.

    context and job come from trusted workflow inputs/needs, not artifact JSON.
    verified_cases is internal strict aggregate output; full driver and per-case
    completion are also checked here before the declared owner's set expands.
    """
    c.fields(context, ('commit', 'run_id', 'attempt', 'host'))
    c.require(context['host'] == 'linux', 'legacy OCI owner requires Linux')
    c.require(job.get('result') == 'success', 'OCI owner job did not succeed')
    directory = Path(directory)
    digest = job.get('outputs', {}).get('oci_interruptions_sha256')
    c.require(isinstance(digest, str) and len(digest) == 64
              and c.digest(directory / 'gate-seal.json') == digest,
              'missing or changed trusted OCI step output')
    seal = c.read_json(directory / 'gate-seal.json')
    c.fields(seal, ('schema_version', 'gate', 'context', 'manifest_sha256', 'files'))
    c.require(type(seal['schema_version']) is int and seal['schema_version'] == 1 and seal['gate'] == GATE
              and seal['context'] == context and seal['manifest_sha256'] == c.digest(manifest_path)
              and c.read_json(manifest_path) == manifest, 'stale or wrong OCI sealed context/manifest')
    c.require(isinstance(seal['files'], dict) and 'approved.json' in seal['files'],
              'missing approved OCI owner plan')
    for name, sha in seal['files'].items():
        c.require(c.digest(gates.inside(directory, name)) == sha, 'OCI owner payload changed')
    rows = reviewed_rows(root, rows, bindings, manifest)
    approved = c.read_json(directory / 'approved.json')
    c.require(approved['context'] == context and approved['origin_path'] ==
              'target/contracts/linux/oci-interruptions/build/build-origin.json',
              'wrong OCI approved context or origin location')
    expected_context = dict(context, authority='ci', owner=None)
    oci_driver.driver_evidence(directory, expected_context, approved['tools'], approved['origin_path'])
    wanted = {(row['binary'], row['test']) for row in rows}
    c.require(wanted <= verified_cases, 'legacy aliases absent from successful aggregate cases')
    found = set()
    for binary, (group, _) in GROUPS.items():
        names = {name for candidate, name in wanted if candidate == binary}
        if not names:
            continue
        plan = approved['plans'][binary]
        c.require(plan['context'] == expected_context and plan['binary'] == binary
                  and plan['source_files'] == ['tests/' + group + '.rs'],
                  'OCI legacy child lost its exact source or group context')
        found |= completion.evidence(plan, directory / group, names)
    c.require(found == wanted, 'OCI legacy aliases did not all complete successfully')
    return found
