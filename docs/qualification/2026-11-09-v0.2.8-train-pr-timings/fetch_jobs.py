"""Fetch the jobs of every CI run on the 0.2.7 and 0.2.8 trains (#1723).

Reads runs-*.jsonl and prs.json beside it, writes jobs/<run id>-<attempt>.json.
"""
import json
import subprocess
import sys
from pathlib import Path

HERE = Path(sys.argv[1])
TRAINS = {"release-0-2-7", "release-0-2-8"}


def base_of(run, heads):
    branch = run["head_branch"] or ""
    if branch.startswith("gh-readonly-queue/"):
        return branch.split("/")[1]
    if run["prs"]:
        return run["prs"][0]["base"]
    return heads.get(branch)


def main():
    heads = {}
    for pr in json.loads((HERE / "prs.json").read_text()):
        heads.setdefault(pr["headRefName"], pr["baseRefName"])
    out = HERE / "jobs"
    out.mkdir(exist_ok=True)
    chosen = []
    for event in ("pull_request", "merge_group"):
        for line in (HERE / f"runs-{event}.jsonl").read_text().splitlines():
            run = json.loads(line)
            run["base"] = base_of(run, heads)
            if run["base"] in TRAINS and run["status"] == "completed":
                chosen.append(run)
    (HERE / "chosen.json").write_text(json.dumps(chosen))
    for run in chosen:
        path = out / f"{run['id']}-{run['run_attempt']}.json"
        if path.exists():
            continue
        jobs = subprocess.run(
            ["gh", "api", "--paginate",
             f"repos/reliaburger/reliaburger/actions/runs/{run['id']}/attempts/{run['run_attempt']}/jobs?per_page=100",
             "-q", ".jobs[]|{name,status,conclusion,created_at,started_at,completed_at,runner_name}"],
            check=True, capture_output=True, text=True).stdout
        path.write_text("[" + ",".join(jobs.split("\n")[:-1]) + "]")
    print(len(chosen), "runs")


main()
