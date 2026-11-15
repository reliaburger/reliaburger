#!/usr/bin/env python3
"""Compare a release candidate's profile with the previous release's.

  compare_profile.py ROOT [--previous previous] [--candidate candidate]
      [--previous-version vX.Y.Z] [--json OUT] [--markdown OUT]

ROOT is the profile step's raw evidence (see profile_windows.py): one
directory per version label, measured in the same VM in alternating rounds,
so host noise hits both builds alike. Nothing here trusts a number measured
on another day.

Every rule from issue #1358 is computed and reported as "would flag". None of
them gates the release yet (maintainer decision, 26 October 2026): each
flagged line needs a sentence in the record's README saying why, and the
thresholds become a gate once a few releases show how noisy the rig is.

Would fail, unless explained:
  - a scenario more than REGRESSION slower, with the confidence interval of
    the ratio of medians excluding 1 (no change);
  - Bun's CPU in the idle cluster above IDLE_CPU_LIMIT of one core;
  - a Bun category above NEW_CATEGORY_SHARE of Bun's CPU that was under
    NEW_CATEGORY_BEFORE before.
Explain, never fails:
  - a category moving more than CATEGORY_MOVE_POINTS percentage points;
  - a scenario more than IMPROVEMENT faster: we want to know why, and that
    it isn't a measuring error.
"""
import argparse
import json
import os
import random
import statistics
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import profile_categories  # noqa: E402
import profile_windows  # noqa: E402

REGRESSION = 0.10
IMPROVEMENT = 0.10
IDLE_CPU_LIMIT = 0.02
NEW_CATEGORY_SHARE = 0.05
NEW_CATEGORY_BEFORE = 0.01
CATEGORY_MOVE_POINTS = 5.0
BOOTSTRAP_RESAMPLES = 2000
CONFIDENCE = 0.95
# Fixed, so the same evidence always gives the same interval.
BOOTSTRAP_SEED = 1358

WOULD_FAIL = "would fail"
EXPLAIN = "explain"
EXTEND_MAP = "extend the map"

# Window name -> (title, summary field, higher is better). Rates first.
RATE_SCENARIOS = [
    ("raw", "Raw VM processes (calibration)"),
    ("host", "Host jobs, default cap"),
    ("shared", "Shared containers"),
    ("fresh", "Fresh containers"),
]
IDLE_METRICS = [
    ("bun_cpu_cores", "Bun's CPU, share of one core", False),
    ("bun_wakeups_per_second", "Bun's context switches a second", False),
    ("bun_rss_kb", "Bun's RSS, KiB", False),
]
CONTROL_METRICS = [
    ("deploy_speed", "Deploy 3 replicas until running, s", False),
    ("scheduler_throughput", "Schedule 10 apps", None),
    ("api_p50_ms", "API round trip p50, ms", False),
    ("api_p99_ms", "API round trip p99, ms", False),
]
# Perf windows whose categories are compared, and the share each table uses.
CATEGORY_WINDOWS = [
    ("perfcpu-host", "host jobs, share of the VM", "categories", "total"),
    ("perfcpu-idle", "idle cluster, share of Bun", "bun_categories", "bun_total"),
]


def median_range(values):
    if not values:
        return None
    return {"median": statistics.median(values), "low": min(values), "high": max(values), "n": len(values)}


def bootstrap_ratio(previous, candidate, resamples=BOOTSTRAP_RESAMPLES, seed=BOOTSTRAP_SEED,
                    confidence=CONFIDENCE):
    """Confidence interval of median(candidate) / median(previous).

    Rounds are paired when both sides have the same count (round r of each
    version ran back to back), so a pair is resampled together; otherwise
    each side is resampled on its own. Returns (low, high), or None when a
    median is zero or a side is empty.
    """
    if not previous or not candidate:
        return None
    rng = random.Random(seed)
    ratios = []
    paired = len(previous) == len(candidate)
    for _ in range(resamples):
        if paired:
            picks = [rng.randrange(len(previous)) for _ in previous]
            before = [previous[i] for i in picks]
            after = [candidate[i] for i in picks]
        else:
            before = [rng.choice(previous) for _ in previous]
            after = [rng.choice(candidate) for _ in candidate]
        denominator = statistics.median(before)
        if denominator == 0:
            continue
        ratios.append(statistics.median(after) / denominator)
    if not ratios:
        return None
    ratios.sort()
    tail = (1 - confidence) / 2
    low = ratios[int(tail * (len(ratios) - 1))]
    high = ratios[int(round((1 - tail) * (len(ratios) - 1)))]
    return low, high


