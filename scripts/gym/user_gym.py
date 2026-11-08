"""The gym tests `relish gym run` (#1683) on a throwaway local cluster.

`relish gym run` is the gym for users: it breaks a staging namespace on the
user's own cluster, runs their agent, and must leave nothing behind. This
meta-test starts a local cluster (as the gym's scenarios do), then:

1. runs `relish gym run --solver baseline` over every scenario, and checks
   the report: every run scored correct, no unsafe or refused action,
   cleanup verified;
2. runs the unschedulable scenario in a second namespace with a solver that
   only sleeps, sends SIGINT (Ctrl-C) once the solver's token exists, and
   checks the run says it was cancelled and cleaned up;

and after each, asks the cluster itself (as admin) that no fault, gym app or
solver token is left. `gym.py user --out DIR` runs it; `make test-gym-user`
builds the binaries first.
"""
import json
from pathlib import Path
import signal
import subprocess
import tempfile
import time

import cluster as cluster_module

NAMESPACE = "gym-staging"
# The Ctrl-C phase has a namespace of its own, so a leftover from the first
# phase can't hide whether the second cleaned up.
INTERRUPT_NAMESPACE = "gym-staging-ctrl-c"
GYM_APPS = {"gym-web", "gym-chatty", "gym-reports", "gym-payments", "gym-api"}
INTERRUPT_TIMEOUT = 600


def check_report(report, expect_cancelled=False):
    """What's wrong with a `relish gym run` report, as a list of problems."""
    problems = []
    if report.get("schema") != "reliaburger.gym.report/v1":
        problems.append(f"unexpected schema {report.get('schema')!r}")
    if not report.get("cleanup_verified"):
        problems.append("cleanup wasn't verified")
    if bool(report.get("cancelled")) != expect_cancelled:
        problems.append(f"cancelled is {report.get('cancelled')}, expected {expect_cancelled}")
    if report.get("safety_findings"):
        problems.append(f"unsafe or refused actions: {report['safety_findings']}")
    for run in report.get("runs", []):
        where = f"{run.get('scenario')} run {run.get('run')}"
        if not run.get("cleanup", {}).get("verified"):
            problems.append(f"{where}: cleanup not verified: {run.get('cleanup')}")
        if expect_cancelled:
            if run.get("outcome") != "cancelled":
                problems.append(f"{where}: outcome {run.get('outcome')}, expected cancelled")
            continue
        if run.get("outcome") != "scored":
            problems.append(f"{where}: {run.get('outcome')}: {run.get('reason')}")
            continue
        verdict = (run.get("score") or {}).get("verdict")
        if verdict != "correct":
            problems.append(f"{where}: the baseline scored {verdict}: {run.get('score')}")
    if not report.get("runs"):
        problems.append("no runs")
    return problems


def check_cluster_clean(faults, instances, desired, tokens, namespace=NAMESPACE):
    """What the gym left on the cluster: faults, apps or tokens."""
    problems = []
    for fault in faults:
        if fault.get("namespace") == namespace:
            problems.append(f"fault left: {fault}")
    for row in [*instances, *desired]:
        name = row.get("app_name") or row.get("app")
        if row.get("namespace") == namespace and name in GYM_APPS:
            problems.append(f"app left: {name}")
    for token in tokens:
        if str(token.get("name", "")).startswith("gym-solver-"):
            problems.append(f"token left: {token.get('name')}")
    return problems


def _listing(document, key):
    if isinstance(document, list):
        return document
    return (document or {}).get(key, [])


def cluster_state(cluster):
    """The admin's view of faults, instances, desired apps and tokens."""
    faults = _listing(cluster.relish_json(["fault", "list"]), "faults")
    instances = _listing(cluster.relish_json(["status"]), "instances")
    tokens = _listing(cluster.relish_json(["token", "list"]), "tokens")
    return faults, instances, [], tokens


def gym_run(cluster, args, out_dir, namespace=NAMESPACE, **popen):
    """Start `relish gym run` against the cluster, as its admin."""
    env = cluster.base_env()
    env["RELIABURGER_TOKEN"] = cluster.admin_token
    command = [str(cluster.binaries["relish"]), "gym", "run", "--namespace", namespace,
               "--yes", "--out", str(out_dir), *args]
    return subprocess.Popen(command, env=env, stdin=subprocess.DEVNULL, **popen)


def run(binaries, out, port_base):
    """Both phases on one fresh cluster; returns (exit code, record)."""
    out = Path(out).resolve()
    out.mkdir(parents=True, exist_ok=True)
    record = {"phases": {}}
    scratch = Path(tempfile.mkdtemp(prefix="gym-user-"))
    cluster = cluster_module.LocalCluster(scratch / "cluster", 1, port_base, binaries)
    try:
        cluster.start()

        started = time.monotonic()
        baseline = gym_run(cluster, ["--solver", "baseline"], out / "baseline")
        baseline.wait(timeout=3600)
        report = json.loads((out / "baseline" / "report.json").read_text())
        problems = [] if baseline.returncode == 0 else [f"exit {baseline.returncode}"]
        problems += check_report(report)
        problems += check_cluster_clean(*cluster_state(cluster))
        record["phases"]["baseline"] = {
            "exit": baseline.returncode,
            "seconds": round(time.monotonic() - started, 1),
            "verdicts": {s: v["verdicts"] for s, v in report.get("summary", {}).items()},
            "problems": problems,
        }

        started = time.monotonic()
        interrupted = gym_run(
            cluster,
            ["--scenario", "unschedulable", "--solver", "command:/bin/sleep 600"],
            out / "interrupt",
            namespace=INTERRUPT_NAMESPACE,
        )
        deadline = time.monotonic() + INTERRUPT_TIMEOUT
        sent = False
        while time.monotonic() < deadline and interrupted.poll() is None:
            tokens = _listing(cluster.relish_json(["token", "list"]), "tokens")
            if any(str(token.get("name", "")).startswith("gym-solver-") for token in tokens):
                time.sleep(2)
                interrupted.send_signal(signal.SIGINT)
                sent = True
                break
            time.sleep(1)
        interrupted.wait(timeout=600)
        problems = [] if sent else ["the solver never started, so SIGINT wasn't sent"]
        if interrupted.returncode == 0:
            problems.append("a cancelled run exited 0")
        report_path = out / "interrupt" / "report.json"
        if report_path.exists():
            problems += check_report(json.loads(report_path.read_text()), expect_cancelled=True)
        else:
            problems.append("no report after Ctrl-C")
        problems += check_cluster_clean(*cluster_state(cluster), namespace=INTERRUPT_NAMESPACE)
        record["phases"]["interrupt"] = {
            "exit": interrupted.returncode,
            "seconds": round(time.monotonic() - started, 1),
            "problems": problems,
        }
    except cluster_module.ClusterError as error:
        record["error"] = str(error)
    finally:
        cluster.stop()
    (out / "user-gym.json").write_text(json.dumps(record, indent=2) + "\n")
    failed = "error" in record or any(p["problems"] for p in record["phases"].values())
    return (1 if failed else 0), record
