"""A pretty job table must not turn missing or orphaned evidence into a pass."""
import json
from pathlib import Path
import tempfile
import unittest
import job_soak as jobs
import job_soak_inventory as inventory
import sustained_check as checker

class Gate(unittest.TestCase):
    def test_missing_heartbeat_and_early_stop_fail_the_snapshot(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory); snap=root/"snap"; snap.mkdir()
            (root/"metadata.json").write_text(json.dumps(dict(job_soak=True,job_stop_at=1000)))
            self.assertTrue(checker.job_findings(root,snap,100,True))
            (snap/"jobs.json").write_text(json.dumps(dict(stopped=True,heartbeat=100,errors=[])))
            self.assertTrue(checker.job_findings(root,snap,100,True))
    def test_exact_owners_do_not_excuse_an_executor_shaped_orphan(self):
        prefix="a"*32; good="default__executor-"+prefix+"-reuse-0"; orphan=good[:-1]+"1"
        proof=dict(prefix=prefix,boot="boot",owners=[dict(id=good,boot="boot",generation="b"*32,
              cgroup="default/executor-"+prefix+"-reuse/0",runtime="shared-runc")],
              resources=dict(runc=[good,orphan],netns=[],lease=[],veth=[],cgroup=[]))
        found=checker.leak_findings({},proof["resources"],[],[],"node",proof)
        self.assertEqual(len(found),1); self.assertIn(orphan,found[0]["detail"])
    def test_final_render_forces_failure_when_job_coverage_is_missing(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory)
            (root/"metadata.json").write_text(json.dumps(dict(job_soak=True,result="PASS",started_at=100,finished_at=200)))
            checker.save_state(root,{"nodes":["n"]})
            self.assertEqual(checker.render(root,root/"report.md"),1)
            self.assertIn("mixed job campaign and positive drain",(root/"report.md").read_text())
            self.assertIn("# Sustained soak (V02): FAIL",(root/"report.md").read_text())
    def test_truncated_and_disk_full_inventory_are_not_clean(self):
        for value in (dict(complete=False),dict(schema=1,complete=True,ts=100,boot="b",owners=[],errors=[],available_kb=200000)):
            with self.assertRaises(inventory.InvalidInventory): inventory.validate(value,100)
    def test_job_work_does_not_extend_existing_tier_or_fault_budgets(self):
        script=(Path(__file__).parent/"qualify-sustained.sh").read_text()
        self.assertIn("duration_text:=90m",script)
        self.assertIn("duration_text:=8h",script)
        self.assertIn("end - 120",script)
        self.assertNotIn("sleep 120",script)
        self.assertIn("mark \"$evidence/jobs\"",script)

    def test_live_snapshot_without_drain_timestamp_fails_without_crashing_render(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory); (root/"jobs").mkdir()
            (root/"metadata.json").write_text(json.dumps(dict(job_soak=True,result="PASS",started_at=100,finished_at=200,job_stop_at=180)))
            (root/"jobs/snapshot.json").write_text(json.dumps(dict(stopped=False,stop_requested_at=None,heartbeat=200)))
            checker.save_state(root,{"nodes":["n"]})
            self.assertEqual(checker.render(root,root/"report.md"),1)
            self.assertIn("# Sustained soak (V02): FAIL",(root/"report.md").read_text())