def compare_series(previous, candidate, higher_is_better=True):
    """Medians, ranges, ratio and interval of one metric, plus its verdict."""
    row = {"previous": median_range(previous), "candidate": median_range(candidate)}
    if not previous or not candidate or statistics.median(previous) == 0:
        row["verdict"] = "not compared"
        return row
    ratio = statistics.median(candidate) / statistics.median(previous)
    interval = bootstrap_ratio(previous, candidate)
    row["ratio"] = ratio
    row["interval"] = interval
    row["verdict"] = rate_verdict(ratio, interval, higher_is_better)
    return row


def rate_verdict(ratio, interval, higher_is_better=True):
    """"regression" (would fail), "improvement" (explain) or "no clear change"."""
    # Express everything as "better" > 1, so one rule covers both directions.
    better = ratio if higher_is_better else (1 / ratio if ratio else float("inf"))
    if interval is None:
        bounds = (better, better)
    elif higher_is_better:
        bounds = interval
    else:
        bounds = tuple(sorted(1 / b if b else float("inf") for b in interval))
    if better < 1 - REGRESSION and bounds[1] < 1:
        return "regression"
    if better > 1 + IMPROVEMENT:
        return "improvement"
    return "no clear change"


def relative_to_raw(collected, label, window):
    """Median of a scenario divided by the same version's raw median: the rig-free rate."""
    raw = profile_windows.values(collected, label, "raw", "rate", usable_only=True)
    rates = profile_windows.values(collected, label, window, "rate", usable_only=True)
    if not raw or not rates or statistics.median(raw) == 0:
        return None
    return statistics.median(rates) / statistics.median(raw)


def control_values(collected, label, metric):
    out = []
    rounds = collected.get(label, {}).get("rounds", {})
    for number in sorted(rounds):
        window = rounds[number].get("control", {})
        if metric in ("api_p50_ms", "api_p99_ms"):
            value = window.get(metric)
        else:
            value = window.get("control_metrics", {}).get(metric, {}).get("value")
        if isinstance(value, (int, float)):
            out.append(value)
    return out


def control_direction(collected, label, metric, default):
    if default is not None:
        return default
    for window in collected.get(label, {}).get("rounds", {}).values():
        found = window.get("control", {}).get("control_metrics", {}).get(metric)
        if found:
            return bool(found.get("higher_is_better"))
    return True


def load_categories(root, label):
    out = {}
    for window, *_ in CATEGORY_WINDOWS:
        path = os.path.join(root, label, "perf", window + ".categories.json")
        if os.path.exists(path):
            with open(path) as source:
                out[window] = json.load(source)
    return out


def category_shifts(previous, candidate, key, total_key):
    """Per category: share before and after, in percent, and the move in points."""
    before_total = previous.get(total_key) or 0
    after_total = candidate.get(total_key) or 0
    names = set(previous.get(key, {})) | set(candidate.get(key, {}))
    rows = []
    for name in profile_categories.ordered_names():
        if name not in names:
            continue
        before = 100 * previous.get(key, {}).get(name, 0.0) / before_total if before_total else 0.0
        after = 100 * candidate.get(key, {}).get(name, 0.0) / after_total if after_total else 0.0
        rows.append({"category": name, "previous": before, "candidate": after, "points": after - before})
    return rows


