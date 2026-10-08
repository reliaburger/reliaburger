#!/usr/bin/env python3
"""Record one real public-CLI submission through unique accepted completion.

Run on a qualification node with matching binaries. The cast preserves wall
clock pauses. This measures full dispatch, not bare processes or raw containers,
and never establishes 24-hour recovery/bounded-cost qualification by itself.
"""
import argparse
import hashlib
import json
import os
import pathlib
import platform
import shlex
import subprocess
import time
import urllib.request


class AcceptedCounts:
    def __init__(self, batch_id):
        self.batch_id = batch_id
        self.total = None
        self.previous = (0, 0, 0)

    def observe(self, summary):
        if summary.get('batch_id') != self.batch_id:
            raise ValueError('summary belongs to another submission')
        keys = ('total', 'succeeded', 'failed', 'not_run', 'retried')
        if any(type(summary.get(k)) is not int or summary[k] < 0 for k in keys):
            raise ValueError('accepted counters must be nonnegative integers')
        if self.total is not None and summary['total'] != self.total:
            raise ValueError('submission count changed')
        if sum(summary[k] for k in ('succeeded', 'failed', 'not_run')) > summary['total']:
            raise ValueError('terminal counters exceed the submitted count')
        counts = tuple(summary[k] for k in ('succeeded', 'failed', 'retried'))
        if any(now < before for now, before in zip(counts, self.previous)):
            raise ValueError('accepted counters went backwards')
        self.total, self.previous = summary['total'], counts
        return counts


class Recording:
    def __init__(self, path, started):
        self.file = pathlib.Path(path).open('x')
        self.started = started
        self.last = 0
        self.file.write(json.dumps(dict(version=2, width=140, height=20,
                                       title='Real Reliaburger accepted job throughput',
                                       timestamp=int(time.time()))) + '\n')

    def emit(self, value, now):
        elapsed = now - self.started
        if elapsed < self.last:
            raise ValueError('recording clock went backwards')
        self.last = elapsed
        self.file.write(json.dumps([elapsed, 'o', value.replace('\n', '\r\n') + '\r\n']) + '\n')
        self.file.flush()

    def close(self):
        self.file.close()


def finish_report(summary, started, accepted_at):
    elapsed = accepted_at - started
    if elapsed <= 0:
        raise ValueError('accepted elapsed time must be positive')
    return dict(accepted_elapsed_seconds=elapsed,
                unique_accepted_successes=summary['succeeded'],
                accepted_successes_per_second=summary['succeeded'] / elapsed,
                terminal_failures=summary['failed'], not_run=summary['not_run'],
                accepted_retries=summary['retried'],
                all_tasks_succeeded=summary['done'] is True and summary['succeeded'] == summary['total'],
                qualified_100m_per_day=False,
                qualification_boundary='Requires matched baselines, 24-hour headroom, concurrent apps, recovery/fault and bounded-cost evidence')


def display(summary, elapsed):
    recent = summary.get('rates', {}).get('successes_per_second')
    interval = summary.get('rates', {}).get('interval_seconds')
    active = summary.get('active_commands')
    mode = summary.get('isolation', 'mixed' if summary.get('kind') == 'manifest' else 'unknown')
    return (f"{summary['succeeded']:,}/{summary['total']:,} unique accepted successes in {elapsed:.2f}s "
            f"({summary['succeeded'] / max(elapsed, 1e-9):.1f}/s whole run); "
            f"{summary['failed']} failures, {summary['retried']} retries\n"
            f"mode {mode}; commands {active if active is not None else 'unknown'}; "
            f"queued {summary.get('queued', 'unknown')}, held {summary.get('held', 'unknown')}; "
            f"recent {recent if recent is not None else 'unknown'}/s over "
            f"{interval if interval is not None else 'unknown'}s")


def cli(prefix, *args):
    result = subprocess.run(prefix + ['--output', 'json'] + list(args), check=True,
                            stdout=subprocess.PIPE, text=True, timeout=30)
    return json.loads(result.stdout)


def probe(url):
    started = time.monotonic()
    with urllib.request.urlopen(url, timeout=5) as response:
        if response.status != 200:
            raise ValueError('service probe did not return 200')
        response.read(65536)
    return (time.monotonic() - started) * 1000


