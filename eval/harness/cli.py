"""rbeval: one entry point for setup, lab lifecycle, runs and reports."""

from __future__ import annotations

import argparse
import datetime as dt
import json
import os
import subprocess
import sys
from pathlib import Path

from .core import ROOT, Config, Redactor, RunLog


def _log(cfg: Config, name: str = "_cli") -> RunLog:
    return RunLog(ROOT / "results" / name, Redactor(cfg.safety["secrets"]["patterns"]))


def cmd_setup(args, cfg: Config) -> int:
    from . import product
    from .lab import Lab
    log = _log(cfg)
    missing = [t for t in ("virsh", "virt-install", "qemu-img", "xorriso", "ssh", "docker", "kind", "kubectl", "curl")
               if subprocess.run(["bash", "-c", f"command -v {t}"], capture_output=True).returncode]
    if missing:
        print(f"missing host tools: {missing}", file=sys.stderr)
        return 1
    paths = product.fetch(cfg, log)
    img = Lab(cfg, log).cached_image()
    print(json.dumps({"product": {k: str(v) for k, v in paths.items()}, "image": str(img)}, indent=2))
    return 0


def cmd_lab(args, cfg: Config) -> int:
    from .cluster import Cluster
    from .lab import Lab
    log = _log(cfg)
    lab = Lab(cfg, log)
    cluster = Cluster(cfg, log, lab)
    if args.action == "up":
        print(json.dumps(lab.up(), indent=2))
        if args.cluster:
            print(json.dumps(cluster.provision(), indent=2, default=str))
    elif args.action == "down":
        lab.down()
        cluster.forget()
    elif args.action == "purge":
        lab.purge()
        cluster.forget()
    elif args.action == "status":
        print(json.dumps({n.name: lab.domain_state(n) for n in lab.nodes}, indent=2))
        if cluster.token_path.exists():
            print(json.dumps(cluster.health(), indent=2))
    elif args.action == "env":
        print(f"export RELIABURGER_ENDPOINT=https://{lab.nodes[0].ip}:9117")
        print(f"export RELIABURGER_CA_CERT={cluster.ca_path}")
        print(f"export RELIABURGER_TOKEN=\"$(cat {cluster.token_path})\"")
    return 0


def cmd_run(args, cfg: Config) -> int:
    run_id = args.run_id or (dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%SZ") + "-" + cfg.product_key)
    run_dir = ROOT / "results" / run_id
    env = os.environ.copy()
    env["RBEVAL_RUN_ID"] = run_id
    env["RBEVAL_PRODUCT"] = cfg.product_key
    if args.keep_lab:
        env["RBEVAL_KEEP_LAB"] = "1"
    argv = [sys.executable, "-m", "behave", "--no-capture", "--no-capture-stderr", "-f", "plain",
            "-o", str(run_dir / "behave.txt"), "-f", "pretty",
            "--junit", "--junit-directory", str(run_dir / "junit")]
    for t in args.tags or []:
        argv += ["--tags", t]
    argv += args.paths or [f"features/{a}" for a in ("vision", "extensibility", "interoperability", "operations")]
    rc = subprocess.call(argv, cwd=ROOT, env=env)
    if (run_dir / "results.json").exists():
        from .report import build_reports
        build_reports(run_dir)
    return rc


def cmd_report(args, cfg: Config) -> int:
    from .report import build_reports
    run_dir = Path(args.run_dir) if args.run_dir else max((ROOT / "results").glob("2*"), key=lambda p: p.stat().st_mtime)
    build_reports(run_dir)
    print(run_dir)
    return 0


def cmd_claims(args, cfg: Config) -> int:
    from .claims import Claims
    c = Claims(cfg)
    bad = 0
    for cid in c.by_id:
        for chk in c.verify(cid):
            if not chk.ok:
                bad += 1
                print(f"BAD {cid} {chk.ref}: {chk.detail}")
    print(f"{len(c.by_id)} claims, {bad} unverified citations at {cfg.product['docs_ref']}")
    return 1 if bad else 0


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(prog="rbeval", description=__doc__)
    p.add_argument("--product", help="product target from config/product.yaml (default: its `default`)")
    sub = p.add_subparsers(dest="cmd", required=True)
    sub.add_parser("setup", help="check host tools, download and verify the build and the guest image")
    lab = sub.add_parser("lab", help="lab lifecycle")
    lab.add_argument("action", choices=["up", "down", "purge", "status", "env"])
    lab.add_argument("--cluster", action="store_true", help="with `up`: also build the cluster")
    run = sub.add_parser("run", help="run scenarios (behave)")
    run.add_argument("paths", nargs="*")
    run.add_argument("--tags", action="append", help="behave tag expression, e.g. smoke or 'not @quickstart'")
    run.add_argument("--keep-lab", action="store_true", help="leave VMs running after the run")
    run.add_argument("--run-id")
    rep = sub.add_parser("report", help="rebuild reports for a run directory")
    rep.add_argument("run_dir", nargs="?")
    sub.add_parser("claims", help="verify every claim citation at the pinned docs ref")
    args = p.parse_args(argv)
    cfg = Config.load(args.product)
    return {"setup": cmd_setup, "lab": cmd_lab, "run": cmd_run, "report": cmd_report, "claims": cmd_claims}[args.cmd](args, cfg)


if __name__ == "__main__":
    sys.exit(main())
