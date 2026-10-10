#!/usr/bin/env python3
"""Real mixed-runtime smoke check for the release campaign (rootful Linux).

Run: sudo python3 scripts/release/job_soak_linux.py --bun /absolute/path/to/bun
Uses a fresh private directory, random ports and only its own Bun PID. It is
not a staged-candidate fast/final qualification and cannot produce a V02 pass.
"""
import argparse
import json
import hashlib
import multiprocessing
import os
from pathlib import Path
import shutil
import socket
import subprocess
import tempfile
import sys
import uuid
import time
import urllib.error
import urllib.request
import job_soak as jobs
import job_soak_inventory as inventory


def port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1",0)); return sock.getsockname()[1]


class API:
    identity="local-smoke"
    def __init__(self,address): self.address=address
    def request(self,method,path,body=None,raw=False):
        request=urllib.request.Request(self.address+path,method=method,
                data=body.encode() if isinstance(body,str) else json.dumps(body).encode() if body is not None else None,
                headers={"Content-Type":"application/toml" if isinstance(body,str) else "application/json"})
        try:
            with urllib.request.urlopen(request,timeout=30 if isinstance(body,str) else 5) as response:
                payload=response.read(jobs.MAX_RESPONSE+1)
        except urllib.error.HTTPError as error:
            if error.code>=500: raise jobs.Unavailable(str(error))
            raise jobs.InvalidEvidence("HTTP "+str(error.code)+" "+error.read(1024).decode(errors="replace"))
        except urllib.error.URLError as error: raise jobs.Unavailable(str(error))
        if len(payload)>jobs.MAX_RESPONSE: raise jobs.InvalidEvidence("response too large")
        return payload.decode() if raw else json.loads(payload)


