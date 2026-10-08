#!/usr/bin/env python3
"""Measure unique accepted successes beside a live service, through the public CLI.

Run on an isolated qualification cluster using matching development binaries.
This is a sustained measurement harness, not a substitute for runtime, failover,
storage-fault and recovery gates. No fault is injected by this script.
"""
import argparse
import hashlib
import importlib.util
import os
import platform
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


def finish_report(*, elapsed, requested_seconds, successes, failures, retries,
                  probes, unavailable, completed, active, headroom):
    if elapsed <= 0:
        raise ValueError('elapsed time must be positive')
    rate = successes / elapsed
    target = 100_000_000 / 86400 * headroom
    return dict(elapsed_seconds=elapsed, requested_seconds=requested_seconds,
                measurement_complete=elapsed >= requested_seconds,
                unique_accepted_successes=successes, terminal_failures=failures,
                accepted_retries=retries, successes_per_second=rate,
                target_with_headroom=target,
                throughput_pass=elapsed >= 86400 and rate >= target and failures == 0 and unavailable == 0,
                qualification_pass=False,
                application_probes=probes, application_failures=unavailable,
                completed_submissions=completed, active_submissions=list(active),
                requires_evidence=['hardware/runtime/image-warmth', 'resource and storage bounds',
                                   'worker loss', 'leader failover', 'delayed control', 'storage failure'])


