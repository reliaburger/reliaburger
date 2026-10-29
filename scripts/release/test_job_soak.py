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
        # The node's own counters name where and by how much.
        with self.assertRaisesRegex(jobs.InvalidEvidence,"cap on node-a: 3 verified, 2 callers, cap 2"):
            jobs.validate_summary(summary(4,nodes=[node]), concurrency=2)


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
    def drain_one_run(self, fault_while_active):
        """Admit one run, maybe through a fault window, and drain it with a known start lag."""
        lag=[0]*16; lag[9]=5
        class API(FakeAPI):
            def request(self,method,path,body=None,raw=False):
                if path=="/v1/jobs/runs": return super().request(method,path,body)
                identity=int(path.split("/")[3])
                request=next(v for v in self.posts if self.runs[(v["name"],v["request_id"])]==identity)
                if path.endswith("/cancel"): return {}
                count=request["definition"]["tasks"]["count"]
                value=summary(identity,total=count,done=True,not_run=count,name=request["name"],
                              nodes=[dict(node="rb-a-1",counters=dict(running=0,start_lag_ms_counts=lag))])
                value.update(runtime=request["definition"]["template"]["runtime"],cpu_request_millicores=25,
                             memory_request_bytes=32<<20)
                return value
        with tempfile.TemporaryDirectory() as directory:
            c=jobs.Campaign(directory,API(),[],"token")
            c.prepare("process","short","work",100)
            self.assertFalse(c.step(100,fault=fault_while_active,stop=True))
            self.assertTrue(c.step(101,stop=True))
            self.assertFalse(c.state["errors"])
            c.state.update(stopped=True,stop_requested_at=101); c.publish(101)
            return lag,c.state["metrics"],jobs.report(json.loads((Path(directory)/"snapshot.json").read_text()))["m8"]
    def test_a_run_that_saw_a_fault_window_counts_for_m8_but_not_the_quiet_start_lag(self):
        # The gated job-start SLO is a quiet-cluster number (#1438).
        lag,metrics,m8=self.drain_one_run(fault_while_active=True)
        self.assertEqual(metrics["start_lag_ms_counts"],lag)
        self.assertEqual(metrics["quiet_start_lag_ms_counts"],[0]*16)
        self.assertEqual(m8["start_lag_samples"],5)
        self.assertEqual(m8["quiet_start_lag_samples"],0)
        self.assertIsNone(m8["quiet_p99_start_lag_ms"])
    def test_a_run_outside_every_fault_window_counts_for_both(self):
        lag,metrics,m8=self.drain_one_run(fault_while_active=False)
        self.assertEqual(metrics["quiet_start_lag_ms_counts"],lag)
        self.assertEqual(m8["quiet_start_lag_samples"],5)
        self.assertEqual(m8["quiet_p99_start_lag_ms"],m8["p99_start_lag_ms"])
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
            # The run drains, so the campaign can stop, but the failure stays
            # in the evidence and fails the final gate.
            self.assertTrue(c.step(101,stop=True))
            self.assertTrue(any("unexpected terminal failure" in error for error in c.state["errors"]))
            c.state["stopped"]=True; c.publish(102)
            final=json.loads(Path(folder,"snapshot.json").read_text())
            self.assertTrue(any("unexpected terminal failure" in problem for problem in jobs.coverage_failures(final)))

    def test_a_failed_run_is_reported_once_with_its_cause_and_still_drains(self):
        # V02 candidate 3: run 26 failed one task at about 05:01 and every
        # later poll raised again, so the run never left the campaign, the
        # final drain ran out of time and the controller's heartbeat went stale.
        class API(FakeAPI):
            cancelled=set()
            def request(self,method,path,body=None,raw=False):
                if path=="/v1/jobs/runs": return super().request(method,path,body)
                identity=int(path.split("/")[3])
                if path.endswith("/cancel"): self.cancelled.add(identity); return {}
                if identity!=7:
                    # The campaign's other runs: clean, and drained once cancelled.
                    request=next(v for v in self.posts if self.runs[(v["name"],v["request_id"])]==identity)
                    template=request["definition"]["template"]; total=request["definition"]["tasks"]["count"]
                    drained=dict(done=True,queued=0,not_run=total) if identity in self.cancelled else {}
                    return summary(identity,total=total,name=request["name"],**drained,
                                   runtime=template["runtime"],cpu_request_millicores=25,memory_request_bytes=int(template["memory"][:-2])<<20)
                if "/results" in path:
                    self.results=path
                    return dict(batch_id=7,rows=[dict(index=5,attempts=1,succeeded=False,run_ms=3)],truncated=False,unreachable=[])
                if path.endswith("/tasks/5/logs"):
                    assert raw; return "cannot pull image: registry leader refused proposal\nsecond line\n"
                failed=dict(failed=1,queued=6,succeeded=1)
                if 7 in self.cancelled: failed=dict(failed=1,not_run=6,succeeded=1,queued=0,done=True)
                return summary(7,**failed,name="fixture",runtime="runc",cpu_request_millicores=25,memory_request_bytes=32<<20)
        with tempfile.TemporaryDirectory() as folder:
            api=API(); c=jobs.Campaign(folder,api,[],"token")
            intent=c.prepare("runc","short","work",100); intent.update(id=7,name="fixture")
            c.step(101); c.step(102)
            self.assertEqual(api.results,"/v1/batch/7/results?failed=true&limit=1")
            self.assertEqual(c.state["errors"],["runc short work (7): unexpected terminal failure in ordinary campaign work: "
                                                "task 5 after 1 attempt(s), no exit code: cannot pull image: registry leader refused proposal"])
            self.assertEqual(c.state["active"]["runc:short"]["previous"]["failed"],1)
            self.assertTrue(c.step(103,stop=True))
            self.assertNotIn("runc:short",c.state["active"])
            self.assertEqual(len(c.state["errors"]),1)
            self.assertFalse(c.state["coverage"]["runc"].get("reuse"))

    def test_a_failure_whose_cause_cannot_be_read_is_still_reported(self):
        class API(FakeAPI):
            def request(self,method,path,body=None,raw=False):
                if "/results" in path: raise jobs.Unavailable("no node answered")
                return summary(7,failed=1,queued=7,name="fixture",runtime="runc",cpu_request_millicores=25,memory_request_bytes=32<<20)
        with tempfile.TemporaryDirectory() as folder:
            c=jobs.Campaign(folder,API(),[],"token")
            intent=c.prepare("runc","short","work",100); intent.update(id=7,name="fixture")
            c.step(101)
            self.assertEqual(c.state["errors"],["runc short work (7): unexpected terminal failure in ordinary campaign work"])


