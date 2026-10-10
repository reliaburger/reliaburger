"""The job campaign must preserve identities and fail closed on incomplete evidence."""
import copy
import json
from pathlib import Path
import tempfile
import unittest

import job_soak as jobs


def summary(identity, total=8, succeeded=0, done=False, **extra):
    value = dict(batch_id=identity, name="work", namespace="default", total=total,
                 succeeded=succeeded, failed=0, not_run=0, retried=0,
                 queued=0 if done else total-succeeded, held=0, active_commands=0,
                 done=done, runtime="runc", nodes=[], run={"trigger":{"manual":{"request_id":"request"}}})
    value.update(extra)
    return value


class Counters(unittest.TestCase):
    def test_conserved_counts_and_positive_drain(self):
        jobs.validate_summary(summary(4))
        jobs.validate_summary(summary(4, succeeded=8, done=True))
        self.assertTrue(jobs.drained(summary(4, succeeded=8, done=True)))
        self.assertFalse(jobs.drained(summary(4, succeeded=8, done=True, active_commands=None)))

    def test_rejects_missing_negative_boolean_and_nonconserved_counts(self):
        for change in ({"total":True}, {"failed":-1}, {"succeeded":9}, {"queued":7},
                       {"batch_id":0}, {"done":"true"}, {"active_commands":9}):
            with self.subTest(change=change), self.assertRaises(jobs.InvalidEvidence):
                jobs.validate_summary(summary(4, **change))
        value=summary(4); del value["retried"]
        with self.assertRaises(jobs.InvalidEvidence): jobs.validate_summary(value)

    def test_same_run_counters_cannot_regress_or_change_total(self):
        old=summary(4,succeeded=3)
        for value in (summary(4,succeeded=2),summary(4,total=9),summary(5)):
            with self.assertRaises(jobs.InvalidEvidence): jobs.validate_summary(value,old)

    def test_verified_activity_cannot_exceed_caller_or_per_profile_cap(self):
        node={"node":"node-a","slots":2,"counters":{"running":2,"active_commands":3}}
        with self.assertRaises(jobs.InvalidEvidence):
            jobs.validate_summary(summary(4,nodes=[node],active_commands=3), concurrency=2)


class FakeAPI:
    def __init__(self): self.posts=[]; self.runs={}; self.lost=False
    def request(self, method, path, body=None):
        if method=="POST" and path=="/v1/jobs/runs":
            self.posts.append(copy.deepcopy(body))
            key=(body["name"],body["request_id"])
            identity=self.runs.setdefault(key,len(self.runs)+1)
            if self.lost:
                self.lost=False; raise jobs.Unavailable("reply lost after commit")
            return {"batch_id":identity}
        raise AssertionError((method,path))


