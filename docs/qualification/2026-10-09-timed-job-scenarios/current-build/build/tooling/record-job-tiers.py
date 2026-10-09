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
RUNTIMES = ['runc', 'shared-runc', 'process']


def human_count(value):
    for scale, suffix in [(1_000_000_000, 'B'), (1_000_000, 'M'), (1000, 'k')]:
        if value >= scale:
            rounded = round(value / scale, 1)
            if rounded >= 1000 and suffix != 'B':
                return human_count(round(value / scale) * scale)
            return f'{rounded:g}{suffix}'
    return str(value)


def build_report(baseline, tiers, elapsed, seconds=60):
    if [row['runtime'] for row in tiers] != RUNTIMES:
        raise ValueError('requires exactly the three public paths in order')
    if any(row.get('requested_seconds') != seconds for row in [baseline, *tiers]):
        raise ValueError('all scenarios require the same measurement window')
    baseline = dict(baseline, extrapolated_runs_per_day=baseline['verified_successes'] * 86400 / seconds)
    tiers = [dict(row, extrapolated_runs_per_day=row['unique_accepted_successes'] * 86400 / seconds) for row in tiers]
    succeeded = (baseline.get('path') == 'Bare' and baseline['measurement_complete'] and baseline['verified_successes'] > 0 and baseline['failures'] == 0 and
                 all(row['measurement_complete'] and row['unique_accepted_successes'] > 0 and not row['terminal_failures'] and not row['application_failures']
                     and not row.get('error') and not row['cleanup_failed_submissions'] for row in tiers))
    return dict(baseline=baseline, tiers=tiers, recording_elapsed_seconds=elapsed,
                measurement_window_seconds=seconds,
                unique_accepted_successes=sum(row['unique_accepted_successes'] for row in tiers),
                all_windows_healthy=succeeded, qualified_100m_per_day=False,
                daily_projection='Extrapolation from the measured window; not an observed daily total',
                comparison_boundary='Raw exits omit admission, limits, durable ownership and accepted outcomes; the baseline is a measured reference for this VM and command.',
                timing='One monotonic recording clock; preparation and cancellation remain visible outside equal measurement windows')



def owned_jobs_retired(data):
    if not data.is_dir():
        return False
    try:
        for path in (data / 'instances/runc/bundles/.intents/records').glob('*/intent.json'):
            row = json.loads(path.read_text())
            if '__executor-' in row['instance_id'] and row['phase']['state'] != 'retired':
                return False
        for path in (data / 'instances/process-owners').glob('*/owner.json'):
            row = json.loads(path.read_text())
            if '/host-executors/' in row['command'][0] and row['phase']['state'] not in ['retired', 'cancelled']:
                return False
    except (OSError, ValueError, KeyError, IndexError):
        return False
    return True


def wait_idle(data, timeout=120):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if owned_jobs_retired(data):
            return
        time.sleep(0.5)
    raise TimeoutError('owned job contexts have not positively retired')


