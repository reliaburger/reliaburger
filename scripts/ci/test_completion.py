"""Synthetic manual/OCI envelope tests; no production runtime qualification."""
import copy,json,tempfile,unittest
from pathlib import Path
from types import SimpleNamespace
import contracts as c
import completion as m

class Completion(unittest.TestCase):
    def setUp(self):
        self.tmp=tempfile.TemporaryDirectory();self.root=Path(self.tmp.name);self.out=self.root/'evidence'
        (self.root/'target').mkdir();(self.root/'target/owned_runc').write_bytes(b'synthetic executable')
        self.ctx=dict(commit='a'*40,run_id='100',attempt='1',host='linux',authority='ci',owner=None)
        self.build=['cargo','test','--features','ebpf','--test','owned_runc','--no-run','--message-format=json']
        self.origin=dict(schema_version=1,context=self.ctx,command=self.build,exit_code=0,artifacts={'reliaburger::owned_runc':dict(executable='target/owned_runc',sha256=c.digest(self.root/'target/owned_runc'))})
        p=self.root/'origin.json';p.write_text(json.dumps(self.origin))
        self.plan=dict(schema_version=1,gate='oci-interruptions',context=self.ctx,binary='reliaburger::owned_runc',executable='target/owned_runc',wrapper=['timeout','420s','sudo','unshare','--mount','--net','--propagation','private'],selectors=['--ignored','--skip','normal_rootless_bun','--skip','actual_host_reboot','--skip','actual_bun_kernel_discovery_host_reboot'],run_options=['--nocapture','--test-threads=1','--format=pretty','--color=never'],source_files=['tests/owned_runc.rs'],build_command=self.build,build_origin='origin.json',build_origin_sha256=c.digest(p))
        self.discovery='first::case: test\nsecond::case: test\n\n2 tests, 0 benchmarks\n'
        self.log='running 2 tests\ntest first::case ... ok\ntest second::case ... diagnostic text\nmore diagnostic text\nok\n\ntest result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 3 filtered out; finished in 0.01s\n'
        self.exit_code=0;self.calls=[]
    def tearDown(self):self.tmp.cleanup()
    def fake_run(self,cmd,**kw):
        self.calls.append(cmd)
        if cmd[0]=='git':
            output=self.ctx['commit']+'\n' if cmd[1]=='rev-parse' else ('tests/owned_runc.rs\n' if '--error-unmatch' in cmd else '')
            return SimpleNamespace(returncode=0,stdout=output)
        kw['stdout'].write((self.discovery if '--list' in cmd else self.log).encode())
        return SimpleNamespace(returncode=0 if '--list' in cmd else self.exit_code)
    def produce(self):return m.produce(self.plan,self.root,self.out,self.fake_run)
    def evidence(self,**kw):return m.evidence(self.plan,self.out,{'first::case','second::case'},**kw)
    def test_actual_wrapper_and_all_skip_selectors_bind_list_and_run(self):
        self.assertEqual(self.produce(),0);self.assertEqual(self.evidence(),{('reliaburger::owned_runc','first::case'),('reliaburger::owned_runc','second::case')})
        commands=[x for x in self.calls if x[0]!='git'];self.assertEqual(len(commands),2)
        self.assertEqual(commands[0][:-3],commands[1][:-4])
    def test_test_start_alone_is_not_completion(self):
        self.log='running 2 tests\ntest first::case ...\n';self.assertEqual(self.produce(),1)
        with self.assertRaisesRegex(c.Invalid,'completion'):self.evidence()
    def test_nonzero_actual_exit_refused_even_with_success_footer(self):
        self.exit_code=124;self.assertEqual(self.produce(),124)
        with self.assertRaisesRegex(c.Invalid,'command failed'):self.evidence()
    def test_ignored_case_does_not_qualify(self):
        self.log=self.log.replace('test first::case ... ok','test first::case ... ignored');self.assertEqual(self.produce(),1)
    def test_failure_case_does_not_qualify(self):
        self.log=self.log.replace('test first::case ... ok','test first::case ... FAILED');self.assertEqual(self.produce(),1)
    def test_incomplete_per_case_results_cannot_hide_behind_footer(self):
        self.log=self.log.replace('test first::case ... ok\n','');self.assertEqual(self.produce(),1)
    def test_footer_count_must_match_selected_discovery(self):
        self.log=self.log.replace('2 passed','1 passed');self.assertEqual(self.produce(),1)
    def test_duplicate_or_unassigned_case_refused(self):
        self.log=self.log.replace('second::case','first::case');self.assertEqual(self.produce(),1)
    def test_stale_attempt_refused(self):
        self.produce();self.plan['context']=dict(self.ctx,attempt='2')
        with self.assertRaisesRegex(c.Invalid,'stale completion context'):self.evidence()
    def test_wrong_binary_receipt_refused(self):
        self.produce();p=self.out/'receipt.json';row=c.read_json(p);row['binary']='reliaburger::owned_network';p.write_text(json.dumps(row))
        with self.assertRaisesRegex(c.Invalid,'binary'):self.evidence()
    def test_current_checkout_does_not_certify_arbitrary_executable(self):
        (self.root/'target/owned_runc').write_bytes(b'changed old executable')
        with self.assertRaisesRegex(c.Invalid,'current build origin'):self.produce()
        self.assertFalse((self.out/'receipt.json').exists())
    def test_failed_builder_cannot_become_an_origin(self):
        self.origin['exit_code']=1;p=self.root/'origin.json';p.write_text(json.dumps(self.origin));self.plan['build_origin_sha256']=c.digest(p)
        with self.assertRaisesRegex(c.Invalid,'builder failed'):self.produce()
    def test_manual_owner_and_session_are_explicit_and_cannot_replace_ci(self):
        self.ctx.update(authority='manual',owner='release operator',run_id='manual:explicit-session')
        p=self.root/'origin.json';p.write_text(json.dumps(self.origin));self.plan['build_origin_sha256']=c.digest(p)
        self.assertEqual(self.produce(),0)
        with self.assertRaisesRegex(c.Invalid,'manual receipt'):self.evidence()
        self.assertTrue(self.evidence(allow_manual=True))
    def test_unknown_selector_refuses_instead_of_widening_selection(self):
        self.plan['selectors'].append('--include-ignored')
        with self.assertRaisesRegex(c.Invalid,'unknown libtest selector'):self.produce()
    def test_old_completion_cannot_survive_current_failed_run(self):
        self.produce();self.exit_code=7;self.assertEqual(self.produce(),7)
        self.assertEqual(c.read_json(self.out/'receipt.json')['exit_code'],7)
    def test_builder_captures_actual_artifact_json_and_exit(self):
        ctx=self.ctx
        def build(cmd,**kw):
            if cmd[0]=='git':return self.fake_run(cmd,**kw)
            row=dict(reason='compiler-artifact',target={'name':'owned_runc'},profile={'test':True},executable=str(self.root/'target/owned_runc'))
            kw['stdout'].write((json.dumps(row)+'\n').encode());return SimpleNamespace(returncode=0)
        out=self.root/'build';self.assertEqual(m.produce_build(ctx,self.build,self.root,out,['reliaburger::owned_runc'],build),0)
        row=c.read_json(out/'build-origin.json');self.assertEqual(row['artifacts']['reliaburger::owned_runc']['sha256'],c.digest(self.root/'target/owned_runc'))
    def test_builder_empty_artifact_set_refused(self):
        def build(cmd,**kw):
            if cmd[0]=='git':return self.fake_run(cmd,**kw)
            kw['stdout'].write(b'');return SimpleNamespace(returncode=0)
        self.assertEqual(m.produce_build(self.ctx,self.build,self.root,self.root/'build',['reliaburger::owned_runc'],build),1)

    def test_actual_pretty_discovery_summary_required(self):
        self.discovery=self.discovery.replace('2 tests, 0 benchmarks','3 tests, 0 benchmarks')
        with self.assertRaisesRegex(c.Invalid,'discovery summary'):self.produce()
        self.assertFalse((self.out/'receipt.json').exists())

if __name__=='__main__':unittest.main()
