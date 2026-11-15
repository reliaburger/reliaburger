"""Draw a soak's resource and job time series as SVG charts (#979).

The soak keeps everything a chart needs in its evidence directory: each heavy
and settle snapshot holds every node's `inventory-<node>.txt` (Bun's pid, RSS,
open fds and CPU ticks), every snapshot taken during a job soak holds the
controller's `jobs.json` (cumulative successes per runtime), and
`events.jsonl` records each fault, upgrade walk and special. This module turns
those into small, static SVG files that sit beside the record.

Standard library only, and deterministic: the same evidence always gives the
same bytes, so a re-render shows no diff. Colours are CSS custom properties
inside each SVG with a dark-scheme override, so the charts follow the reader's
light or dark setting wherever the SVG is shown.
"""
from dataclasses import dataclass, field
import json
import math
from pathlib import Path
from xml.sax.saxutils import escape

# Bun's steady-state RSS targets from the job qualification (D18-C, #668): node 1
# serves the API and gets more room.
API_NODE_TARGET_MIB = 512
OTHER_NODE_TARGET_MIB = 384
# Accepted jobs a second are averaged over windows at least this long.
JOB_WINDOW_SECONDS = 600
# A line breaks where samples are further apart than this (a powered-off node).
GAP_SECONDS = 1800

WIDTH, HEIGHT = 720, 272
LEFT, RIGHT, TOP, BOTTOM = 56, 96, 44, 62

LIGHT = {"surface": "#fcfcfb", "ink": "#0b0b0b", "ink-2": "#52514e", "muted": "#8a8984", "grid": "#e6e5e0",
         "series-1": "#2a78d6", "series-2": "#eb6834", "series-3": "#1baf7a", "series-other": "#8a8984"}
DARK = {"surface": "#1a1a19", "ink": "#f0efec", "ink-2": "#c3c2b7", "muted": "#8f8e86", "grid": "#33332f",
        "series-1": "#3987e5", "series-2": "#d95926", "series-3": "#199e70", "series-other": "#8f8e86"}


def colour(name):
    # The light value is also the fallback, for renderers without custom properties.
    return f"var(--{name},{LIGHT[name]})"


STYLE = (
    "svg{" + ";".join(f"--{key}:{value}" for key, value in LIGHT.items()) + "}\n"
    "@media (prefers-color-scheme: dark){svg{" + ";".join(f"--{key}:{value}" for key, value in DARK.items()) + "}}\n"
    'text{font-family:system-ui,-apple-system,"Segoe UI",sans-serif;font-size:11px;fill:' + colour("ink-2") + "}\n"
    ".title{font-size:13px;font-weight:600;fill:" + colour("ink") + "}\n"
    ".surface{fill:" + colour("surface") + "}\n"
    ".grid{stroke:" + colour("grid") + ";stroke-width:1}\n"
    ".axis{stroke:" + colour("muted") + ";stroke-width:1}\n"
    ".line{fill:none;stroke-width:2;stroke-linejoin:round;stroke-linecap:round}\n"
    + "".join(f".s{slot}{{stroke:{colour(name)}}}.f{slot}{{fill:{colour(name)}}}"
              for slot, name in ((1, "series-1"), (2, "series-2"), (3, "series-3"), (0, "series-other"))) + "\n"
    ".target{stroke:" + colour("ink-2") + ";stroke-width:1;stroke-dasharray:4 3}\n"
    ".mark{stroke:" + colour("ink-2") + ";fill:none;stroke-width:1.5}\n"
    ".mark.failure{stroke:" + colour("ink") + ";stroke-width:2}\n"
    ".mark.special,.mark.upgrade{fill:" + colour("surface") + "}"
)


@dataclass
class Series:
    """A soak's time series, as offsets in seconds from the start of the run."""
    nodes: list
    duration: int
    rss: dict = field(default_factory=dict)
    fds: dict = field(default_factory=dict)
    cpu: dict = field(default_factory=dict)
    jobs: list = field(default_factory=list)
    target_rate: float = None
    marks: list = field(default_factory=list)


@dataclass
class Chart:
    """One written chart: its short name, file name and alt text."""
    name: str
    file: str
    title: str


def read_json(path):
    try:
        return json.loads(Path(path).read_text())
    except (OSError, ValueError):
        return None


def parse_inventory(text):
    values = {}
    for line in text.splitlines():
        key, _, value = line.partition(" ")
        if key in ("boot", "bun_pid", "bun_rss_kb", "bun_fd", "bun_cpu_ticks", "clk_tck") and key not in values:
            values[key] = value.strip()
    return values


def number(value):
    try:
        return int(value)
    except (TypeError, ValueError):
        return None


