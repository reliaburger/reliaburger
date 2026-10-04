"""Synthetic actual-dispatch child wrapper tests; not genuine Cargo proof."""
import copy,json,os,tempfile,unittest
from pathlib import Path
from types import SimpleNamespace
import contracts as c
import coverage_adapter as a
import coverage_contract as k
import test_coverage_contract as base

class Adapter(unittest.TestCase):
    def setUp(self):
        self.fixture=base.Coverage();self.fixture.setUp();self.root=self.fixture.path.parent
        tool=self.root/'genuine-nextest';tool.write_bytes(b'synthetic child executable')
        self.expected=copy.deepcopy(self.fixture.expected);self.expected['tools']['nextest']['sha256']=c.digest(tool);self.expected['tools']['nextest']['path']=str(tool)
        self.expected['run_argv']=[str(tool)]+self.expected['run_argv'][1:];self.expected['nextest_prefix']=[str(tool),'nextest']
        self.config=dict(root=str(self.root),genuine_nextest=str(tool),adapter_path=str(self.root/'overlay/cargo-nextest'),expected=self.expected,directory=str(self.root/'output'),junit_source='target/nextest/ci/junit.xml',source_files=['src/meat/cron.rs'])
        self.env={key:value for key,value in self.expected['instrumentation'].items() if value is not None}
        self.calls=[];self.code=0;self.report=True
    def tearDown(self):self.fixture.tearDown()
    def fake_run(self,cmd,**kwargs):
        self.calls.append((cmd,kwargs))
        if cmd[0]=='git':
            output=self.expected['context']['commit']+'\n' if cmd[1]=='rev-parse' else ('src/meat/cron.rs\n' if '--error-unmatch' in cmd else '')
            return SimpleNamespace(returncode=0,stdout=output)
        if cmd[2:3]==['list']:
            discovery={'rust-build-meta':{},'test-count':1,'rust-suites':{'reliaburger':{'binary-id':'reliaburger','status':'listed','testcases':{'cron::boundary':{'filter-match':{'status':'matches'}}}}}}
            kwargs['stdout'].write(json.dumps(discovery).encode())
        elif cmd[2:3]==['run'] and self.report:
            p=self.root/self.config['junit_source'];p.parent.mkdir(parents=True,exist_ok=True);p.write_text('<testsuites><testsuite name="reliaburger"><testcase classname="reliaburger" name="cron::boundary"/></testsuite></testsuites>')
        return SimpleNamespace(returncode=self.code if cmd[2:3]==['run'] else 0)
    def invoke(self,args=None):return a.invoke(self.expected['run_argv'][1:] if args is None else args,self.config,self.env,self.fake_run)
    def test_actual_child_argv_and_identical_environment_are_preserved(self):
        self.assertEqual(self.invoke(),0);self.assertEqual(len([cmd for cmd,_ in self.calls if cmd[0]!='git']),2)
        child=c.read_json(self.root/'output/child.json');self.assertEqual(child['command'],self.expected['run_argv']);self.assertEqual(child['discovery_command'],k.list_argv(self.expected['run_argv'],self.expected['nextest_prefix']))
        children=[kw for cmd,kw in self.calls if cmd[0]!='git'];self.assertEqual(children[0]['env'],children[1]['env']);self.assertTrue((self.root/'output/interception.claim').is_file())
    def test_help_queries_do_not_claim_test_interception(self):
        self.assertEqual(self.invoke(['nextest','--version']),0);self.assertEqual(len(self.calls),1);self.assertFalse((self.root/'output').exists())
    def test_second_actual_run_refused_before_spawn(self):
        self.invoke();self.calls.clear()
        with self.assertRaisesRegex(c.Invalid,'duplicate actual'):self.invoke()
        self.assertFalse(self.calls)
    def test_failed_actual_child_retains_its_exit(self):
        self.code=19;self.assertEqual(self.invoke(),19);self.assertEqual(c.read_json(self.root/'output/child.json')['exit_code'],19)
    def test_stale_junit_removed_before_run(self):
        p=self.root/self.config['junit_source'];p.parent.mkdir(parents=True);p.write_text('old report');self.report=False
        self.assertEqual(self.invoke(),1);self.assertFalse((self.root/'output/junit.xml').exists())
    def test_overlay_recursion_or_tool_change_refused(self):
        self.config['adapter_path']=self.config['genuine_nextest']
        with self.assertRaisesRegex(c.Invalid,'recursion'):self.invoke()
    def test_inherited_instrumentation_cannot_differ_from_expected(self):
        self.env['RUSTFLAGS']='-C opt-level=3'
        with self.assertRaisesRegex(c.Invalid,'instrumentation differs'):self.invoke()
        self.assertFalse(self.calls)
    def test_cargo_home_bin_is_present_after_overlay(self):
        path=a.overlay_path('/usr/bin:/bin','/tools/cargo-home','/current/overlay')
        self.assertEqual(path.split(os.pathsep),['/current/overlay','/usr/bin','/bin','/tools/cargo-home/bin'])
        self.assertEqual(a.overlay_path(path,'/tools/cargo-home','/current/overlay'),path)

if __name__=='__main__':unittest.main()
