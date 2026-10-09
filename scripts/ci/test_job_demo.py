"""The published job demo must describe its actual uncompressed recording."""
import json
import pathlib
import subprocess
import shutil
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]

class JobDemo(unittest.TestCase):
    def test_visible_demo_follows_tour_with_collapsed_details_and_matches_evidence(self):
        import importlib.util
        spec=importlib.util.spec_from_file_location('timed_tiers',ROOT/'scripts/demo/record-job-tiers.py')
        tiers=importlib.util.module_from_spec(spec);spec.loader.exec_module(tiers)
        page=(ROOT/'docs/website/index.html').read_text()
        section=page.index('<section id="job-throughput"')
        tour=page.index('<section id="tour"')
        self.assertLess(tour,section)
        self.assertRegex(page[page.index('</section>',tour)+10:section],r'^\s*$')
        self.assertLess(section,page.index('<section id="start"'))
        self.assertNotIn('id="jobs-recording"',page)
        self.assertNotIn('Development preview',page)
        self.assertIn('class="section-heading"',page[section:page.index('</section>',section)])
        figure=page.index('id="throughput-recording"',section)
        self.assertNotIn('<details',page[section:figure])
        details=page.index('<details',figure)
        self.assertLess(page.index('</figure>',figure),details)
        self.assertIn('id="job-throughput-details"',page[details:page.index('>',details)])
        self.assertNotIn(' open',page[details:page.index('>',details)])
        self.assertLess(details,page.index('<table',section))
        self.assertLess(details,page.index('Testing rig:',section))
        report=json.loads((ROOT/'docs/website/assets/job-throughput-report.json').read_text())
        recording=[json.loads(line) for line in (ROOT/'docs/website/assets/job-throughput.cast').read_text().splitlines()]
        self.assertTrue(report['all_windows_healthy'])
        self.assertFalse(report['qualified_100m_per_day'])
        self.assertEqual(report['measurement_window_seconds'],60)
        self.assertAlmostEqual(recording[-1][0],report['recording_elapsed_seconds'],delta=1)
        self.assertEqual(report['baseline']['path'],'Bare')
        self.assertEqual([row['runtime'] for row in report['tiers']],['runc','shared-runc','process'])
        self.assertEqual(report['unique_accepted_successes'],sum(row['unique_accepted_successes'] for row in report['tiers']))
        text=page[section:page.index('</section>',section)]
        for row in [report['baseline'],*report['tiers']]:
            self.assertEqual(row['requested_seconds'],60)
            self.assertTrue(row['measurement_complete'])
            count=row['verified_successes'] if row.get('path')=='Bare' else row['unique_accepted_successes']
            self.assertEqual(row['extrapolated_runs_per_day'],count*1440)
            self.assertIn(f'{count:,}',text)
            self.assertIn(tiers.human_count(round(count*1440)),text)
        for row in report['tiers']:
            self.assertEqual(row['terminal_failures'],0)
            self.assertEqual(row['cleanup_failed_submissions'],[])
            self.assertEqual(len(row['post_cutoff_drain_proofs']),len(row['active_submissions']))
        raw=ROOT/'docs/qualification/2026-10-09-timed-job-scenarios/current-build/packaged-bench-windows'
        for asset,evidence in [('job-throughput.cast','jobs.cast'),('job-throughput-report.json','report.json')]:
            self.assertEqual((ROOT/'docs/website/assets'/asset).read_bytes(),(raw/evidence).read_bytes())
        self.assertEqual(recording[0]['version'],2)
        self.assertEqual([row[0] for row in recording[1:]],sorted(row[0] for row in recording[1:]))
        for phrase in ['60 seconds','extrapolated','independent','repeatable','no isolation','VM baseline','durable ownership','development binaries']:
            self.assertIn(phrase,text)
        self.assertRegex(text,r'data-cast="\./assets/job-throughput\.cast"')
        self.assertIn('./assets/job-throughput-report.json',text)

    def test_published_demo_executes_the_packaged_default_commands(self):
        report=json.loads((ROOT/'docs/website/assets/job-throughput-report.json').read_text())
        self.assertEqual(report['source'],'packaged relish bench scenarios')
        scenarios=['jobs-vm-baseline','jobs-containers','jobs-shared-containers','jobs-host-processes']
        self.assertEqual([row['scenario'] for row in report['chapters']],scenarios)
        recording=[json.loads(line) for line in (ROOT/'docs/website/assets/job-throughput.cast').read_text().splitlines()]
        commands=[row[2].strip() for row in recording[1:] if row[2].startswith('$ ')]
        self.assertEqual(commands,['$ relish bench --scenario '+scenario for scenario in scenarios])
        for row in [report['baseline'],*report['tiers']]:
            self.assertEqual(row['concurrency'],27)
            self.assertTrue(row['cleanup_verified'])
            self.assertIsNone(row['error'])

    def test_all_four_packaged_hours_complete_with_matched_profiles_and_drain(self):
        folder=ROOT/'docs/qualification/2026-10-09-timed-job-scenarios/current-build/packaged-bench-hours'
        report=json.loads((folder/'report.json').read_text())
        self.assertTrue(report['all_windows_healthy'])
        self.assertFalse(report['qualified_100m_per_day'])
        rows=[report['baseline'],*report['tiers']]
        self.assertEqual([row['scenario'] for row in rows],['jobs-vm-baseline','jobs-containers','jobs-shared-containers','jobs-host-processes'])
        for row in rows:
            self.assertEqual(row['requested_seconds'],3600)
            self.assertEqual(row['concurrency'],27)
            self.assertTrue(row['measurement_complete'])
            self.assertTrue(row['cleanup_verified'])
            self.assertFalse(row['interrupted'])
            self.assertIsNone(row['error'])
            self.assertEqual(row['terminal_failures'],0)
            self.assertEqual(row['application_failures'],0)
            self.assertGreater(row['application_probes'],0)
            self.assertGreaterEqual(len(row['resource_observations']),100)
            self.assertGreater(row['unique_accepted_successes']+row['verified_successes'],0)
        for row in report['tiers']:
            self.assertEqual(row['cpu_request_millicores'],25)
            self.assertEqual(row['cpu_limit_millicores'],1000)
            self.assertEqual(row['memory_bytes'],32<<20)
            proofs={proof['batch_id']:proof for proof in row['drain_proofs']}
            self.assertEqual(set(row['batch_ids']),set(proofs))
            self.assertTrue(all(proof['done'] is True and proof['held']==0 and proof['active_commands']==0 for proof in proofs.values()))

    def test_results_identify_local_vm_rig_and_workload_limits(self):
        page=(ROOT/'docs/website/index.html').read_text()
        start=page.index('<section id="job-throughput"')
        section=page[start:page.index('</section>',start)]
        for phrase in ['Apple M2 Max','Lima VM','4 vCPU','8 GiB','Ubuntu 24.04','one-core limit','BusyBox']:
            self.assertIn(phrase,section)
        self.assertIn('own hardware',section)

    def test_chapter_controls_stay_hidden_without_javascript_and_have_keyboard_focus(self):
        css = (ROOT / 'docs/website/style.css').read_text()
        self.assertIn('.recording-chapters[hidden] { display: none; }', css)
        self.assertIn('.recording-chapters button:focus-visible', css)

    @unittest.skipUnless(shutil.which('node'), 'requires Node for the browser script contract')
    def test_chapters_seek_the_same_recording_and_preserve_playback_speed(self):
        program = r"""
const fs = require('fs'), vm = require('vm'), assert = require('assert');
const calls = [], speedButton = {getAttribute: () => '100', setAttribute: () => {}};
const speeds = {hidden:true,querySelectorAll:()=>[speedButton],addEventListener:(_,callback)=>speeds.click=callback};
const chapters = {hidden:true,addEventListener:(_,callback)=>chapters.click=callback};
const screen = {addEventListener:()=>{}};
const figure = {classList:{add:()=>{}},getAttribute:key=>key==='data-cast'?'throughput.cast':null,
 querySelector:selector=>selector==='.recording-speed'?speeds:selector==='.recording-chapters'?chapters:screen};
const document = {querySelectorAll:()=>[figure],createElement:()=>({setAttribute:()=>{}}),
 head:{appendChild:element=>queueMicrotask(()=>element.onload())}};
const window = {AsciinemaPlayer:{create:(cast,screen,options)=>{
 calls.push({cast,options});return {dispose:()=>{},addEventListener:()=>{},getCurrentTime:()=>250};
}}};
vm.runInNewContext(fs.readFileSync(process.argv[1],'utf8'),{document,window,Promise,Number,Object,String});
setImmediate(async()=>{
 assert.strictEqual(chapters.hidden,false);
 speeds.click({target:{closest:()=>speedButton}});await Promise.resolve();
 chapters.click({target:{closest:()=>({getAttribute:()=> '350.25'})}});
 assert.strictEqual(calls.length,3);
 assert.strictEqual(calls[2].cast,'throughput.cast');
 assert.strictEqual(calls[2].options.startAt,350.25);
 assert.strictEqual(calls[2].options.speed,100);
 assert.strictEqual(calls[2].options.autoPlay,true);
 for (const bad of ['-1','NaN','Infinity'])
  chapters.click({target:{closest:()=>({getAttribute:()=>bad})}});
 assert.strictEqual(calls.length,3);
 speeds.click({target:{closest:()=>speedButton}});await Promise.resolve();
 assert.strictEqual(calls[3].options.startAt,250);
 assert.strictEqual(calls[3].options.autoPlay,true);
});
"""
        subprocess.run(['node', '-e', program, str(ROOT / 'docs/website/assets/tour-player.js')], check=True)

    @unittest.skipUnless(shutil.which('node'), 'requires Node for the browser script contract')
    def test_all_recordings_load_once_and_keep_independent_speed_controls(self):
        program = r"""
const fs = require('fs'), vm = require('vm'), assert = require('assert');
const calls = [], loads = [], figures = ['tour','throughput'].map(name => {
  const buttons = [1,2].map(speed => ({getAttribute: () => String(speed), setAttribute: () => {}}));
  const speeds = {hidden:true,querySelectorAll: () => buttons,addEventListener: (_, callback) => speeds.click=callback};
  const screen = {name,addEventListener:(event,callback)=>screen[event]=callback};
  const figure = {name, classList:{add:()=>{}},getAttribute: key => key==='data-cast'? name+'.cast':null,
    querySelector: selector => selector==='.recording-speed'?speeds:selector==='.recording-chapters'?null:screen,speeds,buttons,screen};
  return figure;
});
const document = {getElementById: id => figures.find(f => id===f.name+'-recording'),
  querySelectorAll: () => figures, createElement: () => ({setAttribute: (name,value) => loads.push([name,value])}),
  head:{appendChild: element => queueMicrotask(()=>element.onload())}};
const window = {AsciinemaPlayer:{create: (cast,screen,options)=>{
  calls.push({cast,screen,options});return {dispose:()=>{},addEventListener:()=>{},getCurrentTime:()=>17};
}}};
vm.runInNewContext(fs.readFileSync(process.argv[1],'utf8'),{document,window,Promise,Number,Object,String});
setImmediate(async ()=>{
 assert.deepStrictEqual(calls.map(c=>c.cast),['tour.cast','throughput.cast']);
 assert.strictEqual(loads.filter(([name])=>name==='src').length,1);
 figures[1].screen.pointerdown();
 figures[1].speeds.click({target:{closest:()=>figures[1].buttons[1]}});
 await Promise.resolve();
 assert.strictEqual(calls.length,3);assert.strictEqual(calls[2].cast,'throughput.cast');
 assert.strictEqual(calls[2].options.speed,2);assert.strictEqual(calls[2].options.startAt,17);
 assert.strictEqual(calls[0].options.speed,1);
});
"""
        subprocess.run(['node', '-e', program, str(ROOT / 'docs/website/assets/tour-player.js')], check=True)
