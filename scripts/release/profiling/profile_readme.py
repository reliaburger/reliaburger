#!/usr/bin/env python3
"""Write the release profile record's README from its evidence.

  profile_readme.py RECORD_DIR

RECORD_DIR is what qualify-profile.sh leaves: metadata.tsv (one "key<TAB>value"
per line), host-load.tsv, raw/ (profile_windows.py's layout) and
flamegraphs/. This writes summary.json, comparison.json and README.md beside
them. The README's answer and each flagged line's explanation stay as
placeholders: whoever qualifies the release writes those.
"""
import json
import os
import statistics
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import compare_profile  # noqa: E402
import profile_categories  # noqa: E402
import profile_windows  # noqa: E402

ANSWER_PLACEHOLDER = "*To be written by whoever qualifies the release.*"
LABELS = ("previous", "candidate")
VM_CPUS = 4  # the bench VM's shape, for CPU microseconds per task


def read_metadata(path):
    out = {}
    with open(path) as source:
        for line in source:
            key, sep, value = line.rstrip("\n").partition("\t")
            if sep:
                out[key] = value
    return out


def read_host_load(path):
    """Rows of (time, event, load1, compilers) from host-load.tsv."""
    rows = []
    if not os.path.exists(path):
        return rows
    with open(path) as source:
        for line in source:
            fields = line.rstrip("\n").split("\t")
            if len(fields) == 4:
                rows.append(tuple(fields))
    return rows


def session_loads(rows):
    """Per session: (name, load at start, load at end, compilers seen at either edge)."""
    starts, out = {}, []
    for time, event, load, compilers in rows:
        kind, _, session = event.partition(" ")
        if kind == "start":
            starts[session] = (load, compilers)
        elif kind == "end" and session in starts:
            load0, compilers0 = starts.pop(session)
            out.append((session, load0, load, max(int(compilers0 or 0), int(compilers or 0))))
    return out


def sample_summary(rows):
    loads = [float(load) for _, event, load, _ in rows if event == "sample" and load]
    compilers = [int(c) for _, event, _, c in rows if event == "sample" and c.isdigit()]
    if not loads:
        return None
    return {"min": min(loads), "max": max(loads), "median": statistics.median(loads),
            "samples": len(loads), "with_compilers": sum(1 for c in compilers if c > 0)}


def status_paragraph(meta):
    mode = meta.get("mode", "release")
    previous, candidate = meta.get("previous_version", "?"), meta.get("candidate_version", "?")
    if mode == "smoke":
        return ("**Smoke run, not a qualification record.** One round per version and short windows: "
                "it shows the step runs end to end, and its numbers mean nothing. Don't commit it as a "
                "release's profile.")
    if mode == "diagnostic":
        return (f"**Diagnostic run, not a release record.** {candidate} came from "
                f"`{meta.get('candidate_source', '?')}`, not a staged candidate checked against a "
                "qualified digest.")
    return (f"The release profile of {candidate}, measured beside {previous} in the same VM, in "
            "alternating rounds. It's evidence, not a gate: every rule below reports what it would "
            "flag, and each flagged line needs a sentence saying why (maintainer decision, "
            "26 October 2026).")


def version_table(collected, label, version):
    rows = []
    rounds = collected.get(label, {}).get("rounds", {})
    if not rounds:
        return rows
    rows += [f"### {version} ({label})", "",
             "| Scenario | Rate, median (range) | Steady 15-58 s | VM CPU busy | Bun's CPU | Start lag p50 / p99 | Usable windows, task failures |",
             "|---|---:|---:|---:|---:|---:|---|"]
    for window, title in compare_profile.RATE_SCENARIOS:
        rates = profile_windows.values(collected, label, window, "rate")
        if not rates:
            continue
        steady = profile_windows.values(collected, label, window, "steady_rate")
        util = profile_windows.values(collected, label, window, "cpu_util")
        bun = profile_windows.values(collected, label, window, "bun_cpu_cores")
        p50 = profile_windows.values(collected, label, window, "start_lag_p50_ms")
        p99 = profile_windows.values(collected, label, window, "start_lag_p99_ms")
        lag = f"{statistics.median(p50):g} / {statistics.median(p99):g} ms" if p50 and p99 else "-"
        windows = [rounds[n].get(window, {}) for n in sorted(rounds) if window in rounds[n]]
        complete = sum(1 for w in windows if profile_windows.usable(w))
        failures = sum(int(w.get("failures") or 0) for w in windows)
        rows.append(
            "| " + " | ".join([
                title,
                compare_profile.spread(compare_profile.median_range(rates)) + "/s",
                (compare_profile.number(statistics.median(steady), 0) + "/s") if steady else "-",
                f"{100 * statistics.median(util):.0f}%" if util else "-",
                f"{statistics.median(bun):.2f} cores" if bun else "-",
                lag,
                f"{complete} of {len(windows)}, {failures}",
            ]) + " |")
    idle_cpu = profile_windows.values(collected, label, "idle", "bun_cpu_cores")
    if idle_cpu:
        wakeups = profile_windows.values(collected, label, "idle", "bun_wakeups_per_second")
        rss = profile_windows.values(collected, label, "idle", "bun_rss_kb")
        rows += ["", "Idle cluster: Bun used "
                 f"{100 * statistics.median(idle_cpu):.2f}% of one core (median of {len(idle_cpu)})"
                 + (f", {statistics.median(wakeups):,.0f} context switches a second" if wakeups else "")
                 + (f", RSS {statistics.median(rss) / 1024:,.1f} MiB" if rss else "") + "."]
    rows.append("")
    return rows