class Admission(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory(); self.addCleanup(self.temp.cleanup)
        self.path=Path(self.temp.name)/"jobs"; self.api=FakeAPI()
    def campaign(self):
        return jobs.Campaign(self.path,self.api,["http://10.0.0.1:8189"],"test-token")
    def test_lost_submission_reply_reuses_exact_durable_request_after_restart(self):
        first=self.campaign(); intent=first.prepare("runc","short","work",100)
        self.api.lost=True
        with self.assertRaises(jobs.Unavailable): first.admit(intent)
        second=self.campaign(); recovered=second.state["active"]["runc:short"]
        second.admit(recovered)
        self.assertEqual(self.api.posts[0],self.api.posts[1])
        self.assertEqual(len(self.api.runs),1)
        self.assertEqual(recovered["id"],1)
        self.assertNotIn("test-token",self.path.joinpath("state.json").read_text())
    def test_one_intent_per_profile_bounds_pending_work(self):
        c=self.campaign(); c.prepare("runc","short","work",100)
        with self.assertRaises(jobs.InvalidEvidence): c.prepare("runc","short","work",101)
        self.assertEqual(len(c.state["active"]),1)
    def test_two_profiles_share_the_per_node_budget_in_every_runtime(self):
        c=self.campaign()
        for mode in jobs.MODES:
            a=c.prepare(mode,"short","work",100)
            b=c.prepare(mode,"long","work",100)
            ra=c.body(a)["definition"]; rb=c.body(b)["definition"]
            self.assertNotEqual(ra["template"]["memory"],rb["template"]["memory"])
            self.assertEqual(ra["tasks"]["per_node_concurrency"]+rb["tasks"]["per_node_concurrency"],jobs.CAPS[mode])
            self.assertEqual(ra["template"]["runtime"],mode)
            self.assertEqual("image" in ra["template"],mode!="process")
            self.assertEqual("exec" in ra["template"],mode=="process")
    def test_resume_rejects_different_cluster_or_audit_identity(self):
        c=self.campaign(); c.prepare("runc","short","work",100)
        with self.assertRaises(jobs.InvalidEvidence):
            jobs.Campaign(self.path,self.api,["http://10.0.0.2:8189"],"test-token")
        with self.assertRaises(jobs.InvalidEvidence):
            jobs.Campaign(self.path,self.api,["http://10.0.0.1:8189"],"changed-token")
    def test_fenced_replay_requires_two_observations_of_the_same_fingerprint(self):
        c=self.campaign(); intent=c.prepare("process","long","work",100)
        intent["id"]=1
        value=summary(1,status="Unknown",unknown_owners=[{"node":"n","grant_digest":"a"*64}])
        self.assertEqual(c.replay_decisions(intent,value),[])
        self.assertEqual(c.replay_decisions(intent,value),value["unknown_owners"])
        value["unknown_owners"][0]["grant_digest"]="b"*64
        self.assertEqual(c.replay_decisions(intent,value),[])
    def test_expected_failure_cannot_excuse_spawn_failure_or_unrelated_work(self):
        with self.assertRaises(jobs.InvalidEvidence):
            jobs.judge_terminal("failure",summary(1,total=1,failed=1,done=True),[{"index":0,"exit_code":None,"attempts":2}])
        with self.assertRaises(jobs.InvalidEvidence):
            jobs.judge_terminal("work",summary(1,total=1,failed=1,done=True),[])
        jobs.judge_terminal("failure",summary(1,total=1,failed=1,done=True),[{"index":0,"exit_code":7,"attempts":2}])


class Audit(unittest.TestCase):
    def test_effects_are_durable_idempotent_and_bounded(self):
        with tempfile.TemporaryDirectory() as folder:
            path=Path(folder)/"effects.sqlite"
            ledger=jobs.EffectLedger(path,limit=2)
            ledger.accept("run-a",0,100); ledger.accept("run-a",0,101)
            self.assertEqual(ledger.rows("run-a"),[[0,2]])
            reopened=jobs.EffectLedger(path,limit=2)
            self.assertEqual(reopened.rows("run-a"),[[0,2]])
            reopened.accept("run-a",1,102)
            with self.assertRaises(jobs.InvalidEvidence): reopened.accept("run-b",0,103)
            reopened.expire(2000,retention=900)
            reopened.accept("run-b",0,2000)
    def test_audit_checks_union_and_records_repeated_attempts_without_double_credit(self):
        result=jobs.audit_effects(3,[[[0,2],[1,1]],[[1,1],[2,1]]])
        self.assertEqual(result,{"effects":3,"attempts":5,"repeated_attempts":2})
        for ledgers in ([[[0,1],[1,1]]],[[[0,1],[1,1],[2,1],[3,1]]]):
            with self.assertRaises(jobs.InvalidEvidence): jobs.audit_effects(3,ledgers)


class Gate(unittest.TestCase):
    def test_absent_stale_or_errored_job_evidence_fails(self):
        self.assertTrue(jobs.findings(None,100,False))
        self.assertTrue(jobs.findings({"heartbeat":1,"errors":[],"active":[],"progress":{}},200,False))
        self.assertTrue(jobs.findings({"heartbeat":200,"errors":["bad accounting"],"active":[],"progress":{}},200,True))
    def test_outage_grace_does_not_hide_controller_loss_or_reset_accounting(self):
        value={"heartbeat":200,"errors":[],"active":[],"progress":{m:{"last_progress":1} for m in jobs.MODES}}
        self.assertEqual(jobs.findings(value,200,True),[])
        self.assertTrue(jobs.findings(value,400,True))
        self.assertTrue(jobs.findings(value,200,False))
    def test_empty_coverage_never_passes_final_gate(self):
        self.assertTrue(jobs.coverage_failures({}))
    def test_quarantined_or_unknown_activity_is_not_positive_drain(self):
        self.assertFalse(jobs.drained(summary(1,done=True,held=1)))
        self.assertFalse(jobs.drained(summary(1,status="Unknown")))


class Lifecycle(unittest.TestCase):
    def test_controller_resolves_lost_admission_and_drains_that_same_run(self):
        class API(FakeAPI):
            def request(self,method,path,body=None,raw=False):
                if path=="/v1/jobs/runs": return super().request(method,path,body)
                identity=int(path.split("/")[3])
                request=next(v for v in self.posts if self.runs[(v["name"],v["request_id"])]==identity)
                if path.endswith("/cancel"): self.cancelled=identity; return {}
                self.assert_identity=identity
                count=request["definition"]["tasks"]["count"]
                value=summary(identity,total=count,done=True,not_run=count,name=request["name"])
                value["queued"]=0
                value.update(runtime=request["definition"]["template"]["runtime"],cpu_request_millicores=25,
                             memory_request_bytes=32<<20)
                return value
        with tempfile.TemporaryDirectory() as directory:
            api=API(); c=jobs.Campaign(directory,api,[],"token")
            c.prepare("process","short","work",100); api.lost=True
            self.assertFalse(c.step(100,stop=True))
            c=jobs.Campaign(directory,api,[],"token")
            self.assertFalse(c.step(101,stop=True))
            self.assertTrue(c.step(102,stop=True))
            self.assertEqual(len(api.runs),1)
            self.assertEqual(api.cancelled,1)
            self.assertFalse(c.state["errors"])
    def test_timeout_requires_observed_launch_duration_and_timeout_outcome(self):
        value=summary(1,total=1,done=True,failed=1)
        good=dict(index=0,attempts=1,run_ms=3050,started_effect=True)
        jobs.judge_terminal("timeout",value,[good])
        for change in ({"started_effect":False},{"run_ms":0},{"exit_code":-9},{"attempts":2}):
            with self.subTest(change=change),self.assertRaises(jobs.InvalidEvidence):
                jobs.judge_terminal("timeout",value,[dict(good,**change)])
    def test_fault_marker_refuses_old_activity_even_with_a_fresh_heartbeat(self):
        with tempfile.TemporaryDirectory() as directory:
            path=Path(directory)
            active=dict(mode="runc",id=1,observed_at=10,previous={"nodes":[{"node":"n","counters":{"active_commands":1}}]})
            jobs.atomic(path/"snapshot.json",dict(heartbeat=100,active=[active]))
            jobs.mark(path,"n","0:leader-kill",100)
            self.assertEqual(json.loads((path/"faults.jsonl").read_text())["runs"],[])
            active["observed_at"]=100
            jobs.atomic(path/"snapshot.json",dict(heartbeat=100,active=[active]))
            jobs.mark(path,"n","0:leader-kill",100)
            self.assertEqual(json.loads((path/"faults.jsonl").read_text().splitlines()[1])["runs"],[dict(mode="runc",id=1)])

    def test_limit_kill_respects_the_actual_runtime_signal_encoding(self):
        value=summary(1,total=1,failed=1,done=True)
        for code in (-9,137):
            jobs.judge_terminal("limit",value,[dict(index=0,attempts=1,exit_code=code)])
        for code in (None,1,7,0):
            with self.assertRaises(jobs.InvalidEvidence):
                jobs.judge_terminal("limit",value,[dict(index=0,attempts=1,exit_code=code)])

    def test_fresh_fault_proof_requires_the_current_boot_and_actual_process_start(self):
        owner=dict(id="instance",generation="a"*32,boot="boot-a",run=7,index=0,name="run-a",ticks=[900])
        def sample(boot="boot-a",ticks=900,ts=100):
            return dict(node="n",ts=ts,owners=[owner],starts={"run-a":[dict(index=0,boot=boot,ticks=ticks,ts=100)]})
        self.assertEqual(len(jobs.match_activity([sample()],100)),1)
        self.assertEqual(jobs.match_activity([sample(boot="previous")],100),[])
        self.assertEqual(jobs.match_activity([sample(ticks=899)],100),[])
        self.assertEqual(jobs.match_activity([sample(ts=70)],100),[])

    def test_operator_decision_intent_survives_a_lost_reply_without_new_fence(self):
        from unittest.mock import Mock
        with tempfile.TemporaryDirectory() as folder:
            api=FakeAPI(); api.request=Mock(side_effect=[jobs.Unavailable("reply lost"),{}])
            campaign=jobs.Campaign(folder,api,[],"token")
            owner=dict(node="n",grant_digest="a"*64)
            with self.assertRaises(jobs.Unavailable): campaign.acknowledge(7,owner,"known fixture",100)
            resumed=jobs.Campaign(folder,api,[],"token")
            resumed.acknowledge(7,owner,"known fixture",101)
            self.assertEqual(api.request.call_args_list[0],api.request.call_args_list[1])
            self.assertEqual(resumed.state["totals"]["operator_replay_decisions"],1)
            self.assertEqual(len(resumed.state["replay_decisions"]),1)


class DrainBudget(unittest.TestCase):
    def test_expired_drain_budget_does_not_start_another_request(self):
        from unittest.mock import patch
        with tempfile.TemporaryDirectory() as folder:
            ca=Path(folder)/"ca"; ca.write_bytes(b"candidate")
            api=jobs.API([dict(name="n")],9117,ca,Path(folder)/"header")
            api.deadline=100
            with patch("job_soak.time.monotonic",return_value=101),patch("job_soak.subprocess.run") as request:
                with self.assertRaises(jobs.Unavailable): api.request("POST","/cancel")
                request.assert_not_called()
            with patch("job_soak.time.monotonic",return_value=99):
                self.assertEqual(api.remaining(7),1)

    def test_resume_preserves_counts_and_limits_progress_grace(self):
        from unittest.mock import patch
        with tempfile.TemporaryDirectory() as folder:
            c=jobs.Campaign(folder,FakeAPI(),[],"token")
            c.state["progress"]={m:dict(last_progress=1,succeeded=9,retried=2) for m in jobs.MODES}; c.save()
            with patch("job_soak.time.time",return_value=200):
                c=jobs.Campaign(folder,FakeAPI(),[],"token"); c.publish(200)
            snapshot=json.loads(Path(folder,"snapshot.json").read_text())
            self.assertEqual(jobs.findings(snapshot,200,False),[])
            self.assertEqual(snapshot["progress"]["runc"]["succeeded"],9)
            snapshot["heartbeat"]=380
            self.assertTrue(jobs.findings(snapshot,380,False))
            snapshot["heartbeat"]=1
            self.assertTrue(jobs.findings(snapshot,200,False))
            c.state["stopped"]=True; c.save()
            with self.assertRaises(jobs.InvalidEvidence): jobs.Campaign(folder,FakeAPI(),[],"token")


class UnexpectedFailure(unittest.TestCase):
    def test_final_cancellation_cannot_hide_an_ordinary_failure(self):
        class API(FakeAPI):
            def request(self,method,path,body=None):
                if path.endswith("/cancel"): return {}
                return summary(7,total=2,failed=1,not_run=1,done=True,name="fixture",runtime="process",cpu_request_millicores=25,memory_request_bytes=32<<20)
        with tempfile.TemporaryDirectory() as folder:
            c=jobs.Campaign(folder,API(),[],"token")
            intent=c.prepare("process","short","work",100); intent.update(id=7,name="fixture")
            self.assertFalse(c.step(101,stop=True))
            self.assertTrue(any("unexpected terminal failure" in error for error in c.state["errors"]))
