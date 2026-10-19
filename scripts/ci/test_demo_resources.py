"""The demos measure their own resource use (#671).

scripts/demo/resources.py samples Bun and its workloads while a demo runs,
prints a one-line readout at each step and a summary at the end, and can
write the samples as CSV with SVG charts beside them. These tests cover the
parsing, arithmetic and output; tests/demos.rs runs the sampler for real.
"""
import os
import pathlib
import sys
import tempfile
import unittest
import xml.etree.ElementTree as ET

ROOT = pathlib.Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / 'scripts/demo'))

import resources  # noqa: E402

Proc = resources.Proc


def row(elapsed, node='demo-node', **values):
    full = {column: None for column in resources.COLUMNS}
    full.update(elapsed_s=float(elapsed), node=node)
    full.update(values)
    return full


class Parsing(unittest.TestCase):
    def test_cpu_time_reads_the_macos_and_linux_ps_formats(self):
        self.assertEqual(resources.parse_cpu_time('1:02.50'), 62.5)
        self.assertEqual(resources.parse_cpu_time('0:00.07'), 0.07)
        self.assertEqual(resources.parse_cpu_time('00:01:02'), 62.0)
        self.assertEqual(resources.parse_cpu_time('1-00:00:01'), 86401.0)
        self.assertIsNone(resources.parse_cpu_time('soon'))

    def test_ps_output_becomes_a_process_table_with_whole_commands(self):
        text = ('  101     1  40960   0:01.50 /work/bun --config /tmp/demo/node.toml --cluster\n'
                '  202   101   8192   0:00.20 /work/testapp --port 29204\n'
                'garbage line\n')
        table = resources.parse_ps(text)
        self.assertEqual(table[101], Proc(1, 40960, 1.5, '/work/bun --config /tmp/demo/node.toml --cluster'))
        self.assertEqual(table[202].ppid, 101)
        self.assertEqual(len(table), 2)

    def test_proc_stat_survives_a_command_name_with_spaces_and_parentheses(self):
        # state, ppid, then utime and stime at 14 and 15 and rss at 24 (proc(5)).
        fields = ['S', '7'] + ['0'] * 9 + ['250', '50'] + ['0'] * 8 + ['1000', '0']
        stat = '42 (tokio (worker) 1) ' + ' '.join(fields)
        ppid, rss_kib, cpu_seconds = resources.parse_proc_stat(stat, clk_tck=100, page_kib=4)
        self.assertEqual((ppid, rss_kib, cpu_seconds), (7, 4000, 3.0))

    def test_a_guest_report_reads_bun_memory_cpu_and_network(self):
        report = resources.parse_guest_report(
            'uptime 1000.50 3000.00\n'
            'clk_tck 100\n'
            'bun_pid 812\n'
            'bun_rss_kb 51200\n'
            'bun_cpu_ticks 400\n'
            'cpu 100 0 50 9000 10 0 0 0 0 0\n'
            'MemTotal: 2000000 kB\n'
            'MemAvailable: 1500000 kB\n'
            'net eth0 1024 2048\n'
            'net lima0 1024 0\n')
        self.assertEqual(report['uptime'], 1000.5)
        self.assertEqual(report['bun_rss_kib'], 51200)
        self.assertEqual(report['busy_ticks'], 150)
        self.assertEqual(report['memory_used_kib'], 500000)
        self.assertEqual((report['net_rx_bytes'], report['net_tx_bytes']), (2048, 2048))

    def test_a_guest_without_bun_running_reports_no_bun_fields(self):
        report = resources.parse_guest_report('uptime 10.0 0\nclk_tck 100\nbun_pid 0\n')
        self.assertNotIn('bun_rss_kib', report)


