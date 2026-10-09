#!/usr/bin/env python3
"""Record a raw VM baseline and three real public-dispatch tiers without editing time.

Run as root on the isolated measurement node, with matching optimised binaries,
a digest-pinned warm BusyBox image and a real concurrent application. Failed
runs remain on disk. Only complete successful runs should be published.
"""
import argparse
import hashlib
import importlib.util
import json
import pathlib
import platform
import shlex
import subprocess
import time

HERE = pathlib.Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location('measurement', HERE / 'measure-jobs.py')
measurement = importlib.util.module_from_spec(spec)
spec.loader.exec_module(measurement)
TIERS = [('runc', 1000), ('shared-runc', 10000), ('process', 10000)]


def build_report(baseline, tiers, elapsed):
    if [(row['runtime'], row['total']) for row in tiers] != TIERS:
        raise ValueError('requires exactly the three declared public-dispatch tiers in order')
    succeeded = (baseline.get('path') == 'Bare' and baseline['count'] == 1000000 and
                 baseline['verified_successes'] == 1000000 and baseline['failures'] == 0 and
                 all(row['all_tasks_succeeded'] and row['unique_accepted_successes'] == row['total']
                     for row in tiers))
    return dict(baseline=baseline, tiers=tiers, recording_elapsed_seconds=elapsed,
                unique_accepted_successes=sum(row['unique_accepted_successes'] for row in tiers),
                all_tasks_succeeded=succeeded, qualified_100m_per_day=False,
                comparison_boundary='Raw exit statuses omit admission, durable ownership, task ledgers and accepted outcomes. Differences are not pure scheduling overhead.',
                timing='One monotonic recording clock; includes preparation, submission, observation and indexed verification pauses')



def build_manifest(runtime, count, image, host_binary):
    template = (f'exec = {json.dumps(str(host_binary))}\ncommand = ["true"]\ncpu = "100m-1000m"\nmemory = "32Mi"\n' if runtime == 'process' else
                f'image = {json.dumps(image)}\ncommand = ["/bin/busybox", "true"]\ncpu = "100m-1000m"\nmemory = "32Mi"\n')
    return (f'name = "demo-{runtime}-{count}"\nnamespace = "default"\n[[cohort]]\nname = "commands"\n'
            f'count = {count}\nchunk_size = 1000\nmax_attempts = 3\nper_node_concurrency = 27\n'
            f'[cohort.template]\nruntime = "{runtime}"\n' + template)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binaries', required=True, type=pathlib.Path)
    parser.add_argument('--relish', required=True, help='Binary plus explicit endpoint/CA arguments')
    parser.add_argument('--image', required=True)
    parser.add_argument('--host-binary', required=True, type=pathlib.Path)
    parser.add_argument('--service-url', required=True)
    parser.add_argument('--output', required=True, type=pathlib.Path)
    parser.add_argument('--observe-pid', required=True, type=int)
    parser.add_argument('--observe-dir', required=True, type=pathlib.Path)
    parser.add_argument('--timeout', type=int, default=86400)
    options = parser.parse_args()
    if '@sha256:' not in options.image or not options.host_binary.is_absolute():
        parser.error('requires a digest-pinned image and absolute allowlisted matching BusyBox binary')
    options.output.mkdir(parents=True, exist_ok=False)
    started = time.monotonic()
    cast = measurement.Recording(options.output / 'jobs.cast', started)
    rows = []
    baseline = None
    report = dict(all_tasks_succeeded=False, qualified_100m_per_day=False,
                  kernel=platform.release(), architecture=platform.machine(),
                  retry_policy='Default: at most three attempts per task; accepted retries are reported')
    try:
        cast.emit('Raw VM baseline + 3 Reliaburger job tiers\nSame BusyBox true command; actual pauses preserved.\nDefault policy: up to three attempts; accepted retries remain visible.', time.monotonic())
        command = [str(options.binaries / 'job-throughput'), '--path', 'bare', '--root', str(options.output / 'baseline'),
                   '--bun', str(options.binaries / 'bun'), '--image', options.image, '--count', '1000000',
                   '--concurrency', '27', '--service-url', options.service_url, '--timeout', str(options.timeout)]
        cast.emit('Baseline: 1,000,000 raw processes, no durable job dispatch\n$ ' + shlex.join(command), time.monotonic())
        with (options.output / 'baseline.log').open('x') as log:
            subprocess.run(command, stdout=log, stderr=log, check=True, timeout=options.timeout + 60)
        baseline = json.loads((options.output / 'baseline/report.json').read_text())
        if baseline['executable_sha256'] != hashlib.sha256(options.host_binary.read_bytes()).hexdigest():
            raise ValueError('host binary differs from the pinned-image baseline executable')
        cast.emit(f"Raw baseline: {baseline['verified_successes']:,} successful exits in {baseline['elapsed_seconds']:.2f}s "
                  f"({baseline['verified_successes_per_second']:.1f}/s); {baseline['failures']} failures.\n"
                  'Exit statuses only; no ownership journal, resource enforcement or accepted task ledger.', time.monotonic())
        for runtime, count in TIERS:
            manifest = options.output / (runtime + '.toml')
            manifest.write_text(build_manifest(runtime, count, options.image, options.host_binary))
            destination = options.output / runtime
            command = ['python3', str(HERE / 'measure-jobs.py'), str(manifest), '--relish', options.relish,
                       '--service-url', options.service_url, '--output', str(destination), '--timeout', str(options.timeout),
                       '--warmth', 'warm-images-cold-executors', '--observe-pid', str(options.observe_pid),
                       '--observe-dir', str(options.observe_dir)]
            cast.emit(f'Tier: {count:,} {runtime} jobs through public dispatch\n$ ' + shlex.join(command), time.monotonic())
            with (options.output / (runtime + '.log')).open('x') as log:
                process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=log, text=True)
                for line in process.stdout:
                    log.write(line)
                    log.flush()
                    if ('unique accepted successes' in line or line.startswith(('runtime ', 'application '))):
                        print(line, end='', flush=True)
                        cast.emit(line.rstrip(), time.monotonic())
                if process.wait() != 0:
                    raise RuntimeError(f'{runtime} tier failed; raw report and log preserved')
            row = json.loads((destination / 'report.json').read_text())
            row.update(runtime=runtime, total=count)
            rows.append(row)
            cast.emit(f"Verified {runtime}: {row['unique_accepted_successes']:,} accepted successes; selected indexes 0, middle and last succeeded.", time.monotonic())
        cast.emit('All three public tiers complete. The VM baseline remains a separate comparison.\nA short demonstration does not qualify 100 million jobs/day.', time.monotonic())
        report.update(build_report(baseline, rows, time.monotonic() - started))
    except Exception as error:
        report.update(error=str(error), baseline=baseline, tiers=rows, recording_elapsed_seconds=time.monotonic() - started)
        raise
    finally:
        cast.close()
        (options.output / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
    return 0 if report['all_tasks_succeeded'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
