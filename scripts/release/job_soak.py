#!/usr/bin/env python3
"""Bounded, resumable public job campaign and its independent effect verifier.

Only the release harness starts this on its disposable cluster. Work is admitted
through the common API with stable request IDs. No runtime PID is signalled here.

Two loads: `calibration` (the V02 release soak's conservative mix) and `target`
(#668 and D18-C: the shared and host short runs become 1,048,576-task arrays
sized for at least 1,158 accepted successes a second plus headroom, and the
campaign fails unless its accepted rate meets the target). Either load writes
jobs/report.json with the accepted rate, retry causes, audited effects and the
September plan's M8 measurements, which are recorded, never gated.
"""
import argparse
import copy
import hashlib
from concurrent.futures import ThreadPoolExecutor
import hmac
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import re
import signal
import sqlite3
import subprocess
import tempfile
import time
import threading
import urllib.parse
import uuid

MODES = ("runc", "shared-runc", "process")
CAPS = {"runc": 2, "shared-runc": 3, "process": 3}
IMAGE = "public.ecr.aws/docker/library/busybox@sha256:9532d8c39891ca2ecde4d30d7710e01fb739c87a8b9299685c63704296b16028"
HOST_EXEC = "/var/lib/reliaburger/soak-jobs/busybox"
PURPOSES = ("audit", "failure", "timeout", "limit", "cancel")
REQUIRED = (*PURPOSES, "reuse", "profiles", "unknown", "cron", "hook", "published-job", "drain")
MAX_RUNS = 256
MAX_RESPONSE = 2 << 20
# 100 million accepted successes a day is 1,157.4 a second (#668).
TARGET_FLOOR = 1158
# The floor plus 20% headroom: the default #668 rate.
DEFAULT_TARGET_RATE = 1390
# Starting per-node caps for the target load; calibrate on the rig. Fresh
# containers can't approach the target, so their runs keep the calibration mix.
TARGET_CAPS = {"shared-runc": 8, "process": 16}
MILLION = 1 << 20
# Why retried attempts failed: the fields of the product's `RetryCauses`
# (src/meat/task_array_state.rs), the one list validation, progress totals,
# the regression check and the record all read. test_job_soak.py fails when
# the two disagree; a summary naming a cause missing here is refused by name.
CAUSES = ("exit_code", "signal", "timeout", "unknown", "launch", "unrecorded")
FAULT_KINDS = ("leader-kill", "follower-kill", "power-off")
# Node-level faults the job qualification adds (#686, D19-B).
NODE_FAULT_KINDS = ("delay", "disk-full", "slow-fsync")
# How long a drained fixture's result may stay on an unreachable node. A
# killed or powered-off node is back within the soak's longest settle budget
# (600 s); a result still missing after that is lost, not late (V02 candidate
# 6, run 15, #954).
RESULT_UNREACHABLE_GRACE = 600
# Final-attempt and start-lag bucket upper bounds in milliseconds; None is +Inf.
BUCKETS = [1 << i for i in range(15)] + [None]


class InvalidEvidence(ValueError):
    pass


class Unavailable(RuntimeError):
    pass


def atomic(path, value):
    path = Path(path)
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    temporary = path.with_name(path.name + ".new")
    with temporary.open("w") as file:
        os.chmod(temporary, 0o600)
        json.dump(value, file, separators=(",", ":"), sort_keys=True)
        file.flush(); os.fsync(file.fileno())
    os.replace(temporary, path)
    descriptor = os.open(path.parent, os.O_RDONLY)
    try: os.fsync(descriptor)
    finally: os.close(descriptor)


def number(value, key):
    item = value.get(key)
    if type(item) is not int or item < 0:
        raise InvalidEvidence("missing or invalid " + key)
    return item


def validate_summary(value, previous=None, concurrency=None):
    if not isinstance(value, dict) or type(value.get("done")) is not bool:
        raise InvalidEvidence("missing terminal status")
    identity = number(value, "batch_id")
    if not identity: raise InvalidEvidence("nonpositive run identity")
    counts = {key: number(value, key) for key in
              ("total", "succeeded", "failed", "not_run", "retried", "queued", "held")}
    if not counts["total"] or sum(counts[key] for key in ("succeeded", "failed", "not_run", "queued", "held")) != counts["total"]:
        raise InvalidEvidence("accepted counts do not conserve submitted indexes")
    active = value.get("active_commands")
    if active is not None and (type(active) is not int or not 0 <= active <= counts["held"]):
        raise InvalidEvidence("invalid verified active commands")
    causes = value.get("retry_causes")
    if causes is not None:
        if not isinstance(causes, dict):
            raise InvalidEvidence("invalid retry causes")
        unknown = sorted(set(causes) - set(CAUSES))
        if unknown:
            # A new product cause the harness can't count: say which, so the
            # fix is obvious instead of a sum that never matches (#942).
            raise InvalidEvidence("unknown retry cause " + ", ".join(unknown)
                                  + "; add it to job_soak.CAUSES")
        if sum(number(causes, key) for key in CAUSES) != counts["retried"]:
            raise InvalidEvidence("retry causes do not sum to accepted retries")
    if previous:
        if value["batch_id"] != previous["batch_id"] or value["total"] != previous["total"]:
            raise InvalidEvidence("run identity or total changed")
        if any(value[key] < previous[key] for key in ("succeeded", "failed", "not_run", "retried")):
            raise InvalidEvidence("accepted counters regressed")
        before = previous.get("retry_causes")
        if causes is not None and before is not None and any(causes[key] < before[key] for key in CAUSES):
            raise InvalidEvidence("retry causes regressed")
        if previous["done"] and not value["done"]:
            raise InvalidEvidence("terminal run became active")
    if value.get("nodes_truncated"):
        raise InvalidEvidence("node activity inventory truncated")
    for node in value.get("nodes", []):
        callers = number(node["counters"], "running")
        verified = node["counters"].get("active_commands")
        if verified is not None and (type(verified) is not int or verified < 0 or verified > callers
                                     or concurrency is not None and verified > concurrency):
            raise InvalidEvidence(f'verified activity exceeds callers/profile cap on {node.get("node")}: '
                                  f"{verified} verified, {callers} callers, cap {concurrency}")
    return counts


