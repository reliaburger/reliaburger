"""The Reliaburger and Kubernetes comparison record, and the check on its page (#1679).

`render` reads the results.json of several gym runs, one per solver and
platform, and writes one table per scenario with every cell side by side:
the verdicts over the runs, timings, calls, cost and the model, with links
to each run's transcript. It never judges anything itself: the numbers are
the scorer's, and the same scorer scores both platforms.

`check_page` keeps the website's comparison page honest. Every number the
page states must appear in the record it names (or in the lab's pins, for
versions), so the page can't claim a result the runs didn't produce. While
the runs haven't happened, the page names no record and may state nothing
but the pins.

Stdlib only, like the rest of the gym.
"""
import html
import json
import os
from pathlib import Path
import re
import statistics

GYM_DIR = Path(__file__).resolve().parent
REPOSITORY = GYM_DIR.parent.parent
VERSIONS = GYM_DIR / "k8s" / "versions.env"
RUNS_PER_CELL = 5

# How each solver is named in the record, by (platform, solver).
SOLVER_LABELS = {
    ("reliaburger", "claude-code"): "Claude Code with `relish` and `relish mcp`",
    ("k8s", "claude-code"): "Claude Code with `kubectl` and the Kubernetes MCP server",
    ("k8s", "kubectl-ai"): "kubectl-ai",
    ("k8s", "holmesgpt"): "HolmesGPT",
}
PLATFORM_LABELS = {"reliaburger": "Reliaburger", "k8s": "Kubernetes (k3s)"}
# The scenario that exists on one side only, and why.
RELIABURGER_ONLY = {
    "planted-logs-since": (
        "The planted bug is a bug in Reliaburger's own source, found with `relish agent "
        "source`. Kubernetes has no equivalent loop (a solver can't read and patch the source "
        "of the cluster it's debugging), so the Kubernetes side doesn't run it and isn't "
        "scored on it."
    ),
}


def load(path):
    """One results.json, from the file or the directory holding it."""
    path = Path(path)
    if path.is_dir():
        path = path / "results.json"
    document = json.loads(path.read_text())
    document["_dir"] = path.parent
    return document


def solver_of(document):
    """The solver a results.json ran: from its sessions' meta, else its kind."""
    for run in document.get("runs", []):
        meta = run.get("solver_meta") or {}
        if meta.get("solver"):
            return meta["solver"]
    return document["context"].get("solver_kind", "command")


def platform_of(document):
    return document["context"].get("platform", "reliaburger")


def cell_label(platform, solver):
    if solver == "scripted":
        return "Scripted baseline (not an agent)"
    return SOLVER_LABELS.get((platform, solver), solver)


def _seconds(value):
    return f"{value:.0f} s"


def _cost(metas):
    costs = [m.get("cost_usd") for m in metas if isinstance(m.get("cost_usd"), (int, float))]
    if not metas:
        return "not reported"
    if len(costs) < len(metas):
        return f"not reported ({len(metas) - len(costs)} of {len(metas)} runs)"
    return f"${sum(costs):.2f} total, ${statistics.median(costs):.2f} median"


def _models(metas):
    models = sorted({str(m.get("model")) for m in metas if m.get("model")})
    return ", ".join(models) or "not reported"


def cells(documents):
    """Every (scenario, platform, solver) cell: its runs and where they live."""
    found = {}
    for document in documents:
        platform, solver = platform_of(document), solver_of(document)
        for run in document.get("runs", []):
            key = (run["scenario"], platform, solver)
            cell = found.setdefault(key, {"runs": [], "dir": document["_dir"]})
            cell["runs"].append(run)
    return found


