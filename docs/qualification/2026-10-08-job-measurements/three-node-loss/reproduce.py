#!/usr/bin/env python3
"""Qualification fixture: stops node 3 of the explicitly task-owned feature654 cluster.
Create it with the commands in README.md first. Do not point this at another cluster.
"""
import importlib.util,json,os,pathlib,subprocess,time
p=pathlib.Path(os.environ['RELIABURGER_SOURCE'])/'scripts/demo/measure-jobs.py'
spec=importlib.util.spec_from_file_location('measurement',p); m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)
env=os.environ.copy()
for key in ('RELIABURGER_TOKEN','RELIABURGER_ENDPOINT','RELIABURGER_CA_CERT'):env.pop(key,None)
env['RELIABURGER_HOME']=os.environ['RELIABURGER_HOME']
bin=os.environ['RELIABURGER_RELISH']
out=pathlib.Path(os.environ['RELIABURGER_PROOF_OUTPUT']);out.mkdir(exist_ok=False)
(out/'workload.toml').write_bytes(pathlib.Path(__file__).with_name('workload.toml').read_bytes())
def cli(*args):return json.loads(subprocess.check_output([bin,'--output','json',*args],env=env,timeout=60))
def app():return next(x for x in cli('status')if x['app_name']=='hello')
original=app();batch_id=cli('batch','submit',str(pathlib.Path(__file__).with_name('workload.toml')))['batch_id'];samples=(out/'samples.jsonl').open('x');stopped=False;started=False
try:
 deadline=time.monotonic()+120
 while True:
  s=cli('batch-status',str(batch_id));latency=m.probe('http://localhost:38080/')
  samples.write(json.dumps(dict(summary=s,service_latency_ms=latency))+'\n');samples.flush()
  active=any(n['node']=='rb-88f1997db44d-3' and n['counters']['active_commands']>0 for c in s['cohorts']for n in c['nodes'])
  if 0<s['succeeded']<s['total'] and active:break
  if s['done']or time.monotonic()>deadline:raise RuntimeError('no active victim at partial completion')
  time.sleep(.1)
 (out/'before-stop.json').write_text(json.dumps(s,indent=2))
 subprocess.run([bin,'local','stop','--name','feature654','3','--yes'],env=env,check=True,timeout=150);stopped=True
 deadline=time.monotonic()+600
 while True:
  s=cli('batch-status',str(batch_id));latency=m.probe('http://localhost:38080/')
  samples.write(json.dumps(dict(summary=s,service_latency_ms=latency))+'\n');samples.flush()
  if s['done']:break
  if time.monotonic()>deadline:raise TimeoutError('node-loss recovery did not complete')
  time.sleep(1)
 if s['succeeded']!=s['total']or s['failed']or s['not_run']:raise RuntimeError('not all accepted successes')
 after=app()
 if (after['node'],after['pid'])!=(original['node'],original['pid']):raise RuntimeError('original application replaced')
 for bid,index in m.indexed_queries(s):
  result=cli('batch','results',str(bid),'--index',str(index),'--limit','1');m.check_indexed_result(result,bid,index)
  (out/f'results-{bid}-{index}.json').write_text(json.dumps(result,indent=2))
 subprocess.run([bin,'local','start','--name','feature654','3'],env=env,check=True,timeout=150);started=True
 report=dict(total=s['total'],accepted_successes=s['succeeded'],failed=s['failed'],retried=s['retried'],victim='rb-88f1997db44d-3',service_original=original,service_retained=True,qualified_100m_per_day=False)
 (out/'report.json').write_text(json.dumps(report,indent=2));print(json.dumps(report))
finally:
 samples.close()
 if stopped and not started:subprocess.run([bin,'local','start','--name','feature654','3'],env=env,timeout=150)
