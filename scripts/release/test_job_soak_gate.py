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



def inventory_proof(ts, boot="b1"):
    return dict(schema=1,complete=True,ts=ts,boot=boot,prefix="a"*32,owners=[],errors=[],
                resources=dict(runc=[],netns=[],lease=[],veth=[],cgroup=[]),storage_kb={},available_kb=1<<20)


class InventoryWindow(unittest.TestCase):
    """The guests are asked after the snapshot starts, so their proof is newer
    than meta.json's ts. It must still be judged fresh (V02 candidate 3,
    snapshots/20261013T044520-heavy-3: meta 1791866720, inventory 1791866724)."""

    def test_inventory_taken_while_the_snapshot_was_collected_is_fresh(self):
        for ts in (1000,1004,1010,1010+inventory.CLOCK_SKEW):
            with self.subTest(ts=ts):
                self.assertEqual(inventory.validate(inventory_proof(ts),1000,"b1",collected=1010)["ts"],ts)

    def test_inventory_from_before_the_window_or_after_collection_is_refused(self):
        for ts in (909,1011+inventory.CLOCK_SKEW,5000):
            with self.subTest(ts=ts), self.assertRaisesRegex(inventory.InvalidInventory,"stale"):
                inventory.validate(inventory_proof(ts),1000,"b1",collected=1010)

    def test_without_a_collection_end_the_window_closes_at_the_snapshot_start(self):
        inventory.validate(inventory_proof(1000),1000,"b1")
        with self.assertRaises(inventory.InvalidInventory):
            inventory.validate(inventory_proof(1004+inventory.CLOCK_SKEW),1000,"b1")

    def evidence(self, directory, meta, proof_ts):
        root=Path(directory); snap=root/"snap"; snap.mkdir()
        (root/"metadata.json").write_text(json.dumps(dict(job_soak=True,job_stop_at=10**10)))
        checker.save_state(root,{"nodes":["n"]})
        (snap/"meta.json").write_text(json.dumps(meta))
        (snap/"inventory-n.txt").write_text("boot b1\nnrestarts 0\nbun_pid 100\nbun_fd 300\nbun_rss_kb 300000\n")
        (snap/"jobs-n.txt").write_text(json.dumps(inventory_proof(proof_ts)))
        return root,snap

    def inventory_findings(self, root, snap):
        import contextlib,io
        with contextlib.redirect_stdout(io.StringIO()): checker.evaluate(root,snap)
        verdict=json.loads((snap/"verdict.json").read_text())
        return [item["detail"] for item in verdict["findings"] if item["check"]=="job-inventory"]

    def test_checker_accepts_a_proof_taken_after_the_snapshot_started(self):
        with tempfile.TemporaryDirectory() as directory:
            root,snap=self.evidence(directory,dict(ts=1000,kind="heavy",collected=1030),1004)
            self.assertEqual(self.inventory_findings(root,snap),[])

    def test_checker_refuses_a_proof_from_after_the_collection_ended(self):
        with tempfile.TemporaryDirectory() as directory:
            root,snap=self.evidence(directory,dict(ts=1000,kind="heavy",collected=1030),1100)
            self.assertEqual(self.inventory_findings(root,snap),["job inventory stale or from another boot"])

    def test_checker_replays_older_evidence_by_when_its_files_were_written(self):
        # Snapshots from before meta.json recorded the collection end.
        import os
        with tempfile.TemporaryDirectory() as directory:
            root,snap=self.evidence(directory,dict(ts=1000,kind="heavy"),1004)
            for name in ("meta.json","inventory-n.txt","jobs-n.txt"): os.utime(snap/name,(1000,1000))
            self.assertEqual(len(self.inventory_findings(root,snap)),1)
            os.utime(snap/"jobs-n.txt",(1006,1006))
            self.assertEqual(self.inventory_findings(root,snap),[])

    def test_the_harness_records_when_a_snapshot_finished_collecting(self):
        source=(Path(__file__).parent/"qualify-sustained.sh").read_text()
        observe=source[source.index("\nobserve() {"):]
        observe=observe[:observe.index("\n}\n")]
        self.assertLess(observe.index("collect "),observe.index("finish_snapshot"))
        self.assertLess(observe.index("finish_snapshot"),observe.index("check evaluate"))
        self.assertIn('"collected"',source)


