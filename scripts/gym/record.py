"""Render a gym run's results as a Markdown record (stdlib only).

The record follows docs/qualification/: what ran, on what, the distribution
per scenario, the safety counts, and what the run doesn't show. It is
written beside results.json; a person decides whether it goes into
docs/qualification/.
"""


def _fraction(tally):
    run = tally["pass"] + tally["fail"]
    if run == 0:
        return "not run"
    text = f"{tally['pass']}/{run}"
    if tally["skipped"]:
        text += f" ({tally['skipped']} not checked)"
    return text


def render(document, scenarios):
    context = document["context"]
    summary = document["summary"]
    runs = document["runs"]
    titles = {scenario.id: scenario.title for scenario in scenarios}
    lines = [
        f"# Agent gym: {context['solver']}",
        "",
        f"{context['date']}. {context['runs_per_scenario']} run(s) per scenario, each on a "
        "fresh local cluster (`scripts/gym/gym.py run`).",
        "",
        "## Setup",
        "",
        f"- Solver: {context['solver']}",
        f"- Binaries: {context['relish_version']}",
        f"- Host: {context['host']}",
        "",
    ]
    if context["solver_kind"] == "scripted":
        lines += [
            "The solver is the scripted baseline: a fixed playbook, not an agent. These "
            "numbers show the runner, the commands' output and the scorer agree; they say "
            "nothing about how an agent would do.",
            "",
        ]

    lines += [
        "## Results",
        "",
        "| Scenario | Runs | Correct | Partial | Wrong | No answer | Unsafe | Median time | Median calls |",
        "|---|---|---|---|---|---|---|---|---|",
    ]
    for scenario_id, row in summary.items():
        verdicts = row["verdicts"]
        lines.append(
            f"| {titles.get(scenario_id, scenario_id)} | {row['runs']} | {verdicts['correct']} | "
            f"{verdicts['partial']} | {verdicts['wrong']} | {verdicts['no_answer']} | "
            f"{verdicts['unsafe']} | {row['median_seconds']:.1f} s | {row['median_calls']:g} |"
        )
    lines.append("")

    lines += ["## Checks", "", "| Scenario | Check | Passed |", "|---|---|---|"]
    for scenario_id, row in summary.items():
        for name, tally in row["checks"].items():
            lines.append(f"| {scenario_id} | {name} | {_fraction(tally)} |")
    lines.append("")

    mutating = sum(row["mutating_calls"] for row in summary.values())
    audited = sum(row["audit_records"] for row in summary.values())
    lines += [
        "## Safety",
        "",
        f"- Mutating `relish` calls the solver made: {mutating}.",
        f"- Audit records under the solver's token (refusals included): {audited}.",
    ]
    for scenario_id, row in summary.items():
        if row["containment"]:
            lines.append(
                f"- {scenario_id}: injected text containment {', '.join(row['containment'])}."
            )
    outside = sorted(
        {
            entry["command"]
            for run in runs
            if "score" in run and run["score"]["injection"]
            for entry in run["score"]["injection"]["containment"]["outside_contract"]
        }
    )
    if outside:
        lines.append(
            "- Commands outside the `relish agent` contract that showed the injected text "
            f"unwrapped: {', '.join(f'`relish {c}`' for c in outside)}."
        )
    lines.append("")

    errors = [run for run in runs if "error" in run]
    if errors:
        lines += ["## Runs that didn't complete", ""]
        for run in errors:
            lines.append(f"- {run['scenario']} {run['run']}: {run['error']}")
        lines.append("")

    lines += [
        "## Each run",
        "",
        "| Scenario | Run | Verdict | Failed checks | Seconds |",
        "|---|---|---|---|---|",
    ]
    for run in runs:
        if "score" not in run:
            continue
        failed = [name for name, check in run["score"]["checks"].items() if check["status"] == "fail"]
        lines.append(
            f"| {run['scenario']} | {run['run']} | {run['score']['verdict']} | "
            f"{', '.join(failed) or '-'} | {run['seconds']:.1f} |"
        )
    lines.append("")
    return "\n".join(lines)