class RefusedSummary(unittest.TestCase):
    """#968: the controller refused a summary and kept only the message, so
    nobody could see which node's numbers broke the invariant."""

    class API(FakeAPI):
        def request(self,method,path,body=None,raw=False):
            if path=="/v1/jobs/runs": return super().request(method,path,body)
            nodes=[dict(node="node-1",slots=4,counters=dict(running=1,active_commands=1)),
                   dict(node="node-3",slots=4,counters=dict(running=1,active_commands=1))]
            return summary(7,succeeded=7,queued=0,held=1,active_commands=2,nodes=nodes,name="fixture",
                           runtime="shared-runc",cpu_request_millicores=25,memory_request_bytes=64<<20)

    def test_a_refused_summary_is_saved_next_to_its_error(self):
        with tempfile.TemporaryDirectory() as folder:
            c=jobs.Campaign(folder,self.API(),[],"token")
            intent=c.prepare("shared-runc","long","work",100); intent.update(id=7,name="fixture")
            error="shared-runc long work (7): invalid verified active commands"
            def saved():
                records=[json.loads(path.read_text()) for path in sorted(Path(folder,"refused").iterdir())]
                return [record for record in records if record["error"]==error]
            c.step(101)
            self.assertEqual(c.state["errors"],[error])
            self.assertEqual(len(saved()),1)
            record=saved()[0]
            self.assertEqual(record["summary"]["active_commands"],2)
            self.assertEqual(record["summary"]["nodes"][1]["node"],"node-3")
            # The same refusal on later polls keeps the first body only.
            c.step(102); c.step(103)
            self.assertEqual(len(saved()),1)
            self.assertEqual(c.state["errors"].count(error),1)


