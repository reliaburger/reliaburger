"""A pretty job table must not turn missing or orphaned evidence into a pass."""
import copy
import json
import re
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
        self.assertIn("duration_text:=60m",script)
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


class StoredCampaignError(unittest.TestCase):
    """#968: the campaign keeps each error for the whole run. One refused
    summary failed every later settle, so seven later faults read as
    "did not settle" when the cluster had recovered from each."""

    ERROR="shared-runc long work (2): invalid verified active commands"

    def evaluate(self, root, ts):
        import contextlib,io
        snap=root/("snap-"+str(ts)); snap.mkdir()
        (snap/"meta.json").write_text(json.dumps(dict(ts=ts,kind="settle",collected=ts+1)))
        (snap/"status.json").write_text("[]")
        (snap/"jobs.json").write_text(json.dumps(dict(heartbeat=ts,errors=[self.ERROR],active=[],progress={},
                                                       stopped=False,resume_grace_until=10**10)))
        clean=checker.settle_clean
        checker.settle_clean=lambda *arguments: True
        try:
            with contextlib.redirect_stdout(io.StringIO()): code=checker.evaluate(root,snap)
        finally:
            checker.settle_clean=clean
        verdict=json.loads((snap/"verdict.json").read_text())
        return code,verdict["settle_clean"],[(item["severity"],item["detail"]) for item in verdict["findings"] if item["check"]=="jobs"]

    def test_a_stored_error_fails_once_then_stops_blocking_settles(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory)
            (root/"metadata.json").write_text(json.dumps(dict(job_soak=True,job_stop_at=10**10)))
            checker.save_state(root,dict(nodes=[]))
            code,clean,jobs_found=self.evaluate(root,1000)
            self.assertEqual(code,1)
            self.assertFalse(clean)
            self.assertEqual(jobs_found,[("fail",self.ERROR)])
            # An hour later, past the repeat window too: still no new failure,
            # and the settle is judged on its own recovery.
            for ts in (1100,1000+3600):
                code,clean,jobs_found=self.evaluate(root,ts)
                self.assertEqual(code,0)
                self.assertTrue(clean)
                self.assertEqual(jobs_found,[("info","reported earlier: "+self.ERROR)])
            # The campaign record keeps it, so the final verdict still fails.
            final=json.loads((root/("snap-"+str(1000+3600))/"jobs.json").read_text())
            self.assertIn(self.ERROR,jobs.coverage_failures(final))


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

    def test_fast_tier_plans_every_fault_kind_and_special_within_an_hour(self):
        # #991: the fast tiers went from 90 minutes to one hour (maintainer, 17 October 2026).
        code,lines,_=self.plan("--tier","fast")
        self.assertEqual(code,0)
        header=dict(zip(lines[0][::2],lines[0][1::2]))
        self.assertEqual((header["tier"],header["schedule"],header["duration"]),("fast","compressed","3600"))
        self.assertEqual((header["job-load"],header["target-rate"],header["node-faults"]),("target","1390","true"))
        cycles=[line for line in lines[1:] if line[0]=="cycle"]
        self.assertEqual(len({line[1] for line in cycles})*int(header["cycle"]),3600)
        # The guaranteed cycles (0-3) carry every kind and every special, so
        # overrunning slots can't push one past the hour.
        early=[line for line in cycles if int(line[1])<4]
        faults=[line[3] for line in early if line[2]=="node-fault"]
        self.assertEqual(set(faults),{"delay","disk-full","slow-fsync"})
        slots={line[2] for line in early}
        self.assertTrue({"follower-kill","leader-kill","deploy-kill","power-off","chaos","node-fault"}<=slots)
        specials={line[3] for line in early if line[2]=="special"}
        self.assertEqual(specials,{"graceful-stop","quorum-loss","all-off"})

    def test_node_faults_reach_every_node_within_the_fast_hour(self):
        code,lines,_=self.plan("--tier","fast")
        self.assertEqual(code,0)
        placed=[(line[3],line[5]) for line in lines[1:] if line[2]=="node-fault"]
        self.assertEqual([kind for kind,_ in placed],["delay","disk-full","slow-fsync"]*2)
        # The first three put one kind on each node; the next three move each kind on.
        self.assertEqual({node for _,node in placed[:3]},{"1","2","3"})
        for kind in ("delay","disk-full","slow-fsync"):
            self.assertEqual(len({node for k,node in placed if k==kind}),2,kind)
        # Nine cycles still put every kind on every node.
        import subprocess
        result=subprocess.run([str(self.release/"qualify-sustained.sh"),"--node-faults","--schedule","compressed","--duration","90m","--plan"],
                              stdout=subprocess.PIPE,text=True,timeout=60,env=dict(__import__("os").environ,RELIABURGER_SOAK_CAFFEINATED="1"))
        nine={tuple(line.split()[4:7:2]) for line in result.stdout.splitlines() if " node-fault " in line}
        self.assertEqual(len(nine),9)

    def test_final_tier_runs_eight_hours_on_the_full_schedule_at_the_requested_rate(self):
        # #991: one 8-hour job final replaces the separate V02 final.
        code,lines,_=self.plan("--tier","final","--target-rate","2000")
        self.assertEqual(code,0)
        header=dict(zip(lines[0][::2],lines[0][1::2]))
        self.assertEqual((header["tier"],header["schedule"],header["duration"],header["target-rate"]),("final","full","28800","2000"))
        self.assertEqual(sum(1 for line in lines[1:] if line[2]=="node-fault"),8)
        specials=[line[3] for line in lines[1:] if line[2]=="special"]
        self.assertEqual(sorted(set(specials)),["all-off","graceful-stop","quorum-loss"])

    def test_the_final_plans_cycles_and_run_budget_fit_together(self):
        code,lines,_=self.plan("--tier","final")
        self.assertEqual(code,0)
        header=dict(zip(lines[0][::2],lines[0][1::2]))
        budget=next(line for line in lines if line[0]=="job-runs")[1:]
        budget=dict(zip(budget[::2],budget[1::2]))
        cycles=len({line[1] for line in lines if line[0]=="cycle"})
        # The campaign is sized for the whole schedule the plan prints...
        self.assertEqual(cycles*int(header["cycle"]),int(header["duration"]))
        plan=jobs.run_budget(cycles*int(header["cycle"]))
        self.assertEqual((int(budget["expected"]),int(budget["retained-peak"])),(plan["expected_runs"],plan["peak_retained"]))
        # ...and what it holds at once fits its limit.
        self.assertLessEqual(int(budget["retained-peak"]),int(budget["limit"]))

    def test_the_release_soak_fast_tier_is_an_hour_too(self):
        import subprocess
        result=subprocess.run([str(self.release/"qualify-sustained.sh"),"--tier","fast","--plan"],stdout=subprocess.PIPE,
                              text=True,timeout=60,env=dict(__import__("os").environ,RELIABURGER_SOAK_CAFFEINATED="1"))
        self.assertEqual(result.returncode,0)
        self.assertIn("plan: tier fast schedule compressed duration 3600 ",result.stdout)
        specials={line.split()[4] for line in result.stdout.splitlines() if " special " in line}
        self.assertEqual(specials,{"graceful-stop","quorum-loss","all-off"})

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

    def test_every_soak_uses_limas_default_port_forwarder_whatever_the_caller_set(self):
        # The soak tests the gRPC forwarder quickstart users run. 0.2.0's soaks
        # forced the SSH forwarder around Lima 2.1's guest-agent socket leak
        # (#982); Lima 2.2.1 fixes the leak (#983), so a caller's leftover
        # LIMA_SSH_PORT_FORWARDER must not switch it back. The plan reports
        # nothing about Lima, so read the script: it unsets the variable before
        # anything starts a VM and never sets it, and the jobs tier runs it.
        import re
        script=(self.release/"qualify-sustained.sh").read_text()
        code=[line for line in script.splitlines() if not line.lstrip().startswith("#")]
        self.assertFalse([line for line in code if re.search(r"LIMA_SSH_PORT_FORWARDER=",line)])
        unset=code.index("unset LIMA_SSH_PORT_FORWARDER")
        first_vm=min(index for index,line in enumerate(code) if re.search(r"\$relish |\"\$relish\"|\$limactl|\"\$limactl\"",line))
        self.assertLess(unset,first_vm)
        self.assertIn("qualify-sustained.sh",(self.release/"qualify-jobs-sustained.sh").read_text())

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

    def test_the_api_node_gets_512_mib_and_the_others_384_mib(self):
        # Node 1 serves the API the harness and the job controller talk to.
        state={"nodes":["n1","n2","n3"]}
        for node,mib in (("n1",400),("n2",350),("n3",100)):
            for minute in range(0,20):
                self.bun(state,node,60*minute,7,mib*1024)
        host=checker.job_host_metrics(state,0)
        self.assertEqual({node:row["bun_rss_target_mib"] for node,row in host["per_node"].items()},dict(n1=512,n2=384,n3=384))
        # The worst node is the closest to its own target: 350/384 beats 400/512.
        self.assertEqual(host["bun_rss_worst"],dict(node="n2",steady_rss_mib=350,peak_rss_mib=350,target_mib=384))
        lines="\n".join(checker.job_qualification_lines(dict(m8=host)))
        self.assertIn("| n2: 350.00 MiB steady, 350.00 MiB peak | ≤ 384 MiB (≤ 512 MiB on node 1, which serves the API) |",lines)
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


