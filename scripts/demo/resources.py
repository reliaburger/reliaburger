"""Measure a demo's resource use while it runs (#671).

The demos claim Reliaburger is light, so they show it. `sample` runs in the
background for the length of a demo and appends one CSV row per node a
round: Bun's CPU and resident memory, and, where it can see them, its
workloads', the node's and its network traffic. `readout` prints the latest
round on one line (lib.sh shows it at every step, so it lands in a recording
at the moment it describes), `summary` prints each measure's average and
peak, and `chart` draws the samples as SVG with the soak charts' code, the
demo's steps marked along the time axis.

Two sources:

* `--pid PID`: a node running on this machine, as the feature demos start
  it. Bun is the process; its workloads are its descendants plus anything
  whose command line names `--marker` (the process runtime's owners outlive
  Bun, so they aren't its children) and their descendants. Linux reads
  /proc; macOS reads `ps`. Network isn't measured: the node talks to itself
  over loopback, and neither system counts that per process.
* `--quickstart HOME`: the laptop cluster `relish setup --quickstart`
  built, as the tour drives it. Each node is a Lima VM, read through
  `limactl shell`: Bun's memory and CPU, the whole guest's (Bun, its
  workloads and the OS together), and the guest's network traffic.

Standard library only. Anything that can't be measured is left blank, never
guessed, and the summary says so.
"""
import argparse
from collections import namedtuple
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import threading
import time

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "release"))
import soak_charts  # noqa: E402

COLUMNS = (
    "elapsed_s", "node",
    "bun_cpu_percent", "bun_rss_mib",
    "workload_cpu_percent", "workload_rss_mib", "workloads",
    "guest_cpu_percent", "guest_memory_mib",
    "net_rx_kib_s", "net_tx_kib_s",
)

# One process: its parent, resident memory, CPU time used so far and command.
Proc = namedtuple("Proc", "ppid rss_kib cpu_seconds command")

# What one guest reports through `limactl shell`. Only /proc and systemctl,
# so it needs no root and nothing installed.
GUEST_SCRIPT = r"""
echo "uptime $(cat /proc/uptime)"
echo "clk_tck $(getconf CLK_TCK)"
pid=$(systemctl show -p MainPID --value reliaburger.service 2>/dev/null)
echo "bun_pid ${pid:-0}"
if [ "${pid:-0}" -gt 0 ]; then
    echo "bun_rss_kb $(awk '/^VmRSS/ {print $2}' /proc/"$pid"/status)"
    echo "bun_cpu_ticks $(sed 's/.*) //' /proc/"$pid"/stat | awk '{print $12 + $13}')"
fi
head -n 1 /proc/stat
grep -E '^(MemTotal|MemAvailable):' /proc/meminfo
awk -F'[: ]+' 'NR > 2 {print "net", $2, $3, $11}' /proc/net/dev
"""
# The guest's own links. Loopback, bridges and container veths would count
# the same bytes twice.
GUEST_LINK = re.compile(r"^(eth|en|lima)")


def parse_cpu_time(text):
    """Seconds from ps's TIME column: [[dd-]hh:]mm:ss[.hh]; None if unreadable."""
    days, _, rest = text.strip().rpartition("-")
    try:
        seconds = 0.0
        for part in rest.split(":"):
            seconds = seconds * 60 + float(part)
        return seconds + (int(days) * 86400 if days else 0)
    except ValueError:
        return None


def parse_ps(text):
    """`ps -A -ww -o pid=,ppid=,rss=,time=,command=` as {pid: Proc}."""
    table = {}
    for line in text.splitlines():
        fields = line.split(None, 4)
        if len(fields) < 4:
            continue
        try:
            pid, ppid, rss = int(fields[0]), int(fields[1]), int(fields[2])
        except ValueError:
            continue
        cpu = parse_cpu_time(fields[3])
        if cpu is None:
            continue
        table[pid] = Proc(ppid, rss, cpu, fields[4] if len(fields) > 4 else "")
    return table


def parse_proc_stat(text, clk_tck, page_kib):
    """(ppid, rss_kib, cpu_seconds) from /proc/PID/stat."""
    # The command name sits in parentheses and may hold spaces or more
    # parentheses, so count fields from the last closing one.
    fields = text[text.rindex(")") + 2:].split()
    ppid = int(fields[1])
    cpu_seconds = (int(fields[11]) + int(fields[12])) / clk_tck
    return ppid, int(fields[21]) * page_kib, cpu_seconds