def snapshots(evidence):
    """Every snapshot directory with a readable timestamp, oldest first."""
    rows = []
    root = Path(evidence) / "snapshots"
    if not root.is_dir():
        return rows
    for directory in root.iterdir():
        meta = read_json(directory / "meta.json") if directory.is_dir() else None
        if isinstance(meta, dict) and isinstance(meta.get("ts"), int):
            rows.append((meta["ts"], directory.name, directory))
    return [(ts, directory) for ts, _, directory in sorted(rows)]


def event_marks(evidence, start):
    """Faults, upgrade walks, specials and failures, as (offset, kind, label)."""
    marks = []
    path = Path(evidence) / "events.jsonl"
    if not path.exists():
        return marks
    for line in path.read_text().splitlines():
        try:
            event = json.loads(line)
        except ValueError:
            continue
        phase, ts, verdict = event.get("phase", ""), event.get("ts"), event.get("verdict")
        if not isinstance(ts, int):
            continue
        if phase == "failure":
            marks.append((ts - start, "failure", event.get("target") or "failure"))
        elif phase == "upgrade:upgrade":
            marks.append((ts - (event.get("duration") or 0) - start, "upgrade", "upgrade walk"))
        elif phase.startswith("fault:") and verdict in (None, "ok"):
            marks.append((ts - start, "fault", phase[len("fault:"):]))
        elif phase.startswith("special:") and phase != "special:quorum-back" and verdict in (None, "ok"):
            marks.append((ts - start, "special", phase[len("special:"):]))
    kept, last = [], {}
    for offset, kind, label in sorted(marks):
        # A fault logs its start and, for some, a second "ok" line: draw it once.
        if (kind, label) in last and offset - last[(kind, label)] < 60:
            continue
        last[(kind, label)] = offset
        kept.append((offset, kind, label))
    return kept


def collect(evidence):
    """Read a soak's evidence directory into a Series."""
    evidence = Path(evidence)
    metadata = read_json(evidence / "metadata.json") or {}
    state = read_json(evidence / "state.json") or {}
    rows = snapshots(evidence)
    start = metadata.get("started_at") or (rows[0][0] if rows else 0)
    finished = metadata.get("finished_at") or (rows[-1][0] if rows else start)
    series = Series(nodes=list(state.get("nodes") or []), duration=max(0, finished - start))
    previous = {}
    successes = []
    for ts, directory in rows:
        offset = ts - start
        for path in sorted(directory.glob("inventory-*.txt")):
            node = path.stem[len("inventory-"):]
            values = parse_inventory(path.read_text())
            rss, fds = number(values.get("bun_rss_kb")), number(values.get("bun_fd"))
            if rss is None:
                continue
            if node not in series.nodes:
                series.nodes.append(node)
            series.rss.setdefault(node, []).append((offset, round(rss / 1024, 3)))
            if fds is not None:
                series.fds.setdefault(node, []).append((offset, fds))
            process = (values.get("boot"), values.get("bun_pid"))
            ticks, hertz = number(values.get("bun_cpu_ticks")), number(values.get("clk_tck")) or 100
            if ticks is None:
                continue
            last = previous.get(node)
            if last and last[0] == process and offset > last[1] and ticks >= last[2]:
                percent = (ticks - last[2]) / hertz / (offset - last[1]) * 100
                series.cpu.setdefault(node, []).append((offset, round(percent, 3)))
            previous[node] = (process, offset, ticks)
        report = read_json(directory / "jobs.json")
        if isinstance(report, dict) and isinstance(report.get("progress"), dict):
            total = sum(number((row or {}).get("succeeded")) or 0 for row in report["progress"].values())
            successes.append((offset, total))
            if isinstance(report.get("target_rate"), (int, float)):
                series.target_rate = float(report["target_rate"])
    anchor = None
    for offset, total in successes:
        if anchor is None or total < anchor[1]:
            anchor = (offset, total)
        elif offset - anchor[0] >= JOB_WINDOW_SECONDS:
            series.jobs.append((offset, round((total - anchor[1]) / (offset - anchor[0]), 3)))
            anchor = (offset, total)
    if series.target_rate is None and successes:
        try:
            series.target_rate = float(metadata.get("target_rate"))
        except (TypeError, ValueError):
            pass
    series.nodes = [node for node in series.nodes if node in series.rss] or sorted(series.rss)
    series.marks = event_marks(evidence, start)
    return series


def nice_step(span, count=5):
    raw = span / count
    magnitude = 10 ** math.floor(math.log10(raw))
    for multiple in (1, 2, 2.5, 5, 10):
        if raw <= multiple * magnitude:
            return multiple * magnitude
    return 10 * magnitude


def fmt(value):
    return f"{value:.1f}"


def label_number(value):
    return f"{value:,.0f}" if value >= 10 or value == int(value) else f"{value:g}"


def time_step(duration):
    # The demos (#671) run for minutes; a soak's shortest step is five minutes.
    for step in (10, 30, 60, 120, 300, 600, 1800, 3600, 7200, 14400):
        if duration / step <= 13:
            return step
    return 28800