class DrainedCampaignApply(unittest.TestCase):
    """#963: a rolling deploy after the job campaign drained re-applied the job
    fixtures. That re-registered the cron schedules the drain disabled and ran
    every run_before hook and plain job once more, so job work ran (and held
    executor resources) after the campaign had positively drained."""

    release=Path(__file__).parent

    def write(self, metadata, stop_file=False, now=None):
        import os,re,subprocess,time
        source=(self.release/"qualify-sustained.sh").read_text()
        functions="".join(re.search(rf"^{name}\(\) \{{.*?(?:^\}}\n|\}}\n)",source,re.S|re.M).group(0)
                          for name in ("meta_get","jobs_applied","write_workloads"))
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory); evidence=root/"evidence"; (evidence/"jobs").mkdir(parents=True)
            (evidence/"jobs/installed").write_text("")
            if stop_file: (evidence/"jobs/stop").write_text("")
            (evidence/"metadata.json").write_text(json.dumps(metadata))
            (root/"apps.toml").write_text('[app.soak-identity]\nenv = { SOAK_GENERATION = "0" }\n')
            (root/"scripts/release/sustained").mkdir(parents=True)
            (root/"scripts/release/sustained/soak-jobs.toml").write_text('[job.soak-job-cron-runc]\nschedule = "* * * * *"\n')
            # The clock is the only thing faked: a shell function shadows date(1).
            clock=f"date() {{ echo {now if now is not None else int(time.time())}; }}\n"
            subprocess.run(["bash","-c",clock+functions+"write_workloads"],check=True,
                           env=dict(PATH=os.environ["PATH"],HOME=directory,evidence=str(evidence),generation="7",
                                    workloads_template=str(root/"apps.toml"),repository=str(root)))
            return (evidence/"workloads.toml").read_text()

    def test_the_job_fixtures_are_applied_while_the_campaign_runs(self):
        text=self.write(dict(job_stop_at=10**10))
        self.assertIn('SOAK_GENERATION = "7"',text)
        self.assertIn("soak-job-cron-runc",text)

    def test_the_job_fixtures_are_applied_before_a_stop_time_is_known(self):
        self.assertIn("soak-job-cron-runc",self.write({}))

    def test_no_job_fixture_is_applied_once_the_scheduled_stop_has_passed(self):
        text=self.write(dict(job_stop_at=1000),now=2000)
        self.assertIn('SOAK_GENERATION = "7"',text)
        self.assertNotIn("soak-job",text)

    def test_no_job_fixture_is_applied_in_the_minute_before_the_stop(self):
        # An apply that starts just before the stop can commit its schedules
        # after the campaign last disabled them.
        self.assertNotIn("soak-job",self.write(dict(job_stop_at=2030),now=2000))
        self.assertIn("soak-job",self.write(dict(job_stop_at=2061),now=2000))

    def test_no_job_fixture_is_applied_after_an_explicit_stop(self):
        self.assertNotIn("soak-job",self.write(dict(job_stop_at=10**10),stop_file=True))