class ExpectedFailureResult(unittest.TestCase):
    """V02 candidate 6, run 15: the shared-runc short failure fixture drained
    four seconds after the soak killed the Bun on the node that held its one
    result row, so the leader's results page named that node unreachable. The
    harness recorded a permanent error, and every later settle failed on it
    (#954)."""
    class API(FakeAPI):
        def __init__(self, unreachable_pages):
            super().__init__(); self.unreachable_pages=unreachable_pages; self.result_calls=0
        def request(self,method,path,body=None,raw=False):
            if path=="/v1/jobs/runs": return super().request(method,path,body)
            if "/results" in path:
                self.result_calls+=1
                if self.result_calls<=self.unreachable_pages:
                    return dict(batch_id=15,rows=[],truncated=False,unreachable=["node-1"])
                return dict(batch_id=15,rows=[dict(index=0,attempts=2,exit_code=7,succeeded=False,run_ms=3)],
                            truncated=False,unreachable=[])
            return summary(15,total=1,failed=1,queued=0,done=True,name="fixture",runtime="shared-runc",
                           cpu_request_millicores=25,memory_request_bytes=self.memory)
    def campaign(self, folder, api):
        c=jobs.Campaign(folder,api,[],"token")
        intent=c.prepare("shared-runc","short","failure",100); intent.update(id=15,name="fixture")
        api.memory=int(c.body(intent)["definition"]["template"]["memory"][:-2])<<20
        # Only the fixture under test: no other profile's run is admitted.
        c.prepare=lambda *args: None
        return c

    def test_a_result_held_on_a_restarting_node_is_read_again_later(self):
        with tempfile.TemporaryDirectory() as folder:
            api=self.API(unreachable_pages=1); c=self.campaign(folder,api)
            c.step(101)
            self.assertIn("shared-runc:short",c.state["active"])
            self.assertEqual(c.state["errors"],[])
            c.step(140)
            self.assertNotIn("shared-runc:short",c.state["active"])
            self.assertEqual(c.state["errors"],[])
            self.assertTrue(c.state["coverage"]["shared-runc"]["failure"])

    def test_a_result_unreachable_past_the_grace_fails_by_node(self):
        with tempfile.TemporaryDirectory() as folder:
            api=self.API(unreachable_pages=1000); c=self.campaign(folder,api)
            c.step(101)
            c.step(101+jobs.RESULT_UNREACHABLE_GRACE-1)
            self.assertEqual(c.state["errors"],[])
            c.step(101+jobs.RESULT_UNREACHABLE_GRACE)
            self.assertEqual(c.state["errors"],[f"shared-runc short failure (15): failure result incomplete: "
                                                f"node-1 unreachable for {jobs.RESULT_UNREACHABLE_GRACE} s"])

    def test_a_truncated_single_result_page_still_fails_at_once(self):
        class API(self.API):
            def request(self,method,path,body=None,raw=False):
                if "/results" in path: return dict(batch_id=15,rows=[],truncated=True,unreachable=[])
                return super().request(method,path,body,raw)
        with tempfile.TemporaryDirectory() as folder:
            c=self.campaign(folder,API(0)); c.step(101)
            self.assertEqual(c.state["errors"],["shared-runc short failure (15): failure result incomplete"])