class LocalSamples(unittest.TestCase):
    TABLE = {
        1: Proc(0, 100, 0.0, '/sbin/launchd'),
        101: Proc(1, 40960, 1.5, '/work/bun --config /tmp/demo/node.toml'),
        150: Proc(1, 2048, 0.1, '/work/bun process-owner --state /tmp/demo/data/owners/web'),
        202: Proc(150, 8192, 0.2, '/work/testapp --port 29204'),
        303: Proc(101, 1024, 0.0, 'git ls-remote'),
        404: Proc(1, 9999, 50.0, '/other/testapp'),
    }

    def test_workloads_are_bun_descendants_and_processes_naming_the_demo_directory(self):
        self.assertEqual(resources.workload_pids(self.TABLE, 101, '/tmp/demo'), {150, 202, 303})

    def test_the_sampler_and_the_ps_it_runs_are_never_workloads(self):
        # The sampler's own command line names the marker, and ps is its child.
        me = os.getpid()
        table = dict(self.TABLE)
        table[me] = Proc(1, 30000, 0.1, 'python3 resources.py sample --marker /tmp/demo')
        table[me + 1] = Proc(me, 2048, 0.0, 'ps -A -ww -o pid=,ppid=,rss=,time=,command=')
        self.assertEqual(resources.workload_pids(table, 101, '/tmp/demo'), {150, 202, 303})

    def test_a_first_sample_has_memory_but_no_cpu_yet(self):
        sample = resources.local_row(None, self.TABLE, 1.0, 101, '/tmp/demo')
        self.assertEqual(sample['bun_rss_mib'], 40.0)
        self.assertEqual(sample['workload_rss_mib'], 11.0)
        self.assertEqual(sample['workloads'], 3)
        self.assertIsNone(sample['bun_cpu_percent'])
        self.assertIsNone(sample['net_rx_kib_s'])

    def test_cpu_is_the_cpu_time_used_over_the_interval_in_percent_of_one_core(self):
        later = dict(self.TABLE)
        later[101] = later[101]._replace(cpu_seconds=2.0)
        later[202] = later[202]._replace(cpu_seconds=0.45)
        later[505] = Proc(150, 4096, 9.0, '/work/testapp --port 29205')
        sample = resources.local_row(self.TABLE, later, 2.0, 101, '/tmp/demo')
        self.assertEqual(sample['bun_cpu_percent'], 25.0)
        # 505 is new: its memory counts now, its CPU from its second sample on.
        self.assertEqual(sample['workload_cpu_percent'], 12.5)
        self.assertEqual(sample['workload_rss_mib'], 15.0)

    def test_bun_gone_means_no_sample(self):
        table = {pid: proc for pid, proc in self.TABLE.items() if pid != 101}
        self.assertIsNone(resources.local_row(self.TABLE, table, 1.0, 101, '/tmp/demo'))


class GuestSamples(unittest.TestCase):
    FIRST = {'uptime': 100.0, 'clk_tck': 100, 'bun_pid': 7, 'bun_rss_kib': 51200, 'bun_ticks': 1000,
             'busy_ticks': 5000, 'memory_used_kib': 409600, 'net_rx_bytes': 0, 'net_tx_bytes': 0}

    def test_rates_come_from_the_guest_clock_between_two_reports(self):
        second = dict(self.FIRST, uptime=105.0, bun_ticks=1050, busy_ticks=5200,
                      net_rx_bytes=10240, net_tx_bytes=5120)
        sample = resources.guest_row(self.FIRST, second)
        self.assertEqual(sample['bun_cpu_percent'], 10.0)
        self.assertEqual(sample['guest_cpu_percent'], 40.0)
        self.assertEqual(sample['bun_rss_mib'], 50.0)
        self.assertEqual(sample['guest_memory_mib'], 400.0)
        self.assertEqual((sample['net_rx_kib_s'], sample['net_tx_kib_s']), (2.0, 1.0))

    def test_a_restarted_bun_gets_no_cpu_rate_instead_of_a_negative_one(self):
        second = dict(self.FIRST, uptime=105.0, bun_pid=8, bun_ticks=10)
        self.assertIsNone(resources.guest_row(self.FIRST, second)['bun_cpu_percent'])

    def test_a_rebooted_guest_starts_over(self):
        second = dict(self.FIRST, uptime=3.0)
        sample = resources.guest_row(self.FIRST, second)
        self.assertIsNone(sample['guest_cpu_percent'])
        self.assertEqual(sample['guest_memory_mib'], 400.0)