class RunBudget(unittest.TestCase):
    """#989: a 12-hour final ran out of the controller's 256-run budget at
    10.5 hours, then stopped polling the runs it had, so the drain met runs
    the candidate had already pruned (HTTP 404) and failed."""

    release=Path(__file__).parent

    class API:
        """Admits every intent; a run's summary is drained, pending or gone."""
        def __init__(self, finish=True):
            self.finish, self.posts, self.gets, self.gone = finish, {}, [], set()
        def request(self, method, path, body=None, raw=False):
            if path=="/v1/jobs/runs":
                identity=self.posts.setdefault(body["request_id"],(len(self.posts)+1,copy.deepcopy(body)))[0]
                return {"batch_id":identity}
            identity=int(path.split("/")[3])
            if identity in self.gone: raise jobs.Gone("candidate API refused "+method+" "+path+" (HTTP 404)")
            if path.endswith("/cancel"): return {}
            self.gets.append(identity)
            body=next(b for i,b in self.posts.values() if i==identity)
            template=body["definition"]["template"]; count=body["definition"]["tasks"]["count"]
            done=self.finish
            return dict(batch_id=identity,name=body["name"],total=count,succeeded=count if done else 1,failed=0,not_run=0,
                        retried=0,queued=0 if done else count-1,held=0,active_commands=0,done=done,nodes=[],
                        runtime=template["runtime"],cpu_request_millicores=25,
                        memory_request_bytes=int(template["memory"][:-2])<<20)

    def setUp(self):
        self.temp=tempfile.TemporaryDirectory(); self.addCleanup(self.temp.cleanup)
        self.path=Path(self.temp.name)/"jobs"

    def campaign(self, api):
        c=jobs.Campaign(self.path,api,[],"token")
        # Every targeted probe already covered: each slot gets ordinary work.
        for mode in jobs.MODES: c.state["coverage"][mode]={purpose:True for purpose in jobs.PURPOSES}
        return c

    def test_a_final_tier_plan_fits_within_its_limit(self):
        # The 8-hour jobs final that replaces the V02 final (#991), and the
        # 12-hour one that ran out for 0.2.0.
        for hours in (8,12):
            plan=jobs.run_budget(hours*3600)
            self.assertTrue(plan["fits"],hours)
            # 0.2.0's final prepared about 25 runs an hour; the plan allows twice that.
            self.assertGreaterEqual(plan["expected_runs"],25*hours)
            # More runs than the old fixed budget of 256 ever admitted.
            self.assertGreater(plan["expected_runs"],256)
            self.assertLessEqual(plan["peak_retained"],plan["limit"])
            self.assertEqual(plan["limit"],jobs.RETAINED_RUNS)

    def test_the_retention_window_is_the_products(self):
        # A finished run frees its slot once the candidate would have pruned it.
        import re
        source=(Path(__file__).resolve().parents[2]/"src/meat/task_array_store.rs").read_text()
        value=re.search(r"pub const TERMINAL_RETENTION_SECS: u64 = (\d+);",source)
        self.assertEqual(int(value.group(1)),jobs.RETENTION_SECS)

    def test_a_schedule_that_would_not_fit_is_refused_up_front(self):
        import subprocess,sys
        self.assertFalse(jobs.run_budget(12*3600,runs_per_hour=400)["fits"])
        result=subprocess.run([sys.executable,str(self.release/"job_soak.py"),"budget","--duration","43200","--runs-per-hour","400"],
                              stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,timeout=60)
        self.assertNotEqual(result.returncode,0)
        self.assertIn("limit %d" % jobs.RETAINED_RUNS,result.stderr)

    def test_the_plan_prints_the_limit_next_to_the_expected_run_count(self):
        import os,subprocess
        result=subprocess.run([str(self.release/"qualify-jobs-sustained.sh"),"--tier","final","--plan"],stdout=subprocess.PIPE,
                              stderr=subprocess.PIPE,text=True,timeout=60,env=dict(os.environ,RELIABURGER_SOAK_CAFFEINATED="1"))
        self.assertEqual(result.returncode,0,result.stderr)
        header=next(line for line in result.stdout.splitlines() if line.startswith("plan: tier ")).split()[1:]
        duration=int(dict(zip(header[::2],header[1::2]))["duration"])
        line=next(line for line in result.stdout.splitlines() if line.startswith("plan: job-runs "))
        fields=line.split()[2:]
        fields=dict(zip(fields[::2],fields[1::2]))
        plan=jobs.run_budget(duration)
        self.assertEqual(fields,dict(expected=str(plan["expected_runs"]),**{"retained-peak":str(plan["peak_retained"])},
                                     limit=str(jobs.RETAINED_RUNS)))

    def test_finished_verified_runs_free_their_slot(self):
        from unittest.mock import patch
        api=self.API(finish=True)
        with patch.object(jobs,"RETAINED_RUNS",6):
            c=self.campaign(api)
            c.step(1000)                       # six runs admitted
            c.step(1002)                       # all six verified drained
            self.assertEqual(c.state["active"],{})
            c.step(1004)                       # their slots are held for the retention window
            self.assertEqual(len(api.posts),6)
            self.assertIn("retained run budget exhausted",";".join(c.state["errors"]))
            c.step(1002+jobs.RETENTION_SECS)   # the candidate has pruned them: slots free
            self.assertEqual(len(api.posts),12)
            self.assertLess(len(c.state["finished"]),jobs.RETAINED_RUNS)

    def test_an_exhausted_budget_keeps_polling_and_draining_its_runs(self):
        from unittest.mock import patch
        api=self.API(finish=False)
        with patch.object(jobs,"RETAINED_RUNS",3):
            c=self.campaign(api)
            c.step(1000); c.step(1002)
            self.assertEqual(len(api.posts),3)
            self.assertEqual(sorted(set(api.gets)),[1,2,3])
            polled=len(api.gets)
            c.step(1004)
            self.assertGreater(len(api.gets),polled)
            self.assertEqual(sum("retained run budget exhausted" in e for e in c.state["errors"]),1)
            self.assertGreater(c.state["progress"]["runc"]["succeeded"],0)
            api.finish=True
            self.assertTrue(c.step(1006,stop=True))

    def test_a_run_pruned_before_its_drain_was_observed_is_unknown_not_drained(self):
        api=self.API(finish=False)
        c=self.campaign(api)
        c.step(1000); c.step(1002)
        api.gone.add(c.state["active"]["runc:short"]["id"])
        c.step(1004,stop=True)
        self.assertNotIn("runc:short",c.state["active"])
        errors=";".join(c.state["errors"])
        self.assertIn("no longer has this run",errors)
        self.assertIn("outcome is unknown",errors)
        self.assertNotIn("drain",c.state["coverage"]["runc"])

    def test_a_run_pruned_after_its_drain_was_observed_is_drained(self):
        api=self.API(finish=True)
        c=self.campaign(api)
        c.step(1000)
        intent=c.state["active"]["runc:short"]
        intent["previous"]=api.request("GET","/v1/batch/%d" % intent["id"])
        api.gone.add(intent["id"])
        c.step(1002,stop=True)
        self.assertNotIn("runc:short",c.state["active"])
        self.assertEqual(c.state["errors"],[])
        self.assertTrue(c.state["coverage"]["runc"]["drain"])

    def test_http_404_from_the_candidate_is_reported_as_gone(self):
        from unittest.mock import patch
        import subprocess
        with tempfile.TemporaryDirectory() as folder:
            ca=Path(folder)/"ca"; ca.write_bytes(b"candidate")
            api=jobs.API([dict(name="n")],9117,ca,Path(folder)/"header")
            def answer(command,input=None,stdout=None,stderr=None,timeout=None):
                stdout.write(b'{"error":"not found"}\n404'); return subprocess.CompletedProcess(command,0)
            with patch("job_soak.subprocess.run",side_effect=answer):
                with self.assertRaises(jobs.Gone): api.request("GET","/v1/batch/7")
            def refused(command,input=None,stdout=None,stderr=None,timeout=None):
                stdout.write(b'{}\n403'); return subprocess.CompletedProcess(command,0)
            with patch("job_soak.subprocess.run",side_effect=refused):
                with self.assertRaises(jobs.InvalidEvidence) as caught: api.request("GET","/v1/batch/7")
                self.assertNotIsInstance(caught.exception,jobs.Gone)