class TargetLoad(unittest.TestCase):
    """#686: the #668 throughput target, retry causes and the M8 report."""
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory(); self.addCleanup(self.temp.cleanup)
        self.path=Path(self.temp.name)/"jobs"; self.api=FakeAPI()
    def campaign(self, load="target", rate=jobs.DEFAULT_TARGET_RATE):
        return jobs.Campaign(self.path,self.api,[],"token",load=load,target_rate=rate)

    def test_target_load_turns_fast_short_runs_into_million_task_arrays(self):
        c=self.campaign()
        for mode in ("shared-runc","process"):
            tasks=c.body(c.prepare(mode,"short","work",100))["definition"]["tasks"]
            self.assertEqual((tasks["count"],tasks["chunk_size"]),(jobs.MILLION,1000))
            self.assertEqual(tasks["per_node_concurrency"],jobs.TARGET_CAPS[mode])
        # Fresh containers can't approach the rate, so they keep the soak mix.
        self.assertEqual(c.body(c.prepare("runc","short","work",100))["definition"]["tasks"]["count"],262144)
        # Probes and long runs are unchanged.
        self.assertEqual(c.body(c.prepare("process","long","work",100))["definition"]["tasks"]["count"],512)
        calibration=jobs.Campaign(Path(self.temp.name)/"other",self.api,[],"token")
        self.assertEqual(calibration.body(calibration.prepare("process","short","work",100))["definition"]["tasks"]["count"],262144)

    def test_target_load_refuses_a_rate_below_the_daily_floor(self):
        with self.assertRaises(jobs.InvalidEvidence): self.campaign(rate=1000)
        with self.assertRaises(jobs.InvalidEvidence): self.campaign(rate=None)

    def test_resume_refuses_a_changed_load_or_rate(self):
        self.campaign()
        with self.assertRaises(jobs.InvalidEvidence): self.campaign(load="calibration",rate=None)
        with self.assertRaises(jobs.InvalidEvidence): self.campaign(rate=2000)
        self.campaign()

    def test_retry_causes_must_sum_to_retries_and_never_regress(self):
        causes=dict(exit_code=1,signal=1,timeout=0,unknown=0,launch=0,unrecorded=0)
        jobs.validate_summary(summary(4,retried=2,retry_causes=causes))
        with self.assertRaises(jobs.InvalidEvidence):
            jobs.validate_summary(summary(4,retried=3,retry_causes=causes))
        old=summary(4,retried=2,retry_causes=dict(causes,signal=1))
        with self.assertRaises(jobs.InvalidEvidence):
            jobs.validate_summary(summary(4,retried=2,retry_causes=dict(causes,signal=0,exit_code=2)),old)

    def test_a_launch_retry_validates_and_counts(self):
        # V02 candidate 4 (#942): an image pull hit a registry rate limit and
        # Bun retried the launch. The summary said retried 1, launch 1.
        causes=dict(exit_code=0,signal=0,timeout=0,unknown=0,launch=0,unrecorded=0)
        old=summary(2,total=512,succeeded=187,retried=0,retry_causes=causes)
        new=summary(2,total=512,succeeded=187,retried=1,retry_causes=dict(causes,launch=1))
        self.assertEqual(jobs.validate_summary(new,old)["retried"],1)
        with self.assertRaisesRegex(jobs.InvalidEvidence,"regressed"):
            jobs.validate_summary(summary(2,total=512,succeeded=187,retried=1,retry_causes=dict(causes,exit_code=1)),new)

    def test_an_unknown_retry_cause_fails_loudly_by_name(self):
        causes=dict(exit_code=0,signal=0,timeout=0,unknown=0,launch=0,unrecorded=0,quota=1)
        with self.assertRaisesRegex(jobs.InvalidEvidence,"unknown retry cause.*quota.*CAUSES"):
            jobs.validate_summary(summary(4,retried=1,retry_causes=causes))

    def test_a_missing_retry_cause_is_refused(self):
        causes=dict(exit_code=1,signal=0,timeout=0,unknown=0,unrecorded=0)
        with self.assertRaisesRegex(jobs.InvalidEvidence,"launch"):
            jobs.validate_summary(summary(4,retried=1,retry_causes=causes))

    def test_harness_causes_match_the_product_retry_causes(self):
        # The product's RetryCauses fields are the wire names (serde keeps
        # them). A new cause there must be added to CAUSES here, or every
        # summary that uses it is refused and the campaign wedges (#942).
        import re
        source=(Path(__file__).resolve().parents[2]/"src/meat/task_array_state.rs").read_text()
        body=re.search(r"pub struct RetryCauses \{(?P<body>.*?)\n\}",source,re.S)
        self.assertIsNotNone(body)
        fields=re.findall(r"^\s*pub (\w+): u64,",body.group("body"),re.M)
        self.assertEqual(sorted(fields),sorted(jobs.CAUSES))
        self.assertIn("launch",jobs.CAUSES)

    def test_launch_retries_accumulate_in_the_progress_totals(self):
        class API(FakeAPI):
            def __init__(self): super().__init__(); self.polls={}
            def request(self,method,path,body=None):
                if path=="/v1/jobs/runs": return super().request(method,path,body)
                identity=int(path.split("/")[3])
                request=next(v for v in self.posts if self.runs[(v["name"],v["request_id"])]==identity)
                retried=self.polls[identity]=self.polls.get(identity,0)+1
                template=request["definition"]["template"]
                return summary(identity,total=request["definition"]["tasks"]["count"],succeeded=retried,retried=retried,
                               retry_causes=dict(exit_code=0,signal=0,timeout=0,unknown=0,launch=retried,unrecorded=0),
                               name=request["name"],runtime=template["runtime"],cpu_request_millicores=25,
                               memory_request_bytes=int(template["memory"][:-2])<<20)
        api=API(); c=jobs.Campaign(self.path,api,[],"token",load="target",target_rate=jobs.DEFAULT_TARGET_RATE)
        for now in (100,101,102,103): c.step(now)
        self.assertFalse(c.state["errors"])
        self.assertEqual(c.state["progress"]["process"]["retry_causes"]["launch"],6)
        c.state.update(stopped=True,stop_requested_at=103); c.publish(103)
        report=jobs.report(json.loads((self.path/"snapshot.json").read_text()))
        self.assertEqual(report["retry_causes"]["process"]["launch"],6)

    def test_retry_causes_accumulate_per_runtime_across_polls(self):
        class API(FakeAPI):
            def __init__(self): super().__init__(); self.polls={}
            def request(self,method,path,body=None):
                if path=="/v1/jobs/runs": return super().request(method,path,body)
                identity=int(path.split("/")[3])
                request=next(v for v in self.posts if self.runs[(v["name"],v["request_id"])]==identity)
                retried=self.polls[identity]=self.polls.get(identity,0)+1
                template=request["definition"]["template"]
                return summary(identity,total=request["definition"]["tasks"]["count"],succeeded=retried,retried=retried,
                               retry_causes=dict(exit_code=retried,signal=0,timeout=0,unknown=0,launch=0,unrecorded=0),
                               name=request["name"],runtime=template["runtime"],cpu_request_millicores=25,
                               memory_request_bytes=int(template["memory"][:-2])<<20)
        api=API(); c=jobs.Campaign(self.path,api,[],"token",load="target",target_rate=jobs.DEFAULT_TARGET_RATE)
        for now in (100,101,102,103): c.step(now)
        self.assertFalse(c.state["errors"])
        # Both process runs reach three retries over three polls.
        self.assertEqual(c.state["progress"]["process"]["retried"],6)
        self.assertEqual(c.state["progress"]["process"]["retry_causes"]["exit_code"],6)
        self.assertEqual(c.state["progress"]["process"]["retry_causes"]["signal"],0)

    def test_report_records_m8_metrics_and_gates_only_the_target_rate(self):
        c=self.campaign()
        c.state["started"]=1000
        for mode in jobs.MODES: c.state["progress"][mode]["succeeded"]=0
        c.state["progress"]["process"]["succeeded"]=1390*3600
        lag=[0]*16; lag[3]=990; lag[9]=10
        c.record_metrics(dict(total=jobs.MILLION,succeeded=jobs.MILLION,elapsed_seconds=540,state_bytes=1200,
                              nodes=[dict(counters=dict(start_lag_ms_counts=lag))]))
        c.sample_raft(dict(last_log_index=100,leader="a"),1000)
        c.sample_raft(dict(last_log_index=3700,leader="b"),4600)
        c.state.update(stopped=True,stop_requested_at=4600); c.publish(4600)
        snapshot=json.loads((self.path/"snapshot.json").read_text())
        result=jobs.report(snapshot)
        self.assertEqual(result["accepted_successes_per_second"],1390)
        self.assertTrue(result["target_met"])
        m8=result["m8"]
        self.assertEqual(m8["million_task_array_wall_seconds"]["max"],540)
        self.assertEqual(m8["p99_start_lag_ms"],8)
        self.assertEqual(m8["raft_entries_per_second"],1.0)
        self.assertEqual(m8["raft_leaders"],["a","b"])
        self.assertEqual(m8["raft_state_bytes_max"],1200)
        self.assertNotIn("accepted rate",";".join(jobs.coverage_failures(snapshot)))
        snapshot["progress"]["process"]["succeeded"]=1000*3600
        self.assertFalse(jobs.report(snapshot)["target_met"])
        self.assertIn("below the 1390/s target",";".join(jobs.coverage_failures(snapshot)))
        # The calibration load records the same metrics but never gates the rate.
        snapshot.update(load="calibration",target_rate=None)
        self.assertIsNone(jobs.report(snapshot)["target_met"])
        self.assertNotIn("accepted rate",";".join(jobs.coverage_failures(snapshot)))

    def test_node_fault_kinds_are_required_only_when_enabled(self):
        coverage={m:{**{item:True for item in jobs.REQUIRED},**{"fault-kind:"+k:True for k in jobs.FAULT_KINDS}} for m in jobs.MODES}
        snapshot=dict(stopped=True,active=[],errors=[],coverage=coverage)
        self.assertEqual(jobs.coverage_failures(snapshot),[])
        missing=jobs.coverage_failures(snapshot,(),jobs.FAULT_KINDS+jobs.NODE_FAULT_KINDS)
        self.assertEqual(len(missing),3)
        self.assertIn("active disk-full overlap",missing[0])

    def test_percentile_reports_the_bucket_upper_bound(self):
        self.assertIsNone(jobs.percentile([0]*16,0.99))
        counts=[0]*16; counts[15]=1
        self.assertIsNone(jobs.percentile(counts,0.99))
        counts=[0]*16; counts[0]=99; counts[10]=1
        self.assertEqual(jobs.percentile(counts,0.99),1)
        self.assertEqual(jobs.percentile(counts,1.0),1024)