class SmallCampaign(jobs.Campaign):
    def body(self,intent):
        body=super().body(intent)
        if intent["purpose"]=="work":
            body["definition"]["tasks"]["count"]=4
            if intent["profile"]=="long":
                args=body["definition"]["template"]["command"]
                at=3 if intent["mode"]!="process" else 2
                args[at]=args[at].replace("sleep 20","sleep 0.2")
        return body


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bun",type=Path,required=True)
    parser.add_argument("--seconds",type=int,default=240)
    parser.add_argument("--image-cache",type=Path,help="copy a stopped fixture's image cache to avoid repeat registry pulls")
    options=parser.parse_args()
    if os.geteuid()!=0 or not Path("/sys/fs/cgroup/cgroup.controllers").exists():
        parser.error("rootful Linux with cgroup v2 required")
    if not os.environ.get("RB_JOB_SMOKE_MOUNTED"):
        # Network allocation defaults to /etc/hostname. Give this disposable
        # Bun a private identity without changing the development VM's file.
        with tempfile.NamedTemporaryFile(mode="w",prefix="rb-soak-hostname-",dir="/var/tmp") as hostfile:
            hostfile.write("rbsoak-"+uuid.uuid4().hex[:12]+"\n"); hostfile.flush()
            env=dict(os.environ,RB_JOB_SMOKE_MOUNTED="1")
            return subprocess.call(["unshare","--mount","bash","-c",
                'mount --make-rprivate /; mount --bind "$1" /etc/hostname; shift; exec "$@"',
                "--",hostfile.name,sys.executable,*sys.argv],env=env)
    root=Path(tempfile.mkdtemp(prefix="rb-release-job-smoke-",dir="/var/tmp"))
    # Rootfs mappings must traverse the fixture's parent; private state itself
    # stays 0700 and the parent cannot be listed by other users.
    root.chmod(0o711)
    data=root/"data"; data.mkdir(mode=0o711)
    if options.image_cache:
        # Copy image content and its catalogue only, preserving shifted rootfs
        # ownership. Never copy jobs, owner journals or a running Bun data dir.
        subprocess.run(["cp","-a",str(options.image_cache),str(root/"images")],check=True)
        catalog=options.image_cache.parent/"data/pickle-catalog.json"
        if catalog.exists():
            # A catalogue is persisted state: retain its actual format stamp,
            # so a different-generation candidate refuses it normally.
            shutil.copyfile(catalog.parent/"state-format.json",data/"state-format.json")
            shutil.copyfile(catalog,data/"pickle-catalog.json")
    host=root/"busybox"
    jobs.HOST_EXEC=str(host)
    apiport,registryport,auditport=port(),port(),8189
    # The normal perimeter intentionally drops the 10000–60000 published-port
    # range. This separate authenticated private verifier must sit outside it.
    with socket.socket() as check: check.bind(("0.0.0.0",auditport))
    config=root/"node.toml"
    config.write_text(f'[node]\nname="job-smoke"\n[storage]\ndata="{data}"\nimages="{root}/images"\nlogs="{root}/logs"\nmetrics="{root}/metrics"\n[images]\nregistry_port={registryport}\n[process_workloads]\nallowed_binaries=["{host}"]\nmount_isolation=false\n')
    token="smoke-verifier-token"; ledger=root/"effects.sqlite"
    with socket.socket(socket.AF_INET,socket.SOCK_DGRAM) as route:
        route.connect(("1.1.1.1",1)); private_address=route.getsockname()[0]
    auditor=multiprocessing.Process(target=jobs.serve,args=(ledger,token,private_address,auditport))
    auditor.start()
    bun=None
    try:
        with (root/"bun.log").open("w") as output:
            bun=subprocess.Popen([str(options.bun),"--config",str(config),"--runtime","mixed","--listen",f"127.0.0.1:{apiport}"],stdout=output,stderr=subprocess.STDOUT)
            api=API(f"http://127.0.0.1:{apiport}")
            for _ in range(30):
                try: api.request("GET","/v1/health"); break
                except jobs.Unavailable: time.sleep(1)
            else: raise RuntimeError("Bun did not become ready")
            app=f'[app.soak-identity]\nimage="{jobs.IMAGE}"\ncpu="50m-1"\nmemory="32Mi"\ncommand=["/bin/busybox","sh","-c","trap \\"exit 0\\" TERM; while true; do sleep 1; done"]\n'
            fixtures=Path(__file__).with_name("sustained").joinpath("soak-jobs.toml").read_text().replace("/var/lib/reliaburger/soak-jobs/busybox",str(host))
            api.request("POST","/v1/apply",app,raw=True)
            for _ in range(60):
                status=api.request("GET","/v1/status")
                if any(v["app_name"]=="soak-identity" and v["state"]=="running" for v in status): break
                time.sleep(1)
            else:
                jobs.atomic(root/"startup-status.json",status)
                raise RuntimeError("fixture app did not start: "+json.dumps(status))
            initial_app=next(v["id"] for v in status if v["app_name"]=="soak-identity" and v["state"]=="running")
            image=root/"images/rootfs/public.ecr.aws/docker/library/busybox"/jobs.IMAGE.split("@")[1].replace(":","%3A")
            files=sorted(image.glob("gen-*/bin/busybox"))
            if not files or len({hashlib.sha256(f.read_bytes()).hexdigest() for f in files})!=1:
                raise RuntimeError("pinned host BusyBox copy unavailable or inconsistent")
            shutil.copyfile(files[0],host); host.chmod(0o755)
            # Change the app revision to exercise run_before as a real deploy gate.
            api.request("POST","/v1/apply",app+'env={SOAK_GENERATION="1"}\n'+fixtures,raw=True)
            app_identity=None
            for _ in range(60):
                status=api.request("GET","/v1/status")
                running=[v for v in status if v["app_name"]=="soak-identity" and v["state"]=="running"]
                if len(running)==1 and running[0]["id"]!=initial_app:
                    app_identity=running[0]["id"]; break
                time.sleep(1)
            else: raise RuntimeError("fixture app deploy did not finish")
            named={f"soak-job-{kind}-{mode}":(mode,cover) for mode in jobs.MODES for kind,cover in (("cron","cron"),("hook","hook"),("after","published-job"))}
            campaign=SmallCampaign(root/"jobs",api,[f"http://{private_address}:{auditport}"],token,
                                   lambda name:[jobs.EffectLedger(ledger).rows(name)],
                                   lambda intents:jobs.match_activity([dict(node="job-smoke",owners=inventory.fresh_activity(data),
                                       starts={i["name"]:jobs.EffectLedger(ledger).starts(i["name"]) for i in intents if i["mode"]=="runc"})],int(time.time())))
            deadline=time.monotonic()+options.seconds; last_owner_count=0; seen_owners=set()
            while time.monotonic()<deadline:
                if bun.poll() is not None: raise RuntimeError("fixture Bun exited; see "+str(root/"bun.log"))
                campaign.step(int(time.time()))
                if campaign.state["errors"]: raise RuntimeError("; ".join(campaign.state["errors"]))
                rows=api.request("GET","/v1/batch/summaries")["batches"]
                for value in rows:
                    if value.get("name") not in named or value.get("kind")=="schedule": continue
                    jobs.validate_summary(value)
                    if value["failed"]: raise RuntimeError("named fixture failed: "+value["name"])
                    if jobs.drained(value) and value["succeeded"]==value["total"]:
                        mode,cover=named[value["name"]]; campaign.state["coverage"][mode][cover]=True
                status=api.request("GET","/v1/status")
                app_rows=[v for v in status if v["app_name"]=="soak-identity"]
                if len(app_rows)!=1 or app_rows[0]["state"]!="running":
                    raise RuntimeError("app stopped while jobs ran")
                identity=app_rows[0]["id"]
                if app_identity is None: app_identity=identity
                if app_identity!=identity: raise RuntimeError("app was replaced while jobs ran")
                view=inventory.collect(data)
                prefix=view["prefix"]
                own=[e for e in view["errors"] if prefix and "executor-"+prefix in e]
                if own: raise RuntimeError("; ".join(own))
                last_owner_count=max(last_owner_count,len(view["owners"]))
                seen_owners.update(owner["runtime"] for owner in view["owners"])
                if all(all(campaign.state["coverage"][mode].get(p) for p in (*jobs.PURPOSES,"profiles","reuse","cron","hook","published-job")) for mode in jobs.MODES): break
                time.sleep(1)
            else: raise RuntimeError("fixture coverage deadline: "+json.dumps(campaign.state["coverage"]))
            if seen_owners != set(jobs.MODES): raise RuntimeError("missing live executor ownership evidence: "+str(set(jobs.MODES)-seen_owners))
            for mode in jobs.MODES: api.request("POST",f"/v1/jobs/definitions/soak-job-cron-{mode}/default/disable")
            view=inventory.collect(data)
            # This shared development VM may contain unrelated fixtures. Only
            # demand proofs for this node's exact executor identity.
            prefix=view["prefix"]
            own=[e for e in view["errors"] if "executor-"+prefix in e]
            if own: raise RuntimeError("; ".join(own))
            last_owner_count=max(last_owner_count,len(view["owners"]))
            for _ in range(30):
                if campaign.step(int(time.time()),stop=True): break
                time.sleep(1)
            else: raise RuntimeError("smoke campaign did not drain")
            time.sleep(3) # Smoke-only idle-retirement observation, not a V02 allowance.
            final=inventory.collect(data)
            owned=[o for o in final["owners"] if "executor-"+prefix in o["id"]]
            leftovers=[v for values in final["resources"].values() for v in values if prefix in v]
            if owned or leftovers: raise RuntimeError("job resources remain after smoke drain")
            print(json.dumps(dict(result="PASS",fixture=str(root),coverage=campaign.state["coverage"],
                                 accepted=campaign.state["progress"],audited=campaign.state["totals"],
                                 observed_live_owners=last_owner_count,observed_owner_runtimes=sorted(seen_owners),app_instance=app_identity,
                                 host_binary_sha256=hashlib.sha256(host.read_bytes()).hexdigest(),drained=True),indent=1))
    finally:
        if bun and bun.poll() is None and 'campaign' in locals():
            for _ in range(20):
                try:
                    if campaign.step(int(time.time()),stop=True): break
                except (jobs.Unavailable,jobs.InvalidEvidence): pass
                time.sleep(1)
        if bun and bun.poll() is None:
            bun.terminate()
            try: bun.wait(timeout=5)
            except subprocess.TimeoutExpired: bun.kill(); bun.wait()
        auditor.terminate(); auditor.join(timeout=5)
        # Keep compact state and Bun log for debugging; no successful task rows.
        print("smoke evidence:",root)
    return 0
if __name__=="__main__": raise SystemExit(main())
