"""Render a gym run's results as a Markdown record (stdlib only).

The record follows docs/qualification/: what ran, on what, the distribution
per scenario, the safety counts, and what the run doesn't show. It is
written beside results.json; a person decides whether it goes into
docs/qualification/.
"""
import statistics


def _fraction(tally):
    run = tally["pass"] + tally["fail"]
    if run == 0:
        return "not run"
    text = f"{tally['pass']}/{run}"
    if tally["skipped"]:
        text += f" ({tally['skipped']} not checked)"
    return text


def _tokens(runs, summary, titles):
    """The tokens-and-context table, for a solver that reports tokens (local_model.py).

    Medians per scenario: tokens sent and generated over the session, the
    largest prompt one turn sent against the context window, the turns that
    filled the window, and the turns.
    """
    metas = {}
    for run in runs:
        meta = run.get("solver_meta") or {}
        if "score" in run and isinstance(meta.get("prompt_tokens"), int):
            metas.setdefault(run["scenario"], []).append(meta)
    if not metas:
        return []
    lines = [
        "## Tokens and context",
        "",
        "Medians over each scenario's runs. Tokens in are summed over every turn, so a "
        "long conversation counts its history each time it's sent.",
        "",
        "| Scenario | Tokens in | Tokens out | Largest prompt | Turns that filled the window | Turns |",
        "|---|---|---|---|---|---|",
    ]
    for scenario_id in summary:
        found = metas.get(scenario_id)
        if not found:
            continue

        def median(key):
            return int(statistics.median(meta.get(key) or 0 for meta in found))

        windows = sorted({meta.get("num_ctx") for meta in found if meta.get("num_ctx")})
        window = " or ".join(str(w) for w in windows) or "unknown"
        lines.append(
            f"| {titles.get(scenario_id, scenario_id)} | {median('prompt_tokens')} | "
            f"{median('completion_tokens')} | {median('peak_prompt_tokens')} of {window} | "
            f"{median('context_full_turns')} | {median('turns')} |"
        )
    lines.append("")
    return lines


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
        f"- Platform: {'Kubernetes (k3s)' if context.get('platform') == 'k8s' else 'Reliaburger'}",
        f"- Binaries: {context['relish_version']}",
        f"- Host: {context['host']}",
    ]
    metas = [run["solver_meta"] for run in runs if run.get("solver_meta")]
    if metas:
        def distinct(key):
            values = sorted({str(meta.get(key)) for meta in metas if meta.get(key) is not None})
            return ", ".join(values) or "not reported"
        costs = [meta["cost_usd"] for meta in metas if isinstance(meta.get("cost_usd"), (int, float))]
        lines += [
            f"- Client: {distinct('client')} {distinct('client_version')}",
            f"- Model: {distinct('model')}",
            f"- Session dates: {distinct('date')}",
        ]
        if costs:
            lines.append(f"- Reported cost: ${sum(costs):.2f} over {len(costs)} session(s)")
    lines.append("")
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
    refused = sorted(
        {
            " ".join([entry.get("program", "relish"), *entry["argv"]])
            for run in runs
            if "score" in run
            for entry in run["score"]["safety"].get("refused_calls", [])
        }
    )
    lines += [
        "## Safety",
        "",
        f"- Mutating calls the solver made: {mutating}.",
        f"- Audit records under the solver's token (refusals included): {audited}.",
        f"- Commands refused as usage errors before sending anything: "
        f"{sum(row.get('refused_calls', 0) for row in summary.values())}"
        + (f" ({', '.join(f'`{c}`' for c in refused)})." if refused else "."),
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
    program = "kubectl" if context.get("platform") == "k8s" else "relish"
    if outside:
        lines.append(
            "- Commands outside the `relish agent` contract that showed the injected text "
            f"unwrapped: {', '.join(f'`{program} {c}`' for c in outside)}."
        )
    lines.append("")

    lines += _tokens(runs, summary, titles)

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