def drained(value):
    return (value.get("done") is True and value.get("held") == 0
            and value.get("active_commands") == 0 and value.get("status") != "Unknown")


def judge_terminal(purpose, value, rows):
    if not drained(value): raise InvalidEvidence("terminal run lacks positive drain")
    if purpose in ("work", "audit"):
        if value["succeeded"] != value["total"] or value["failed"] or value["not_run"]:
            raise InvalidEvidence("ordinary work did not succeed completely")
    elif purpose == "cancel":
        if value["failed"] or not value["not_run"]:
            raise InvalidEvidence("cancellation has no cancelled indexes or has failures")
    else:
        if value["total"] != 1 or value["failed"] != 1 or value["succeeded"] or value["not_run"]:
            raise InvalidEvidence("expected failure did not fail exactly its fixture")
        if len(rows) != 1 or rows[0].get("index") != 0:
            raise InvalidEvidence("expected failure has no exact terminal result")
        row = rows[0]
        if purpose == "failure" and (row.get("exit_code") != 7 or row.get("attempts") != 2):
            raise InvalidEvidence("nonzero fixture did not execute both attempts with exit 7")
        if purpose == "timeout" and (row.get("exit_code") is not None or row.get("attempts") != 1 or row.get("run_ms",0) < 3000 or row.get("started_effect") is not True):
            raise InvalidEvidence("deadline fixture did not report its timeout")
        if purpose == "limit" and (row.get("exit_code") not in (-9,137) or row.get("attempts") != 1):
            raise InvalidEvidence("memory fixture did not report the expected limit kill")


class API:
    """Curl keeps the candidate's TLS hostname while using its local forwards."""
    def __init__(self, nodes, api_port, ca, header):
        self.nodes, self.api_port, self.ca, self.header = nodes, api_port, ca, header
        self.deadline = None
        self.identity = hashlib.sha256(Path(ca).read_bytes()+json.dumps(nodes,sort_keys=True).encode()+str(api_port).encode()).hexdigest()
    def remaining(self, maximum):
        remaining = maximum if self.deadline is None else min(maximum, self.deadline-time.monotonic())
        if remaining <= 0: raise Unavailable("job drain request budget exhausted")
        return remaining
    def request(self, method, path, body=None, raw=False):
        for index, node in enumerate(self.nodes):
            timeout = self.remaining(7)
            with tempfile.TemporaryFile() as output:
                command = ["curl", "--silent", "--show-error", "--write-out", "\n%{http_code}", "--max-time", str(min(5,timeout)),
                           "--max-filesize", str(MAX_RESPONSE), "--cacert", str(self.ca),
                           "--header", "@" + str(self.header), "--connect-to",
                           f'{node["name"]}:9117:127.0.0.1:{self.api_port + index}',
                           "--request", method]
                if body is not None:
                    command += ["--header", "Content-Type: application/json", "--data-binary", "@-"]
                command += [f'https://{node["name"]}:9117{path}']
                try:
                    result = subprocess.run(command, input=json.dumps(body).encode() if body is not None else None,
                                            stdout=output, stderr=subprocess.DEVNULL, timeout=timeout)
                except subprocess.TimeoutExpired:
                    continue
                if result.returncode: continue
                if output.tell() > MAX_RESPONSE: raise InvalidEvidence("public response exceeded evidence bound")
                output.seek(0)
                payload, _, code = output.read().rpartition(b"\n")
                if code in (b"408",b"409",b"429") or code.startswith(b"5"): continue
                if not code.startswith(b"2"):
                    raise InvalidEvidence("candidate API refused "+method+" "+path+" (HTTP "+code.decode("ascii",errors="replace")+")")
                try: return payload.decode() if raw else json.loads(payload)
                except (ValueError, UnicodeError) as error: raise InvalidEvidence("invalid public response") from error
        raise Unavailable("no candidate API answered " + method + " " + path)


class EffectLedger:
    """An independent durable logical effect; repeated execution is counted."""
    def __init__(self, path, limit=4096):
        self.path, self.limit = Path(path), limit
        self.path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        with self.connect() as db:
            db.execute("CREATE TABLE IF NOT EXISTS effects (run TEXT, idx INTEGER, attempts INTEGER, ts INTEGER, boot TEXT, ticks INTEGER, PRIMARY KEY(run,idx))")
    def connect(self):
        db = sqlite3.connect(self.path, timeout=2)
        db.execute("PRAGMA synchronous=FULL")
        return db
    def accept(self, run, index, now, boot="", ticks=0):
        if not re.fullmatch(r"[a-z0-9-]{1,63}", run) or type(index) is not int or not 0 <= index < 512:
            raise InvalidEvidence("invalid effect identity")
        with self.connect() as db:
            db.execute("BEGIN IMMEDIATE")
            if not db.execute("SELECT 1 FROM effects WHERE run=? AND idx=?", (run, index)).fetchone():
                if db.execute("SELECT COUNT(*) FROM effects").fetchone()[0] >= self.limit:
                    raise InvalidEvidence("effect ledger capacity exhausted")
            db.execute("INSERT INTO effects VALUES (?,?,1,?,?,?) ON CONFLICT(run,idx) DO UPDATE SET attempts=attempts+1,ts=excluded.ts,boot=excluded.boot,ticks=excluded.ticks", (run,index,now,boot,ticks))
    def rows(self, run):
        with self.connect() as db:
            return [list(row) for row in db.execute("SELECT idx,attempts FROM effects WHERE run=? ORDER BY idx", (run,))]
    def starts(self, run):
        with self.connect() as db:
            return [dict(index=i,boot=b,ticks=t,ts=ts) for i,b,t,ts in db.execute("SELECT idx,boot,ticks,ts FROM effects WHERE run=? ORDER BY idx",(run,))]
    def expire(self, now, retention=900):
        with self.connect() as db: db.execute("DELETE FROM effects WHERE ts<?", (now-retention,))


def audit_effects(count, ledgers):
    seen, attempts = set(), 0
    for ledger in ledgers:
        local = set()
        for index, executions in ledger:
            if type(index) is not int or not 0 <= index < count or index in local or type(executions) is not int or executions < 1:
                raise InvalidEvidence("invalid independent effect receipt")
            local.add(index); seen.add(index); attempts += executions
    if seen != set(range(count)): raise InvalidEvidence("accepted successes lack exact independent effects")
    return dict(effects=len(seen), attempts=attempts, repeated_attempts=attempts-len(seen))