def cell_row(cell, out_dir):
    runs = cell["runs"]
    scored = [run for run in runs if "score" in run]
    verdicts = {v: 0 for v in ("correct", "partial", "wrong", "no_answer", "unsafe")}
    for run in scored:
        verdicts[run["score"]["verdict"]] += 1
    seconds = [run["seconds"] for run in scored]
    calls = [run["score"]["calls"] for run in scored]
    metas = [run["solver_meta"] for run in runs if run.get("solver_meta")]
    links = []
    for run in runs:
        transcript = cell["dir"] / run["scenario"] / run["run"] / "solver.stdout"
        links.append(f"[{run['run']}]({os.path.relpath(transcript, out_dir)})")
    return {
        "runs": len(runs),
        "errors": len(runs) - len(scored),
        "verdicts": verdicts,
        "median_seconds": statistics.median(seconds) if seconds else None,
        "range": (min(seconds), max(seconds)) if seconds else None,
        "median_calls": statistics.median(calls) if calls else None,
        "cost": _cost(metas),
        "model": _models(metas),
        "links": links,
    }


def kubernetes_did_better(reliaburger, kubernetes):
    """The ways a Kubernetes cell beat the Reliaburger Claude Code cell, in words."""
    better = []
    if kubernetes["verdicts"]["correct"] > reliaburger["verdicts"]["correct"]:
        better.append(
            f"more correct runs ({kubernetes['verdicts']['correct']} against "
            f"{reliaburger['verdicts']['correct']})"
        )
    if kubernetes["verdicts"]["unsafe"] < reliaburger["verdicts"]["unsafe"]:
        better.append(
            f"fewer unsafe runs ({kubernetes['verdicts']['unsafe']} against "
            f"{reliaburger['verdicts']['unsafe']})"
        )
    if (
        kubernetes["median_seconds"] is not None
        and reliaburger["median_seconds"] is not None
        and kubernetes["median_seconds"] < reliaburger["median_seconds"]
    ):
        better.append(
            f"a shorter median time ({_seconds(kubernetes['median_seconds'])} against "
            f"{_seconds(reliaburger['median_seconds'])})"
        )
    return better


def render(paths, out_dir=None):
    """The comparison record as Markdown, from results.json files or their directories."""
    documents = [load(path) for path in paths]
    out_dir = Path(out_dir) if out_dir is not None else Path.cwd()
    found = cells(documents)
    scenarios = sorted({key[0] for key in found})
    lines = [
        "# Agents on Reliaburger and on Kubernetes",
        "",
        "Generated by `scripts/gym/gym.py compare` from these gym runs "
        "([#1679](https://github.com/reliaburger/reliaburger/issues/1679)):",
        "",
    ]
    for document in documents:
        context = document["context"]
        lines.append(
            f"- {PLATFORM_LABELS[platform_of(document)]}, "
            f"{cell_label(platform_of(document), solver_of(document))}: {context['date']}, "
            f"{context['runs_per_scenario']} run(s) per scenario, host {context['host']}, "
            f"`{os.path.relpath(document['_dir'], out_dir)}`"
        )
        for key in ("relish_version", "platform_versions", "kubectl_version"):
            value = context.get(key)
            if value and value != "none (Kubernetes)":
                lines.append(f"  - {key.replace('_', ' ')}: " + "; ".join(str(value).splitlines()))
    lines += [
        "",
        "Every cell was scored by the same scorer (`scripts/gym/score.py`), with the same "
        "unsafe, refused and decoy rules. On Kubernetes a run is unsafe when the API server's "
        "audit log holds a write, or an exec, attach or port-forward, from the solver's service "
        "account, allowed or refused.",
        "",
    ]
    short = sorted(
        f"{scenario} ({cell_label(platform, solver)}, {PLATFORM_LABELS[platform]}): "
        f"{len(cell['runs'])} run(s)"
        for (scenario, platform, solver), cell in found.items()
        if len(cell["runs"]) != RUNS_PER_CELL
    )
    if short:
        lines += [f"**Cells without {RUNS_PER_CELL} runs:** " + "; ".join(short) + ".", ""]

    better_lines = []
    for scenario in scenarios:
        here = {(p, s): cell_row(cell, out_dir) for (sc, p, s), cell in found.items() if sc == scenario}
        lines += [f"## {scenario}", ""]
        if scenario in RELIABURGER_ONLY:
            lines += [RELIABURGER_ONLY[scenario], ""]
        lines += [
            "| Platform | Solver | Model | Runs | Correct | Partial | Wrong | No answer | Unsafe "
            "| Median time | Range | Median calls | Cost | Transcripts |",
            "|---|---|---|---|---|---|---|---|---|---|---|---|---|---|",
        ]
        for (platform, solver), row in sorted(here.items(), key=lambda item: (item[0][0] != "reliaburger", item[0])):
            verdicts = row["verdicts"]
            span = f"{_seconds(row['range'][0])} to {_seconds(row['range'][1])}" if row["range"] else "-"
            median = _seconds(row["median_seconds"]) if row["median_seconds"] is not None else "-"
            calls = f"{row['median_calls']:g}" if row["median_calls"] is not None else "-"
            runs = f"{row['runs']}" + (f" ({row['errors']} harness errors)" if row["errors"] else "")
            lines.append(
                f"| {PLATFORM_LABELS[platform]} | {cell_label(platform, solver)} | {row['model']} | "
                f"{runs} | {verdicts['correct']} | {verdicts['partial']} | {verdicts['wrong']} | "
                f"{verdicts['no_answer']} | {verdicts['unsafe']} | {median} | {span} | {calls} | "
                f"{row['cost']} | {' '.join(row['links'])} |"
            )
        lines.append("")
        ours = here.get(("reliaburger", "claude-code"))
        if ours is None:
            continue
        for (platform, solver), row in sorted(here.items()):
            if platform != "k8s":
                continue
            for way in kubernetes_did_better(ours, row):
                better_lines.append(f"- {scenario}, {cell_label(platform, solver)}: {way}.")

    lines += ["## Where Kubernetes did better", ""]
    lines += better_lines or [
        "- Nowhere, by correct runs, unsafe runs or median time. Read the tables: a cell can "
        "still be better on cost or on calls."
    ]
    lines.append("")
    return "\n".join(lines)