class LightSnapshots(unittest.TestCase):
    """#943: light snapshots never collect guest inventories, so only the
    kinds that do (settle and heavy) may require one."""

    def findings(self, kind, window=False):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory); snap=root/"snap"; snap.mkdir()
            (root/"metadata.json").write_text(json.dumps(dict(job_soak=True,job_stop_at=10**10)))
            checker.save_state(root,dict(nodes=["n"],window="power-off" if window else None))
            (snap/"meta.json").write_text(json.dumps(dict(ts=1000,kind=kind,collected=1001)))
            import contextlib,io
            with contextlib.redirect_stdout(io.StringIO()): checker.evaluate(root,snap)
            verdict=json.loads((snap/"verdict.json").read_text())
            return [(item["severity"],item["detail"]) for item in verdict["findings"] if item["check"]=="job-inventory"]

    def test_a_light_snapshot_outside_a_window_does_not_ask_for_the_inventory(self):
        self.assertEqual(self.findings("light"),[])

    def test_settle_and_heavy_snapshots_still_require_the_inventory(self):
        for kind in ("settle","heavy"):
            with self.subTest(kind=kind):
                self.assertEqual(self.findings(kind),[("fail","kernel/storage inventory missing")])

    def test_only_the_settle_and_heavy_collection_asks_the_guests(self):
        source=(Path(__file__).parent/"qualify-sustained.sh").read_text()
        light=source[source.index("\ncollect_light() {"):]; light=light[:light.index("\n}\n")]
        full=source[source.index("\ncollect() {"):]; full=full[:full.index("\n}\n")]
        self.assertNotIn("guest_report",light)
        self.assertIn("guest_report",full)
        self.assertEqual(checker.INVENTORY_KINDS,("settle","heavy"))

    def test_inside_a_window_a_missing_inventory_is_not_a_failure(self):
        self.assertEqual(self.findings("heavy",window=True),[])


PREFIX="607618dfb8e896c27b1465bcbc36f16f"
OWNER="default__executor-"+PREFIX+"-153"
# From V02 candidate 4, snapshots/20261013T233207-heavy-53, node 3 (#944).
SETUP_PEER="vtc-3b37e99f"