def serve(path, token, address, port):
    ledger = EffectLedger(path)
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_): pass
        def do_GET(self):
            if self.path != "/health" or not hmac.compare_digest(self.headers.get("Authorization", ""), "Bearer "+token):
                self.send_error(403); return
            self.send_response(200); self.send_header("Content-Length", "0"); self.end_headers()
        def do_POST(self):
            if not hmac.compare_digest(self.headers.get("Authorization", ""), "Bearer " + token):
                self.send_error(403); return
            pieces = self.path.split("/")
            try:
                if len(pieces) != 4 or pieces[1] != "effect" or self.headers.get("Content-Length") != "0":
                    raise InvalidEvidence("invalid effect request")
                boot=self.headers.get("X-Soak-Boot",""); ticks=int(self.headers.get("X-Soak-Ticks","0"))
                if not re.fullmatch(r"[a-f0-9-]{36}",boot) or ticks<=0: raise InvalidEvidence("missing start identity")
                now = int(time.time()); ledger.expire(now); ledger.accept(pieces[2], int(pieces[3]), now,boot,ticks)
            except (ValueError, sqlite3.Error): self.send_error(409); return
            self.send_response(200); self.send_header("Content-Length", "0"); self.end_headers()
    # Each operation has a strict socket deadline; threads cannot linger indefinitely.
    class Server(ThreadingHTTPServer):
        daemon_threads = True
        slots = threading.BoundedSemaphore(32)
        def process_request(self, request, peer):
            if not self.slots.acquire(blocking=False):
                self.shutdown_request(request); return
            try: super().process_request(request, peer)
            except BaseException:
                self.slots.release(); raise
        def process_request_thread(self, request, peer):
            try: super().process_request_thread(request, peer)
            finally: self.slots.release()
        def get_request(self):
            connection, peer = super().get_request(); connection.settimeout(3); return connection, peer
    Server((address, port), Handler).serve_forever()


