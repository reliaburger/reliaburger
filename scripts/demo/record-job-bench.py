#!/usr/bin/env python3
"""Record the four packaged Relish job scenarios without changing elapsed time.

Preparation selects a matched, allowlisted BusyBox on the same Linux node.
The optional observation paths and service URL belong to this publication
harness; the benchmark itself is entirely built into Relish.
"""
import argparse
import hashlib
import importlib.util
import json
import os
import pathlib
import shlex
import signal
import subprocess
import threading
import time

HERE=pathlib.Path(__file__).resolve().parent
spec=importlib.util.spec_from_file_location('measurement',HERE/'measure-jobs.py')
measurement=importlib.util.module_from_spec(spec);spec.loader.exec_module(measurement)
spec=importlib.util.spec_from_file_location('tiers',HERE/'record-job-tiers.py')
tiers=importlib.util.module_from_spec(spec);spec.loader.exec_module(tiers)
SCENARIOS=['jobs-vm-baseline','jobs-containers','jobs-shared-containers','jobs-host-processes']
RUNTIMES=dict(zip(SCENARIOS[1:],['runc','shared-runc','process']))


def command(scenario,seconds=60,concurrency=27,prefix=None):
    if scenario not in SCENARIOS or not 1<=seconds<=86400 or not 1<=concurrency<=256:
        raise ValueError('invalid scenario, duration or concurrency')
    result=list(prefix or ['relish'])+['bench','--scenario',scenario]
    if seconds!=60:result+=['--seconds',str(seconds)]
    if concurrency!=27:result+=['--concurrency',str(concurrency)]
    return result


def published_row(source,observations):
    if source.get('schema_version')!=1 or source.get('scenario') not in SCENARIOS:
        raise ValueError('requires the packaged benchmark report')
    if not source.get('measurement_complete') or source.get('interrupted') or not source.get('cleanup_verified') or source.get('error'):
        raise ValueError('measurement or positive drain incomplete; preserve the failed report')
    row=dict(source,**observations)
    seconds=source['requested_seconds']
    if source['scenario']=='jobs-vm-baseline':
        row.update(path='Bare',failures=source['terminal_failures'],elapsed_seconds=seconds,
                   actual_elapsed_including_drain_seconds=source['elapsed_including_drain_seconds'],
                   verified_successes_per_second=source['verified_successes']/seconds)
    else:
        active=set(source['active_submissions'])
        proofs=[proof for proof in source['drain_proofs'] if proof['batch_id'] in active]
        if len(proofs)!=len(active) or any(proof['done'] is not True or proof['held']!=0 or proof['active_commands']!=0 for proof in proofs):
            raise ValueError('missing actual positive cancellation proof')
        row.update(runtime=RUNTIMES[source['scenario']],successes_per_second=source['unique_accepted_successes']/seconds,
                   cleanup_failed_submissions=source.get('cleanup_failed_submissions',[]),
                   post_cutoff_drain_proofs=proofs)
    return row