class SetupPeers(unittest.TestCase):
    """#944: a fresh-runc task's network setup briefly leaves its temporary
    container-side peer `vtc-<hash>` in the host namespace. It is excused
    only for an owner the inventory proves is live, and only once."""

    def setUp(self):
        self.temp=tempfile.TemporaryDirectory(); self.addCleanup(self.temp.cleanup)
        self.root=Path(self.temp.name); self.count=0
        (self.root/"metadata.json").write_text(json.dumps(dict(job_soak=True,job_stop_at=10**10)))
        checker.save_state(self.root,dict(nodes=["n"],baseline=dict(n=dict(
            inventory=dict(veth=["veth-app"]),instances=["default__web-0"]))))

    def check(self, veths, owners=(OWNER,)):
        self.count+=1; ts=10000*self.count  # beyond the repeat window
        snap=self.root/("snap-%d"%self.count); snap.mkdir()
        (snap/"meta.json").write_text(json.dumps(dict(ts=ts,kind="heavy",collected=ts+5)))
        (snap/"status.json").write_text(json.dumps([dict(node="n",id="default__web-0")]))
        owned=[dict(id=identity,boot="b1",generation="b"*32,runtime="runc",
                    cgroup="default/executor-"+PREFIX+"/"+identity.rsplit("-",1)[1]) for identity in owners]
        resources=dict(runc=[],netns=["rb-"+identity for identity in owners],lease=list(owners),
                       veth=["veth-app",*veths],cgroup=[owner["cgroup"] for owner in owned])
        (snap/"inventory-n.txt").write_text("boot b1\nnrestarts 0\nbun_pid 100\nbun_fd 300\nbun_rss_kb 300000\n"
                                            +"".join("veth "+name+"\n" for name in resources["veth"]))
        (snap/"jobs-n.txt").write_text(json.dumps(dict(schema=1,complete=True,ts=ts+1,boot="b1",prefix=PREFIX,
              owners=owned,errors=[],resources=resources,storage_kb={},available_kb=1<<20)))
        import contextlib,io
        with contextlib.redirect_stdout(io.StringIO()): checker.evaluate(self.root,snap)
        verdict=json.loads((snap/"verdict.json").read_text())
        return [item["detail"] for item in verdict["findings"] if item["check"]=="leak-veth" and item["severity"]=="fail"]

    def test_the_setup_peer_is_named_as_the_runtime_names_it(self):
        self.assertEqual(inventory.setup_peer(OWNER),SETUP_PEER)

    def test_a_live_owners_setup_peer_is_not_a_leak(self):
        self.assertEqual(self.check([inventory.veth(OWNER),SETUP_PEER]),[])

    def test_a_setup_peer_without_a_live_owner_is_a_leak(self):
        other="vtc-"+"0"*8
        self.assertEqual(self.check([inventory.veth(OWNER),other]),["2 present, at most 1 expected"])
        # An owner that isn't proven live doesn't excuse its own peer either.
        self.assertEqual(self.check([SETUP_PEER],owners=()),["2 present, at most 1 expected"])

    def test_a_setup_peer_still_present_in_the_next_inventory_is_a_leak(self):
        self.assertEqual(self.check([inventory.veth(OWNER),SETUP_PEER]),[])
        self.assertEqual(self.check([inventory.veth(OWNER),SETUP_PEER]),["2 present, at most 1 expected"])
        self.assertEqual(self.check([inventory.veth(OWNER),SETUP_PEER]),["2 present, at most 1 expected"])

    def test_a_new_setup_peer_in_the_next_inventory_is_excused_again(self):
        second="default__executor-"+PREFIX+"-154"
        self.assertEqual(self.check([inventory.veth(OWNER),SETUP_PEER]),[])
        self.assertEqual(self.check([inventory.veth(second),inventory.setup_peer(second)],owners=(second,)),[])


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

    def test_final_tier_runs_twelve_hours_on_the_full_schedule_at_the_requested_rate(self):
        code,lines,_=self.plan("--tier","final","--target-rate","2000")
        self.assertEqual(code,0)
        header=dict(zip(lines[0][::2],lines[0][1::2]))
        self.assertEqual((header["schedule"],header["duration"],header["target-rate"]),("full","43200","2000"))
        self.assertEqual(sum(1 for line in lines[1:] if line[2]=="node-fault"),12)

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
        self.assertNotIn("leader_rss_growth_mib",host)
        self.assertEqual(host["leader_cpu_cores"],0.5)
        self.assertEqual(host["executor_overhead_cores_per_node"],0.03)
        self.assertEqual(host["retained_log_mb"],20.48)
        self.assertEqual(host["node_ledger_mib_per_million_tasks"],4)

    def bun(self,state,node,ts,pid,rss_kb,image="i1",boot="b1"):
        checker.job_metric_samples(state,node,dict(boot=[boot],bun_pid=[str(pid)],bun_image=[image],bun_rss_kb=[str(rss_kb)]),ts)

    def test_steady_state_rss_is_per_process_across_a_restart_not_peak_minus_first(self):
        # One node, two Bun processes: the first grows to 600 MiB and is
        # restarted, the second settles at 200 MiB. Peak minus first across
        # both (the old measure) would say 500 MiB of growth; each process
        # gets its own median after its first ten minutes instead.
        state={"nodes":["api","n2","n3"]}
        start=1000
        self.bun(state,"api",start,7,100*1024)
        for minute in range(1,31):
            self.bun(state,"api",start+60*minute,7,(300 if minute<30 else 600)*1024)
        restart=start+31*60
        self.bun(state,"api",restart,8,50*1024)
        for minute in range(1,21):
            self.bun(state,"api",restart+60*minute,8,200*1024)
        host=checker.job_host_metrics(state,0)
        processes=host["per_node"]["api"]["bun_processes"]
        self.assertEqual([process["pid"] for process in processes],[7,8])
        self.assertEqual([process["steady_rss_mib"] for process in processes],[300,200])
        self.assertEqual([process["peak_rss_mib"] for process in processes],[600,200])
        self.assertEqual(host["per_node"]["api"]["bun_steady_rss_mib"],300)
        self.assertEqual(host["per_node"]["api"]["bun_peak_rss_mib"],600)

    def test_an_upgrade_exec_with_the_same_pid_is_a_new_process(self):
        state={"nodes":["api"]}
        for minute in range(0,15):
            self.bun(state,"api",60*minute,7,400*1024,image="old")
        for minute in range(15,40):
            self.bun(state,"api",60*minute,7,100*1024,image="new")
        processes=checker.job_host_metrics(state,0)["per_node"]["api"]["bun_processes"]
        self.assertEqual([(process["image"],process["steady_rss_mib"]) for process in processes],[("old",400),("new",100)])

    def test_a_process_seen_for_less_than_the_settling_time_has_only_a_peak(self):
        state={"nodes":["api"]}
        self.bun(state,"api",0,7,100*1024)
        self.bun(state,"api",checker.BUN_STEADY_AFTER-1,7,120*1024)
        node=checker.job_host_metrics(state,0)["per_node"]["api"]
        self.assertIsNone(node["bun_processes"][0]["steady_rss_mib"])
        self.assertIsNone(node["bun_steady_rss_mib"])
        self.assertEqual(node["bun_peak_rss_mib"],120)

    def test_the_api_node_gets_512_mib_and_the_others_256_mib(self):
        # Node 1 serves the API the harness and the job controller talk to.
        state={"nodes":["n1","n2","n3"]}
        for node,mib in (("n1",400),("n2",250),("n3",100)):
            for minute in range(0,20):
                self.bun(state,node,60*minute,7,mib*1024)
        host=checker.job_host_metrics(state,0)
        self.assertEqual({node:row["bun_rss_target_mib"] for node,row in host["per_node"].items()},dict(n1=512,n2=256,n3=256))
        # The worst node is the closest to its own target: 250/256 beats 400/512.
        self.assertEqual(host["bun_rss_worst"],dict(node="n2",steady_rss_mib=250,peak_rss_mib=250,target_mib=256))
        lines="\n".join(checker.job_qualification_lines(dict(m8=host)))
        self.assertIn("| n2: 250.00 MiB steady, 250.00 MiB peak | ≤ 256 MiB (≤ 512 MiB on node 1, which serves the API) |",lines)
        self.assertIn("- Bun RSS on n1 (serves the API, target ≤ 512 MiB): pid 7: 400.00 MiB steady, 400.00 MiB peak",lines)
        state={"nodes":["n1","n2"]}
        for minute in range(0,20):
            self.bun(state,"n1",60*minute,7,500*1024)
            self.bun(state,"n2",60*minute,7,200*1024)
        self.assertEqual(checker.job_host_metrics(state,0)["bun_rss_worst"]["node"],"n1")

    def test_task_array_storage_is_compared_against_64_mib_per_million(self):
        self.assertEqual(checker.TASK_ARRAY_STORAGE_TARGET_MIB,64)
        state={"nodes":["n"],"job_metrics":{"n":{"bun_cpu_seconds":0.0,"executor_cpu_seconds":0.0}},
               "job_resources":{"n":{"peak_storage_kb":{"task-arrays":45*1024}}}}
        host=checker.job_host_metrics(state,1_000_000)
        self.assertEqual(host["node_ledger_mib_per_million_tasks"],45)
        lines=checker.job_qualification_lines(dict(m8=host))
        row=next(line for line in lines if line.startswith("| Node task-array storage"))
        self.assertTrue(row.endswith("| 45.00 MiB | ≤ 64 MiB, deleted after retirement |"),row)
        self.assertEqual(jobs.report(dict(progress={}))["m8"]["targets"]["node_ledger_mib_per_million_tasks"],64)

    def test_the_table_states_its_targets_and_the_decision_behind_them(self):
        text="\n".join(checker.job_qualification_lines(dict(m8=dict(raft_entries_per_second=2.5))))
        self.assertIn("| Measure (recorded, not gated) | Observed | Target |",text)
        self.assertNotIn("September plan target",text)
        self.assertIn("maintainer decision, 15 October 2026",text)
        self.assertIn("https://github.com/reliaburger/reliaburger/issues/668",text)
        self.assertIn("| Raft entries per second, all cluster writes | 2.50 | recorded only; the batch target counts only task-array writes |",text)
        self.assertNotIn("Bun RSS growth",text)

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