def compare(collected, categories, previous="previous", candidate="candidate"):
    """The whole comparison: rates, idle, control, categories, and every finding."""
    result = {"rates": [], "idle": [], "control": [], "categories": {}, "findings": []}
    findings = result["findings"]

    for window, title in RATE_SCENARIOS:
        before = profile_windows.values(collected, previous, window, "rate", usable_only=True)
        after = profile_windows.values(collected, candidate, window, "rate", usable_only=True)
        failed = [label for label in (previous, candidate)
                  if len(profile_windows.values(collected, label, window, "rate"))
                  > len(profile_windows.values(collected, label, window, "rate", usable_only=True))]
        for label in failed:
            findings.append({"severity": EXPLAIN, "rule": "failed-window", "subject": f"{title}, {label}",
                             "detail": "a window's bench failed or didn't complete; it's left out of the comparison"})
        if not before and not after:
            continue
        row = compare_series(before, after)
        row.update(window=window, title=title,
                   steady_previous=median_range(profile_windows.values(collected, previous, window, "steady_rate")),
                   steady_candidate=median_range(profile_windows.values(collected, candidate, window, "steady_rate")),
                   relative_previous=relative_to_raw(collected, previous, window),
                   relative_candidate=relative_to_raw(collected, candidate, window))
        result["rates"].append(row)
        add_series_finding(findings, title, row)

    for field, title, higher in IDLE_METRICS:
        before = profile_windows.values(collected, previous, "idle", field)
        after = profile_windows.values(collected, candidate, "idle", field)
        if not before and not after:
            continue
        row = compare_series(before, after, higher)
        row.update(field=field, title=title)
        result["idle"].append(row)
    idle_cpu = median_range(profile_windows.values(collected, candidate, "idle", "bun_cpu_cores"))
    if idle_cpu and idle_cpu["median"] > IDLE_CPU_LIMIT:
        findings.append({"severity": WOULD_FAIL, "rule": "idle-cpu", "subject": "idle cluster",
                         "detail": f"Bun used {100 * idle_cpu['median']:.1f}% of one core with no work, "
                                   f"over the {100 * IDLE_CPU_LIMIT:.0f}% limit"})

    for metric, title, higher in CONTROL_METRICS:
        before = control_values(collected, previous, metric)
        after = control_values(collected, candidate, metric)
        if not before and not after:
            continue
        higher = control_direction(collected, candidate, metric, higher)
        row = compare_series(before, after, higher)
        row.update(metric=metric, title=title)
        result["control"].append(row)
        add_series_finding(findings, "Control plane: " + title, row)

    for window, title, key, total_key in CATEGORY_WINDOWS:
        before = categories.get(previous, {}).get(window)
        after = categories.get(candidate, {}).get(window)
        for label, document in ((previous, before), (candidate, after)):
            share = (document or {}).get("uncategorised_share", 0.0)
            if share > profile_categories.UNCATEGORISED_LIMIT:
                findings.append({"severity": EXTEND_MAP, "rule": "uncategorised",
                                 "subject": f"{label}, {title}",
                                 "detail": profile_categories.uncategorised_warning(share)})
        if not before or not after:
            continue
        rows = category_shifts(before, after, key, total_key)
        result["categories"][window] = {"title": title, "rows": rows}
        for row in rows:
            if abs(row["points"]) > CATEGORY_MOVE_POINTS:
                findings.append({"severity": EXPLAIN, "rule": "category-move", "subject": f"{row['category']} ({title})",
                                 "detail": f"{row['previous']:.1f}% to {row['candidate']:.1f}%, "
                                           f"{row['points']:+.1f} points"})
        bun_before = before.get("bun_total") or 0
        bun_after = after.get("bun_total") or 0
        for name, samples in sorted(after.get("bun_categories", {}).items()):
            if name == profile_categories.BUN_UNCATEGORISED or not bun_after:
                continue
            share_after = samples / bun_after
            share_before = before.get("bun_categories", {}).get(name, 0.0) / bun_before if bun_before else 0.0
            if share_after > NEW_CATEGORY_SHARE and share_before < NEW_CATEGORY_BEFORE:
                findings.append({"severity": WOULD_FAIL, "rule": "new-category", "subject": f"{name} ({title})",
                                 "detail": f"{100 * share_after:.1f}% of Bun's CPU, "
                                           f"from {100 * share_before:.1f}% in the previous release"})
    return result


def add_series_finding(findings, title, row):
    verdict = row.get("verdict")
    if verdict not in ("regression", "improvement"):
        return
    detail = f"ratio of medians {row['ratio']:.3f}"
    if row.get("interval"):
        detail += f" (95% interval {row['interval'][0]:.3f}-{row['interval'][1]:.3f})"
    if verdict == "regression":
        findings.append({"severity": WOULD_FAIL, "rule": "regression", "subject": title,
                         "detail": f"more than {100 * REGRESSION:.0f}% worse: " + detail})
    else:
        findings.append({"severity": EXPLAIN, "rule": "improvement", "subject": title,
                         "detail": f"more than {100 * IMPROVEMENT:.0f}% better: " + detail})


def number(value, digits=1):
    if value is None:
        return "-"
    return f"{value:,.{digits}f}"


def spread(stats, digits=1, scale=1.0):
    if not stats:
        return "-"
    return (f"{number(stats['median'] * scale, digits)} "
            f"({number(stats['low'] * scale, digits)}-{number(stats['high'] * scale, digits)})")