def read_proc_table():
    """Every process from /proc (Linux)."""
    clk_tck = os.sysconf("SC_CLK_TCK")
    page_kib = os.sysconf("SC_PAGE_SIZE") // 1024
    table = {}
    for entry in os.scandir("/proc"):
        if not entry.name.isdigit():
            continue
        try:
            with open(f"/proc/{entry.name}/stat") as stat:
                ppid, rss_kib, cpu_seconds = parse_proc_stat(stat.read(), clk_tck, page_kib)
            with open(f"/proc/{entry.name}/cmdline", "rb") as cmdline:
                command = cmdline.read().replace(b"\0", b" ").decode(errors="replace").strip()
        except (OSError, ValueError, IndexError):
            continue
        table[int(entry.name)] = Proc(ppid, rss_kib, cpu_seconds, command)
    return table


def read_ps_table():
    """Every process from ps (macOS, or any system without /proc)."""
    result = subprocess.run(["ps", "-A", "-ww", "-o", "pid=,ppid=,rss=,time=,command="],
                            capture_output=True, text=True, timeout=10, check=False)
    return parse_ps(result.stdout)


def read_table():
    return read_proc_table() if os.path.isdir("/proc/self") else read_ps_table()


def workload_pids(table, bun_pid, marker):
    """Bun's descendants and the processes naming `marker`, with theirs."""
    children = {}
    for pid, proc in table.items():
        children.setdefault(proc.ppid, []).append(pid)
    roots = [bun_pid] + [pid for pid, proc in table.items() if marker and marker in proc.command]
    # The sampler names the marker too, and the ps it runs is its child.
    sampler = os.getpid()
    found, stack = set(), list(roots)
    while stack:
        pid = stack.pop()
        if pid in found or pid == sampler:
            continue
        found.add(pid)
        stack.extend(children.get(pid, []))
    found.discard(bun_pid)
    return {pid for pid in found if pid in table}


def percent(used_seconds, interval):
    if used_seconds < 0 or interval <= 0:
        return None
    return round(used_seconds / interval * 100, 3)


def empty_row():
    return {column: None for column in COLUMNS}


def local_row(previous, table, interval, bun_pid, marker):
    """One sample of a local node, or None once Bun has gone."""
    bun = table.get(bun_pid)
    if bun is None:
        return None
    workloads = workload_pids(table, bun_pid, marker)
    sample = empty_row()
    sample["bun_rss_mib"] = round(bun.rss_kib / 1024, 3)
    sample["workload_rss_mib"] = round(sum(table[pid].rss_kib for pid in workloads) / 1024, 3)
    sample["workloads"] = len(workloads)
    if previous is not None and bun_pid in previous:
        sample["bun_cpu_percent"] = percent(bun.cpu_seconds - previous[bun_pid].cpu_seconds, interval)
        used = 0.0
        for pid in workloads:
            # A process seen for the first time has no earlier reading; its
            # CPU counts from its second sample on.
            if pid in previous and table[pid].cpu_seconds >= previous[pid].cpu_seconds:
                used += table[pid].cpu_seconds - previous[pid].cpu_seconds
        sample["workload_cpu_percent"] = percent(used, interval)
    return sample


def parse_guest_report(text):
    """What GUEST_SCRIPT printed, as numbers; missing readings are left out."""
    report, rx, tx, memory = {}, 0, 0, {}
    links = False
    for line in text.splitlines():
        fields = line.split()
        if not fields:
            continue
        try:
            key = fields[0]
            if key == "uptime":
                report["uptime"] = float(fields[1])
            elif key == "clk_tck":
                report["clk_tck"] = int(fields[1])
            elif key == "bun_pid" and int(fields[1]) > 0:
                report["bun_pid"] = int(fields[1])
            elif key == "bun_rss_kb":
                report["bun_rss_kib"] = int(fields[1])
            elif key == "bun_cpu_ticks":
                report["bun_ticks"] = int(fields[1])
            elif key == "cpu":
                ticks = [int(value) for value in fields[1:]]
                # Everything but idle (4th) and iowait (5th).
                report["busy_ticks"] = sum(ticks) - sum(ticks[3:5])
            elif key in ("MemTotal:", "MemAvailable:"):
                memory[key] = int(fields[1])
            elif key == "net" and GUEST_LINK.match(fields[1]):
                rx += int(fields[2])
                tx += int(fields[3])
                links = True
        except (IndexError, ValueError):
            continue
    if "bun_pid" not in report:
        report.pop("bun_rss_kib", None)
        report.pop("bun_ticks", None)
    if len(memory) == 2:
        report["memory_used_kib"] = memory["MemTotal:"] - memory["MemAvailable:"]
    if links:
        report["net_rx_bytes"], report["net_tx_bytes"] = rx, tx
    return report