def time_label(offset, step):
    if step < 60:
        return f"{offset:g} s"
    offset = int(offset)
    return f"{offset // 3600} h" if step >= 3600 else f"{offset // 60} min"


def node_label(node, index):
    tail = node.rsplit("-", 1)[-1]
    return f"node {tail}" if tail.isdigit() else node


def spread(positions, gap=13):
    """Nudge direct labels apart so they don't overlap, keeping their order."""
    placed = []
    for key, y in sorted(positions, key=lambda item: item[1]):
        if placed and y - placed[-1][1] < gap:
            y = placed[-1][1] + gap
        placed.append((key, y))
    return dict(placed)


# What a soak's marks mean, drawn along the bottom of each chart.
SOAK_KEY = (("fault", "fault"), ("upgrade", "upgrade walk"), ("special", "special"), ("failure", "failure"))


def draw(title, unit, lines, duration, marks, targets=(), floor_max=0, key=SOAK_KEY):
    """One chart: `lines` is [(label, slot, [(offset, value)])], `targets` [(value, label)].

    `key` lists the mark kinds to explain under the chart, as (kind, label).
    """
    values = [value for _, _, points in lines for _, value in points] + [value for value, _ in targets]
    top = max(values + [floor_max, 1e-9])
    step = nice_step(top * 1.08)
    ymax = step * math.ceil(top * 1.08 / step)
    duration = max(duration, max((offset for _, _, points in lines for offset, _ in points), default=1), 1)
    width, height = WIDTH - LEFT - RIGHT, HEIGHT - TOP - BOTTOM
    x = lambda offset: LEFT + width * max(0, min(offset, duration)) / duration
    y = lambda value: TOP + height * (1 - value / ymax)
    out = [f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {WIDTH} {HEIGHT}" width="{WIDTH}" height="{HEIGHT}" '
           f'role="img" aria-labelledby="t">',
           f'<title id="t">{escape(title)}</title>', f"<style>{STYLE.strip()}</style>",
           f'<rect class="surface" x="0" y="0" width="{WIDTH}" height="{HEIGHT}" rx="6"/>',
           f'<text class="title" x="{LEFT}" y="20">{escape(title)}</text>']
    # One series needs no legend or direct label: the title names it.
    named = lines if len(lines) > 1 else []
    legend_x = WIDTH - RIGHT
    for label, slot, _ in reversed(named):
        legend_x -= 8 + 7 * len(label) + 16
        out.append(f'<rect class="f{slot}" x="{legend_x}" y="12" width="10" height="10" rx="2"/>'
                   f'<text x="{legend_x + 14}" y="21">{escape(label)}</text>')
    tick = 0.0
    while tick <= ymax + 1e-9:
        out.append(f'<line class="grid" x1="{LEFT}" y1="{fmt(y(tick))}" x2="{LEFT + width}" y2="{fmt(y(tick))}"/>'
                   f'<text x="{LEFT - 6}" y="{fmt(y(tick) + 4)}" text-anchor="end">{label_number(tick)}</text>')
        tick += step
    out.append(f'<text x="{LEFT}" y="{TOP - 8}">{escape(unit)}</text>')
    tstep = time_step(duration)
    for offset in range(0, int(duration) + 1, tstep):
        out.append(f'<line class="axis" x1="{fmt(x(offset))}" y1="{TOP + height}" x2="{fmt(x(offset))}" '
                   f'y2="{TOP + height + 4}"/>'
                   f'<text x="{fmt(x(offset))}" y="{TOP + height + 16}" text-anchor="middle">'
                   f'{time_label(offset, tstep)}</text>')
    for value, label in targets:
        out.append(f'<line class="target" x1="{LEFT}" y1="{fmt(y(value))}" x2="{LEFT + width}" y2="{fmt(y(value))}"/>'
                   f'<text x="{LEFT + width - 4}" y="{fmt(y(value) - 4)}" text-anchor="end">{escape(label)}</text>')
    ends = []
    for label, slot, points in lines:
        segments, current = [], []
        for offset, value in points:
            if current and offset - current[-1][0] > GAP_SECONDS:
                segments.append(current)
                current = []
            current.append((offset, value))
        if current:
            segments.append(current)
        for segment in segments:
            if len(segment) == 1:
                offset, value = segment[0]
                out.append(f'<circle class="f{slot}" cx="{fmt(x(offset))}" cy="{fmt(y(value))}" r="2"/>')
                continue
            path = " ".join(f"{fmt(x(offset))},{fmt(y(value))}" for offset, value in segment)
            out.append(f'<polyline class="line s{slot}" points="{path}"><title>{escape(label)}</title></polyline>')
        if points and named:
            ends.append((label, y(points[-1][1])))
    for label, ypos in spread(ends).items():
        out.append(f'<text x="{LEFT + width + 6}" y="{fmt(ypos + 4)}">{escape(label)}</text>')
    strip = TOP + height + 30
    for offset, kind, label in marks:
        if 0 <= offset <= duration:
            out.append(mark_shape(kind, x(offset), strip,
                                  f"{escape(kind)}: {escape(label)} at {time_label(offset, min(tstep, 60))}"))
    key_x, key_y = LEFT, HEIGHT - 9
    for kind, label in key:
        out.append(mark_shape(kind, key_x + 5, key_y - 4, None)
                   + f'<text x="{fmt(key_x + 14)}" y="{key_y}">{label}</text>')
        key_x += 24 + 6.5 * len(label)
    out.append("</svg>")
    return "\n".join(out) + "\n"