def ratio_text(row):
    if "ratio" not in row:
        return "-"
    text = f"{row['ratio']:.3f}"
    if row.get("interval"):
        text += f" ({row['interval'][0]:.3f}-{row['interval'][1]:.3f})"
    return text


def markdown(result, previous_version, candidate_version):
    """The README's "Compared with vPREV" section."""
    out = [f"## Compared with {previous_version}", "",
           f"Both versions ran in the same VM, in alternating rounds. Ratios are {candidate_version} over "
           f"{previous_version}, with a {100 * CONFIDENCE:.0f}% bootstrap interval over the rounds. "
           "The \"vs raw\" columns divide each scenario by the same version's raw-process rate, "
           "so a slower rig doesn't read as a slower release.", ""]
    if result["rates"]:
        out += [f"| Scenario | {previous_version}, median (range) | {candidate_version}, median (range) "
                "| Ratio (interval) | Steady, before / after | vs raw, before / after | Verdict |",
                "|---|---:|---:|---:|---:|---:|---|"]
        for row in result["rates"]:
            steady = (f"{spread(row['steady_previous'], 0)} / {spread(row['steady_candidate'], 0)}"
                      if row["steady_previous"] or row["steady_candidate"] else "-")
            relative = f"{number(row['relative_previous'], 3)} / {number(row['relative_candidate'], 3)}"
            out.append(f"| {row['title']} | {spread(row['previous'])}/s | {spread(row['candidate'])}/s "
                       f"| {ratio_text(row)} | {steady} | {relative} | {row['verdict']} |")
        out.append("")
    if result["idle"]:
        out += ["Idle cluster, no work (absolute numbers):", "",
                f"| Measure | {previous_version} | {candidate_version} |", "|---|---:|---:|"]
        for row in result["idle"]:
            scale, digits = (100.0, 2) if row["field"] == "bun_cpu_cores" else (1.0, 0)
            unit = "%" if row["field"] == "bun_cpu_cores" else ""
            out.append(f"| {row['title']} | {spread(row['previous'], digits, scale)}{unit} "
                       f"| {spread(row['candidate'], digits, scale)}{unit} |")
        out.append("")
    if result["control"]:
        out += [f"| Control plane | {previous_version} | {candidate_version} | Ratio (interval) | Verdict |",
                "|---|---:|---:|---:|---|"]
        for row in result["control"]:
            out.append(f"| {row['title']} | {spread(row['previous'], 2)} | {spread(row['candidate'], 2)} "
                       f"| {ratio_text(row)} | {row['verdict']} |")
        out.append("")
    for window, block in result["categories"].items():
        out += [f"Category shifts, {block['title']} (`{window}`):", "",
                f"| Category | {previous_version} | {candidate_version} | Points |", "|---|---:|---:|---:|"]
        for row in block["rows"]:
            out.append(f"| {row['category']} | {row['previous']:.1f}% | {row['candidate']:.1f}% "
                       f"| {row['points']:+.1f} |")
        out.append("")
    out += ["### Would flag", ""]
    if not result["findings"]:
        out += ["Nothing. No rule from the release profile step would flag this comparison.", ""]
    else:
        out += ["The rules don't gate a release yet (maintainer decision, 26 October 2026). Each line "
                "below needs a sentence saying why, in place of its *Explanation*.", ""]
        for finding in result["findings"]:
            out.append(f"- **{finding['severity']}**, {finding['rule']}: {finding['subject']}. "
                       f"{finding['detail']}. *Explanation: to be written.*")
        out.append("")
    return "\n".join(out)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("root")
    parser.add_argument("--previous", default="previous")
    parser.add_argument("--candidate", default="candidate")
    parser.add_argument("--previous-version", default="the previous release")
    parser.add_argument("--candidate-version", default="the candidate")
    parser.add_argument("--json")
    parser.add_argument("--markdown")
    args = parser.parse_args()
    collected = profile_windows.collect(args.root)
    categories = {label: load_categories(args.root, label) for label in (args.previous, args.candidate)}
    result = compare(collected, categories, args.previous, args.candidate)
    if args.json:
        with open(args.json, "w") as out:
            json.dump(result, out, indent=2, sort_keys=True)
    text = markdown(result, args.previous_version, args.candidate_version)
    if args.markdown:
        with open(args.markdown, "w") as out:
            out.write(text + "\n")
    else:
        print(text)


if __name__ == "__main__":
    main()