class JobQualification(unittest.TestCase):
    """#686: the job tiers, node faults and the D18-C record."""
    release=Path(__file__).parent

    def plan(self,*arguments):
        import subprocess
        result=subprocess.run([str(self.release/"qualify-jobs-sustained.sh"),*arguments,"--plan"],
                              stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,timeout=60,
                              env=dict(__import__("os").environ,RELIABURGER_SOAK_CAFFEINATED="1"))
        return result.returncode,[line.split()[1:] for line in result.stdout.splitlines() if line.startswith("plan: ")],result.stderr

    def test_fast_tier_plans_every_node_fault_on_every_node_within_ninety_minutes(self):
        code,lines,_=self.plan("--tier","fast")
        self.assertEqual(code,0)
        header=dict(zip(lines[0][::2],lines[0][1::2]))
        self.assertEqual((header["tier"],header["schedule"],header["duration"]),("fast","compressed","5400"))
        self.assertEqual((header["job-load"],header["target-rate"],header["node-faults"]),("target","1390","true"))
        faults=[line[3] for line in lines[1:] if line[2]=="node-fault"]
        self.assertEqual(faults,["delay","disk-full","slow-fsync"]*3)
        slots={line[2] for line in lines[1:]}
        self.assertTrue({"follower-kill","leader-kill","power-off","chaos","node-fault"}<=slots)
        specials={line[3] for line in lines[1:] if line[2]=="special"}
        self.assertEqual(specials,{"graceful-stop","quorum-loss","all-off"})

    def test_final_tier_runs_a_day_on_the_full_schedule_at_the_requested_rate(self):
        code,lines,_=self.plan("--tier","final","--target-rate","2000")
        self.assertEqual(code,0)
        header=dict(zip(lines[0][::2],lines[0][1::2]))
        self.assertEqual((header["schedule"],header["duration"],header["target-rate"]),("full","86400","2000"))
        self.assertEqual(sum(1 for line in lines[1:] if line[2]=="node-fault"),24)

    def test_job_tiers_refuse_a_rate_below_a_hundred_million_a_day_or_a_changed_schedule(self):
        code,_,error=self.plan("--tier","fast","--target-rate","1000")
        self.assertNotEqual(code,0); self.assertIn("1158",error)
        for option in (["--duration","2h"],["--schedule","full"],["--job-load","calibration"],["--node-faults"]):
            code,_,error=self.plan("--tier","fast",*option)
            self.assertNotEqual(code,0); self.assertIn("fixed by the job qualification tier",error)
        code,_,_=self.plan("--tier","weekly")
        self.assertNotEqual(code,0)

    def test_the_release_soak_keeps_its_schedule_without_node_faults(self):
        import subprocess
        result=subprocess.run([str(self.release/"qualify-sustained.sh"),"--tier","fast","--plan"],stdout=subprocess.PIPE,
                              text=True,timeout=60,env=dict(__import__("os").environ,RELIABURGER_SOAK_CAFFEINATED="1"))
        self.assertEqual(result.returncode,0)
        self.assertIn("job-load calibration target-rate none node-faults false",result.stdout)
        self.assertNotIn("node-fault ",result.stdout)

    def test_cpu_samples_count_forward_only_within_one_process_and_helper(self):
        state={}
        checker.job_metric_samples(state,"n",dict(bun_pid=["7"],bun_cpu_ticks=["1000"],clk_tck=["100"],executor_cpu=["d/e/0/helper 1000000"]),100)
        checker.job_metric_samples(state,"n",dict(bun_pid=["7"],bun_cpu_ticks=["6000"],clk_tck=["100"],executor_cpu=["d/e/0/helper 6000000","d/e/1/helper 5"]),200)
        # A restarted Bun and a retired helper start new baselines.
        checker.job_metric_samples(state,"n",dict(bun_pid=["8"],bun_cpu_ticks=["10"],clk_tck=["100"],executor_cpu=["d/e/1/helper 1000005"]),300)
        metrics=state["job_metrics"]["n"]
        self.assertEqual((metrics["bun_cpu_seconds"],metrics["bun_wall_seconds"]),(50.0,100))
        self.assertEqual((metrics["executor_cpu_seconds"],metrics["executor_wall_seconds"]),(6.0,200))
        state["trends"]={"n":{"rss_kb":{"first":102400,"max":153600},"disk_kb logs":{"first":0,"max":20000}}}
        state["job_resources"]={"n":{"peak_storage_kb":{"task-arrays":8192}}}
        host=checker.job_host_metrics(state,2_000_000)
        self.assertEqual(host["leader_rss_growth_mib"],50)
        self.assertEqual(host["leader_cpu_cores"],0.5)
        self.assertEqual(host["executor_overhead_cores_per_node"],0.03)
        self.assertEqual(host["retained_log_mb"],20.48)
        self.assertEqual(host["node_ledger_mib_per_million_tasks"],4)

    def test_render_writes_the_job_report_beside_the_record_and_never_replaces_it(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory); (root/"jobs").mkdir()
            (root/"metadata.json").write_text(json.dumps(dict(job_soak=True,node_faults=True,started_at=100,finished_at=5000,job_stop_at=4800)))
            snapshot=dict(stopped=True,active=[],errors=[],heartbeat=4900,started=100,stop_requested_at=4800,load="target",target_rate=1390,
                          progress={m:dict(succeeded=0,retried=0) for m in jobs.MODES},coverage={},totals={},metrics={})
            snapshot["progress"]["process"]["succeeded"]=1390*4700
            (root/"jobs/snapshot.json").write_text(json.dumps(snapshot))
            checker.save_state(root,{"nodes":["a","b","c"]})
            self.assertEqual(checker.render(root,root/"record.md"),1)
            report=json.loads((root/"record.jobs.json").read_text())
            self.assertTrue(report["target_met"])
            self.assertEqual(report["gate"]["nodes"],3)
            self.assertIn("slow-fsync",report["gate"]["fault_kinds"])
            text=(root/"record.md").read_text()
            self.assertIn("## Job qualification (D18-C)",text)
            self.assertIn("active slow-fsync overlap",text)
            (root/"other.jobs.json").write_text("{}")
            with self.assertRaises(SystemExit): checker.render(root,root/"other.md")

    def test_every_node_fault_has_a_parseable_start_and_stop_script(self):
        import re,subprocess
        source=(self.release/"qualify-sustained.sh").read_text()
        function=re.search(r"^node_fault_script\(\) \{\n.*?^\}\n",source,re.S|re.M).group(0)
        for kind in jobs.NODE_FAULT_KINDS:
            for action in ("start","stop"):
                script=subprocess.run(["bash","-c",function+'node_fault_script "$1" "$2"',"_",kind,action],
                                      stdout=subprocess.PIPE,text=True,check=True).stdout
                self.assertTrue(script.strip(),(kind,action))
                subprocess.run(["bash","-n","-c",script],check=True)
        self.assertIn("netem delay",function); self.assertIn("fallocate",function); self.assertIn("IOWriteIOPSMax",function)