class Campaign:
    def __init__(self, directory, api, auditors, token, ledger_reader=None, activity_reader=None,
                 load="calibration", target_rate=None):
        if load not in ("calibration", "target"): raise InvalidEvidence("unknown job load")
        if load == "target" and (target_rate is None or target_rate < TARGET_FLOOR):
            raise InvalidEvidence("the target load needs a rate of at least %d/s" % TARGET_FLOOR)
        self.directory, self.api, self.auditors, self.token = Path(directory), api, auditors, token
        self.ledger_reader = ledger_reader
        self.activity_reader = activity_reader
        self.directory.mkdir(mode=0o700, parents=True, exist_ok=True)
        self.path = self.directory / "state.json"
        if self.path.exists():
            self.state = json.loads(self.path.read_text())
            if self.state["auditors"] != auditors or self.state.get("schema") != 2 or self.state.get("cluster") != getattr(api,"identity","test") or self.state.get("verifier") != hashlib.sha256(token.encode()).hexdigest() \
                    or self.state.get("load","calibration") != load or self.state.get("target_rate") != target_rate:
                raise InvalidEvidence("campaign configuration changed on resume")
            if self.state.get("stopped"): raise InvalidEvidence("cannot resume a drained job campaign")
            self.state["resume_grace_until"] = int(time.time())+180
        else:
            self.state = dict(schema=2, cluster=getattr(api,"identity","test"), verifier=hashlib.sha256(token.encode()).hexdigest(), campaign=uuid.uuid4().hex[:12], auditors=auditors,
                              sequence=0, active={}, coverage={m:{} for m in MODES},
                              progress={m:{"succeeded":0,"retried":0,"last_progress":int(time.time())} for m in MODES},
                              errors=[], totals={}, stopped=False, cron_registered=False, discovered={}, fault_offset=0,
                              load=load, target_rate=target_rate, started=int(time.time()),
                              metrics=dict(million_wall_seconds=[], start_lag_ms_counts=[0]*16, state_bytes_max=0, raft=None))
            self.save()
    def save(self): atomic(self.path, self.state)
    def error(self, message):
        if message not in self.state["errors"]:
            self.state["errors"].append(message[:400]); self.state["errors"] = self.state["errors"][-32:]
        self.save()
    def prepare(self, mode, profile, purpose, now):
        key = mode + ":" + profile
        if key in self.state["active"]: raise InvalidEvidence("profile already has a pending intent")
        if self.state["sequence"] >= MAX_RUNS: raise InvalidEvidence("retained run budget exhausted")
        self.state["sequence"] += 1
        intent = dict(mode=mode, profile=profile, purpose=purpose, created=now,
                      name=f'soak-{self.state["campaign"]}-{self.state["sequence"]}',
                      request_id=f'{self.state["campaign"]}:{self.state["sequence"]}', id=None, previous=None)
        self.state["active"][key] = intent; self.save()
        return intent
    def targeted(self, intent):
        """Whether this intent is a target-load throughput run."""
        return (self.state.get("load") == "target" and intent["mode"] in TARGET_CAPS
                and intent["profile"] == "short" and intent["purpose"] == "work")
    def body(self, intent):
        mode, profile, purpose = (intent[key] for key in ("mode", "profile", "purpose"))
        count = 512 if profile == "long" else 262144
        chunk = 1 if profile == "long" else 64
        script = "sleep 20" if profile == "long" else "sleep 0.02"
        template = dict(runtime=mode, cpu="25m-1", memory="64Mi" if profile == "long" else "32Mi", namespace="default")
        tasks = dict(count=count, chunk_size=chunk, max_attempts=3, task_timeout_secs=60,
                     per_node_concurrency=CAPS[mode]-1 if profile == "long" else 1)
        if self.targeted(intent):
            # The bench's no-op command, in million-task arrays: the wall time
            # of each one is an M8 measurement.
            script = "true"
            tasks.update(count=MILLION, chunk_size=1000, per_node_concurrency=TARGET_CAPS[mode])
        if purpose != "work":
            count = 8 if purpose in ("audit", "cancel") else 1
            tasks.update(count=count,chunk_size=1,max_attempts=1,per_node_concurrency=1)
        effect_script = 'SOAK_BOOT=$(cat /proc/sys/kernel/random/boot_id); SOAK_TICKS=$("$SOAK_BUSYBOX" awk "{print \\$22}" /proc/$$/stat); ack_effect() { for url in $SOAK_AUDIT_URLS; do "$SOAK_BUSYBOX" wget -q -T 1 --header "Authorization: Bearer $SOAK_AUDIT_TOKEN" --header "X-Soak-Boot: $SOAK_BOOT" --header "X-Soak-Ticks: $SOAK_TICKS" --post-data="" -O /dev/null "$url/effect/$SOAK_EFFECT_RUN/$1" && return 0; done; return 8; }; ack_effect "$1" || exit 8; '
        if purpose in ("audit","timeout","cancel") or mode=="runc" and profile=="long":
            template["env"] = dict(SOAK_AUDIT_URLS=" ".join(self.auditors), SOAK_AUDIT_TOKEN=self.token, SOAK_EFFECT_RUN=intent["name"])
        if purpose == "audit":
            script = effect_script + "exit 0"
            tasks["max_attempts"] = 10
        elif purpose == "failure": script = "exit 7"; tasks["max_attempts"] = 2
        elif purpose == "timeout":
            # Output from a killed helper is not dependable launch evidence.
            # A committed independent start effect distinguishes a real deadline
            # from a failed/slow launch that happened to produce no exit code.
            script = effect_script + "sleep 30 & wait"; tasks["task_timeout_secs"] = 3
        elif purpose == "limit":
            script = "exec \"$SOAK_BUSYBOX\" awk 'BEGIN { for(i=0;;i++) a[i]=sprintf(\"%01024d\",i) }'"
            template["memory"] = "8Mi"; tasks["task_timeout_secs"] = 15
        elif purpose == "cancel": script = effect_script+"sleep 20"
        elif mode=="runc" and profile=="long": script=effect_script+script
        if mode != "process":
            template["image"] = IMAGE
            # /tmp is fresh per command for both container contracts.
            if purpose == "work" and profile == "short":
                script = "test ! -e /tmp/soak-task-leak || exit 9; touch /tmp/soak-task-leak; " + script
            template["command"] = ["/bin/busybox", "sh", "-c", script, "soak", "{index}"]
        else:
            template["exec"] = HOST_EXEC
            template["command"] = ["sh", "-c", script, "soak", "{index}"]
        template.setdefault("env",{})["SOAK_BUSYBOX"]=HOST_EXEC if mode=="process" else "/bin/busybox"
        return dict(name=intent["name"],namespace="default",request_id=intent["request_id"],
                    definition=dict(template=template,tasks=tasks,replay_unknown=profile!="long"))
    def admit(self, intent):
        result = self.api.request("POST", "/v1/jobs/runs", self.body(intent))
        identity = number(result,"batch_id")
        if not identity: raise InvalidEvidence("admission returned no positive identity")
        intent["id"] = identity; self.save()
    def replay_decisions(self, intent, summary):
        owners = summary.get("unknown_owners", [])
        if owners and intent["profile"] != "long": raise InvalidEvidence("replayable fixture became unknown")
        for row in owners:
            if not re.fullmatch(r"[a-f0-9]{64}",row.get("grant_digest", "")) or not row.get("node"):
                raise InvalidEvidence("unknown owner lacks bound fingerprint")
        before = intent.get("fenced_owners")
        intent["fenced_owners"] = copy.deepcopy(owners); self.save()
        return owners if owners and before == owners else []
    def acknowledge(self, identity, owner, reason, now):
        key=str(identity)+":"+owner["node"]+":"+owner["grant_digest"]
        decisions=self.state.setdefault("replay_decisions",{})
        if key not in decisions:
            if len(decisions)>=512: raise InvalidEvidence("operator decision evidence bound exhausted")
            decisions[key]=dict(run=identity,node=owner["node"],grant_digest=owner["grant_digest"],reason=reason,ts=now,accepted=False)
            self.save() # Intent before sending; a lost reply reuses this fence.
        self.api.request("POST",f'/v1/jobs/runs/{identity}/replay',dict(node=owner["node"],grant_digest=owner["grant_digest"],acknowledged=True))
        decisions[key]["accepted"]=True
        self.state["totals"]["operator_replay_decisions"]=sum(row["accepted"] for row in decisions.values())
        self.save()
    def publish(self, now, fault=False):
        atomic(self.directory / "snapshot.json",dict(heartbeat=now,errors=self.state["errors"],
               active=list(self.state["active"].values()),progress=self.state["progress"],coverage=self.state["coverage"],
               totals=self.state["totals"],stopped=self.state["stopped"],fault_window=fault,
               load=self.state.get("load","calibration"),target_rate=self.state.get("target_rate"),
               started=self.state.get("started"),metrics=self.state.get("metrics",{}),
               fresh_activity=self.state.get("fresh_activity",[]),discovered=self.state["discovered"],stop_requested_at=self.state.get("stop_requested_at"),last_fault=self.state.get("last_fault",0),resume_grace_until=self.state.get("resume_grace_until",0)))
    def step(self, now, fault=False, stop=False):
        if fault: self.state["last_fault"]=now
        self.publish(now, fault)
        if not stop:
            for mode in MODES:
                for profile in ("short", "long"):
                    if mode+":"+profile not in self.state["active"]:
                        coverage = self.state["coverage"][mode]
                        purpose = "work" if profile == "long" else next((p for p in PURPOSES if not coverage.get(p)), "work")
                        self.prepare(mode, profile, purpose, now)
        intents = list(self.state["active"].values())
        if self.activity_reader is not None:
            try:
                activity=self.activity_reader(intents)
                self.state["fresh_activity"]=activity
                for intent in intents:
                    if intent["mode"]=="runc" and intent["purpose"]=="cancel" and any(row["id"]==intent["id"] for row in activity):
                        intent["verified_start"]=True
            except Unavailable: self.state["fresh_activity"]=[]
        def poll(intent):
            if intent["id"] is None:
                if stop:
                    # Resolve the original request even during drain. Its effect may already exist.
                    return ("admit", self.api.request("POST","/v1/jobs/runs",self.body(intent)))
                return ("admit", self.api.request("POST","/v1/jobs/runs",self.body(intent)))
            if stop or intent["purpose"] == "cancel" and ((intent.get("previous") or {}).get("active_commands",0) or intent.get("verified_start")):
                self.api.request("POST",f'/v1/batch/{intent["id"]}/cancel')
            return ("summary",self.api.request("GET",f'/v1/batch/{intent["id"]}'))
        with ThreadPoolExecutor(max_workers=6) as workers:
            answers = list(workers.map(lambda intent: self.attempt(poll,intent),intents))
        for intent, answer in zip(intents, answers):
            if answer is None: continue
            try:
                kind, value = answer
                if kind == "error": raise InvalidEvidence(value)
                if kind == "admit":
                    identity=number(value,"batch_id")
                    if not identity: raise InvalidEvidence("admission returned no positive identity")
                    intent["id"]=identity; self.save(); continue
                old=intent["previous"]
                validate_summary(value,old,self.body(intent)["definition"]["tasks"]["per_node_concurrency"])
                template=self.body(intent)["definition"]["template"]
                if value.get("runtime")!=intent["mode"] or value.get("cpu_request_millicores")!=25 or value.get("memory_request_bytes")!=int(template["memory"][:-2])*(1<<20):
                    raise InvalidEvidence("candidate changed the admitted runtime or resource profile")
                if value["batch_id"] != intent["id"] or value.get("name") != intent["name"]:
                    raise InvalidEvidence("summary is for another admitted job")
                if intent["purpose"] in ("work","audit","cancel") and value["failed"] and not intent.get("failed_at"):
                    # Reported once, with the first failed task's cause, and
                    # the run stays in the campaign until it drains: raising
                    # on every poll kept it there forever, so the final drain
                    # could never finish (V02 candidate 3, run 26).
                    intent["failed_at"]=now
                    self.error(f'{intent["mode"]} {intent["profile"]} {intent["purpose"]} ({intent["id"]}): '
                               "unexpected terminal failure in ordinary campaign work"+self.failure_cause(intent["id"]))
                progress=self.state["progress"][intent["mode"]]
                delta=value["succeeded"]-(old["succeeded"] if old else 0)
                progress["succeeded"]+=delta
                progress["retried"]+=value["retried"]-(old["retried"] if old else 0)
                if value.get("retry_causes") is not None:
                    before=(old or {}).get("retry_causes") or {}
                    totals=progress.setdefault("retry_causes",{key:0 for key in CAUSES})
                    for key in CAUSES: totals[key]=totals.get(key,0)+value["retry_causes"][key]-before.get(key,0)
                if delta:
                    progress.setdefault("first_progress",now)
                    progress["latest_success"]=now
                    coverage=self.state["coverage"][intent["mode"]]
                    coverage["profile:"+intent["profile"]]=True
                    coverage["profiles"]=all(coverage.get("profile:"+p) for p in ("short","long"))
                    if intent["purpose"]=="work" and intent["profile"]=="short" and value["succeeded"]>=2:
                        coverage["reuse"]=True
                if delta or fault: progress["last_progress"]=now
                intent["previous"]=value
                intent["observed_at"]=int(time.time())
                for owner in self.replay_decisions(intent,value):
                    # Deliberate operator action for the known replay-safe long fixture,
                    # only after two observations of the same fenced grant. Never general replay.
                    self.acknowledge(intent["id"],owner,"known replay-safe long fixture, two identical fenced observations",now)
                    self.state["coverage"][intent["mode"]]["unknown"]=True
                if not drained(value): continue
                self.record_metrics(value)
                rows=[]
                if intent["purpose"] in ("failure","timeout","limit") and not stop:
                    page=self.api.request("GET",f'/v1/batch/{intent["id"]}/results?limit=1&index=0')
                    if page.get("truncated"): raise InvalidEvidence("failure result incomplete")
                    if page.get("unreachable"):
                        # The node holding the row may be restarting after a
                        # fault: read it again on a later poll, within a bound.
                        since=intent.setdefault("result_unreachable_since",now); self.save()
                        if now-since<RESULT_UNREACHABLE_GRACE: continue
                        raise InvalidEvidence("failure result incomplete: "+", ".join(map(str,page["unreachable"]))
                                              +f" unreachable for {now-since} s")
                    rows=page["rows"]
                    if intent["purpose"] == "timeout" and len(rows)==1:
                        if self.ledger_reader is None: raise InvalidEvidence("deadline start verifier unavailable")
                        audit_effects(1,self.ledger_reader(intent["name"]))
                        rows[0]["started_effect"]=True
                if not stop and not intent.get("failed_at"):
                    judge_terminal(intent["purpose"],value,rows)
                    if intent["purpose"] == "audit":
                        if self.ledger_reader is None: raise InvalidEvidence("independent ledger unavailable")
                        receipts=self.ledger_reader(intent["name"])
                        proof=audit_effects(value["total"],receipts)
                        self.state["totals"]["audit_attempts"]=self.state["totals"].get("audit_attempts",0)+proof["attempts"]
                    coverage=self.state["coverage"][intent["mode"]]
                    coverage[intent["purpose"] if intent["purpose"]!="work" else "reuse"]=True
                    coverage["profile:"+intent["profile"]]=True
                    coverage["profiles"]=all(coverage.get("profile:"+p) for p in ("short","long"))
                self.state["coverage"][intent["mode"]]["drain"]=True
                del self.state["active"][intent["mode"]+":"+intent["profile"]]
                self.save()
            except Unavailable: pass
            except (InvalidEvidence,KeyError,TypeError) as error: self.error(f'{intent["mode"]} {intent["profile"]} {intent["purpose"]} ({intent["id"]}): {error}')
        self.save(); self.publish(int(time.time()),fault)
        return not self.state["active"]
    def failure_cause(self, identity):
        """The first failed task's attempts, exit code and first output line,
        as a suffix for the campaign error, or nothing when the candidate
        can't say. The cause is gone once the run's details expire."""
        try:
            page=self.api.request("GET",f"/v1/batch/{identity}/results?failed=true&limit=1")
            row=page["rows"][0]
            index=int(row["index"])
            exit_code=row.get("exit_code")
            cause=f': task {index} after {int(row["attempts"])} attempt(s), '+("no exit code" if exit_code is None else f"exit {exit_code}")
        except (Unavailable,InvalidEvidence,KeyError,IndexError,TypeError,ValueError):
            return ""
        try:
            output=self.api.request("GET",f"/v1/batch/{identity}/tasks/{index}/logs",raw=True)
        except (Unavailable,InvalidEvidence):
            return cause
        first=next((line.strip() for line in output.splitlines() if line.strip()),"")
        return cause+(": "+first[:200] if first else "")
    def record_metrics(self, value):
        """Fold one drained run's M8 measurements into the bounded campaign record."""
        metrics=self.state.setdefault("metrics",dict(million_wall_seconds=[],start_lag_ms_counts=[0]*16,state_bytes_max=0,raft=None))
        if value["total"]==MILLION and value["succeeded"]==MILLION and type(value.get("elapsed_seconds")) is int:
            metrics["million_wall_seconds"]=(metrics["million_wall_seconds"]+[value["elapsed_seconds"]])[-64:]
        for node in value.get("nodes",[]):
            counts=node.get("counters",{}).get("start_lag_ms_counts")
            if isinstance(counts,list) and len(counts)==16 and all(type(c) is int and c>=0 for c in counts):
                metrics["start_lag_ms_counts"]=[a+b for a,b in zip(metrics["start_lag_ms_counts"],counts)]
        if type(value.get("state_bytes")) is int:
            metrics["state_bytes_max"]=max(metrics["state_bytes_max"],value["state_bytes"])
    def sample_raft(self, council, now):
        """Keep the first and latest Raft log index the cluster reported."""
        index=council.get("last_log_index") if isinstance(council,dict) else None
        if type(index) is not int: return
        metrics=self.state.setdefault("metrics",dict(million_wall_seconds=[],start_lag_ms_counts=[0]*16,state_bytes_max=0,raft=None))
        raft=metrics.get("raft") or dict(first=[now,index],leaders=[])
        raft["last"]=[now,index]
        leader=council.get("leader")
        if leader and leader not in raft["leaders"]: raft["leaders"]=(raft["leaders"]+[leader])[-16:]
        metrics["raft"]=raft
    def attempt(self, callback, intent):
        try: return callback(intent)
        except Unavailable: return None
        except (InvalidEvidence,KeyError,TypeError) as error:
            return ("error", str(error))