def category_tables(root, label, version, collected):
    rows = []
    categories = compare_profile.load_categories(root, label)
    perf = collected.get(label, {}).get("perf", {})
    for window, title, key, total_key in compare_profile.CATEGORY_WINDOWS:
        document = categories.get(window)
        if not document:
            continue
        total = document.get(total_key) or 0
        if not total:
            continue
        rate = perf.get(window, {}).get("forks_per_second") if window.endswith("host") else None
        rows += [f"{version}, {title} (`{window}`, {document.get('total', 0):,.0f} samples):", "",
                 "| Category | Share | µs per task |" if rate else "| Category | Share |",
                 "|---|---:|---:|" if rate else "|---|---:|"]
        for name in profile_categories.ordered_names():
            samples = document.get(key, {}).get(name, 0.0)
            if not samples:
                continue
            share = samples / total
            line = f"| {name} | {100 * share:.1f}% |"
            if rate:
                vm_share = samples / (document.get("total") or 1)
                line += f" {vm_share * VM_CPUS * 1e6 / rate:.1f} |"
            rows.append(line)
        if document.get("uncategorised_share", 0.0) > profile_categories.UNCATEGORISED_LIMIT:
            rows += ["", "**" + profile_categories.uncategorised_warning(document["uncategorised_share"]) + ".**"]
        rows.append("")
    return rows


def files_under(directory, suffix):
    if not os.path.isdir(directory):
        return []
    return sorted(name for name in os.listdir(directory) if name.endswith(suffix))


def rig_table(meta):
    def get(key):
        return meta.get(key) or "unknown"
    rows = ["| Item | Value |", "|---|---|",
            f"| Host | {get('host_model')}, {get('host_cpu')}, {get('host_memory')}, {get('host_os')} |",
            f"| VM | `{get('vm_name')}`, Lima {get('lima_version')}, {get('vm_shape')}, made for this run and deleted after |",
            f"| Guest | {get('guest_os')}, Linux {get('guest_kernel')}, {get('runc_version')}, {get('perf_version')} |",
            f"| BusyBox | Ubuntu's `busybox-static`, SHA-256 `{get('busybox_sha256')}` |"]
    for label in LABELS:
        version = get(f"{label}_version")
        rows.append(f"| {label.capitalize()} | {version}, commit `{get(label + '_commit')}`, "
                    f"from `{get(label + '_source')}`; `bun` `{get(label + '_bun_sha256')}`, "
                    f"`relish` `{get(label + '_relish_sha256')}` |")
        rows.append(f"| {label.capitalize()}, profiling build | {get(label + '_profiling')} |")
    if meta.get("qualified_digest"):
        rows.append(f"| Qualified digest | `{meta['qualified_digest']}` (candidate.json) |")
    rows.append(f"| Scenarios | {get('scenarios')}; {get('window')} s windows, the `relish bench` defaults "
                "otherwise, beside a 500m BusyBox web app on a fresh single-node cluster |")
    return rows


def wall_time(meta):
    def minutes(key):
        try:
            return f"{int(meta[key]) // 60} min"
        except (KeyError, ValueError):
            return "unknown"
    text = (f"Estimated {minutes('estimate_seconds')} before the run; it took {minutes('wall_seconds')}. "
            f"{meta.get('rounds_run', '?')} rounds per version")
    dropped = meta.get("rounds_dropped", "0")
    if dropped not in ("", "0"):
        text += (f", {dropped} of the {meta.get('rounds_planned', '?')} planned rounds dropped to fit "
                 "the 2-hour budget (rounds go, never scenarios)")
    return text + "."