class Observations:
    def __init__(self,options):
        self.options=options;self.started=time.monotonic();self.stop=threading.Event()
        self.rows=[];self.probes=0;self.failures=0;self.latencies=[];self.error=None
        self.starts={pid:measurement.process_start(pid) for pid in options.observe_pid}
        self.thread=threading.Thread(target=self.run,daemon=True)

    def run(self):
        next_observation=0
        try:
            while not self.stop.is_set():
                try:self.latencies.append(measurement.probe(self.options.service_url));self.probes+=1
                except Exception:self.failures+=1
                elapsed=time.monotonic()-self.started
                if elapsed>=next_observation:
                    import shutil
                    storage={str(path):dict(measurement.bounded_storage(path),filesystem_available_bytes=shutil.disk_usage(path).free) for path in self.options.observe_dir}
                    cpu=pathlib.Path('/proc/stat').read_text().splitlines()[0].split()[1:]
                    self.rows.append(dict(elapsed_seconds=elapsed,cpu_ticks=[int(v) for v in cpu],storage=storage,
                        processes=[measurement.process_observation(pid,start) for pid,start in self.starts.items()]))
                    next_observation=elapsed+30
                self.stop.wait(1)
        except Exception as error:self.error=str(error)

    def finish(self):
        self.stop.set();self.thread.join(10)
        if self.thread.is_alive() or self.error:raise RuntimeError(self.error or 'observation thread did not drain')
        return dict(application_probes=self.probes,application_failures=self.failures,
                    resource_observations=self.rows,
                    service_latency_ms=dict(samples=len(self.latencies),maximum=max(self.latencies,default=None),
                        p95=sorted(self.latencies)[min(len(self.latencies)-1,int(len(self.latencies)*0.95))] if self.latencies else None),
                    observation_boundary='Service and resource observations span preparation, measured work and positive drain')


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--relish',default='relish')
    parser.add_argument('--host-binary',required=True,type=pathlib.Path)
    parser.add_argument('--service-url',required=True)
    parser.add_argument('--output',required=True,type=pathlib.Path)
    parser.add_argument('--observe-pid',action='append',default=[],type=int)
    parser.add_argument('--observe-dir',action='append',default=[],type=pathlib.Path)
    parser.add_argument('--seconds',default=60,type=int)
    parser.add_argument('--concurrency',default=27,type=int)
    options=parser.parse_args()
    if not options.host_binary.is_absolute() or len(options.observe_pid)>8:parser.error('requires an absolute matching BusyBox and at most eight observed processes')
    command(SCENARIOS[0],options.seconds,options.concurrency)
    options.output.mkdir(parents=True,exist_ok=False)
    started=time.monotonic();cast=measurement.Recording(options.output/'jobs.cast',started)
    report=dict(all_windows_healthy=False,qualified_100m_per_day=False,source='packaged relish bench scenarios',chapters=[],tiers=[])
    baseline=None;rows=[]
    prefix=shlex.split(options.relish)
    try:
        cast.emit(f'VM baseline + 3 Reliaburger scenarios, {options.seconds}s each\nSame pinned BusyBox true and concurrency {options.concurrency}; actual pauses preserved.\nConfigured endpoint and allowlisted BusyBox; JSON reports saved separately.\nDaily rates are extrapolated, not observed daily totals.',time.monotonic())
        for scenario in SCENARIOS:
            for path in options.observe_dir:tiers.wait_idle(path)
            destination=options.output/scenario;destination.mkdir()
            env=dict(os.environ,RELIABURGER_BENCH_EXEC=str(options.host_binary),RELIABURGER_BENCH_REPORT=str(destination/'report.json'))
            args=command(scenario,options.seconds,options.concurrency,prefix)
            report['chapters'].append(dict(scenario=scenario,start_seconds=time.monotonic()-started))
            cast.emit('$ '+shlex.join(args),time.monotonic())
            observer=Observations(options);observer.thread.start()
            process=None
            try:
                with (destination/'terminal.log').open('x') as log:
                    process=subprocess.Popen(args,env=env,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True)
                    for line in process.stdout:
                        log.write(line);log.flush();print(line,end='',flush=True);cast.emit(line.rstrip(),time.monotonic())
                    code=process.wait()
                observations=observer.finish()
                source=json.loads((destination/'report.json').read_text())
                row=published_row(source,observations)
                if code!=0 or row['application_failures'] or row['terminal_failures']:raise RuntimeError(f'{scenario} failed; report preserved')
                if scenario==SCENARIOS[0]:
                    baseline=row
                    if row['executable_sha256']!=hashlib.sha256(options.host_binary.read_bytes()).hexdigest():raise ValueError('baseline differs from the allowed host BusyBox')
                else:rows.append(row)
                (destination/'observations.json').write_text(json.dumps(observations,indent=2)+'\n')
            finally:
                observer.stop.set()
                if process is not None and process.poll() is None:
                    process.send_signal(signal.SIGINT)
                    try:process.wait(timeout=160)
                    except subprocess.TimeoutExpired:raise RuntimeError('benchmark did not drain after interruption; inspect its report and owned IDs')
                observer.thread.join(10)
            for path in options.observe_dir:tiers.wait_idle(path)
            cast.emit('Owned commands drained; fixture executor retirement verified before the next scenario.',time.monotonic())
        cast.emit('All four real windows complete. VM exits omit scheduler guarantees.\nPublic counts are unique accepted outcomes; runs/day are extrapolated.',time.monotonic())
        report.update(tiers.build_report(baseline,rows,time.monotonic()-started,seconds=options.seconds))
    except BaseException as error:
        report.update(error=str(error),baseline=baseline,tiers=rows,recording_elapsed_seconds=time.monotonic()-started)
        raise
    finally:
        cast.close();(options.output/'report.json').write_text(json.dumps(report,indent=2)+'\n')
    return 0 if report['all_windows_healthy'] else 1


if __name__=='__main__':raise SystemExit(main())