class HostForward(unittest.TestCase):
    """#987: in 0.2.0's jobs final a node was cut off from the host twice
    while the cluster stayed healthy, and the harness reported a dozen
    product-looking failures instead of one forwarding failure."""

    NODES=["rb-a-1","rb-a-2","rb-a-3"]

    def setUp(self):
        self.temp=tempfile.TemporaryDirectory(); self.addCleanup(self.temp.cleanup)
        self.root=Path(self.temp.name)
        self.evidence=self.root/"evidence"; self.evidence.mkdir()
        checker.save_state(self.evidence,{"nodes":self.NODES})
        self.count=0

    def serve(self, handler=None):
        """A local HTTP server on a free port, standing in for a forward that works."""
        import http.server,threading
        class Answer(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                self.send_response(200); self.send_header("Content-Length","2"); self.end_headers(); self.wfile.write(b"ok")
            def log_message(self,*arguments): pass
        server=http.server.HTTPServer(("127.0.0.1",0),handler or Answer)
        threading.Thread(target=server.serve_forever,daemon=True).start()
        self.addCleanup(server.server_close); self.addCleanup(server.shutdown)
        return server.server_address[1]

    def closed_port(self, count=1):
        """The first of `count` consecutive ports nothing listens on."""
        import socket
        while True:
            with socket.socket() as first:
                first.bind(("127.0.0.1",0)); port=first.getsockname()[1]
                try:
                    others=[socket.socket() for _ in range(count-1)]
                    for offset,other in enumerate(others,1): other.bind(("127.0.0.1",port+offset))
                except OSError:
                    continue
                finally:
                    for other in others: other.close()
                return port

    def fake_limactl(self, code="200", exit_code=0):
        """`limactl shell VM sudo curl …` answering from inside the guest."""
        path=self.root/"limactl"
        path.write_text(f"#!/bin/sh\nprintf '%s' '{code}'\nexit {exit_code}\n")
        path.chmod(0o755)
        return path

    def snapshot(self, ts, kind="settle", **files):
        self.count+=1
        directory=self.evidence/"snapshots"/str(self.count); directory.mkdir(parents=True)
        (directory/"meta.json").write_text(json.dumps({"ts":ts,"kind":kind}))
        for name,content in files.items():
            (directory/name).write_text(content if isinstance(content,str) else json.dumps(content))
        return directory

    def healthy(self, **extra):
        nodes=[{"node_id":n,"state":"alive","is_council":True,"is_leader":n=="rb-a-1"} for n in self.NODES]
        report={"critical":[],"warnings":[],"ok":[{"id":"replicas","description":"replicas"}]}
        return {"wtf.json":report,"nodes.json":nodes,"status.json":[],"http.txt":"200 0.01\n",**extra}

    def evaluate(self, directory):
        import contextlib,io
        with contextlib.redirect_stdout(io.StringIO()):
            code=checker.evaluate(self.evidence,directory)
        return code,json.loads((directory/"verdict.json").read_text())

    def cut(self, node="rb-a-2", name="api", port=29118, host="refused"):
        return [dict(node=node,name=name,port=port,host=host,guest="ok")]

    def test_a_host_probe_names_why_a_forward_failed(self):
        self.assertIsNone(checker.http_probe(self.serve(),"http","/"))
        self.assertEqual(checker.http_probe(self.closed_port(),"http","/"),"refused")
        import http.server
        class Hangup(http.server.BaseHTTPRequestHandler):
            # Lima's gRPC forwarder accepts on the host and hangs up when
            # the guest side is gone.
            def do_GET(self): self.close_connection=True
            def log_message(self,*arguments): pass
        self.assertIn(checker.http_probe(self.serve(Hangup),"http","/"),("empty reply","reset"))

    def test_curl_exit_codes_name_the_error(self):
        self.assertEqual(checker.curl_reason(7),"refused")
        self.assertEqual(checker.curl_reason(28),"timeout")
        self.assertEqual(checker.curl_reason(35),"TLS")
        self.assertEqual(checker.curl_reason(52),"empty reply")
        self.assertEqual(checker.curl_reason(56),"reset")
        self.assertEqual(checker.curl_reason(99),"curl exit 99")
        import contextlib,io
        out=io.StringIO()
        with contextlib.redirect_stdout(out): self.assertEqual(checker.main(["curl-reason","7"]),0)
        self.assertEqual(out.getvalue().strip(),"refused")

    def test_a_refused_forward_on_a_node_healthy_inside_is_recorded_with_both_sides(self):
        snapshot=self.snapshot(1000,kind="light")
        ingress=self.serve(); api=self.closed_port(2)
        code=checker.main(["forwards",str(snapshot),"--limactl",str(self.fake_limactl()),
                           "--api-port",str(api),"--ingress-port",str(ingress),"--registry-port",str(self.closed_port()),
                           "--node","rb-a-1","--node","rb-a-2","--node","rb-a-3","--down","3"])
        self.assertEqual(code,0)
        rows=json.loads((snapshot/"forwards.json").read_text())
        by={(row["node"],row["name"]):row for row in rows}
        self.assertEqual(by[("rb-a-2","api")],dict(node="rb-a-2",name="api",port=api+1,host="refused",guest="ok"))
        self.assertEqual(by[("rb-a-1","registry")]["host"],"refused")
        self.assertEqual(by[("rb-a-1","ingress")]["host"],"ok")
        self.assertIsNone(by[("rb-a-1","ingress")]["guest"])
        self.assertNotIn("rb-a-3",{row["node"] for row in rows})

    def test_a_forward_down_inside_the_guest_too_is_not_a_host_forward(self):
        snapshot=self.snapshot(1000,kind="light")
        api=self.closed_port(2)
        checker.main(["forwards",str(snapshot),"--limactl",str(self.fake_limactl("000",7)),
                      "--api-port",str(api-1),"--ingress-port",str(self.serve()),"--registry-port",str(self.serve()),
                      "--node","rb-a-1","--node","rb-a-2","--node","rb-a-3","--down","1","--down","3"])
        rows=json.loads((snapshot/"forwards.json").read_text())
        self.assertEqual(rows,[dict(node="rb-a-2",name="api",port=api,host="refused",guest="refused")])
        code,verdict=self.evaluate(self.snapshot(1000,**self.healthy(**{"forwards.json":rows})))
        self.assertNotIn("host-forward",[item["check"] for item in verdict["findings"]])

    def test_a_cut_forward_is_one_failure_and_no_settle_timeouts(self):
        # Node 1's ingress forward is cut; the node answers inside, so the
        # podinfo probe's 000 is the forward's, not the product's.
        rows=self.cut("rb-a-1","ingress",28080)
        verdicts=[self.evaluate(self.snapshot(ts,**self.healthy(**{"forwards.json":rows,"http.txt":"000 0.00\n"})))
                  for ts in (1000,1030,1090,1120,1900,4000)]
        for code,verdict in verdicts:
            self.assertEqual(code,0 if verdict is not verdicts[2][1] else 1)
            self.assertTrue(verdict["settle_clean"])
            self.assertNotIn("ingress-http",[item["check"] for item in verdict["findings"] if item["severity"]=="fail"])
        failures=[item for _,verdict in verdicts for item in verdict["findings"]
                  if item["check"]=="host-forward" and item["severity"]=="fail"]
        self.assertEqual(len(failures),1)
        self.assertEqual(failures[0]["target"],"rb-a-1 28080")
        self.assertIn("refused",failures[0]["detail"])

    def test_a_forward_that_comes_back_and_breaks_again_fails_again(self):
        rows=self.cut()
        for ts in (1000,1070): self.evaluate(self.snapshot(ts,**self.healthy(**{"forwards.json":rows})))
        self.evaluate(self.snapshot(1100,**self.healthy(**{"forwards.json":[]})))
        self.evaluate(self.snapshot(5000,**self.healthy(**{"forwards.json":rows})))
        code,verdict=self.evaluate(self.snapshot(5070,**self.healthy(**{"forwards.json":rows})))
        self.assertEqual(code,1)
        self.assertEqual([item["check"] for item in verdict["findings"] if item["severity"]=="fail"],["host-forward"])

    def test_a_cut_registry_forward_excuses_the_registry_check(self):
        rows=self.cut("rb-a-1","registry",25050)
        registry=dict(problems=["push failed: connection refused"],unavailable=[])
        _,verdict=self.evaluate(self.snapshot(1000,**self.healthy(**{"forwards.json":rows,"registry.json":registry})))
        self.assertNotIn("registry",[item["check"] for item in verdict["findings"] if item["severity"]=="fail"])

    def test_host_agent_logs_are_kept_before_lima_truncates_them(self):
        import gzip
        lima=self.root/"lima"; (lima/"rb-a-1").mkdir(parents=True); (lima/"rb-a-2").mkdir()
        (lima/"rb-a-1/ha.stderr.log").write_text("failed to set up forwarding\n")
        self.assertEqual(checker.main(["keep-host-logs",str(self.evidence),"--lima-home",str(lima),"--reason","power-off","rb-a-1","rb-a-2"]),0)
        (lima/"rb-a-1/ha.stderr.log").write_text("second boot\n")
        self.assertEqual(checker.main(["keep-host-logs",str(self.evidence),"--lima-home",str(lima),"--reason","end","rb-a-1"]),0)
        kept={path.name.split("-",1)[1]:gzip.decompress(path.read_bytes()).decode()
              for path in (self.evidence/"lima").glob("rb-a-1/*.ha.stderr.log.gz")}
        self.assertEqual(kept,{"power-off.ha.stderr.log.gz":"failed to set up forwarding\n","end.ha.stderr.log.gz":"second boot\n"})
        self.assertEqual(list((self.evidence/"lima").glob("rb-a-2/*")),[])


class ControllerFallback(unittest.TestCase):
    """#987: the controller moved silently to the next node when node 1
    didn't answer, so a leak-rss on the node that took the load had no
    visible cause."""

    def api(self, failing):
        from unittest.mock import patch
        import subprocess
        folder=tempfile.TemporaryDirectory(); self.addCleanup(folder.cleanup)
        ca=Path(folder.name)/"ca"; ca.write_bytes(b"candidate")
        api=jobs.API([dict(name="n1"),dict(name="n2"),dict(name="n3")],9117,ca,Path(folder.name)/"header")
        def answer(command,input=None,stdout=None,stderr=None,timeout=None):
            target=next(part for part in command if part.startswith("https://"))
            if any(target.startswith("https://"+name+":") for name in failing): return subprocess.CompletedProcess(command,7)
            stdout.write(b'{"ok":true}\n200'); return subprocess.CompletedProcess(command,0)
        patcher=patch("job_soak.subprocess.run",side_effect=answer); patcher.start(); self.addCleanup(patcher.stop)
        return api

    def test_every_fallback_is_counted_per_node(self):
        api=self.api({"n1"})
        for _ in range(3): api.request("GET","/v1/batch/1")
        self.assertEqual(api.fallbacks,dict(missed=dict(n1=3),answered=dict(n2=3)))
        api=self.api(set())
        api.request("GET","/v1/batch/1")
        self.assertEqual(api.fallbacks,dict(missed={},answered={}))

    def test_fallbacks_reach_the_snapshot_report_and_record(self):
        api=self.api({"n1","n2"})
        api.request("GET","/v1/batch/1")
        with tempfile.TemporaryDirectory() as folder:
            campaign=jobs.Campaign(Path(folder)/"jobs",api,[],"token")
            campaign.publish(100)
            snapshot=json.loads((Path(folder)/"jobs/snapshot.json").read_text())
        self.assertEqual(snapshot["api_fallbacks"],dict(missed=dict(n1=1,n2=1),answered=dict(n3=1)))
        report=jobs.report(snapshot)
        self.assertEqual(report["api_fallbacks"],snapshot["api_fallbacks"])
        text="\n".join(checker.job_qualification_lines(report))
        self.assertIn("API fallbacks: n1 missed 1, n2 missed 1; n3 answered 1",text)


class CurrentPairExpectations(unittest.TestCase):
    """#1108: a rotation during the all-off hold moved `current` on while
    every node was down, and the special pushed it on power-on without
    `ingress-expect`, so the checker failed every node for serving it."""

    def functions(self):
        source=(Path(__file__).parent/"qualify-sustained.sh").read_text()
        bodies={}
        for header in re.finditer(r"\n([a-z_]+)\(\) \{\n",source):
            body=source[header.end():]
            bodies[header.group(1)]=body[:body.index("\n}\n")]
        return bodies

    def test_every_push_of_the_current_pair_records_what_the_node_must_serve(self):
        pushes={name:body for name,body in self.functions().items()
                if 'push_pair "$node" "$tls/current/' in body}
        self.assertIn("push_current_pair",pushes)
        for name,body in pushes.items():
            with self.subTest(function=name):
                push=body.index('push_pair "$node" "$tls/current/')
                self.assertIn("check ingress-expect",body[push:])

    def test_power_on_and_the_all_off_special_push_through_the_helper(self):
        bodies=self.functions()
        for name in ("power_on","special_all_off"):
            with self.subTest(function=name):
                self.assertIn("push_current_pair",bodies[name])
                self.assertNotIn('push_pair "$node" "$tls/current/',bodies[name])


class ControllerExitStatus(unittest.TestCase):
    """#1035: macOS runs the harness under /bin/bash 3.2. Once the pid space
    wraps and a later command gets the job controller's old pid, bash 3.2
    drops the controller from its job table without saving its status, so
    finish()'s `wait` failed with "not a child of this shell" (127) and the
    controller's real exit status never reached the verdict. The controller
    now records its own status in jobs/exit-status and finish() reads that."""

    release=Path(__file__).parent
    # The shell macOS runs the harness with; elsewhere this is bash 5.
    bash="/bin/bash" if Path("/bin/bash").exists() else "bash"

    def harness_functions(self, *names):
        source=(self.release/"qualify-sustained.sh").read_text()
        functions=""
        for name in names:
            start=re.search(rf"^{name}\(\) \{{",source,re.M).start()
            # One-line helpers end on their own line; the rest at a closing brace in column 0.
            line=source[start:source.index("\n",start)+1]
            functions+=line if line.rstrip().endswith("}") else source[start:source.index("\n}\n",start)+3]
        return functions

    def finish(self, status=None):
        """Run the harness's finish() for a controller that has already exited
        and that the shell no longer knows: its pid is one this shell never
        started, which is what bash 3.2 sees after the pid is reused."""
        import os,subprocess
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory); evidence=root/"evidence"; (evidence/"jobs").mkdir(parents=True)
            (evidence/"metadata.json").write_text(json.dumps(dict(tier="fast")))
            if status is not None: (evidence/"jobs/exit-status").write_text(f"{status}\n")
            script=("set -euo pipefail\n"+self.harness_functions("meta","meta_get","real_home_state","finish")
                    +"check() { return 0; }\n"
                    +'keep=true; vm=(); home=; real_before=$(real_home_state)\n'
                    # A pid that has exited and was never this shell's child.
                    +"jobs_pid=$(/bin/sh -c 'echo $$')\n"
                    +"trap finish EXIT\n")
            result=subprocess.run([self.bash,"-c",script],stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,
                                  env=dict(PATH=os.environ["PATH"],HOME=directory,evidence=str(evidence),record=str(root/"record.md")))
            return result,json.loads((evidence/"metadata.json").read_text())

    def test_a_controller_that_exited_cleanly_passes_after_the_shell_forgot_it(self):
        result,metadata=self.finish(status=0)
        self.assertNotIn("not a child",result.stderr)
        self.assertEqual(result.returncode,0,result.stderr)
        self.assertNotIn("result",metadata)

    def test_a_controller_that_failed_fails_the_run_and_names_its_status(self):
        result,metadata=self.finish(status=1)
        self.assertNotEqual(result.returncode,0)
        self.assertEqual(metadata["result"],"FAIL")
        self.assertIn("Job controller exited with status 1",metadata["notes"])

    def test_a_controller_that_left_no_status_fails_the_run(self):
        result,metadata=self.finish()
        self.assertNotEqual(result.returncode,0)
        self.assertEqual(metadata["result"],"FAIL")
        self.assertIn("Job controller exited without recording its exit status",metadata["notes"])

    def test_starting_the_controller_clears_a_status_left_by_an_earlier_run(self):
        import os,subprocess
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory); evidence=root/"evidence"; (evidence/"jobs").mkdir(parents=True)
            (evidence/"jobs/exit-status").write_text("0\n")
            controller=root/"controller.py"; controller.write_text("import time\ntime.sleep(5)\n")
            script=(self.harness_functions("start_jobs")+'start_jobs\nkill "$jobs_pid"\n')
            subprocess.run([self.bash,"-c",script],check=True,
                           env=dict(PATH=os.environ["PATH"],evidence=str(evidence),job_controller=str(controller),
                                    job_load="target",target_rate="",api_port="1",ca="ca",limactl="limactl"))
            self.assertFalse((evidence/"jobs/exit-status").exists())

    def test_the_run_command_records_its_exit_status_for_the_harness(self):
        import sys
        from unittest import mock
        with tempfile.TemporaryDirectory() as directory:
            evidence=Path(directory); (evidence/"jobs").mkdir()
            argv=["job_soak.py","run","--evidence",str(evidence),"--api-port","1","--ca","ca","--limactl","limactl"]
            with mock.patch.object(sys,"argv",argv), mock.patch.object(jobs,"run",return_value=1):
                self.assertEqual(jobs.main(),1)
            self.assertEqual((evidence/"jobs/exit-status").read_text().strip(),"1")
            with mock.patch.object(sys,"argv",argv), mock.patch.object(jobs,"run",side_effect=RuntimeError("boom")):
                with self.assertRaises(RuntimeError): jobs.main()
            self.assertEqual((evidence/"jobs/exit-status").read_text().strip(),"1")
            with mock.patch.object(sys,"argv",argv), mock.patch.object(jobs,"run",return_value=0):
                self.assertEqual(jobs.main(),0)
            self.assertEqual((evidence/"jobs/exit-status").read_text().strip(),"0")