def guest_row(previous, current):
    """One sample of a quickstart node from two consecutive reports."""
    sample = empty_row()
    if "bun_rss_kib" in current:
        sample["bun_rss_mib"] = round(current["bun_rss_kib"] / 1024, 3)
    if "memory_used_kib" in current:
        sample["guest_memory_mib"] = round(current["memory_used_kib"] / 1024, 3)
    if not previous or "uptime" not in current or current["uptime"] <= previous.get("uptime", current["uptime"]):
        return sample
    interval = current["uptime"] - previous["uptime"]
    hertz = current.get("clk_tck") or 100

    def rate(key):
        if key in current and key in previous and current[key] >= previous[key]:
            return current[key] - previous[key]
        return None

    if current.get("bun_pid") == previous.get("bun_pid") and rate("bun_ticks") is not None:
        sample["bun_cpu_percent"] = percent(rate("bun_ticks") / hertz, interval)
    if rate("busy_ticks") is not None:
        sample["guest_cpu_percent"] = percent(rate("busy_ticks") / hertz, interval)
    for key, column in (("net_rx_bytes", "net_rx_kib_s"), ("net_tx_bytes", "net_tx_kib_s")):
        if rate(key) is not None:
            sample[column] = round(rate(key) / 1024 / interval, 3)
    return sample


# --- CSV ---------------------------------------------------------------------

def format_value(value):
    if value is None:
        return ""
    if isinstance(value, float):
        return repr(round(value, 3))
    return str(value)


def write_header(out):
    out.write(",".join(COLUMNS) + "\n")
    out.flush()


def write_row(out, sample):
    out.write(",".join(format_value(sample.get(column)) for column in COLUMNS) + "\n")
    out.flush()


def read_rows(path):
    """Every complete row of a samples file; none if it's missing."""
    try:
        lines = Path(path).read_text().splitlines()
    except OSError:
        return []
    rows = []
    for line in lines[1:]:
        fields = line.split(",")
        if len(fields) != len(COLUMNS):
            continue
        try:
            sample = {}
            for column, value in zip(COLUMNS, fields):
                if column == "node":
                    sample[column] = value
                elif value == "":
                    sample[column] = None
                elif column == "workloads":
                    sample[column] = int(value)
                else:
                    sample[column] = float(value)
        except ValueError:
            continue
        rows.append(sample)
    return rows


def read_steps(path):
    """The demo's steps as (offset in seconds, heading), from `offset<TAB>heading` lines."""
    steps = []
    try:
        lines = Path(path).read_text().splitlines()
    except OSError:
        return steps
    for line in lines:
        offset, _, heading = line.partition("\t")
        try:
            steps.append((float(offset), heading.strip()))
        except ValueError:
            continue
    return steps


# --- Summary, readout and charts ---------------------------------------------

# (label, column, unit) in the order the summary prints them.
MEASURES = (
    ("bun CPU", "bun_cpu_percent", "%"),
    ("bun memory", "bun_rss_mib", "MiB"),
    ("workload CPU", "workload_cpu_percent", "%"),
    ("workload memory", "workload_rss_mib", "MiB"),
    ("node CPU", "guest_cpu_percent", "%"),
    ("node memory", "guest_memory_mib", "MiB"),
    ("network in", "net_rx_kib_s", "KiB/s"),
    ("network out", "net_tx_kib_s", "KiB/s"),
)


def rounds(rows):
    """[(elapsed, {column: total across nodes, or None})], oldest first."""
    grouped = {}
    for sample in rows:
        grouped.setdefault(sample["elapsed_s"], []).append(sample)
    result = []
    for elapsed in sorted(grouped):
        totals = {}
        for column in COLUMNS[2:]:
            values = [sample[column] for sample in grouped[elapsed] if sample[column] is not None]
            totals[column] = sum(values) if values else None
        result.append((elapsed, totals))
    return result


