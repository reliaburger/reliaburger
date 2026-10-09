#!/usr/bin/env python3
"""Crash a positively identified qualification Bun during partial completion.

Use only a task-owned persistent cluster, a mixed reusable-container manifest
and a separately deployed application. Credentials come from the ordinary
Relish environment. The replacement keeps running and its PID replaces the
provided PID file; retain its original binary and journals until retirement.
This is a single-node crash proof, not a throughput or multi-node qualification.
"""
import argparse
import http.client
import importlib.util
import json
import os
import pathlib
import signal
import socket
import ssl
import subprocess
import time
import urllib.parse

spec = importlib.util.spec_from_file_location('job_measurement', pathlib.Path(__file__).with_name('measure-jobs.py'))
measurement = importlib.util.module_from_spec(spec)
spec.loader.exec_module(measurement)


def verify_owner(pid, bun, config, proc=pathlib.Path('/proc')):
    owner = proc / str(pid)
    if pid <= 1 or owner.joinpath('exe').resolve() != bun.resolve():
        raise ValueError('PID does not identify the original qualification binary')
    args = owner.joinpath('cmdline').read_bytes().removesuffix(b'\0').split(b'\0')
    config_args = [i for i, arg in enumerate(args[:-1]) if arg == b'--config']
    if (len(config_args) != 1 or args[config_args[0] + 1] != os.fsencode(config.resolve())
            or b'--cluster' not in args):
        raise ValueError('PID does not identify the original qualification configuration')
    return [str(bun.resolve()), *map(os.fsdecode, args[1:])]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('manifest', type=pathlib.Path)
    parser.add_argument('--bun', required=True, type=pathlib.Path)
    parser.add_argument('--relish', required=True)
    parser.add_argument('--config', required=True, type=pathlib.Path)
    parser.add_argument('--pid-file', required=True, type=pathlib.Path)
    parser.add_argument('--service-url', required=True)
    parser.add_argument('--service-name', required=True)
    parser.add_argument('--node-name', required=True)
    parser.add_argument('--output', required=True, type=pathlib.Path)
    options = parser.parse_args()
    endpoint = urllib.parse.urlsplit(os.environ['RELIABURGER_ENDPOINT'])
    if endpoint.scheme != 'https' or not endpoint.hostname:
        parser.error('requires a real TLS endpoint and normal Relish credentials')
    options.output.mkdir(mode=0o700, parents=True, exist_ok=False)
    (options.output / 'workload.toml').write_bytes(options.manifest.read_bytes())
    prefix = [options.relish]
    def cli(*args):
        return measurement.cli(prefix, *args)
    def service():
        return next(row for row in cli('status') if row['app_name'] == options.service_name)
    observations = (options.output / 'observations.jsonl').open('x')
    def observe(batch_id):
        summary = cli('batch-status', str(batch_id))
        observations.write(json.dumps(summary) + '\n')
        observations.flush()
        return summary
    original_service = service()
    measurement.probe(options.service_url)
    pid = int(options.pid_file.read_text())
    verify_owner(pid, options.bun, options.config)
    batch_id = cli('batch', 'submit', str(options.manifest.resolve()))['batch_id']
    deadline = time.monotonic() + 90
    while True:
        summary = observe(batch_id)
        if 0 < summary['succeeded'] < summary['total'] and (summary.get('active_commands') or 0) > 0:
            break
        if summary['done'] or time.monotonic() >= deadline:
            if not summary['done']:
                cli('batch', 'cancel', str(batch_id))
            raise RuntimeError('no partial accepted completion with verified active commands')
        time.sleep(0.1)
    # Check again immediately before signalling; never infer authority from a PID alone.
    descriptor = os.pidfd_open(pid)
    try:
        original_command = verify_owner(pid, options.bun, options.config)
        signal.pidfd_send_signal(descriptor, signal.SIGKILL)
    finally:
        os.close(descriptor)
    (options.output / 'killed.json').write_text(json.dumps(dict(
        pid=pid, accepted_successes=summary['succeeded'], active_commands=summary['active_commands'])))
    measurement.probe(options.service_url)
    with (options.output / 'replacement.log').open('ab') as log:
        replacement = subprocess.Popen(original_command, stdout=log, stderr=log, start_new_session=True)
    options.pid_file.write_text(str(replacement.pid))
    deadline = time.monotonic() + 180
    while True:
        if replacement.poll() is not None:
            raise RuntimeError('replacement Bun exited; original journals retained')
        try:
            summary = observe(batch_id)
            if summary['done']:
                break
        except (subprocess.TimeoutExpired, subprocess.CalledProcessError):
            pass
        if time.monotonic() >= deadline:
            raise TimeoutError('replacement did not reach accepted completion')
        time.sleep(0.2)
    if (summary['succeeded'] != summary['total'] or summary['failed'] or summary['not_run']
            or service()['pid'] != original_service['pid']):
        raise RuntimeError('recovery failed accepted completion or original application adoption')
    measurement.probe(options.service_url)
    cohort = summary['cohorts'][0]
    stale_request = dict(version=dict(epoch=0, term=0, index=0), known=[], arrays=[dict(
        template=None, resources=dict(cpu_millicores=100, memory_bytes=33554432, gpus=0),
        batch_id=cohort['batch_id'], spec=dict(count=cohort['total'], chunk_size=8,
        max_attempts=3, per_node_concurrency=2), program='/unused', args=[], env=[],
        held=[], stopping=False, replay_unknown=True)])
    context = ssl.create_default_context(cafile=os.environ['RELIABURGER_CA_CERT'])
    connection = http.client.HTTPSConnection(options.node_name, endpoint.port or 443,
                                              context=context, timeout=30)
    connection._create_connection = lambda address, timeout, source_address=None: socket.create_connection(
        (endpoint.hostname, endpoint.port or 443), timeout, source_address)
    connection.connect()
    if ('URI', 'spiffe://reliaburger/node/' + options.node_name) not in connection.sock.getpeercert()['subjectAltName']:
        raise ValueError('wrong node certificate identity')
    connection.request('POST', '/v1/batch/array/sync', body=json.dumps(stale_request), headers={
        'Content-Type': 'application/json', 'Authorization': 'Bearer ' + os.environ['RELIABURGER_TOKEN']})
    response = connection.getresponse()
    if response.status != 200:
        raise RuntimeError('stale control probe failed')
    stale = json.loads(response.read(1 << 20))
    connection.close()
    (options.output / 'stale-response.json').write_text(json.dumps(stale, indent=2))
    if stale['arrays'][0]['slots'] != 0 or 'stale' not in stale['arrays'][0]['refused']:
        raise RuntimeError('stale control was not fenced')
    for result_id, index in measurement.indexed_queries(summary):
        result = cli('batch', 'results', str(result_id), '--index', str(index), '--limit', '1')
        measurement.check_indexed_result(result, result_id, index)
        (options.output / f'results-{result_id}-{index}.json').write_text(json.dumps(result, indent=2))
    report = dict(batch_id=batch_id, total=summary['total'], accepted_successes=summary['succeeded'],
                  failures=summary['failed'], retries=summary['retried'], service_pid_retained=True,
                  stale_control_refused=True, qualified_100m_per_day=False)
    (options.output / 'report.json').write_text(json.dumps(report, indent=2))
    observations.close()
    print(json.dumps(report, indent=2))


if __name__ == '__main__':
    main()