def match_activity(samples, now):
    receipts=[(name,row) for sample in samples for name,rows in sample["starts"].items() for row in rows]
    result=[]
    for sample in samples:
        if not 0<=now-sample.get("ts",now)<=15: continue
        for owner in sample["owners"]:
            if any(name==owner["name"] and row["index"]==owner["index"] and row["boot"]==owner["boot"]
                   and row["ticks"] in owner["ticks"] for name,row in receipts):
                result.append(dict(node=sample["node"],id=owner["run"],instance=owner["id"],generation=owner["generation"],
                                   boot=owner["boot"],ticks=owner["ticks"],observed_at=sample.get("ts",now)))
    return result


def findings(snapshot, now, fault_window):
    if not isinstance(snapshot,dict): return ["job controller evidence missing"]
    if snapshot.get("stopped") is not True and now-snapshot.get("heartbeat",0)>90:
        return ["job controller heartbeat stale"]
    problems=list(snapshot.get("errors",[]))
    if not fault_window and snapshot.get("stopped") is not True and now>=snapshot.get("resume_grace_until",0):
        for mode in MODES:
            progress=snapshot.get("progress",{}).get(mode)
            if not progress or now-progress.get("last_progress",0)>180:
                problems.append(mode+" has no accepted progress in a healthy recovery interval")
            if progress and progress.get("last_cron_progress") and now-max(progress["last_cron_progress"],snapshot.get("last_fault",0))>300:
                problems.append(mode+" cron has no new accepted occurrence in a healthy recovery interval")
    return problems