def bounded_storage(path, max_files=4096):
    size = entries = files = 0
    pending = [pathlib.Path(path)]
    seen = set()
    while pending:
        with os.scandir(pending.pop()) as children:
            for entry in children:
                if entries >= max_files:
                    return dict(allocated_bytes_observed=size, files_observed=files, complete=False)
                entries += 1
                metadata = entry.stat(follow_symlinks=False)
                identity = metadata.st_dev, metadata.st_ino
                if identity not in seen:
                    seen.add(identity)
                    size += metadata.st_blocks * 512
                if entry.is_dir(follow_symlinks=False):
                    pending.append(pathlib.Path(entry.path))
                else:
                    files += 1
    return dict(allocated_bytes_observed=size, files_observed=files, complete=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('manifest', type=pathlib.Path)
    parser.add_argument('--relish', default='relish', help='Binary plus endpoint and CA arguments')
    parser.add_argument('--service-url', required=True, help='Real concurrent application endpoint')
    parser.add_argument('--output', required=True, type=pathlib.Path)
    parser.add_argument('--timeout', type=int, default=3600)
    parser.add_argument('--warmth', required=True, choices=['cold-images-and-executors', 'warm-images-cold-executors', 'warm-images-and-executors'])
    parser.add_argument('--observe-dir', action='append', default=[], type=pathlib.Path,
                        help='Node-local storage; traversal stops at 4096 files, reporting incomplete bounds')
    options = parser.parse_args()
    if options.timeout < 1:
        parser.error('timeout must be positive')
    options.output.mkdir(mode=0o700, parents=True, exist_ok=False)
    source = options.manifest.read_bytes()
    (options.output / 'workload.toml').write_bytes(source)
    prefix = shlex.split(options.relish)
    command = prefix + ['--output', 'json', 'batch', 'submit', str(options.manifest.resolve())]
    metadata = dict(kernel=platform.release(), architecture=platform.machine(),
                    logical_cpus=os.cpu_count(), operator_declared_warmth=options.warmth,
                    workload_sha256=hashlib.sha256(source).hexdigest(), command=command,
                    execution_path='public full dispatch; runtime/isolation as admitted',
                    host_process_contract='Owned host execution has no hard CPU/memory enforcement',
                    timing='Monotonic clock before submit process launch through observed accepted terminal summary')
    batch_id = None
    terminal = False
    started = time.monotonic()
    cast = Recording(options.output / 'jobs.cast', started)
    report = dict(metadata, qualified_100m_per_day=False)
    samples = (options.output / 'samples.jsonl').open('x')
    try:
        cast.emit('$ ' + shlex.join(command), time.monotonic())
        answer = cli(prefix, 'batch', 'submit', str(options.manifest.resolve()))
        batch_id = answer['batch_id']
        counts = AcceptedCounts(batch_id)
        while True:
            summary = cli(prefix, 'batch-status', str(batch_id))
            counts.observe(summary)
            accepted_at = time.monotonic()
            elapsed = accepted_at - started
            terminal = summary['done'] is True
            # Freeze success timing before service probes and indexed verification.
            if terminal:
                report.update(finish_report(summary, started, accepted_at))
            latency = probe(options.service_url)
            summary.pop('nodes', None)
            for cohort in summary.get('cohorts', []):
                cohort.pop('nodes', None)
            observation = dict(elapsed_seconds=elapsed, summary=summary, service_latency_ms=latency,
                               storage={str(path): bounded_storage(path) for path in options.observe_dir})
            samples.write(json.dumps(observation) + '\n')
            samples.flush()
            text = display(summary, elapsed) + f'\napplication 200 in {latency:.2f}ms'
            print(text, flush=True)
            cast.emit(text, time.monotonic())
            if terminal:
                details = []
                for index in sorted({0, summary['total'] // 2, summary['total'] - 1}):
                    if index >= 0:
                        details.append(cli(prefix, 'batch', 'results', str(batch_id), '--index', str(index), '--limit', '1'))
                (options.output / 'indexed-results.json').write_text(json.dumps(details, indent=2) + '\n')
                break
            if elapsed >= options.timeout:
                raise TimeoutError('submission did not reach accepted terminal state within the measurement timeout')
            time.sleep(1)
    except Exception as error:
        report.update(error=str(error), all_tasks_succeeded=False)
        raise
    finally:
        if batch_id is not None and not terminal:
            try:
                subprocess.run(prefix + ['batch', 'cancel', str(batch_id)], check=True, timeout=30)
            except Exception as error:
                report['cleanup_error'] = str(error)
        samples.close()
        cast.close()
        (options.output / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps(report, indent=2))
    return 0 if report.get('all_tasks_succeeded') else 1


if __name__ == '__main__':
    raise SystemExit(main())