def render(record):
    meta = read_metadata(os.path.join(record, "metadata.tsv"))
    root = os.path.join(record, "raw")
    collected = profile_windows.collect(root) if os.path.isdir(root) else {}
    categories = {label: compare_profile.load_categories(root, label) for label in LABELS}
    comparison = compare_profile.compare(collected, categories)
    with open(os.path.join(record, "summary.json"), "w") as out:
        json.dump(collected, out, indent=2, sort_keys=True)
    with open(os.path.join(record, "comparison.json"), "w") as out:
        json.dump(comparison, out, indent=2, sort_keys=True)

    previous = meta.get("previous_version", "the previous release")
    candidate = meta.get("candidate_version", "the candidate")
    lines = [f"# Where the time goes in {candidate} ({meta.get('date', 'undated')})", "",
             status_paragraph(meta), "",
             "## Answer", "",
             "<!-- Where the time goes in this release, and what changed: a short paragraph, "
             "and one line for the release log. -->", ANSWER_PLACEHOLDER, ""]
    lines += compare_profile.markdown(comparison, previous, candidate).splitlines()
    lines += ["", "## Rates", ""]
    for label in LABELS:
        lines += version_table(collected, label, meta.get(f"{label}_version", label))
    lines += ["## Where the time goes", ""]
    for label in LABELS:
        lines += category_tables(root, label, meta.get(f"{label}_version", label), collected)
    svgs = files_under(os.path.join(record, "flamegraphs"), ".svg")
    if svgs:
        lines += ["Flamegraphs (hover a frame for its name and share):", ""]
        lines += [f"- [`flamegraphs/{name}`](flamegraphs/{name})" for name in svgs]
        lines.append("")
    extras = []
    for label in LABELS:
        perf_dir = os.path.join(root, label, "perf")
        for name in files_under(perf_dir, ".latency.txt") + files_under(perf_dir, ".timeline.txt"):
            extras.append(f"- [`raw/{label}/perf/{name}`](raw/{label}/perf/{name})")
    if extras:
        lines += ["`perf sched latency` and the probe's grant-cycle timeline:", ""] + extras + [""]

    lines += ["## The rig", ""] + rig_table(meta) + [""]
    lines += ["### Host load", "",
              "The 1-minute load and the count of `rustc` and `cargo` processes on the host, at the start "
              "and end of every session. No round is thrown away for load: on a busy Mac every round would "
              "be. The alternating rounds take the noise instead.", ""]
    loads = session_loads(read_host_load(os.path.join(record, "host-load.tsv")))
    if loads:
        lines += ["| Session | Load before | Load after | Compilers seen |", "|---|---:|---:|---:|"]
        lines += [f"| {name} | {before} | {after} | {compilers} |" for name, before, after, compilers in loads]
        lines.append("")
    samples = sample_summary(read_host_load(os.path.join(record, "host-load.tsv")))
    if samples:
        lines += [f"Sampled every 10 s: load {samples['min']:.2f} to {samples['max']:.2f} "
                  f"(median {samples['median']:.2f}), compilers running in {samples['with_compilers']} "
                  f"of {samples['samples']} samples.", ""]
    lines += ["### Wall time", "", wall_time(meta), ""]
    lines += ["## Reproduce", "", "```sh", meta.get("command", "scripts/release/qualify-profile.sh ..."), "```", "",
              "## Files", "",
              "| Path | What |", "|---|---|",
              "| `metadata.tsv`, `host-load.tsv` | What the script knew about the run, and the host's load |",
              "| `summary.json`, `comparison.json` | Every window's numbers, and the comparison with its findings |",
              "| `raw/LABEL/rN/` | One rate session: bench reports and progress, CPU snapshots, `vmstat`, "
              "`batch-status`, `binaries.txt` |",
              "| `raw/LABEL/perf/` | The profiling session: folded stacks, top frames, category tables, "
              "`perf sched latency`, the probe timeline |",
              "| `flamegraphs/` | On-CPU and off-CPU flamegraphs, and the differential ones |", ""]
    with open(os.path.join(record, "README.md"), "w") as out:
        out.write("\n".join(lines))
    return comparison


def main():
    if len(sys.argv) != 2:
        print(__doc__, file=sys.stderr)
        sys.exit(2)
    comparison = render(sys.argv[1])
    for finding in comparison["findings"]:
        print(f"would flag: {finding['severity']}, {finding['rule']}: {finding['subject']}: {finding['detail']}")


if __name__ == "__main__":
    main()
