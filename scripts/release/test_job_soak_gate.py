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