def existing_window_report(*, elapsed, requested_seconds, initial, final,
                           probes, unavailable, batch_id, done, headroom):
    if any(now < before for now, before in zip(final, initial)):
        raise ValueError('accepted counters went backwards')
    successes, failures, retries = (now - before for now, before in zip(final, initial))
    report = finish_report(elapsed=elapsed, requested_seconds=requested_seconds,
        successes=successes, failures=failures, retries=retries, probes=probes,
        unavailable=unavailable, completed=int(done), active=[] if done else [batch_id],
        headroom=headroom)
    report.update(existing_batch_id=batch_id, initial_accepted_counts=list(initial),
                  measurement_ownership='read-only; submission remains owned by its caller')
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("manifest", type=pathlib.Path)
    parser.add_argument("--relish", default="relish", help="CLI binary plus explicit endpoint/CA arguments")
    parser.add_argument("--app-url", required=True, help="Service readiness/order URL on the qualification cluster")
    parser.add_argument("--seconds", type=int, default=86400)
    parser.add_argument("--existing-batch", type=int, help="Observe an existing submission without submitting or cancelling work")
    parser.add_argument("--window", type=int, default=4, choices=range(1, 5))
    parser.add_argument("--headroom", type=float, default=1.2)
    parser.add_argument("--output", type=pathlib.Path, default=pathlib.Path("job-qualification"))
    parser.add_argument('--observe-dir', action='append', default=[], type=pathlib.Path)
    parser.add_argument('--observe-pid', action='append', default=[], type=int)
    options = parser.parse_args()
    if len(options.observe_pid) > 8 or any(pid <= 1 for pid in options.observe_pid):
        parser.error('observe at most eight positive process identities')
    module = importlib.util.spec_from_file_location('measurement', pathlib.Path(__file__).with_name('measure-jobs.py'))
    measurement = importlib.util.module_from_spec(module)
    module.loader.exec_module(measurement)
    process_starts = {pid: measurement.process_start(pid) for pid in options.observe_pid}
    if options.seconds < 1 or options.headroom < 1:
        parser.error("seconds must be positive and headroom at least 1")
    options.output.mkdir(parents=True, exist_ok=False)
    source = options.manifest.read_text()
    (options.output / "workload.toml").write_text(source)
    prefix = shlex.split(options.relish)
    trackers = {}
    initial_existing = None
    active = {}
    if options.existing_batch is not None:
        if options.existing_batch <= 0:
            parser.error('existing batch identity must be positive')
        summary = cli(prefix, 'batch-status', str(options.existing_batch))
        tracker = measurement.AcceptedCounts(options.existing_batch)
        initial_existing = tracker.observe(summary)
        active[options.existing_batch] = initial_existing
        trackers[options.existing_batch] = tracker
    started = time.monotonic()
    deadline = started + options.seconds
    completed = 0
    last_batch_id = 0
    successes = failures = retries = probes = unavailable = 0
    report = dict(qualification_pass=False, reason='Requires separate recovery, fault and bounded-cost evidence',
                  kernel=platform.release(), architecture=platform.machine(), logical_cpus=os.cpu_count(),
                  workload_sha256=hashlib.sha256(source.encode()).hexdigest(),
                  command_prefix=prefix, timing=('Monotonic read-only window; the existing submission remains running' if options.existing_batch is not None else 'Monotonic measurement window; active work is cancelled after the cutoff'))
    latencies = []
    observations = []
    next_observation = 0
    try:
        with (options.output / "samples.jsonl").open("w") as samples:
            while time.monotonic() < deadline:
                while options.existing_batch is None and len(active) < options.window:
                    answer = cli(prefix, "batch", "submit", str(options.manifest.resolve()))
                    batch_id = answer["batch_id"]
                    if batch_id <= last_batch_id:
                        raise RuntimeError("submission reused a task identity")
                    last_batch_id = batch_id
                    active[batch_id] = (0, 0, 0)
                    trackers[batch_id] = measurement.AcceptedCounts(batch_id)
                for batch_id in list(active):
                    summary = cli(prefix, "batch-status", str(batch_id))
                    now_counts = trackers[batch_id].observe(summary)
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
                        del trackers[batch_id]
                try:
                    probe_started = time.monotonic()
                    with urllib.request.urlopen(options.app_url, timeout=5) as response:
                        if response.status != 200:
                            raise RuntimeError("application returned non-200")
                        response.read(65536)
                    latencies.append((time.monotonic() - probe_started) * 1000)
                    probes += 1
                except Exception:
                    unavailable += 1
                    raise
                elapsed = time.monotonic() - started
                if elapsed >= next_observation:
                    storage = {}
                    for path in options.observe_dir:
                        row = measurement.bounded_storage(path)
                        filesystem = os.statvfs(path)
                        row['filesystem_available_bytes'] = filesystem.f_bavail * filesystem.f_frsize
                        row['filesystem_total_bytes'] = filesystem.f_blocks * filesystem.f_frsize
                        storage[str(path)] = row
                    observation = dict(elapsed_seconds=elapsed, storage=storage,
                                       processes=[measurement.process_observation(pid, start) for pid, start in process_starts.items()])
                    observations.append(observation)
                    samples.write(json.dumps(dict(observation=observation)) + '\n')
                    next_observation = elapsed + 30
                    print(f'{elapsed:.1f}s: {successes:,} unique accepted successes, {failures} failures, {retries} retries; '
                          f'{successes / max(elapsed, 1e-9):.1f}/s; {probes} successful service probes', flush=True)
                samples.flush()
                time.sleep(min(1, max(0, deadline - time.monotonic())))
        elapsed = time.monotonic() - started
        report.update(finish_report(elapsed=elapsed, requested_seconds=options.seconds,
                      successes=successes, failures=failures, retries=retries, probes=probes,
                      unavailable=unavailable, completed=completed, active=active, headroom=options.headroom))
        if initial_existing is not None:
            final_existing = tuple(before + delta for before, delta in
                                   zip(initial_existing, (successes, failures, retries)))
            report.update(existing_window_report(elapsed=elapsed, requested_seconds=options.seconds,
                initial=initial_existing, final=final_existing, probes=probes, unavailable=unavailable,
                batch_id=options.existing_batch, done=completed > 0, headroom=options.headroom))
        report['service_latency_ms'] = dict(samples=len(latencies), maximum=max(latencies, default=None),
                     p95=sorted(latencies)[min(len(latencies)-1, int(len(latencies)*0.95))] if latencies else None)
        report['resource_observations'] = observations
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
        for batch_id in active if options.existing_batch is None else []:
            try:
                result = subprocess.run(prefix + ["batch", "cancel", str(batch_id)], timeout=30, check=False)
                if result.returncode:
                    cleanup_errors.append(batch_id)
            except Exception:
                cleanup_errors.append(batch_id)
        report["cleanup_failed_submissions"] = cleanup_errors
        (options.output / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))
    return 0 if report.get("measurement_complete") and not report.get("error") and not report.get("cleanup_failed_submissions") else 1


if __name__ == "__main__":
    raise SystemExit(main())