# --- The page check -------------------------------------------------------

NUMBER = re.compile(r"(?<![A-Za-z0-9_.#])\d+(?:\.\d+)*")
MAIN = re.compile(r"<main\b[^>]*>(.*?)</main>", re.S | re.I)
RECORD_META = re.compile(r'<meta\s+name="gym-record"\s+content="([^"]*)"', re.I)
PENDING = "No results yet"


def numbers_in(text):
    """Every number in `text`, as written.

    Issue numbers (`#1679`) and numbers glued to the end of a word (`v1.37.2`,
    the `3` in `k3s`) aren't claims about results, so they're skipped, on the
    page and in the record alike. A unit after a number (`30s`) doesn't hide it.
    """
    return set(NUMBER.findall(text))


def page_text(page):
    """The visible text of a page's <main>: tags, scripts and styles removed."""
    found = MAIN.search(page)
    body = found.group(1) if found else page
    body = re.sub(r"<(script|style)\b.*?</\1>", " ", body, flags=re.S | re.I)
    body = re.sub(r"<[^>]+>", " ", body)
    return html.unescape(body)


def pinned_numbers(versions=VERSIONS):
    """The numbers in the lab's pins (versions), which a page may quote."""
    values = []
    for line in Path(versions).read_text().splitlines():
        line = line.strip()
        if line and not line.startswith("#") and "=" in line:
            values.append(line.partition("=")[2])
    return numbers_in(" ".join(values))


def check_page(page_path, repository=REPOSITORY, versions=VERSIONS):
    """What's wrong with the comparison page: numbers no record backs, or a missing record."""
    page = Path(page_path).read_text()
    problems = []
    named = RECORD_META.search(page)
    if named is None:
        return ["the page has no <meta name=\"gym-record\">"]
    allowed = pinned_numbers(versions)
    record = named.group(1).strip()
    text = page_text(page)
    if record:
        path = Path(repository) / record
        if not path.is_file():
            return [f"the page names the record {record}, which doesn't exist"]
        allowed |= numbers_in(path.read_text())
    elif PENDING not in text:
        problems.append(f"a page with no record must say {PENDING!r}")
    stray = sorted(numbers_in(text) - allowed, key=lambda n: [int(p) for p in n.split(".")])
    if stray:
        where = f"the record {record}" if record else "the lab's pins (no record yet)"
        problems.append(f"numbers on the page that aren't in {where}: {', '.join(stray)}")
    return problems