class Output(unittest.TestCase):
    ROWS = [
        row(0, bun_rss_mib=40.0, workload_rss_mib=10.0, workloads=1),
        row(1, bun_cpu_percent=10.0, bun_rss_mib=42.0, workload_cpu_percent=2.0, workload_rss_mib=12.0, workloads=2),
        row(2, bun_cpu_percent=30.0, bun_rss_mib=44.0, workload_cpu_percent=4.0, workload_rss_mib=14.0, workloads=2),
    ]

    def test_csv_round_trips_with_blanks_for_what_was_not_measured(self):
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / 'resources.csv'
            with open(path, 'w') as out:
                resources.write_header(out)
                for sample in self.ROWS:
                    resources.write_row(out, sample)
            text = path.read_text()
            self.assertTrue(text.startswith(','.join(resources.COLUMNS) + '\n'))
            self.assertEqual(resources.read_rows(path), self.ROWS)

    def test_a_missing_or_torn_csv_reads_as_no_rows(self):
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / 'resources.csv'
            self.assertEqual(resources.read_rows(path), [])
            path.write_text(','.join(resources.COLUMNS) + '\n1.0,demo-node,5')
            self.assertEqual(resources.read_rows(path), [])

    def test_the_summary_gives_the_average_and_peak_of_each_measure(self):
        text = '\n'.join(resources.format_summary(self.ROWS))
        self.assertIn('3 samples over 2 s', text)
        self.assertRegex(text, r'bun CPU\s+20\.0 %\s+30\.0 %')
        self.assertRegex(text, r'bun memory\s+42\.0 MiB\s+44\.0 MiB')
        self.assertRegex(text, r'workload memory\s+12\.0 MiB\s+14\.0 MiB')
        self.assertIn('network: not measured', text)

    def test_the_summary_adds_up_the_nodes_of_each_round(self):
        rows = [row(0, node='a', bun_rss_mib=40.0), row(0, node='b', bun_rss_mib=60.0),
                row(5, node='a', bun_rss_mib=50.0, net_rx_kib_s=1.0, net_tx_kib_s=2.0),
                row(5, node='b', bun_rss_mib=70.0, net_rx_kib_s=3.0, net_tx_kib_s=4.0)]
        text = '\n'.join(resources.format_summary(rows))
        self.assertIn('2 nodes, added together', text)
        self.assertRegex(text, r'bun memory\s+110\.0 MiB\s+120\.0 MiB')
        self.assertRegex(text, r'network in\s+4\.0 KiB/s\s+4\.0 KiB/s')
        self.assertNotIn('not measured', text)

    def test_no_samples_says_so(self):
        self.assertEqual(resources.format_summary([]), ['resource use: no samples recorded'])

    def test_the_readout_shows_the_latest_round_on_one_line(self):
        self.assertEqual(resources.format_readout(self.ROWS),
                         'resources: bun 44.0 MiB, 30.0 % CPU · 2 workloads 14.0 MiB, 4.0 % CPU')
        self.assertEqual(resources.format_readout([]), '')

    def test_charts_draw_cpu_and_memory_with_the_steps_marked(self):
        with tempfile.TemporaryDirectory() as directory:
            out = pathlib.Path(directory)
            written = resources.write_charts(self.ROWS, [(1.0, 'Deploy v1')], out)
            self.assertEqual(written, ['cpu.svg', 'memory.svg'])
            for name in written:
                svg = (out / name).read_text()
                ET.fromstring(svg)
                self.assertIn('step: Deploy v1', svg)
            again = pathlib.Path(directory) / 'again'
            resources.write_charts(self.ROWS, [(1.0, 'Deploy v1')], again)
            self.assertEqual((out / 'cpu.svg').read_bytes(), (again / 'cpu.svg').read_bytes())

    def test_a_single_sample_draws_no_chart(self):
        with tempfile.TemporaryDirectory() as directory:
            self.assertEqual(resources.write_charts(self.ROWS[:1], [], pathlib.Path(directory)), [])

    def test_steps_read_from_their_file(self):
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / 'steps.tsv'
            path.write_text('0\tStarting a cluster\n12\tDeploy v1\nnot a step\n')
            self.assertEqual(resources.read_steps(path), [(0.0, 'Starting a cluster'), (12.0, 'Deploy v1')])


if __name__ == '__main__':
    unittest.main()