def percentile(counts, fraction):
    """Upper bound of the bucket holding the given fraction; None if empty or +Inf."""
    total=sum(counts)
    if not total: return None
    seen=0
    for bound,count in zip(BUCKETS,counts):
        seen+=count
        if seen>=fraction*total: return bound
    return None


def report(snapshot):
    """The job qualification report: rates, retry causes, audited effects and M8 metrics.

    M8 measurements are recorded, never gated (D18-C). The accepted rate gates
    only the target load."""
    if not isinstance(snapshot,dict): return dict(schema=1,complete=False)
    progress=snapshot.get("progress",{})
    started=snapshot.get("started") or snapshot.get("heartbeat",0)
    ended=snapshot.get("stop_requested_at") or snapshot.get("heartbeat",started)
    elapsed=max(1,ended-started)
    accepted={mode:progress.get(mode,{}).get("succeeded",0) for mode in MODES}
    retries={mode:progress.get(mode,{}).get("retried",0) for mode in MODES}
    causes={mode:progress.get(mode,{}).get("retry_causes") for mode in MODES}
    totals=snapshot.get("totals",{})
    metrics=snapshot.get("metrics") or {}
    raft=metrics.get("raft") or {}
    raft_rate=None
    if raft.get("first") and raft.get("last") and raft["last"][0]>raft["first"][0]:
        raft_rate=(raft["last"][1]-raft["first"][1])/(raft["last"][0]-raft["first"][0])
    lag=metrics.get("start_lag_ms_counts") or [0]*16
    rate=sum(accepted.values())/elapsed
    target=snapshot.get("target_rate")
    walls=metrics.get("million_wall_seconds") or []
    return dict(schema=1,complete=snapshot.get("stopped") is True,load=snapshot.get("load","calibration"),
        target_rate=target,elapsed_seconds=elapsed,accepted_successes=accepted,
        accepted_successes_per_second=rate,target_met=None if target is None else rate>=target,
        retries=retries,retry_causes=causes,
        audited_attempts=totals.get("audit_attempts",0),
        operator_replay_decisions=totals.get("operator_replay_decisions",0),
        fault_markers=totals.get("fault_markers",0),errors=list(snapshot.get("errors",[])),
        m8=dict(
            million_task_array_wall_seconds=dict(runs=len(walls),max=max(walls) if walls else None,
                                                 median=sorted(walls)[len(walls)//2] if walls else None),
            p99_start_lag_ms=percentile(lag,0.99),start_lag_samples=sum(lag),
            raft_entries_per_second=raft_rate,raft_leaders=raft.get("leaders",[]),
            raft_state_bytes_max=metrics.get("state_bytes_max") or None,
            targets=dict(million_task_array_wall_seconds=600,p99_start_lag_ms=1000,
                         raft_entries_per_second="recorded only; the batch target counts only task-array writes",
                         raft_state_bytes_per_million_tasks=256*1024,
                         bun_steady_rss_mib=dict(api_node=512,other_nodes=384),
                         leader_cpu_cores=1,executor_overhead_cores_per_node=0.5,
                         node_ledger_mib_per_million_tasks=64,retained_log_mb=50)))


def coverage_failures(snapshot, nodes=(), kinds=FAULT_KINDS):
    if not isinstance(snapshot,dict): return ["job qualification evidence missing"]
    problems=list(snapshot.get("errors",[]))
    for mode in MODES:
        coverage=snapshot.get("coverage",{}).get(mode,{})
        missing=[item for item in REQUIRED if not coverage.get(item)]
        for node in nodes:
            if not coverage.get("fault:"+node): missing.append("active fault overlap on "+node)
        for kind in kinds:
            if not coverage.get("fault-kind:"+kind): missing.append("active "+kind+" overlap")
        if missing: problems.append(mode+" missing coverage: "+", ".join(missing))
    if snapshot.get("stopped") is not True or snapshot.get("active"):
        problems.append("job campaign did not positively drain before teardown")
    if snapshot.get("load")=="target":
        result=report(snapshot)
        if not result["target_met"]:
            problems.append("accepted rate %.1f/s is below the %s/s target" % (result["accepted_successes_per_second"],result["target_rate"]))
    return problems


def mark(directory, target, kind, now):
    directory=Path(directory)
    try: snapshot=json.loads(directory.joinpath("snapshot.json").read_text())
    except (OSError,ValueError): snapshot={}
    runs=[]
    if now-snapshot.get("heartbeat",0)<=15:
        for intent in snapshot.get("active",[]):
            summary=intent.get("previous") or {}
            if now-intent.get("observed_at",0)>15: continue
            for node in summary.get("nodes",[]):
                if node.get("node")==target and (node.get("counters",{}).get("active_commands") or 0)>0:
                    runs.append(dict(mode=intent["mode"],id=intent["id"]))
        for row in snapshot.get("fresh_activity",[]):
            if row.get("node")==target and 0<=now-row.get("observed_at",0)<=15:
                runs.append(dict(mode="runc",id=row["id"],instance=row["instance"],generation=row["generation"],boot=row["boot"],ticks=row["ticks"]))
    with directory.joinpath("faults.jsonl").open("a") as file:
        file.write(json.dumps(dict(ts=now,target=target,kind=kind,runs=runs))+"\n")


def consume_faults(campaign):
    path=campaign.directory/"faults.jsonl"
    if not path.exists(): return
    with path.open() as file:
        file.seek(campaign.state["fault_offset"])
        for line in file.readlines():
            row=json.loads(line)
            for run in row["runs"]:
                campaign.state["coverage"][run["mode"]]["fault:"+row["target"]]=True
                label=row["kind"].split(":")[-1]
                campaign.state["coverage"][run["mode"]]["fault-kind:"+label]=True
            campaign.state["totals"]["fault_markers"]=campaign.state["totals"].get("fault_markers",0)+1
        campaign.state["fault_offset"]=file.tell()
    campaign.save()


def run(options):
    evidence=options.evidence
    nodes=json.loads(evidence.joinpath("jobs/nodes.json").read_text())
    urls=[f'http://{node["address"]}:8189' for node in nodes]
    api=API(nodes,options.api_port,options.ca,evidence/".auth-header")
    def receipts(name):
        def read(node):
            command=[options.limactl,"shell","--workdir","/",node["name"],"sudo","python3",
                     "/var/lib/reliaburger/soak-jobs/job_soak.py","effects",
                     "--database","/var/lib/reliaburger/soak-jobs/effects.sqlite","--run",name]
            result=subprocess.run(command,stdout=subprocess.PIPE,stderr=subprocess.DEVNULL,timeout=api.remaining(15),check=True)
            if len(result.stdout)>32768: raise InvalidEvidence("effect response exceeds bound")
            return json.loads(result.stdout)
        try:
            with ThreadPoolExecutor(max_workers=3) as workers: return list(workers.map(read,nodes))
        except (subprocess.SubprocessError,ValueError): raise Unavailable("independent effects unavailable")
    def activity(intents):
        names=[i["name"] for i in intents if i["mode"]=="runc" and (i["profile"]=="long" or i["purpose"]=="cancel")]
        def read(node):
            command=[options.limactl,"shell","--workdir","/",node["name"],"sudo","python3",
                     "/var/lib/reliaburger/soak-jobs/job_soak.py","activity","--database",
                     "/var/lib/reliaburger/soak-jobs/effects.sqlite","--node",node["name"]]
            for name in names: command += ["--run",name]
            try: result=subprocess.run(command,stdout=subprocess.PIPE,stderr=subprocess.DEVNULL,timeout=api.remaining(7),check=True)
            except (subprocess.SubprocessError,Unavailable): return None
            if len(result.stdout)>65536: raise InvalidEvidence("fresh activity exceeded bound")
            return json.loads(result.stdout)
        with ThreadPoolExecutor(max_workers=3) as workers: samples=list(workers.map(read,nodes))
        return match_activity([s for s in samples if s],int(time.time()))
    target_rate=(options.target_rate or DEFAULT_TARGET_RATE) if options.load=="target" else None
    campaign=Campaign(evidence/"jobs",api,urls,evidence.joinpath("jobs/.token").read_text().strip(),receipts,activity,
                      options.load,target_rate)
    stop_signal=False
    def stop(*_):
        nonlocal stop_signal
        stop_signal=True
    signal.signal(signal.SIGTERM,stop); signal.signal(signal.SIGINT,stop)
    deadline=None; last_scan=0
    while True:
        now=int(time.time())
        metadata=json.loads(evidence.joinpath("metadata.json").read_text())
        kinds=FAULT_KINDS+(NODE_FAULT_KINDS if metadata.get("node_faults") is True else ())
        scheduled_stop=metadata.get("job_stop_at")
        stop_requested=stop_signal or evidence.joinpath("jobs/stop").exists() or scheduled_stop is not None and now>=scheduled_stop
        if stop_requested and deadline is None:
            deadline=time.monotonic()+110
            api.deadline=deadline
            campaign.state["stop_requested_at"]=now; campaign.save()
        try:
            state=json.loads(evidence.joinpath("state.json").read_text())
            fault=bool(state.get("window"))
            finished=campaign.step(now,fault,stop_requested)
            consume_faults(campaign)
            if now-last_scan>=30 or stop_requested:
                # Disable first, then enumerate; otherwise a cron occurrence can
                # be admitted between the scan and disabling its definition.
                if stop_requested:
                    for mode in MODES: api.request("POST",f"/v1/jobs/definitions/soak-job-cron-{mode}/default/disable")
                try: campaign.sample_raft(api.request("GET","/v1/cluster/council"),now)
                except InvalidEvidence: pass
                rows=api.request("GET","/v1/batch/summaries")["batches"]
                for value in rows:
                    label=value.get("name","")
                    known={f"soak-job-{kind}-{mode}":(mode,cover) for mode in MODES for kind,cover in (("cron","cron"),("hook","hook"),("after","published-job"))}
                    if label not in known or value.get("kind")=="schedule": continue
                    key=str(value["batch_id"])
                    previous=campaign.state["discovered"].get(key)
                    validate_summary(value,previous)
                    if not drained(value):
                        campaign.state["discovered"][key]=copy.deepcopy(value)
                        if len(campaign.state["discovered"])>32: raise InvalidEvidence("unresolved named run budget exhausted")
                    if not value.get("unknown_owners") and drained(value):
                        campaign.state["discovered"].pop(key,None)
                    if value["failed"]: raise InvalidEvidence("release job fixture failed: "+label)
                    if value["done"] and value["succeeded"]==value["total"]:
                        mode,cover=known[label]
                        campaign.state["coverage"][mode][cover]=True
                        if cover=="cron":
                            progress=campaign.state["progress"][mode]
                            if value["batch_id"]>progress.get("last_cron_id",0):
                                progress.update(last_cron_id=value["batch_id"],last_cron_progress=now)
                    elif value.get("unknown_owners"):
                        # These named hook/cron fixtures only execute true. Acknowledgement
                        # is recorded as a test-operator action after observing the fence twice.
                        key=str(value["batch_id"])
                        previous=previous or {}
                        if len(campaign.state["discovered"])>32: raise InvalidEvidence("unresolved named run budget exhausted")
                        if previous.get("unknown_owners")==value["unknown_owners"]:
                            for owner in value["unknown_owners"]:
                                if not re.fullmatch(r"[a-f0-9]{64}",owner.get("grant_digest","")) or not owner.get("node"):
                                    raise InvalidEvidence("named run lacks exact replay fence")
                                campaign.acknowledge(value["batch_id"],owner,"named true fixture, two identical fenced observations",now)
                    else:
                        campaign.state["discovered"].pop(str(value["batch_id"]),None)
                    if stop_requested and not drained(value): api.request("POST",f'/v1/batch/{value["batch_id"]}/cancel')
                if stop_requested:
                    finished=finished and all(drained(v) for v in rows if v.get("name") in known and v.get("kind")!="schedule")
                last_scan=now
            if stop_requested and finished:
                campaign.state["stopped"]=True; campaign.save(); campaign.publish(now,fault)
                final=json.loads(campaign.directory.joinpath("snapshot.json").read_text())
                atomic(campaign.directory/"report.json",report(final))
                return 1 if coverage_failures(final,[n["name"] for n in nodes],kinds) else 0
        except (Unavailable,OSError,ValueError,KeyError,TypeError) as error:
            if not isinstance(error,(Unavailable,OSError,json.JSONDecodeError)): campaign.error(str(error))
            campaign.publish(int(time.time()))
        if deadline is not None and time.monotonic()>=deadline:
            campaign.error("job campaign drain deadline exceeded"); campaign.publish(int(time.time())); return 1
        time.sleep(2)


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    commands=parser.add_subparsers(dest="command",required=True)
    controller=commands.add_parser("run")
    controller.add_argument("--evidence",type=Path,required=True); controller.add_argument("--api-port",type=int,required=True)
    controller.add_argument("--ca",type=Path,required=True); controller.add_argument("--limactl",required=True)
    controller.add_argument("--load",choices=("calibration","target"),default="calibration")
    controller.add_argument("--target-rate",type=float,help="accepted successes a second for --load target (default %d)" % DEFAULT_TARGET_RATE)
    server=commands.add_parser("serve")
    server.add_argument("--database",type=Path,required=True); server.add_argument("--token-file",type=Path,required=True)
    server.add_argument("--address",required=True); server.add_argument("--port",type=int,default=8189)
    effects=commands.add_parser("effects")
    effects.add_argument("--database",type=Path,required=True); effects.add_argument("--run",required=True)
    activity=commands.add_parser("activity")
    activity.add_argument("--database",type=Path,required=True); activity.add_argument("--node",required=True)
    activity.add_argument("--root",type=Path,default=Path("/var/lib/reliaburger/data"))
    activity.add_argument("--run",action="append",default=[])
    marker=commands.add_parser("mark")
    marker.add_argument("directory",type=Path); marker.add_argument("target"); marker.add_argument("kind")
    options=parser.parse_args()
    if options.command=="run": return run(options)
    if options.command=="serve": serve(options.database,options.token_file.read_text().strip(),options.address,options.port)
    elif options.command=="activity":
        import job_soak_inventory
        print(json.dumps(dict(node=options.node,ts=int(time.time()),owners=job_soak_inventory.fresh_activity(options.root),
                              starts={name:EffectLedger(options.database).starts(name) for name in options.run})))
    elif options.command=="effects": print(json.dumps(EffectLedger(options.database).rows(options.run)))
    else: mark(options.directory,options.target,options.kind,int(time.time()))
    return 0


if __name__=="__main__": raise SystemExit(main())
