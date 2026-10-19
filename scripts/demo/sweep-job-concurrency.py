#!/usr/bin/env python3
"""Sweep cold and warmed public job throughput at matched resource requests.

The caller owns a dedicated node and must keep other measurements/builds idle.
One large submission stays active across each pair of windows. Every window
counts newly accepted outcomes only; cancellation/drain never adds credit.
"""
import argparse
import importlib.util
import json
import os
import pathlib
import shlex
import subprocess
import time

HERE = pathlib.Path(__file__).resolve().parent

def load(name, filename):
    spec = importlib.util.spec_from_file_location(name, HERE / filename)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module

tiers = load('tiers', 'record-job-tiers.py')
qualification = load('qualification', 'qualify-jobs.py')


def admission_bound(runtime, concurrency, cpu_request, job_cpu_budget):
    reservation = cpu_request + (10 if runtime != 'runc' else 0)
    slots = job_cpu_budget // reservation
    return min(concurrency, slots, 32 if runtime != 'runc' else 256)


def window_summary(report, samples):
    summaries = [row['summary'] for row in samples if 'summary' in row]
    result = {key: report.get(key) for key in ['requested_seconds', 'unique_accepted_successes',
        'terminal_failures', 'accepted_retries', 'application_failures', 'initial_accepted_counts',
        'last_accepted_sample_seconds', 'error', 'service_latency_ms']}
    def maximum(field):
        values = [s[field] for s in summaries if s.get(field) is not None]
        return max(values, default=None)
    result.update(successes_per_second=report['unique_accepted_successes'] / report['requested_seconds'],
        healthy=bool(report.get('window_pass')),
        maximum_sampled_active_commands=maximum('active_commands'),
        maximum_sampled_other_in_flight_attempts=maximum('other_in_flight_attempts'),
        maximum_sampled_held_tasks=maximum('held'))
    return result


def cancel_submission(prefix, batch_id):
    subprocess.run(prefix + ["batch", "cancel", str(batch_id)], check=True, timeout=30, stdout=subprocess.DEVNULL)
    return qualification.wait_cancelled(prefix, batch_id)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--relish', required=True)
    parser.add_argument('--image', required=True)
    parser.add_argument('--host-binary', required=True, type=pathlib.Path)
    parser.add_argument('--service-url', required=True)
    parser.add_argument('--observe-dir', required=True, type=pathlib.Path)
    parser.add_argument('--observe-pid', required=True, action='append', type=int)
    parser.add_argument('--output', required=True, type=pathlib.Path)
    parser.add_argument('--seconds', default=60, type=int)
    parser.add_argument('--concurrency', default=[1,4,8,16,27,32,48,64], nargs='+', type=int)
    parser.add_argument('--cpu-request', default=25, type=int)
    parser.add_argument('--job-cpu-budget', default=3000, type=int)
    parser.add_argument('--runtimes', default=tiers.RUNTIMES, nargs='+', choices=tiers.RUNTIMES)
    options = parser.parse_args()
    if options.seconds < 1 or not options.concurrency or any(not 1 <= c <= 256 for c in options.concurrency) or not 1 <= options.cpu_request <= 1000:
        parser.error('positive windows, concurrency 1–256 and request 1–1000m required')
    if '@sha256:' not in options.image or not options.host_binary.is_absolute():
        parser.error('requires digest-pinned image and absolute matching host executable')
    options.output.mkdir(parents=True, exist_ok=False)
    prefix = shlex.split(options.relish)
    report = dict(cpu_request_millicores=options.cpu_request, cpu_limit_millicores=1000,
        memory_bytes=33554432, window_seconds=options.seconds, rows=[], complete=False,
        methodology='Cold executors, warm image. Submit once, observe cold window after submit response, then immediately observe a second warm window with counter subtraction. Cancel/drain only afterwards. One active submission.',
        activity_caveat='Active command snapshots are sampled at node sync and may miss short commands; configured concurrency is a cap, not an observation of continuous occupancy.',
        reusable_executor_pool_limit=32, job_cpu_budget_millicores=options.job_cpu_budget,
        binary_sha256=__import__('hashlib').sha256(pathlib.Path(prefix[0]).read_bytes()).hexdigest())
    try:
        for runtime in options.runtimes:
            for concurrency in options.concurrency:
                tiers.wait_idle(options.observe_dir)
                dest = options.output / f'{runtime}-c{concurrency}'
                dest.mkdir()
                manifest = dest / 'workload.toml'
                manifest.write_text(tiers.build_manifest(runtime,16000000,options.image,options.host_binary,
                    concurrency=concurrency,cpu_request=options.cpu_request))
                submitted = qualification.cli(prefix, 'batch', 'submit', str(manifest.resolve()))
                batch_id = submitted['batch_id']
                (dest/'submission.json').write_text(json.dumps(submitted,indent=2)+'\n')
                row = dict(runtime=runtime,concurrency=concurrency,batch_id=batch_id,
                    admission_upper_bound=admission_bound(runtime,concurrency,options.cpu_request,options.job_cpu_budget),
                    receipt_chunk_size=1 if runtime=='runc' else 1000)
                report['rows'].append(row)
                try:
                    for phase in ['cold','warm']:
                        command = ['python3',str(HERE/'qualify-jobs.py'),str(manifest),'--relish',options.relish,
                            '--app-url',options.service_url,'--seconds',str(options.seconds),'--existing-batch',str(batch_id),
                            '--output',str(dest/phase),'--poll-seconds','0.25','--progress-seconds','30',
                            '--observe-dir',str(options.observe_dir)]
                        for pid in options.observe_pid:command += ['--observe-pid',str(pid)]
                        print(f'{runtime} concurrency {concurrency} {phase} started',flush=True)
                        with (dest/(phase+'.log')).open('x') as log:
                            result = subprocess.run(command,stdout=log,stderr=log)
                        window = json.loads((dest/phase/'report.json').read_text())
                        samples = [json.loads(line) for line in (dest/phase/'samples.jsonl').read_text().splitlines()]
                        row[phase] = window_summary(window,samples)
                        print(f'{runtime} concurrency {concurrency} {phase}: {row[phase]["successes_per_second"]:.1f}/s, healthy={row[phase]["healthy"]}',flush=True)
                        if result.returncode or not row[phase]['healthy']:
                            raise RuntimeError(f'{runtime} concurrency {concurrency} {phase} unhealthy; evidence preserved')
                finally:
                    row['drain_proof'] = cancel_submission(prefix,batch_id)
                    tiers.wait_idle(options.observe_dir)
                    (options.output/'report.json').write_text(json.dumps(report,indent=2)+'\n')
        report['complete'] = True
    except Exception as error:
        report['error'] = str(error)
        raise
    finally:
        (options.output/'report.json').write_text(json.dumps(report,indent=2)+'\n')
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
