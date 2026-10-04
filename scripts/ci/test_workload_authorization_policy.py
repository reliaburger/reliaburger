"""Source architecture controls; these do not qualify HTTP or runtime behaviour."""
from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[2]


def compact(source):
    """Discard standalone comments and whitespace for this finite call contract."""
    text = '\n'.join(line for line in source.splitlines() if not line.lstrip().startswith('//'))
    return re.sub(r'\s+', '', text)


def shared_rule(source):
    if 'pub fn authorize_workload(' not in source:
        return False
    start = source.index('pub fn authorize_workload(')
    end = source.index('\n/// Enforce a principal', start)
    body = compact(source[start:end])
    expected = compact('''
        authorize_scoped(ctx, app, namespace)?;
        authorize_permission(ctx, crate::config::PermissionAction::Deploy,
                             app, namespace, permissions,)?;
        if host_execution {
            authorize_permission(ctx, crate::config::PermissionAction::HostExec,
                                 app, namespace, permissions,)?;
        }
        Ok(())
    ''')
    return body.partition('{')[2].rsplit('}', 1)[0] == expected


def caller_rule(source, kind):
    if kind == 'apply':
        start = source.index('    for (app_name, namespace, host_execution) in targets {')
        end = source.index('    let identities:', start)
        expected = '''crate::sesame::auth::authorize_workload(
            auth.as_deref(), app_name, namespace, host_execution, &permissions,
        )'''
        before = source[source.index('    let targets ='):start]
        if before.count('spec.script.is_some() || spec.exec.is_some()') != 2:
            return False
    else:
        start = source.index('    let permissions = super::api::permission_map(&state).await;')
        end = source.index('    // Preserve the caller on the second hop', start)
        expected = '''crate::sesame::auth::authorize_workload(
            auth.as_deref(), &job.name, job.namespace(),
            job.spec.exec.is_some() || job.spec.script.is_some(), &permissions,
        )'''
    body = compact(source[start:end])
    if kind == 'apply':
        expected_body = 'for (app_name, namespace, host_execution) in targets {'
    else:
        expected_body = 'let permissions = super::api::permission_map(&state).await; for job in &jobs {'
    expected_body += 'if let Err(response) = ' + expected + ' { return response; } }'
    return body == compact(expected_body)



class WorkloadAuthorizationPolicy(unittest.TestCase):
    def test_apply_delegates_each_resolved_target_without_local_policy_duplicate(self):
        source = (ROOT / 'src/bun/api/apply.rs').read_text()
        self.assertTrue(caller_rule(source, 'apply'))

    def test_batch_delegates_each_resolved_target_before_forwarding(self):
        source = (ROOT / 'src/bun/batch.rs').read_text()
        self.assertTrue(caller_rule(source, 'batch'))

    def test_shared_rule_preserves_scope_deploy_and_conditional_host_exec_order(self):
        source = (ROOT / 'src/sesame/auth.rs').read_text()
        self.assertTrue(shared_rule(source))

    def test_guard_detects_bypass_duplicate_or_missing_host_exec(self):
        auth = (ROOT / 'src/sesame/auth.rs').read_text()
        self.assertTrue(shared_rule(auth))
        self.assertFalse(shared_rule(auth.replace('PermissionAction::HostExec', 'PermissionAction::Deploy')))
        for kind, path in [('apply', 'src/bun/api/apply.rs'), ('batch', 'src/bun/batch.rs')]:
            source = (ROOT / path).read_text()
            self.assertFalse(caller_rule(source.replace('authorize_workload(', 'authorize_scoped('), kind))
            self.assertFalse(caller_rule(source.replace('authorize_workload(', 'authorize_permission('), kind))
            if kind == 'apply':
                anchor = 'for (app_name, namespace, host_execution) in targets {'
                duplicate = 'let _ = crate::sesame::auth::authorize_scoped(auth.as_deref(), app_name, namespace);'
            else:
                anchor = 'for job in &jobs {'
                duplicate = 'let _ = crate::sesame::auth::authorize_scoped(auth.as_deref(), &job.name, job.namespace());'
            self.assertFalse(caller_rule(source.replace(anchor, anchor + duplicate), kind))
            self.assertFalse(caller_rule(source.replace(anchor, anchor + 'if false {').replace(
                'return response;', 'return response; }'), kind))
        batch = (ROOT / 'src/bun/batch.rs').read_text()
        self.assertFalse(caller_rule(batch.replace('job.spec.exec.is_some() || job.spec.script.is_some()', 'false'), 'batch'))


if __name__ == '__main__':
    unittest.main()