def node_count(rows):
    return len({sample["node"] for sample in rows})


def format_summary(rows):
    """Lines of the summary table: each measure's average and peak."""
    if not rows:
        return ["resource use: no samples recorded"]
    series = rounds(rows)
    duration = series[-1][0] - series[0][0]
    nodes = node_count(rows)
    where = f"on {nodes} nodes, added together" if nodes > 1 else f"on {rows[0]['node']}"
    lines = [f"Resource use: {len(series)} samples over {duration:.0f} s {where}",
             f"  {'':<18}{'average':>8}{'':7}{'peak':>8}"]
    measured = set()
    for label, column, unit in MEASURES:
        values = [totals[column] for _, totals in series if totals[column] is not None]
        if not values:
            continue
        measured.add(column)
        average = sum(values) / len(values)
        lines.append(f"  {label:<18}{average:>8.1f} {unit:<6}{max(values):>8.1f} {unit}")
    if measured & {"bun_cpu_percent", "workload_cpu_percent", "guest_cpu_percent"}:
        lines.append("  CPU is a percentage of one core.")
    if "net_rx_kib_s" not in measured:
        lines.append("  network: not measured (a local node talks over loopback, "
                     "which neither Linux nor macOS counts per process)")
    return lines


def format_readout(rows):
    """The latest round on one line, or '' before the first sample."""
    if not rows:
        return ""
    _, totals = rounds(rows)[-1]
    nodes = node_count(rows)

    def part(name, memory, cpu):
        if totals[memory] is None:
            return None
        text = f"{name} {totals[memory]:.1f} MiB"
        if totals[cpu] is not None:
            text += f", {totals[cpu]:.1f} % CPU"
        return text

    workloads = totals["workloads"]
    parts = [part("bun", "bun_rss_mib", "bun_cpu_percent"),
             part(f"{workloads:.0f} workload{'' if workloads == 1 else 's'}" if workloads is not None
                  else "workloads",
                  "workload_rss_mib", "workload_cpu_percent"),
             part("nodes" if nodes > 1 else "node", "guest_memory_mib", "guest_cpu_percent")]
    if totals["net_rx_kib_s"] is not None and totals["net_tx_kib_s"] is not None:
        parts.append(f"network {totals['net_rx_kib_s']:.1f} in, {totals['net_tx_kib_s']:.1f} out KiB/s")
    text = " · ".join(item for item in parts if item)
    prefix = f"resources ({nodes} nodes): " if nodes > 1 else "resources: "
    return prefix + text if text else ""


def write_charts(rows, steps, directory):
    """Draw CPU, memory and network charts into `directory`; return the file names."""
    series = rounds(rows)
    if not series:
        return []
    start = series[0][0]
    duration = series[-1][0] - start
    nodes = node_count(rows)
    added = f", {nodes} nodes added together" if nodes > 1 else ""

    def points(column):
        return [(elapsed - start, totals[column]) for elapsed, totals in series if totals[column] is not None]

    plans = (
        ("cpu.svg", f"CPU during the demo{added}", "% of one core",
         (("bun", 1, "bun_cpu_percent"), ("workloads", 2, "workload_cpu_percent"), ("node", 3, "guest_cpu_percent"))),
        ("memory.svg", f"Resident memory during the demo{added}", "MiB",
         (("bun", 1, "bun_rss_mib"), ("workloads", 2, "workload_rss_mib"), ("node", 3, "guest_memory_mib"))),
        ("network.svg", f"Network traffic during the demo{added}", "KiB/s",
         (("in", 1, "net_rx_kib_s"), ("out", 2, "net_tx_kib_s"))),
    )
    marks = [(offset - start, "step", heading) for offset, heading in steps]
    written = []
    for name, title, unit, wanted in plans:
        lines = [(label, slot, points(column)) for label, slot, column in wanted if points(column)]
        if not any(len(line[2]) > 1 for line in lines):
            continue
        directory = Path(directory)
        directory.mkdir(parents=True, exist_ok=True)
        svg = soak_charts.draw(title, unit, lines, duration, marks, key=(("step", "demo step"),))
        (directory / name).write_text(svg)
        written.append(name)
    return written


# --- Sampling ------------------------------------------------------------------

class Stop:
    """Set by SIGTERM or SIGINT, so a sample in progress finishes its row."""

    def __init__(self):
        self.event = threading.Event()
        signal.signal(signal.SIGTERM, lambda *_: self.event.set())
        signal.signal(signal.SIGINT, lambda *_: self.event.set())


