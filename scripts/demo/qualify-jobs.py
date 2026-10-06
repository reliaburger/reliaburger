#!/usr/bin/env python3
"""Measure unique accepted successes beside a live service, through the public CLI.

Run on an isolated qualification cluster using matching development binaries.
This is a sustained measurement harness, not a substitute for runtime, failover,
storage-fault and recovery gates. No fault is injected by this script.
"""
import argparse
import json
import pathlib
import shlex
import subprocess
import time
import urllib.request


def cli(prefix, *args):
    result = subprocess.run(prefix + ["--output", "json"] + list(args), check=True,
                            stdout=subprocess.PIPE, text=True, timeout=30)
    return json.loads(result.stdout)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("manifest", type=pathlib.Path)
    parser.add_argument("--relish", default="relish", help="CLI binary plus explicit endpoint/CA arguments")
    parser.add_argument("--app-url", required=True, help="Service readiness/order URL on the qualification cluster")
    parser.add_argument("--seconds", type=int, default=86400)
    parser.add_argument("--window", type=int, default=4, choices=range(1, 5))
    parser.add_argument("--headroom", type=float, default=1.2)
    parser.add_argument("--output", type=pathlib.Path, default=pathlib.Path("job-qualification"))
    options = parser.parse_args()
    if options.seconds < 1 or options.headroom < 1:
        parser.error("seconds must be positive and headroom at least 1")
    options.output.mkdir(parents=True, exist_ok=False)
    source = options.manifest.read_text()
    (options.output / "workload.toml").write_text(source)
    prefix = shlex.split(options.relish)
    started = time.monotonic()
    deadline = started + options.seconds
    active = {}
    completed = 0
    last_batch_id = 0
    successes = failures = retries = probes = unavailable = 0
    report = {"qualification_pass": False, "reason": "Requires separate recovery, fault and bounded-cost evidence"}
    try:
        with (options.output / "samples.jsonl").open("w") as samples:
            while time.monotonic() < deadline:
                while len(active) < options.window:
                    answer = cli(prefix, "batch", "submit", str(options.manifest.resolve()))
                    batch_id = answer["batch_id"]
                    if batch_id <= last_batch_id:
                        raise RuntimeError("submission reused a task identity")
                    last_batch_id = batch_id
                    active[batch_id] = (0, 0, 0)
                for batch_id in list(active):
                    summary = cli(prefix, "batch-status", str(batch_id))
                    now_counts = tuple(summary[k] for k in ("succeeded", "failed", "retried"))
                    previous = active[batch_id]
                    if any(now < old for now, old in zip(now_counts, previous)):
                        raise RuntimeError("accepted counters went backwards")
                    successes += now_counts[0] - previous[0]
                    failures += now_counts[1] - previous[1]
                    retries += now_counts[2] - previous[2]
                    active[batch_id] = now_counts
                    # Keep aggregate distributions and requests, without copying
                    # every node counter into every one-second observation.
                    summary.pop("nodes", None)
                    for profile in summary.get("cohorts", []):
                        profile.pop("nodes", None)
                    samples.write(json.dumps({"elapsed_seconds": time.monotonic() - started,
                                              "summary": summary}) + "\n")
                    if summary["done"]:
                        completed += 1
                        del active[batch_id]
                try:
                    with urllib.request.urlopen(options.app_url, timeout=5) as response:
                        if response.status != 200:
                            raise RuntimeError("application returned non-200")
                        response.read(65536)
                    probes += 1
                except Exception:
                    unavailable += 1
                    raise
                samples.flush()
                time.sleep(min(1, max(0, deadline - time.monotonic())))
        elapsed = time.monotonic() - started
        rate = successes / elapsed
        target = 100_000_000 / 86400 * options.headroom
        report.update({"elapsed_seconds": elapsed, "unique_accepted_successes": successes,
                       "terminal_failures": failures, "accepted_retries": retries,
                       "successes_per_second": rate, "target_with_headroom": target,
                       "throughput_pass": elapsed >= 86400 and rate >= target,
                       "application_probes": probes, "application_failures": unavailable,
                       "completed_submissions": completed, "active_submissions": list(active),
                       "requires_evidence": ["hardware/runtime/image-warmth", "resource and storage bounds",
                                             "worker loss", "leader failover", "delayed control", "storage failure"]})
    except Exception as error:
        report.update({"error": str(error), "elapsed_seconds": time.monotonic() - started,
                       "unique_accepted_successes": successes, "terminal_failures": failures,
                       "accepted_retries": retries, "application_probes": probes,
                       "application_failures": unavailable, "completed_submissions": completed,
                       "active_submissions": list(active), "throughput_pass": False})
        raise
    finally:
        # End the harness's remaining work through its own stable parent IDs.
        # The caller retains the cluster and can export results before pruning.
        cleanup_errors = []
        for batch_id in active:
            try:
                result = subprocess.run(prefix + ["batch", "cancel", str(batch_id)], timeout=30, check=False)
                if result.returncode:
                    cleanup_errors.append(batch_id)
            except Exception:
                cleanup_errors.append(batch_id)
        report["cleanup_failed_submissions"] = cleanup_errors
        (options.output / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))
    return 0 if report.get("throughput_pass") else 1


if __name__ == "__main__":
    raise SystemExit(main())