def build_manifest(runtime, count, image, host_binary):
    # Slow fresh-container receipts must be visible within a one-minute window.
    if runtime == "runc":
        count = min(count, 60000)
    chunk = 1 if runtime == "runc" else 1000
    template = (f'exec = {json.dumps(str(host_binary))}\ncommand = ["true"]\ncpu = "100m-1000m"\nmemory = "32Mi"\n' if runtime == 'process' else
                f'image = {json.dumps(image)}\ncommand = ["/bin/busybox", "true"]\ncpu = "100m-1000m"\nmemory = "32Mi"\n')
    return (f'name = "demo-{runtime}-{count}"\nnamespace = "default"\n[[cohort]]\nname = "commands"\n'
            f'count = {count}\nchunk_size = {chunk}\nmax_attempts = 3\nper_node_concurrency = 27\n'
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
    parser.add_argument('--seconds', type=int, default=60)
    parser.add_argument('--queued-count', type=int, default=16000000, help='Work per submission (fresh containers capped at 60,000)')
    parser.add_argument('--timeout', type=int, default=86400)
    options = parser.parse_args()
    if '@sha256:' not in options.image or not options.host_binary.is_absolute():
        parser.error('requires a digest-pinned image and absolute allowlisted matching BusyBox binary')
    if options.seconds <= 0 or not 0 < options.queued_count <= 16777216:
        parser.error('positive window and queued count between 1 and 16777216 required')
    options.output.mkdir(parents=True, exist_ok=False)
    started = time.monotonic()
    cast = measurement.Recording(options.output / 'jobs.cast', started)
    rows = []
    baseline = None
    report = dict(all_windows_healthy=False, qualified_100m_per_day=False,
                  kernel=platform.release(), architecture=platform.machine(),
                  retry_policy='Default: at most three attempts per task; accepted retries are reported')
    try:
        cast.emit(f'VM baseline + 3 Reliaburger scenarios, {options.seconds}s each\nSame BusyBox true command and concurrency 27; actual pauses preserved.\nDefault policy: up to three attempts; accepted retries remain visible.', time.monotonic())
        command = [str(options.binaries / 'job-throughput'), '--path', 'bare', '--root', str(options.output / 'baseline'),
                   '--bun', str(options.binaries / 'bun'), '--image', options.image, '--seconds', str(options.seconds),
                   '--concurrency', '27', '--service-url', options.service_url, '--timeout', str(min(options.timeout, options.seconds + 100))]
        wait_idle(options.observe_dir)
        cast.emit(f'VM baseline: raw processes for {options.seconds}s, concurrency 27\n$ ' + shlex.join(command), time.monotonic())
        with (options.output / 'baseline.log').open('x') as log:
            process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=log, text=True)
            for line in process.stdout:
                log.write(line)
                log.flush()
                if 'raw successful exits within the window' in line:
                    print(line, end='', flush=True)
                    cast.emit(line.rstrip(), time.monotonic())
            if process.wait() != 0:
                raise RuntimeError('VM baseline failed; report and log preserved')
        baseline = json.loads((options.output / 'baseline/report.json').read_text())
        if baseline['executable_sha256'] != hashlib.sha256(options.host_binary.read_bytes()).hexdigest():
            raise ValueError('host binary differs from the pinned-image baseline executable')
        cast.emit(f"Raw baseline: {baseline['verified_successes']:,} successful exits in {baseline['elapsed_seconds']:.2f}s "
                  f"({baseline['verified_successes_per_second']:.1f}/s); {baseline['failures']} failures.\n"
                  'Exit statuses only; no ownership journal, resource enforcement or accepted task ledger.', time.monotonic())
        for runtime in RUNTIMES:
            wait_idle(options.observe_dir)
            count = min(options.queued_count, 60000) if runtime == 'runc' else options.queued_count
            manifest = options.output / (runtime + '.toml')
            manifest.write_text(build_manifest(runtime, count, options.image, options.host_binary))
            destination = options.output / runtime
            command = ['python3', str(HERE / 'qualify-jobs.py'), str(manifest), '--relish', options.relish,
                       '--app-url', options.service_url, '--output', str(destination), '--seconds', str(options.seconds),
                       '--window', '1', '--progress-seconds', '5', '--poll-seconds', '0.25',
                       '--observe-pid', str(options.observe_pid), '--observe-dir', str(options.observe_dir)]
            cast.emit(f'Scenario: {runtime} through public dispatch for {options.seconds}s, concurrency 27\n$ ' + shlex.join(command), time.monotonic())
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
            row.update(runtime=runtime, queued_count_per_submission=count, concurrency=27,
                       receipt_chunk_size=1 if runtime == "runc" else 1000)
            rows.append(row)
            cast.emit(f"{runtime}: {row['unique_accepted_successes']:,} accepted successes in {options.seconds}s; "
                      f"{row['terminal_failures']} failures, {row['accepted_retries']} retries. "
                      f"{human_count(round(row['unique_accepted_successes'] * 86400 / options.seconds))}/day extrapolated.", time.monotonic())
        wait_idle(options.observe_dir)
        cast.emit('All four measurement windows complete. Daily figures are extrapolations.\nThe VM baseline counts raw exits; public jobs count durable accepted outcomes.', time.monotonic())
        report.update(build_report(baseline, rows, time.monotonic() - started, seconds=options.seconds))
    except Exception as error:
        report.update(error=str(error), baseline=baseline, tiers=rows, recording_elapsed_seconds=time.monotonic() - started)
        raise
    finally:
        cast.close()
        (options.output / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
    return 0 if report['all_windows_healthy'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