def sample_local(args, out, start, stop):
    previous, previous_at = None, None
    while not stop.event.is_set():
        now = time.time()
        try:
            table = read_table()
        except (OSError, subprocess.SubprocessError):
            table = {}
        interval = now - previous_at if previous_at else 0
        sample = local_row(previous, table, interval, args.pid, args.marker)
        if sample is not None:
            sample.update(elapsed_s=round(now - start, 3), node=args.node)
            write_row(out, sample)
            previous, previous_at = table, now
        stop.event.wait(max(0.0, args.interval - (time.time() - now)))


def quickstart_nodes(home, cluster):
    """(limactl, environment, VM names) for a quickstart cluster, or None."""
    import json
    try:
        state = json.loads((home / "clusters" / cluster / "state.json").read_text())
        names = [node["name"] for node in state["nodes"]]
    except (OSError, ValueError, KeyError, TypeError):
        return None
    bundled = sorted(home.glob("tools/lima-*/bin/limactl"))
    limactl = str(bundled[-1]) if bundled else shutil.which("limactl")
    if not limactl or not names:
        return None
    environment = dict(os.environ)
    if (home / "lima").is_dir():
        environment["LIMA_HOME"] = str(home / "lima")
    return limactl, environment, names


def guest_report(limactl, environment, name):
    try:
        result = subprocess.run([limactl, "shell", "--workdir", "/", name, "sh", "-c", GUEST_SCRIPT],
                                capture_output=True, text=True, timeout=20, env=environment, check=False)
    except (OSError, subprocess.SubprocessError):
        return {}
    return parse_guest_report(result.stdout) if result.returncode == 0 else {}


def sample_quickstart(args, out, start, stop):
    found = quickstart_nodes(Path(args.quickstart), args.cluster)
    if found is None:
        print(f"resources.py: no quickstart cluster in {args.quickstart}", file=sys.stderr)
        return
    limactl, environment, names = found
    previous = {}
    while not stop.event.is_set():
        now = time.time()
        reports = {}
        threads = [threading.Thread(target=lambda n=name: reports.__setitem__(n, guest_report(limactl, environment, n)))
                   for name in names]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join()
        for name in names:
            report = reports.get(name) or {}
            if not report:
                continue
            sample = guest_row(previous.get(name), report)
            sample.update(elapsed_s=round(now - start, 3), node=name)
            write_row(out, sample)
            previous[name] = report
        stop.event.wait(max(0.0, args.interval - (time.time() - now)))


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    commands = parser.add_subparsers(dest="command", required=True)
    sample = commands.add_parser("sample", help="sample until SIGTERM, appending to --csv")
    source = sample.add_mutually_exclusive_group(required=True)
    source.add_argument("--pid", type=int, help="Bun's pid, for a node on this machine")
    source.add_argument("--quickstart", help="RELIABURGER_HOME of a quickstart cluster")
    sample.add_argument("--marker", default="", help="text in the command line of Bun's detached workloads")
    sample.add_argument("--node", default="local", help="the local node's name")
    sample.add_argument("--cluster", default="laptop", help="the quickstart cluster's name")
    sample.add_argument("--csv", required=True)
    sample.add_argument("--interval", type=float, default=1.0)
    sample.add_argument("--start", type=float, help="epoch seconds the demo started (default: now)")
    for name in ("readout", "summary"):
        commands.add_parser(name).add_argument("--csv", required=True)
    chart = commands.add_parser("chart")
    chart.add_argument("--csv", required=True)
    chart.add_argument("--steps", required=True)
    chart.add_argument("--out", required=True)
    args = parser.parse_args(argv)

    if args.command == "sample":
        stop = Stop()
        start = args.start or time.time()
        with open(args.csv, "w") as out:
            write_header(out)
            if args.pid is not None:
                sample_local(args, out, start, stop)
            else:
                sample_quickstart(args, out, start, stop)
        return 0
    rows = read_rows(args.csv)
    if args.command == "readout":
        text = format_readout(rows)
        if text:
            print(text)
    elif args.command == "summary":
        print("\n".join(format_summary(rows)))
    else:
        for name in write_charts(rows, read_steps(args.steps), args.out):
            print(Path(args.out) / name)
    return 0


if __name__ == "__main__":
    sys.exit(main())
