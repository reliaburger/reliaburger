#!/usr/bin/env python3
"""Exact, private job owners and a bounded Linux kernel/storage inventory.

Names identify candidates, not ownership. Never export launch specs or nonces.
The collector fails closed if it cannot complete a scan or reconcile turnover.
"""
import argparse
import json
import hashlib
import os
from pathlib import Path
import re
import stat
import subprocess
import time

LIMIT = 4096
class InvalidInventory(ValueError): pass


def safe_bytes(path, boundary, uid, limit=1<<20):
    path, boundary = Path(path), Path(boundary)
    try: parents = path.relative_to(boundary).parts[:-1]
    except ValueError: raise InvalidInventory("record outside private boundary")
    descriptor = os.open(boundary, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        for component in (None, *parents):
            if component is not None:
                child = os.open(component, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=descriptor)
                os.close(descriptor); descriptor = child
            info = os.fstat(descriptor)
            if info.st_uid != uid or info.st_mode & 0o022:
                raise InvalidInventory("unsafe owner directory")
        file = os.open(path.name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=descriptor)
        try:
            info = os.fstat(file)
            if not stat.S_ISREG(info.st_mode) or info.st_uid != uid or info.st_nlink != 1 or info.st_mode & 0o022 or info.st_size > limit:
                raise InvalidInventory("unsafe owner record")
            value = os.read(file, limit+1)
            if len(value) > limit: raise InvalidInventory("owner record exceeds bound")
            return value
        finally: os.close(file)
    except OSError as error: raise InvalidInventory("owner record unavailable or unsafe") from error
    finally: os.close(descriptor)


def safe_json(path, boundary, uid):
    try: return json.loads(safe_bytes(path, boundary, uid))
    except (ValueError, UnicodeError) as error: raise InvalidInventory("invalid owner JSON") from error


def identity_parts(identity, prefix):
    if not re.fullmatch(r"[a-fA-F0-9]{32}", prefix): raise InvalidInventory("invalid executor identity")
    match = re.fullmatch(r"default__executor-"+re.escape(prefix)+r"(?P<pool>-reuse|-host)?-(?P<slot>0|[1-9][0-9]*)", identity)
    if not match: raise InvalidInventory("foreign executor identity")
    pool, slot = match.group("pool") or "", int(match.group("slot"))
    if slot >= (256 if not pool else 32): raise InvalidInventory("executor slot outside pool")
    return pool, slot, "default/executor-"+prefix+pool+"/"+str(slot)


def owner_proof(record, identity, prefix, boot):
    pool, slot, group = identity_parts(identity, prefix)
    if not boot or record.get("boot_id") != boot: raise InvalidInventory("owner is from another boot")
    phase = record.get("phase", {}).get("state")
    if "launch" in record:
        if pool != "-host": raise InvalidInventory("native owner is outside its host executor pool")
        launch = record.get("launch", {})
        if launch.get("instance_id") != identity or phase not in ("prepared", "running", "retiring") or not re.fullmatch(r"[a-f0-9]{32}", record.get("nonce", "")):
            raise InvalidInventory("native executor lacks live private owner")
        if phase == "running" and (type(record.get("phase", {}).get("pid")) is not int or record["phase"]["pid"] <= 0):
            raise InvalidInventory("native executor lacks owner PID")
        spec, generation, runtime = launch.get("spec", {}), None, "process"
    else:
        if pool == "-host": raise InvalidInventory("container owner is in the native host pool")
        if record.get("instance_id") != identity or phase not in ("owned", "retiring") or not re.fullmatch(r"[a-f0-9]{32}", record.get("generation", "")):
            raise InvalidInventory("container executor lacks live owned generation")
        spec, generation, runtime = record.get("spec", {}), record["generation"], "shared-runc" if pool else "runc"
    cgroup = spec.get("linux", {}).get("cgroupsPath")
    allowed = {"/reliaburger/"+group, "/reliaburger/"+group+"/helper"}
    if pool=="-host" and "launch" in record and cgroup=="/unused":
        args=spec.get("process",{}).get("args",[])
        if len(args)!=3 or args[2]!="/sys/fs/cgroup/reliaburger/"+group+"/helper/cgroup.procs" or not args[0].endswith("/host-executors/"+identity+"/helper"):
            raise InvalidInventory("native helper launch and cgroup identity disagree")
    elif cgroup not in allowed: raise InvalidInventory("executor owner and cgroup identity disagree")
    return dict(id=identity, runtime=runtime, cgroup=group, generation=generation, boot=boot)


def veth(identity):
    raw = "veth-"+identity+"-h"
    if len(raw.encode()) <= 15: return raw
    digest = 0xcbf29ce484222325
    for byte in identity.encode(): digest = ((digest ^ byte)*0x100000001b3) & ((1<<64)-1)
    return "veth-%010x" % (digest & 0xffffffffff)


def command(args):
    result = subprocess.run(args, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=15)
    if result.returncode or len(result.stdout) > (1<<20): raise InvalidInventory("kernel/storage inventory command failed: "+args[0])
    return result.stdout.decode()


def entries(path):
    if not path.exists(): return []
    with os.scandir(path) as directory:
        result=[]
        for entry in directory:
            result.append(entry.name)
            if len(result)>LIMIT: raise InvalidInventory("inventory cardinality bound exceeded")
    return sorted(result)


def kernel(root):
    state = root/"instances/runc/state"
    runc = command(["runc", "--root", str(state), "list", "-q"]).splitlines() if state.exists() else []
    netns = entries(Path("/run/netns"))
    links = command(["ip", "-j", "link", "show", "type", "veth"])
    groups = Path("/sys/fs/cgroup/reliaburger")
    cgroups=[]
    for namespace in entries(groups):
        if not (groups/namespace).is_dir(): continue
        for app in entries(groups/namespace):
            if not (groups/namespace/app).is_dir(): continue
            for slot in entries(groups/namespace/app):
                path=groups/namespace/app/slot
                if path.is_dir(): cgroups.append(namespace+"/"+app+"/"+slot)
                if len(cgroups)>LIMIT: raise InvalidInventory("cgroup inventory exceeds bound")
    leasepath=root/"instances/runc/bundles/.network-leases.json"
    leases = list(safe_json(leasepath, root, os.geteuid()).get("allocations", {})) if leasepath.exists() else []
    return dict(runc=runc, netns=netns, veth=[v["ifname"] for v in json.loads(links)], cgroup=cgroups, lease=leases)


def collect(root):
    root = Path(root)
    boot = Path("/proc/sys/kernel/random/boot_id").read_text().strip()
    prefixpath=root/"batch-executor-id"
    prefix = safe_bytes(prefixpath, root, os.geteuid(), 32).decode() if prefixpath.exists() else None
    errors=[]; owners={}; resources={}
    # A disappearing owner/resource is normal churn. Recheck positive absence;
    # never excuse a still-present kernel object solely because its name fits.
    for attempt in range(3):
        resources=kernel(root); owners={}; errors=[]
        candidates=set(resources["runc"]+resources["lease"])
        journals={}
        for base in (root/"instances/runc/bundles/.intents/records",root/"instances/process-owners"):
            for identity in entries(base):
                if not prefix or not identity.startswith("default__executor-"+prefix+"-"): continue
                filename="owner.json" if base.name=="process-owners" else "intent.json"
                path=base/identity/filename
                try:
                    record=safe_json(path,root,os.geteuid())
                    live=record.get("phase",{}).get("state") in ("owned","prepared","running","retiring")
                    if "launch" in record:
                        live=live or (Path("/tmp")/("rbp-"+str(os.geteuid())+"-"+record.get("nonce",""))/"control.sock").exists()
                    if "-host-" in identity:
                        live=live or (Path("/tmp")/("rbhx-"+hashlib.sha256(identity.encode()).hexdigest())).exists()
                    if "-reuse-" in identity:
                        live=live or (root/"instances/runc/bundles/.executors"/identity/"control").exists()
                    if live: candidates.add(identity); journals[identity]=(path,record)
                except InvalidInventory:
                    # An unsafe record cannot justify resources. When it still
                    # exists after the bounded retry, make it an explicit failure.
                    if path.exists(): candidates.add(identity)
        candidates.update(n[3:] for n in resources["netns"] if n.startswith("rb-"))
        for group in resources["cgroup"]:
            parts=group.split("/")
            if len(parts)==3 and parts[1].startswith("executor-"):
                candidates.add(parts[0]+"__"+parts[1]+"-"+parts[2])
        for identity in candidates:
            if not identity.startswith("default__executor-"): continue
            try:
                pool, _, group=identity_parts(identity, prefix or "")
                native=root/"instances/process-owners"/identity/"owner.json"
                container=root/"instances/runc/bundles/.intents/records"/identity/"intent.json"
                journal=native if native.exists() and (pool=="-host" or not container.exists()) else container
                record=journals.get(identity,(journal,None))[1] or safe_json(journal,root,os.geteuid())
                proof=owner_proof(record,identity,prefix,boot)
                if "launch" in record:
                    socketpath=Path("/tmp")/("rbp-"+str(os.geteuid())+"-"+record["nonce"])/"control.sock"
                    proof["owner_socket_present"]=socketpath.exists()
                path=Path("/sys/fs/cgroup/reliaburger")/group
                # Limits are evidence of the actual cgroup configuration, not a
                # substitute for the scheduler's resource commitment ledger.
                proof["limits"]={key:(path/key).read_text().strip() for key in ("cpu.max","memory.max")} if path.exists() else {}
                owners[identity]=proof
            except (InvalidInventory,OSError) as error: errors.append(identity+": "+str(error))
        current=kernel(root)
        resources={kind:sorted(set(values)&set(current[kind])) for kind,values in resources.items()}
        alive=set(resources["runc"]+resources["lease"]+[n[3:] for n in resources["netns"] if n.startswith("rb-")])
        alive.update(g.split("/")[0]+"__"+g.split("/")[1]+"-"+g.split("/")[2] for g in resources["cgroup"])
        # Active journals also count after the last kernel object disappears:
        # terminal publication and owner-socket retirement must finish too.
        alive.update(journals)
        errors=[e for e in errors if e.split(":",1)[0] in alive or e.split(":",1)[0] in candidates and "unsafe" in e]
        owners={i:o for i,o in owners.items() if i in alive}
        if not errors: break
    sizes={}
    for name in ("task-arrays","job-state","instances"):
        path=root/name
        sizes[name]=int(command(["du","-skx",str(path)]).split()[0]) if path.exists() else 0
    space=os.statvfs(root)
    return dict(schema=1,ts=int(time.time()),complete=True,boot=boot,prefix=prefix,
                owners=list(owners.values()),resources=resources,errors=errors,storage_kb=sizes,
                available_kb=space.f_bavail*space.f_frsize//1024)


def fresh_activity(root):
    root=Path(root); boot=Path("/proc/sys/kernel/random/boot_id").read_text().strip()
    prefixpath=root/"batch-executor-id"
    if not prefixpath.exists(): return []
    prefix=safe_bytes(prefixpath,root,os.geteuid(),32).decode()
    base=root/"instances/runc/bundles/.intents/records"; active=[]
    for identity in entries(base):
        if not re.fullmatch(r"default__executor-"+re.escape(prefix)+r"-[0-9]+",identity): continue
        try:
            record=safe_json(base/identity/"intent.json",root,os.geteuid())
            proof=owner_proof(record,identity,prefix,boot)
            env=dict(row.split("=",1) for row in record["spec"]["process"].get("env",[]) if "=" in row)
            run=int(env["RELIABURGER_BATCH_ID"]); index=int(env["RELIABURGER_TASK_INDEX"])
            name=env["SOAK_EFFECT_RUN"]
            path=Path("/sys/fs/cgroup/reliaburger")/proof["cgroup"]
            if "populated 1" not in (path/"cgroup.events").read_text(): continue
            ticks=[]
            for pid in (path/"cgroup.procs").read_text().splitlines():
                try:
                    # comm can contain spaces: fields after its final ')' start at 3.
                    fields=Path("/proc",pid,"stat").read_text().rsplit(")",1)[1].split()
                    ticks.append(int(fields[19]))
                except (OSError,ValueError,IndexError): pass
            if ticks:
                active.append(dict(proof,run=run,index=index,name=name,ticks=ticks))
        except (InvalidInventory,OSError,ValueError,KeyError): continue
    return active


def validate(value, now, boot=None):
    if not isinstance(value,dict) or value.get("schema")!=1 or value.get("complete") is not True:
        raise InvalidInventory("complete job kernel/storage inventory missing")
    if not 0 <= now-value.get("ts",0) <= 90 or boot and value.get("boot")!=boot:
        raise InvalidInventory("job inventory stale or from another boot")
    if value.get("errors"): raise InvalidInventory("; ".join(value["errors"][:4]))
    if type(value.get("available_kb")) is not int or value["available_kb"] < 256*1024:
        raise InvalidInventory("job storage has less than 256 MiB free")
    if len(value.get("owners",[]))>320: raise InvalidInventory("owner inventory exceeds executor pools")
    return value


def exemptions(value):
    result={kind:set() for kind in ("runc","netns","lease","veth","cgroup")}
    for owner in value["owners"]:
        identity=owner["id"]
        _, _, group=identity_parts(identity,value["prefix"])
        if owner["boot"]!=value["boot"] or owner["cgroup"]!=group: raise InvalidInventory("invalid published ownership")
        expected="process" if "-host-" in identity else "shared-runc" if "-reuse-" in identity else None
        if owner.get("runtime") not in ("process","runc","shared-runc") or expected and owner["runtime"]!=expected:
            raise InvalidInventory("owner runtime disagrees with its pool")
        if owner["runtime"]!="process" and not re.fullmatch(r"[a-f0-9]{32}",owner.get("generation", "")):
            raise InvalidInventory("published owner lacks exact generation")
        result["cgroup"].add(group)
        if owner["runtime"]!="process":
            result["runc"].add(identity); result["netns"].add("rb-"+identity)
            result["lease"].add(identity); result["veth"].add(veth(identity))
    return result


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root",type=Path,default=Path("/var/lib/reliaburger/data"))
    options=parser.parse_args()
    try:
        value=collect(options.root)
        for kind,values in value["resources"].items():
            for item in values: print(kind,item)
    except (InvalidInventory,OSError,subprocess.SubprocessError,ValueError) as error:
        value=dict(schema=1,complete=False,ts=int(time.time()),errors=[str(error)])
    print("@@ jobs")
    print(json.dumps(value,separators=(",",":")))
    print("@@ inventory")
    return 0
if __name__=="__main__": raise SystemExit(main())
