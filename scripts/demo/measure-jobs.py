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
    modes = {cohort.get('runtime', 'unknown') for cohort in summary.get('cohorts', [])}
    mode = summary.get('runtime', next(iter(modes)) if len(modes) == 1 else
                       'mixed' if modes else 'unknown')
    return (f"{summary['succeeded']:,}/{summary['total']:,} unique accepted successes in {elapsed:.2f}s "
            f"({summary['succeeded'] / max(elapsed, 1e-9):.1f}/s whole run); "
            f"{summary['failed']} failures, {summary['retried']} retries\n"
            f"runtime {mode}; commands {active if active is not None else 'unknown'}; "
            f"queued {summary.get('queued', 'unknown')}, held {summary.get('held', 'unknown')}; "
            f"recent {recent if recent is not None else 'unknown'}/s over "
            f"{interval if interval is not None else 'unknown'}s")



def indexed_queries(summary):
    # A manifest has no worker ledger of its own. Indexes are local to each
    # admitted cohort; use its stable identity rather than the parent's ID.
    rows = summary.get('cohorts') if summary.get('kind') == 'manifest' else [summary]
    if not isinstance(rows, list) or not 1 <= len(rows) <= 32:
        raise ValueError('indexed verification requires 1–32 admitted cohorts')
    queries = []
    seen = set()
    for row in rows:
        batch_id, total = row.get('batch_id'), row.get('total')
        if type(batch_id) is not int or batch_id <= 0 or batch_id in seen:
            raise ValueError('indexed verification requires distinct admitted identities')
        if type(total) is not int or total <= 0:
            raise ValueError('indexed verification requires a positive cohort count')
        seen.add(batch_id)
        queries.extend((batch_id, index) for index in sorted({0, total // 2, total - 1}))
    return queries



def check_indexed_result(result, batch_id, index):
    rows = result.get('rows')
    if (result.get('batch_id') != batch_id or result.get('unreachable') or
            not isinstance(rows, list) or len(rows) != 1 or
            rows[0].get('index') != index or rows[0].get('succeeded') is not True):
        raise ValueError('selected indexed outcome is unavailable, mismatched or unsuccessful')


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
    size = entries = files = raced = 0
    pending = [pathlib.Path(path)]
    seen = set()
    while pending:
        try:
            with os.scandir(pending.pop()) as children:
                for entry in children:
                    if entries >= max_files:
                        return dict(allocated_bytes_observed=size, files_observed=files,
                                    raced_entries=raced, complete=False)
                    entries += 1
                    try:
                        metadata = entry.stat(follow_symlinks=False)
                        directory = entry.is_dir(follow_symlinks=False)
                    except FileNotFoundError:
                        raced += 1
                        continue
                    identity = metadata.st_dev, metadata.st_ino
                    if identity not in seen:
                        seen.add(identity)
                        size += metadata.st_blocks * 512
                    if directory:
                        pending.append(pathlib.Path(entry.path))
                    else:
                        files += 1
        except FileNotFoundError:
            raced += 1
    # Atomic writes/retirement can change the tree while it is read. That
    # observation is partial evidence, never proof of a complete storage bound.
    return dict(allocated_bytes_observed=size, files_observed=files,
                raced_entries=raced, complete=raced == 0)


def process_start(pid, proc=pathlib.Path('/proc')):
    # The command name may contain spaces and parentheses. Fields after its
    # final closing parenthesis start with state; starttime is field 22.
    fields = (proc / str(pid) / 'stat').read_text().rsplit(')', 1)[1].split()
    return int(fields[19])


def process_observation(pid, start, proc=pathlib.Path('/proc')):
    row = dict(pid=pid, start_ticks=start, complete=False)
    try:
        if process_start(pid, proc) != start:
            return row
        fields = dict(line.split(':', 1) for line in
                      (proc / str(pid) / 'status').read_text().splitlines() if ':' in line)
        rss = int(fields['VmRSS'].split()[0]) * 1024
        peak = int(fields['VmHWM'].split()[0]) * 1024
        if process_start(pid, proc) == start:
            row.update(rss_bytes=rss, peak_rss_bytes=peak, complete=True)
    except (OSError, ValueError, IndexError, KeyError):
        pass
    return row


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
    parser.add_argument('--observe-pid', action='append', default=[], type=int,
                        help='Node-local process RSS/peak RSS, fenced by its original start time (at most eight)')
    options = parser.parse_args()
    if len(options.observe_pid) > 8 or any(pid <= 1 for pid in options.observe_pid):
        parser.error('observe at most eight positive process identities')
    process_starts = {pid: process_start(pid) for pid in options.observe_pid}
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
                               storage={str(path): bounded_storage(path) for path in options.observe_dir},
                               processes=[process_observation(pid, start) for pid, start in process_starts.items()])
            samples.write(json.dumps(observation) + '\n')
            samples.flush()
            text = display(summary, elapsed) + f'\napplication 200 in {latency:.2f}ms'
            print(text, flush=True)
            cast.emit(text, time.monotonic())
            if terminal:
                details = []
                for result_id, index in indexed_queries(summary):
                    query = ('batch', 'results', str(result_id), '--index', str(index), '--limit', '1')
                    result = cli(prefix, *query)
                    check_indexed_result(result, result_id, index)
                    details.append(result)
                    cast.emit('$ ' + shlex.join(prefix + list(query)), time.monotonic())
                    cast.emit(json.dumps(result, separators=(',', ':')), time.monotonic())
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
