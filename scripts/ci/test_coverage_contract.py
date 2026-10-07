"""Synthetic0.9.1 opaque-context/owner tests; no real Cargo interception."""
import copy,json,tempfile,unittest
from pathlib import Path
import contracts as c
import coverage_contract as k

class Coverage(unittest.TestCase):
    def setUp(self):
        self.tmp=tempfile.TemporaryDirectory();self.path=Path(self.tmp.name)/'child.json'
        self.tools={name:dict(path='/tools/'+name,sha256='a'*64,version=version,version_command=['/tools/'+name,'--version'],exit_code=0) for name,version in [('cargo','cargo 1.98.0'),('rustc','rustc 1.98.0 (synthetic fixture)\ncommit-hash: '+ 'a'*40 +'\nhost: x86_64-unknown-linux-gnu\nrelease: 1.98.0\nLLVM version: 22'),('nextest','cargo-nextest 0.9.145'),('coverage','cargo-llvm-cov 0.9.1')]}
        self.tools['rustc']['version_command'].append('--verbose')
        self.tools['coverage']['version_command'].insert(1, 'llvm-cov')
        self.env=k.snapshot_environment({'RUSTC_WRAPPER':'/tools/coverage','CARGO_LLVM_COV':'1','__CARGO_LLVM_COV_RUSTC_WRAPPER':'1','__CARGO_LLVM_COV_RUSTC_WRAPPER_RUSTFLAGS':'-C\x1finstrument-coverage\x1f--cfg=coverage','__CARGO_LLVM_COV_RUSTC_WRAPPER_CRATE_NAMES':'reliaburger,bun,relish','LLVM_PROFILE_FILE':'/work/target/llvm-cov-target/reliaburger-%p-%8m.profraw'})
        run=['cargo','nextest','run','--profile','ci','--features=ebpf','--target-dir','/work/target/llvm-cov-target','--run-ignored=default','-E','all() & not binary(oci_crash)','--no-tests=fail','--no-fail-fast','--test-threads=2','--retries=0']
        ctx=dict(commit='a'*40,run_id='100',attempt='1',host='linux')
        stages=[['cargo','llvm-cov','clean','--workspace'],['cargo','llvm-cov','--no-report','nextest','--profile','ci','--no-tests=fail'],['cargo','llvm-cov','report','--failure-mode','all','--lcov'],['cargo','llvm-cov','report','--failure-mode','all','--html'],['cargo','llvm-cov','report','--failure-mode','all','--fail-under-lines','78.65']]
        self.expected=dict(context=ctx,owner_command=['make','coverage'],nonce='current-owner-nonce',tools=copy.deepcopy(self.tools),instrumentation=copy.deepcopy(self.env),target_dir='/work/target/llvm-cov-target',nextest_prefix=['cargo','nextest'],run_argv=run,stages=stages)
        self.child=dict(schema_version=1,context=ctx,nonce='current-owner-nonce',tools=self.tools,run_environment=copy.deepcopy(self.env),list_environment=copy.deepcopy(self.env),target_dir='/work/target/llvm-cov-target',command=run,discovery_command=k.list_argv(run,['cargo','nextest']),exit_code=0,discovery_sha256='d'*64,junit_sha256='e'*64)
        self.owner=dict(schema_version=1,context=ctx,owner_command=['make','coverage'],nonce='current-owner-nonce',exit_code=0,interception_count=1,child_sha256='',stages=[dict(command=x,exit_code=0) for x in stages])
        self.write()
    def tearDown(self):self.tmp.cleanup()
    def write(self):self.path.write_text(json.dumps(self.child,sort_keys=True,indent=2)+'\n');self.owner['child_sha256']=c.digest(self.path)
    def check(self):return k.owner_completion(self.owner,self.expected,self.path)
    def test_actual_opaque_wrapper_and_selectors_bind_discovery_without_second_execution(self):
        self.assertTrue(self.check());args=self.child['discovery_command']
        self.assertIn('--features=ebpf',args);self.assertIn('--target-dir',args);self.assertIn('all() & not binary(oci_crash)',args)
        self.assertNotIn('--test-threads=2',args);self.assertNotIn('--no-tests=fail',args)
        self.assertEqual(sum('nextest' in x and '--no-report' in x for x in self.expected['stages']),1)
    def test_absent_environment_values_remain_distinct_from_empty(self):
        self.child['list_environment']['RUSTFLAGS']='';self.write()
        with self.assertRaisesRegex(c.Invalid,'instrumentation differs'):self.check()
    def test_list_in_an_uninstrumented_context_is_refused(self):
        self.child['list_environment']['__CARGO_LLVM_COV_RUSTC_WRAPPER']=None;self.write()
        with self.assertRaisesRegex(c.Invalid,'instrumentation differs'):self.check()
    def test_target_dir_mismatch_refused(self):
        self.child['target_dir']='/work/target/debug';self.write()
        with self.assertRaisesRegex(c.Invalid,'another target'):self.check()
    def test_nonzero_floor_cannot_be_hidden_by_passing_tests(self):
        self.owner['stages'][-1]['exit_code']=1
        with self.assertRaisesRegex(c.Invalid,'floor stage failed'):self.check()
    def test_actual_owner_failure_refused(self):
        self.owner['exit_code']=2
        with self.assertRaisesRegex(c.Invalid,'owner failed'):self.check()
    def test_missing_or_duplicate_interception_refused(self):
        for count in [0,2,True]:
            self.owner['interception_count']=count
            with self.assertRaisesRegex(c.Invalid,'interception'):self.check()
    def test_version_query_cannot_supply_run_receipt(self):
        with self.assertRaisesRegex(c.Invalid,'actual run'):k.list_argv(['cargo','nextest','--version'],['cargo','nextest'])
    def test_unaudited_coverage_version_refused_even_if_plan_pins_it(self):
        self.child['tools']['coverage']['version']='cargo-llvm-cov0.8.7';self.expected['tools']=copy.deepcopy(self.child['tools']);self.write()
        with self.assertRaisesRegex(c.Invalid,'unaudited cargo-llvm-cov'):self.check()
    def test_changed_executable_tool_context_refused(self):
        self.child['tools']['coverage']['sha256']='b'*64;self.write()
        with self.assertRaisesRegex(c.Invalid,'coverage toolchain'):self.check()
    def test_unknown_nextest_option_is_not_silently_removed(self):
        with self.assertRaisesRegex(c.Invalid,'unreviewed actual nextest'):k.list_argv(self.expected['run_argv']+['--made-up'],['cargo','nextest'])
    def test_nonzero_retries_refused(self):
        with self.assertRaisesRegex(c.Invalid,'retries'):k.list_argv(['cargo','nextest','run','--retries','2'],['cargo','nextest'])
    def test_selection_spelling_and_archive_remap_are_preserved(self):
        run=['/tools/cargo-nextest','nextest','run','--profile=ci','--archive-file','archive.tar.zst','--workspace-remap=/work','--target-dir-remap','/work/target','--run-ignored','only','-E','binary(suite) & not test(skip)','--retries=0']
        listed=k.list_argv(run,['/tools/cargo-nextest','nextest'])
        self.assertEqual(listed,['/tools/cargo-nextest','nextest','list']+run[3:-1]+['--message-format=json'])
    def test_private_keys_are_opaque_and_unknown_context_fails_closed(self):
        with self.assertRaisesRegex(c.Invalid,'unreviewed private'):k.snapshot_environment({'__CARGO_LLVM_COV_NEW_PRIVATE':'x'})
    def test_credentials_are_not_collected_into_context(self):
        env=k.snapshot_environment({'GITHUB_TOKEN':'secret','REGISTRY_PASSWORD':'secret','CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS':'-C opt-level=1'})
        self.assertNotIn('GITHUB_TOKEN',env);self.assertNotIn('REGISTRY_PASSWORD',env);self.assertEqual(env['CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS'],'-C opt-level=1')
    def test_child_bytes_cannot_change_after_owner_completion(self):
        self.path.write_text(json.dumps(self.child))
        with self.assertRaisesRegex(c.Invalid,'child receipt changed'):self.check()
    def test_stale_synthetic_merge_checkout_or_attempt_refused(self):
        self.child['context']=dict(self.child['context'],commit='b'*40);self.write()
        with self.assertRaisesRegex(c.Invalid,'stale coverage child context'):self.check()
    def test_existing_floor_cannot_be_lowered(self):
        self.expected['stages'][-1][-1]='70';self.owner['stages'][-1]['command']=self.expected['stages'][-1]
        with self.assertRaisesRegex(c.Invalid,'floor missing or changed'):self.check()
    def test_uninstrumented_wrapper_is_not_accepted_even_by_a_self_chosen_context(self):
        self.child['run_environment']['RUSTC_WRAPPER']='/tools/plain';self.child['list_environment']=copy.deepcopy(self.child['run_environment']);self.expected['instrumentation']=copy.deepcopy(self.child['run_environment']);self.write()
        with self.assertRaisesRegex(c.Invalid,'compiler wrapper'):self.check()

    def test_compiler_override_context_is_preserved_and_mismatch_refused(self):
        captured=k.snapshot_environment({'CARGO_BUILD_RUSTC':'/tools/compiler','CARGO_PROFILE_RELEASE_OVERFLOW_CHECKS':'true','CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER':'custom runner'})
        self.assertEqual(captured['CARGO_BUILD_RUSTC'],'/tools/compiler')
        self.assertEqual(captured['CARGO_PROFILE_RELEASE_OVERFLOW_CHECKS'],'true')
        self.child['list_environment']['CARGO_BUILD_RUSTC']='/other/compiler';self.write()
        with self.assertRaisesRegex(c.Invalid,'instrumentation differs'):self.check()
    def prepare_reports(self):
        discovery={'rust-build-meta':{},'test-count':1,'rust-suites':{'reliaburger':{'binary-id':'reliaburger','status':'listed','testcases':{'cron::boundary':{'filter-match':{'status':'matches'}}}}}}
        (self.path.parent/'discovery.json').write_text(json.dumps(discovery))
        (self.path.parent/'junit.xml').write_text('<testsuites><testsuite name="reliaburger"><testcase classname="reliaburger" name="cron::boundary"/></testsuite></testsuites>')
        self.child['discovery_sha256']=c.digest(self.path.parent/'discovery.json');self.child['junit_sha256']=c.digest(self.path.parent/'junit.xml');self.write()
        (self.path.parent/'owner.json').write_text(json.dumps(self.owner))
    def test_current_completion_also_requires_exact_discovery_and_execution(self):
        self.prepare_reports();self.assertEqual(k.evidence(self.expected,self.path.parent,{('reliaburger','cron::boundary')}),{('reliaburger','cron::boundary')})
    def test_successful_owner_without_required_case_is_refused(self):
        self.prepare_reports()
        with self.assertRaisesRegex(c.Invalid,'required cases'):k.evidence(self.expected,self.path.parent,{('reliaburger','other::case')})
    def test_skipped_junit_case_cannot_be_hidden_by_success_owner(self):
        self.prepare_reports();p=self.path.parent/'junit.xml';p.write_text(p.read_text().replace('/>','><skipped/></testcase>',1));self.child['junit_sha256']=c.digest(p);self.write();(self.path.parent/'owner.json').write_text(json.dumps(self.owner))
        with self.assertRaisesRegex(c.Invalid,'did not pass'):k.evidence(self.expected,self.path.parent,{('reliaburger','cron::boundary')})
    def test_report_changed_after_actual_child_completion_is_refused(self):
        self.prepare_reports();(self.path.parent/'discovery.json').write_text('{}')
        with self.assertRaisesRegex(c.Invalid,'report hash'):k.evidence(self.expected,self.path.parent,{('reliaburger','cron::boundary')})

    def test_version_collector_records_actual_command_output_and_exit(self):
        from types import SimpleNamespace
        commands={}
        for name in self.tools:
            path=self.path.parent/name;path.write_bytes(('synthetic tool '+name).encode());commands[name]=[str(path)]+(['llvm-cov','--version'] if name=='coverage' else ['--version']+(['--verbose'] if name=='rustc' else []))
        calls=[]
        def run(command,**kw):
            calls.append(command);return SimpleNamespace(returncode=0,stdout=self.tools[Path(command[0]).name]['version']+'\n')
        actual=k.capture_tools(commands,run)
        self.assertEqual(len(calls),4);self.assertEqual(actual['rustc']['version_command'],commands['rustc']);self.assertEqual(actual['coverage']['sha256'],c.digest(self.path.parent/'coverage'))
        k.tool_context(actual,copy.deepcopy(actual))
        actual['coverage']['exit_code']=1
        with self.assertRaisesRegex(c.Invalid,'version command failed'):k.tool_context(actual,copy.deepcopy(actual))
    def test_short_rustc_version_cannot_replace_actual_verbose_identity(self):
        self.child['tools']['rustc']['version_command']=['/tools/rustc','--version'];self.expected['tools']=copy.deepcopy(self.child['tools']);self.write()
        with self.assertRaisesRegex(c.Invalid,'verbose identity'):self.check()

    def test_audited_1_98_1_patch_binds_its_own_actual_identity(self):
        for name in ('rustc','cargo'):
            self.child['tools'][name]['version']=self.child['tools'][name]['version'].replace('1.98.0','1.98.1')
            self.child['tools'][name]['sha256']='b'*64
        self.expected['tools']=copy.deepcopy(self.child['tools']);self.write()
        self.assertTrue(self.check())
    def test_unreviewed_rust_or_cargo_patch_is_not_accepted(self):
        for name in ('rustc','cargo'):
            tools=copy.deepcopy(self.tools);tools[name]['version']=tools[name]['version'].replace('1.98.0','1.98.2')
            with self.assertRaisesRegex(c.Invalid,'unaudited'):k.tool_context(tools,copy.deepcopy(tools))
    def test_accepted_patch_cannot_reuse_another_tools_expected_digest(self):
        self.child['tools']['rustc']['version']=self.child['tools']['rustc']['version'].replace('1.98.0','1.98.1');self.write()
        with self.assertRaisesRegex(c.Invalid,'toolchain context'):self.check()
    def test_report_tool_override_cannot_hide_in_a_self_selected_context(self):
        self.child['run_environment']['LLVM_COV']='/other/llvm-cov';self.child['list_environment']=copy.deepcopy(self.child['run_environment']);self.expected['instrumentation']=copy.deepcopy(self.child['run_environment']);self.write()
        with self.assertRaisesRegex(c.Invalid,'report-tool/bootstrap'):self.check()

    def query_commands(self):
        commands = {}
        for name, tool in self.tools.items():
            path = self.path.parent / name
            path.write_text('synthetic tool ' + name)
            commands[name] = [str(path)] + tool['version_command'][1:]
        return commands

    def test_coverage_version_collector_accepts_required_cargo_subcommand(self):
        from types import SimpleNamespace
        commands = self.query_commands()
        actual = k.capture_tools(commands, lambda command, **kwargs: SimpleNamespace(
            returncode=0, stdout=self.tools[Path(command[0]).name]['version']))
        self.assertEqual(actual['coverage']['version_command'][1:], ['llvm-cov', '--version'])
        k.tool_context(actual, copy.deepcopy(actual))

    def test_coverage_version_collector_refuses_bare_or_unreviewed_query_before_dispatch(self):
        from types import SimpleNamespace
        for arguments in (['--version'], ['llvm-cov', '--version', '--verbose'],
                          ['llvm-cov', 'report', '--version']):
            commands = self.query_commands()
            commands['coverage'] = [commands['coverage'][0]] + arguments
            calls = []

            def run(command, **kwargs):
                calls.append(command)
                return SimpleNamespace(returncode=0, stdout='synthetic version')

            with self.subTest(arguments=arguments):
                with self.assertRaisesRegex(c.Invalid, 'unaudited tool version query'):
                    k.capture_tools(commands, run)
                self.assertNotIn(commands['coverage'], calls)

    def test_coverage_context_refuses_missing_subcommand_even_with_matching_pinned_version(self):
        for arguments in (['--version'], ['llvm-cov', 'report', '--version']):
            tools = copy.deepcopy(self.tools)
            tools['coverage']['version_command'] = ['/tools/coverage'] + arguments
            with self.subTest(arguments=arguments):
                with self.assertRaisesRegex(c.Invalid, 'coverage version query'):
                    k.tool_context(tools, copy.deepcopy(tools))

if __name__=='__main__':unittest.main()