class DayLoad(unittest.TestCase):
    """#1032: 2 billion host-job tasks in one day on three nodes."""
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory(); self.addCleanup(self.temp.cleanup)
        self.path=Path(self.temp.name)/"jobs"; self.api=FakeAPI()
    def campaign(self, rate=jobs.DAY_DEFAULT_RATE):
        # A fresh directory each time: one directory is one campaign.
        self.path=Path(self.temp.name)/("jobs-%d" % len(list(Path(self.temp.name).iterdir())))
        return jobs.Campaign(self.path,self.api,[],"token",load="day",target_rate=rate)
    def day_snapshot(self, host=jobs.DAY_DEFAULT_RATE*86400, nodes=None, hours=None):
        c=self.campaign()
        c.state["started"]=1_000_000
        for mode in jobs.MODES: c.state["progress"][mode]["succeeded"]=0
        c.state["progress"]["process"]["succeeded"]=host
        third=host//3
        c.state["per_node"]=nodes if nodes is not None else {
            name:dict(succeeded=third,slots=27,restarted=False) for name in ("node-1","node-2","node-3")}
        rate=host//86400
        c.state["hour_marks"]=[[hour*3600,hour*3600*rate] for hour in range(25)] if hours is None else hours
        c.state.update(stopped=True,stop_requested_at=1_086_400); c.publish(1_086_400)
        return json.loads((self.path/"snapshot.json").read_text())

    def test_two_billion_a_day_is_the_default_and_a_billion_the_floor(self):
        self.assertEqual(jobs.DAY_DEFAULT_RATE,23149)
        self.assertLess((jobs.DAY_DEFAULT_RATE-1)*86400,2_000_000_000)
        self.assertGreaterEqual(jobs.DAY_DEFAULT_RATE*86400,2_000_000_000)
        self.assertGreaterEqual(jobs.DAY_FLOOR*86400,1_000_000_000)
        self.assertLess((jobs.DAY_FLOOR-1)*86400,1_000_000_000)
        with self.assertRaises(jobs.InvalidEvidence): self.campaign(rate=jobs.DAY_FLOOR-1)
        with self.assertRaises(jobs.InvalidEvidence): self.campaign(rate=None)
        self.campaign(rate=jobs.DAY_FLOOR)

    def test_day_load_runs_host_work_as_the_biggest_arrays_the_api_accepts(self):
        c=self.campaign()
        tasks=c.body(c.prepare("process","short","work",100))["definition"]["tasks"]
        self.assertEqual((tasks["count"],tasks["chunk_size"],tasks["per_node_concurrency"]),(1<<24,1000,27))
        # Only host jobs carry the day's load; the containers keep the soak mix.
        self.assertEqual(c.body(c.prepare("shared-runc","short","work",100))["definition"]["tasks"]["count"],262144)
        self.assertEqual(c.body(c.prepare("runc","short","work",100))["definition"]["tasks"]["count"],262144)
        self.assertEqual(c.body(c.prepare("process","long","work",100))["definition"]["tasks"]["count"],512)

    def test_each_nodes_host_successes_add_up_across_polls_runs_and_a_restart(self):
        c=self.campaign()
        first=dict(id=1,mode="process"); second=dict(id=2,mode="process")
        view=lambda node,succeeded,slots=27: dict(node=node,slots=slots,counters=dict(succeeded=succeeded))
        c.count_nodes(first,dict(nodes=[view("node-1",100),view("node-2",50)]))
        c.count_nodes(first,dict(nodes=[view("node-1",300),view("node-2",50)]))
        c.count_nodes(second,dict(nodes=[view("node-1",10)]))
        # node-2 restarted and reopened the run: its counter starts again.
        c.count_nodes(first,dict(nodes=[view("node-2",20,slots=30)]))
        nodes=c.state["per_node"]
        self.assertEqual(nodes["node-1"],dict(succeeded=310,slots=27,restarted=False))
        self.assertEqual(nodes["node-2"],dict(succeeded=70,slots=30,restarted=True))

    def test_a_clean_day_passes_and_reports_the_day_in_full(self):
        snapshot=self.day_snapshot()
        day=jobs.report(snapshot)["day"]
        self.assertEqual(day["host_successes"],jobs.DAY_DEFAULT_RATE*86400)
        self.assertEqual(day["host_successes_per_second"],jobs.DAY_DEFAULT_RATE)
        self.assertTrue(day["target_met"]); self.assertTrue(day["floor_met"])
        self.assertTrue(day["per_node"]["balanced"])
        self.assertEqual(len(day["hourly"]),24)
        self.assertTrue(day["slowdown_ok"])
        self.assertTrue(jobs.report(snapshot)["target_met"])
        coverage={m:{item:True for item in jobs.REQUIRED if item!="unknown"} for m in jobs.MODES}
        snapshot["coverage"]=coverage
        # No faults, so no lost node to replay and no fault overlap to owe.
        self.assertEqual(jobs.coverage_failures(snapshot),[])

    def test_a_day_below_the_target_fails_and_says_whether_it_made_the_floor(self):
        snapshot=self.day_snapshot(host=15000*86400)
        day=jobs.report(snapshot)["day"]
        self.assertFalse(day["target_met"]); self.assertTrue(day["floor_met"])
        self.assertIn("host-job rate 15,000.0/s is below the 23149/s target",";".join(jobs.coverage_failures(snapshot)))

    def test_one_node_over_its_fair_share_fails_the_day(self):
        nodes={"node-1":dict(succeeded=41,slots=27,restarted=False),"node-2":dict(succeeded=30,slots=27,restarted=False),
               "node-3":dict(succeeded=29,slots=27,restarted=False)}
        snapshot=self.day_snapshot(nodes=nodes)
        self.assertFalse(jobs.report(snapshot)["day"]["per_node"]["balanced"])
        self.assertIn("node-1 took 41.0% of the host-job successes",";".join(jobs.coverage_failures(snapshot)))
        # Exactly 40% of three equal nodes' work is at the limit, not over it.
        nodes["node-1"]["succeeded"]=40; nodes["node-2"]["succeeded"]=31
        self.assertTrue(jobs.report(self.day_snapshot(nodes=nodes))["day"]["per_node"]["balanced"])
        self.assertIn("no per-node host-job successes",";".join(jobs.coverage_failures(self.day_snapshot(nodes={}))))

    def test_a_last_hour_much_slower_than_the_first_fails_the_day(self):
        marks=[[0,0],[3600,3600*20000]]+[[hour*3600,3600*20000+(hour-1)*3600*19000] for hour in range(2,25)]
        marks[-1][1]=marks[-2][1]+3600*17000
        snapshot=self.day_snapshot(hours=marks)
        day=jobs.report(snapshot)["day"]
        self.assertEqual((day["first_hour"],day["last_hour"]),(20000,17000))
        self.assertFalse(day["slowdown_ok"])
        self.assertIn("last hour ran at 17,000.0/s",";".join(jobs.coverage_failures(snapshot)))
        marks[-1][1]=marks[-2][1]+3600*18000
        self.assertTrue(jobs.report(self.day_snapshot(hours=marks))["day"]["slowdown_ok"])
        self.assertIn("fewer than two full hours",";".join(jobs.coverage_failures(self.day_snapshot(hours=marks[:2]))))

    def test_hour_marks_fall_on_the_hour_from_the_start_and_stay_bounded(self):
        c=self.campaign(); c.state["started"]=1000
        for now in range(1000,1000+3*3600+1,600):
            c.state["progress"]["process"]["succeeded"]=now-1000
            c.mark_hours(now)
        self.assertEqual(c.state["hour_marks"],[[1000,0],[4600,3600],[8200,7200],[11800,10800]])
        c.state["hour_marks"]=[[i,i] for i in range(jobs.MAX_HOUR_MARKS)]
        c.mark_hours(10**9)
        self.assertEqual(len(c.state["hour_marks"]),jobs.MAX_HOUR_MARKS)