def mark_shape(kind, cx, cy, title):
    """A fault or demo-step tick, upgrade triangle, special diamond or failure cross centred on (cx, cy)."""
    if kind in ("fault", "step"):
        tag, shape = "line", f'x1="{fmt(cx)}" y1="{fmt(cy - 4)}" x2="{fmt(cx)}" y2="{fmt(cy + 4)}"'
    elif kind == "upgrade":
        tag, shape = "path", f'd="M{fmt(cx)},{fmt(cy - 5)} L{fmt(cx + 5)},{fmt(cy + 4)} L{fmt(cx - 5)},{fmt(cy + 4)} Z"'
    elif kind == "special":
        tag, shape = "path", (f'd="M{fmt(cx)},{fmt(cy - 5)} L{fmt(cx + 5)},{fmt(cy)} L{fmt(cx)},{fmt(cy + 5)} '
                              f'L{fmt(cx - 5)},{fmt(cy)} Z"')
    else:
        tag, shape = "path", (f'd="M{fmt(cx - 4)},{fmt(cy - 4)} L{fmt(cx + 4)},{fmt(cy + 4)} '
                              f'M{fmt(cx + 4)},{fmt(cy - 4)} L{fmt(cx - 4)},{fmt(cy + 4)}"')
    if title is None:
        return f'<{tag} class="mark {kind}" {shape}/>'
    return f'<{tag} class="mark {kind}" {shape}><title>{title}</title></{tag}>'


def node_lines(nodes, data):
    lines = []
    for index, node in enumerate(nodes):
        if data.get(node):
            lines.append((node_label(node, index), index + 1 if index < 3 else 0, data[node]))
    return lines


def write_charts(evidence, directory):
    """Write the soak's charts into `directory`; return the Charts written, in order."""
    series = collect(evidence)
    plans = []
    if sum(len(points) for points in series.rss.values()) > 1:
        api = node_label(series.nodes[0], 0) if series.nodes else "node 1"
        targets = [(API_NODE_TARGET_MIB, f"{api} target {API_NODE_TARGET_MIB} MiB"),
                   (OTHER_NODE_TARGET_MIB, f"other nodes' target {OTHER_NODE_TARGET_MIB} MiB")]
        plans.append((Chart("rss", "rss.svg", "Bun resident memory per node"), "MiB",
                      node_lines(series.nodes, series.rss), targets, 0))
    if sum(len(points) for points in series.fds.values()) > 1:
        plans.append((Chart("fds", "fds.svg", "Bun open file descriptors per node"), "open fds",
                      node_lines(series.nodes, series.fds), (), 0))
    if any(series.cpu.values()):
        plans.append((Chart("cpu", "cpu.svg", "Bun CPU per node"), "% of one core",
                      node_lines(series.nodes, series.cpu), (), 0))
    if series.jobs:
        targets = [(series.target_rate, f"target {label_number(series.target_rate)}/s")] if series.target_rate else []
        plans.append((Chart("jobs", "jobs.svg", "Accepted jobs a second"),
                      f"accepted a second, {JOB_WINDOW_SECONDS // 60}-minute windows",
                      [("accepted jobs", 1, series.jobs)], targets, 0))
    if not plans:
        return []
    directory = Path(directory)
    directory.mkdir(parents=True, exist_ok=True)
    written = []
    for chart, unit, lines, targets, floor_max in plans:
        (directory / chart.file).write_text(draw(chart.title, unit, lines, series.duration, series.marks,
                                                 targets, floor_max))
        written.append(chart)
    return written


def section(written, relative):
    """The record's Charts section, linking each chart under `relative`."""
    lines = ["## Charts", ""]
    if not written:
        return lines + ["No resource samples were recorded.", ""]
    lines += ["Time runs from the start of the soak. The marks under each chart are its faults, upgrade walks, "
              "specials and recorded failures; the dashed lines are targets.", ""]
    for chart in written:
        lines += [f"![{chart.title}]({relative}/{chart.file})", ""]
    return lines
