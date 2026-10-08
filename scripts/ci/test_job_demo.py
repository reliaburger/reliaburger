"""The published job demo must describe its actual uncompressed recording."""
import json
import pathlib
import subprocess
import shutil
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]

class JobDemo(unittest.TestCase):
    def test_visible_demo_precedes_install_and_matches_accepted_evidence(self):
        page = (ROOT / 'docs/website/index.html').read_text()
        section = page.index('<section id="job-throughput"')
        self.assertLess(section, page.index('<section id="install"'))
        figure = page.index('id="throughput-recording"', section)
        self.assertNotIn('<details', page[section:figure])
        report = json.loads((ROOT / 'docs/website/assets/job-throughput-report.json').read_text())
        recording = [json.loads(line) for line in
                     (ROOT / 'docs/website/assets/job-throughput.cast').read_text().splitlines()]
        self.assertTrue(report['all_tasks_succeeded'])
        self.assertFalse(report['qualified_100m_per_day'])
        self.assertGreaterEqual(recording[-1][0], report['accepted_elapsed_seconds'])
        self.assertEqual(recording[0]['version'], 2)
        raw = ROOT / 'docs/qualification/2026-10-08-job-measurements/release-public/release-public-reused-50000'
        self.assertEqual((ROOT / 'docs/website/assets/job-throughput.cast').read_bytes(),
                         (raw / 'jobs.cast').read_bytes())
        self.assertEqual((ROOT / 'docs/website/assets/job-throughput-report.json').read_bytes(),
                         (raw / 'report.json').read_bytes())
        self.assertEqual([row[0] for row in recording[1:]],
                         sorted(row[0] for row in recording[1:]))
        text = page[section:page.index('</section>', section)]
        self.assertIn(f"{report['unique_accepted_successes']:,}", text)
        self.assertIn(f"{report['accepted_elapsed_seconds']:.2f}", text)
        self.assertIn('development binaries', text)
        self.assertIn('24-hour', text)
        self.assertRegex(text, r'data-cast="\./assets/job-throughput\.cast"')
        self.assertIn('./assets/job-throughput-report.json', text)

    @unittest.skipUnless(shutil.which('node'), 'requires Node for the browser script contract')
    def test_all_recordings_load_once_and_keep_independent_speed_controls(self):
        program = r"""
const fs = require('fs'), vm = require('vm'), assert = require('assert');
const calls = [], loads = [], figures = ['tour','jobs','throughput'].map(name => {
  const buttons = [1,2].map(speed => ({getAttribute: () => String(speed), setAttribute: () => {}}));
  const speeds = {hidden:true,querySelectorAll: () => buttons,addEventListener: (_, callback) => speeds.click=callback};
  const screen = {name,addEventListener:(event,callback)=>screen[event]=callback};
  const figure = {name, classList:{add:()=>{}},getAttribute: key => key==='data-cast'? name+'.cast':null,
    querySelector: selector => selector==='.recording-speed'?speeds:screen,speeds,buttons,screen};
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
 assert.deepStrictEqual(calls.map(c=>c.cast),['tour.cast','jobs.cast','throughput.cast']);
 assert.strictEqual(loads.filter(([name])=>name==='src').length,1);
 figures[2].screen.pointerdown();
 figures[2].speeds.click({target:{closest:()=>figures[2].buttons[1]}});
 await Promise.resolve();
 assert.strictEqual(calls.length,4);assert.strictEqual(calls[3].cast,'throughput.cast');
 assert.strictEqual(calls[3].options.speed,2);assert.strictEqual(calls[3].options.startAt,17);
 assert.strictEqual(calls[0].options.speed,1);
});
"""
        subprocess.run(['node', '-e', program, str(ROOT / 'docs/website/assets/tour-player.js')], check=True)
