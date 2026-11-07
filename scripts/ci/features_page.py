#!/usr/bin/env python3
"""Write and check reliaburger.com/features.html from docs/features.toml (#1729).

docs/features.toml says, feature by feature, what's ready in the released
version and what isn't. This script turns it into the website's feature status
page and checks that the two stay honest:

    features_page.py write          write docs/website/features.html
    features_page.py check          the TOML's schema, its docs links, and a
                                    page that matches it
    features_page.py check-issues   ask GitHub that every linked issue is
                                    still open (needs gh, and GH_TOKEN in CI)

`make features-page` runs `write`. The page's Markdown copy,
docs/website/features.md, comes from `make agent-docs` like every other
page's, so run that after `write`.

Standard library only (Python 3.11 or later, for tomllib). Fixtures:
scripts/ci/test_features_page.py.
"""
import argparse
from dataclasses import dataclass
import html
from pathlib import Path
import re
import subprocess
import sys
import tomllib

REPO = Path(__file__).resolve().parents[2]
SOURCE = "docs/features.toml"
PAGE = "docs/website/features.html"
REPOSITORY = "reliaburger/reliaburger"
GITHUB = f"https://github.com/{REPOSITORY}"

# The four statuses, in the order the page explains them, with what each one
# promises. The page and the TOML's header comment say the same thing.
STATUSES = {
    "ready": (
        "Ready",
        "Built, documented, tested and exercised by the release gate, with no "
        "known serious problems.",
    ),
    "beta": (
        "Beta",
        "Works and is tested, but has known gaps, limited soak evidence, or may "
        "change shape.",
    ),
    "alpha": ("Alpha", "Usable to try, but incomplete or likely to change."),
    "planned": ("Planned", "Not built yet. The row names its milestone and issue."),
}

TOP_KEYS = {"release", "released", "reviewed", "milestones", "group"}
GROUP_KEYS = {"id", "title", "feature"}
FEATURE_REQUIRED = {"name", "status", "summary", "docs", "evidence", "issues", "restrictions"}
FEATURE_OPTIONAL = {"since", "milestone", "needs_maintainer"}
VERSION = re.compile(r"\d+\.\d+\.\d+")
CODE_SPAN = re.compile(r"`([^`]+)`")


class SchemaError(Exception):
    """docs/features.toml doesn't have the shape the page needs."""


@dataclass
class Feature:
    group: str
    name: str
    status: str
    summary: str
    docs: list
    evidence: str
    issues: list
    restrictions: str
    since: str | None
    milestone: str | None
    needs_maintainer: str | None


@dataclass
class Group:
    id: str
    title: str
    features: list


@dataclass
class Catalogue:
    release: str
    released: str
    reviewed: str
    milestones: dict
    groups: list

    def features(self):
        return [feature for group in self.groups for feature in group.features]


def _expect(condition, message):
    if not condition:
        raise SchemaError(message)


def _string(table, key, where, allow_empty=False):
    value = table.get(key)
    _expect(isinstance(value, str), f"{where}: `{key}` must be a string")
    _expect(allow_empty or value.strip(), f"{where}: `{key}` is empty")
    return value


def load(text):
    """The catalogue in `text` (TOML), or SchemaError naming what's wrong."""
    try:
        data = tomllib.loads(text)
    except tomllib.TOMLDecodeError as error:
        raise SchemaError(f"not valid TOML: {error}") from error
    unknown = set(data) - TOP_KEYS
    _expect(not unknown, f"unknown top-level keys: {sorted(unknown)}")
    release = _string(data, "release", "top level")
    _expect(VERSION.fullmatch(release), f"release {release!r} isn't a version like 0.2.7")
    released = _string(data, "released", "top level")
    reviewed = _string(data, "reviewed", "top level")
    milestones = data.get("milestones")
    _expect(isinstance(milestones, dict) and milestones, "`[milestones]` is missing")
    for title, number in milestones.items():
        _expect(isinstance(number, int) and number > 0, f"milestone {title!r} needs a number")
    groups = data.get("group")
    _expect(isinstance(groups, list) and groups, "there are no `[[group]]` tables")
    seen_groups, seen_rows = set(), set()
    catalogue = Catalogue(release, released, reviewed, milestones, [])
    for index, table in enumerate(groups):
        where = f"group {index + 1}"
        unknown = set(table) - GROUP_KEYS
        _expect(not unknown, f"{where}: unknown keys {sorted(unknown)}")
        group_id = _string(table, "id", where)
        _expect(re.fullmatch(r"[a-z][a-z0-9-]*", group_id), f"{where}: id {group_id!r} isn't a slug")
        _expect(group_id not in seen_groups, f"{where}: duplicate id {group_id!r}")
        seen_groups.add(group_id)
        title = _string(table, "title", where)
        rows = table.get("feature")
        _expect(isinstance(rows, list) and rows, f"group {group_id!r} has no features")
        group = Group(group_id, title, [])
        for row in rows:
            feature = _feature(group_id, row, milestones)
            key = row_id(feature)
            _expect(key not in seen_rows, f"two features are called {feature.name!r} in {group_id!r}")
            seen_rows.add(key)
            group.features.append(feature)
        catalogue.groups.append(group)
    return catalogue


def _feature(group_id, row, milestones):
    name = row.get("name", "?")
    where = f"{group_id}: {name!r}"
    missing = FEATURE_REQUIRED - set(row)
    _expect(not missing, f"{where}: missing {sorted(missing)}")
    unknown = set(row) - FEATURE_REQUIRED - FEATURE_OPTIONAL
    _expect(not unknown, f"{where}: unknown keys {sorted(unknown)}")
    status = _string(row, "status", where)
    _expect(status in STATUSES, f"{where}: status {status!r} isn't one of {sorted(STATUSES)}")
    docs = row["docs"]
    _expect(
        isinstance(docs, list) and docs and all(isinstance(d, str) and d for d in docs),
        f"{where}: `docs` must list at least one path",
    )
    issues = row["issues"]
    _expect(
        isinstance(issues, list) and all(isinstance(n, int) and n > 0 for n in issues),
        f"{where}: `issues` must be a list of issue numbers",
    )
    _expect(len(set(issues)) == len(issues), f"{where}: an issue is listed twice")
    since = row.get("since")
    milestone = row.get("milestone")
    if status == "planned":
        _expect(since is None, f"{where}: a planned feature has no `since`")
        _expect(isinstance(milestone, str), f"{where}: a planned feature needs a `milestone`")
        _expect(milestone in milestones, f"{where}: milestone {milestone!r} isn't in [milestones]")
        _expect(issues, f"{where}: a planned feature needs an issue")
    else:
        _expect(milestone is None, f"{where}: only a planned feature has a `milestone`")
        _expect(
            isinstance(since, str) and VERSION.fullmatch(since),
            f"{where}: a built feature needs `since`, a version like 0.1.0",
        )
    needs = row.get("needs_maintainer")
    _expect(needs is None or (isinstance(needs, str) and needs.strip()), f"{where}: empty `needs_maintainer`")
    return Feature(
        group=group_id,
        name=_string(row, "name", where),
        status=status,
        summary=_string(row, "summary", where),
        docs=docs,
        evidence=_string(row, "evidence", where),
        issues=issues,
        restrictions=_string(row, "restrictions", where, allow_empty=True),
        since=since,
        milestone=milestone,
        needs_maintainer=needs,
    )


def version_key(version):
    return tuple(int(part) for part in version.split("."))


def check_versions(catalogue):
    """A built feature can't have shipped after the release the page describes."""
    problems = []
    for feature in catalogue.features():
        if feature.since and version_key(feature.since) > version_key(catalogue.release):
            problems.append(
                f"{feature.group}: {feature.name!r} says since {feature.since}, after {catalogue.release}"
            )
    return problems


# --- Markdown headings and their anchors -------------------------------------


def slug(heading):
    """GitHub's anchor for a Markdown heading's text."""
    text = re.sub(r"\[([^\]]*)\]\([^)]*\)", r"\1", heading)
    text = text.replace("`", "").strip().lower()
    text = re.sub(r"[^\w\- ]", "", text)
    return text.replace(" ", "-")


def headings(markdown):
    """Each heading's text, level and anchor, in order, outside code fences."""
    found, counts, fence = [], {}, None
    for line in markdown.splitlines():
        stripped = line.lstrip()
        marker = re.match(r"(`{3,}|~{3,})", stripped)
        if marker:
            if fence is None:
                fence = marker.group(1)[0]
            elif stripped.startswith(fence * 3):
                fence = None
            continue
        if fence is not None:
            continue
        match = re.match(r"(#{1,6})\s+(.*?)\s*#*\s*$", line)
        if not match:
            continue
        text = match.group(2)
        base = slug(text)
        number = counts.get(base, 0)
        counts[base] = number + 1
        found.append((len(match.group(1)), text, base if number == 0 else f"{base}-{number}"))
    return found


def resolve_doc(reference, repo):
    """The heading text a docs reference lands on, or ValueError if it doesn't."""
    path, _, anchor = reference.partition("#")
    _expect_doc(not path.startswith("/") and ".." not in Path(path).parts, f"{reference}: use a path from the repository root")
    file = repo / path
    _expect_doc(file.is_file(), f"{reference}: {path} doesn't exist")
    if file.suffix != ".md":
        _expect_doc(not anchor, f"{reference}: only Markdown anchors can be checked")
        return path
    found = headings(file.read_text(encoding="utf-8"))
    if not anchor:
        titles = [text for level, text, _ in found if level == 1]
        return titles[0] if titles else path
    for _, text, key in found:
        if key == anchor:
            return text
    raise ValueError(f"{reference}: {path} has no heading with anchor #{anchor}")


def _expect_doc(condition, message):
    if not condition:
        raise ValueError(message)


def check_docs(catalogue, repo):
    problems = []
    for feature in catalogue.features():
        for reference in feature.docs:
            try:
                resolve_doc(reference, repo)
            except ValueError as error:
                problems.append(f"{feature.group}: {feature.name!r}: {error}")
    return problems


# --- The page ------------------------------------------------------------------


def inline(text):
    """`text` as HTML: escaped, with `code` spans."""
    parts = CODE_SPAN.split(text)
    out = []
    for index, part in enumerate(parts):
        escaped = html.escape(part, quote=False)
        out.append(f"<code>{escaped}</code>" if index % 2 else escaped)
    return "".join(out)


def row_id(feature):
    words = re.sub(r"[^a-z0-9]+", "-", feature.name.replace("`", "").lower()).strip("-")
    return f"{feature.group}-{words}"


def issue_link(number):
    return f'<a href="{GITHUB}/issues/{number}">#{number}</a>'


def milestone_link(catalogue, title):
    number = catalogue.milestones[title]
    return f'<a href="{GITHUB}/milestone/{number}">{html.escape(title)}</a>'


def doc_link(catalogue, reference, repo):
    label = resolve_doc(reference, repo)
    label = re.sub(r"\[([^\]]*)\]\([^)]*\)", r"\1", label)
    url = f"{GITHUB}/blob/v{catalogue.release}/{reference}"
    return f'<a href="{html.escape(url)}">{inline(label)}</a>'


def counts(catalogue):
    tally = {status: 0 for status in STATUSES}
    for feature in catalogue.features():
        tally[feature.status] += 1
    return tally


def render_row(catalogue, feature, repo):
    label, _ = STATUSES[feature.status]
    if feature.status == "planned" and VERSION.fullmatch(feature.milestone):
        when = f"Planned for {milestone_link(catalogue, feature.milestone)}"
    elif feature.status == "planned":
        when = f"Planned, no release yet ({milestone_link(catalogue, feature.milestone)})"
    else:
        when = html.escape(feature.since)
    restrictions = inline(feature.restrictions) if feature.restrictions else "None listed"
    issues = ", ".join(issue_link(n) for n in feature.issues) if feature.issues else "None open"
    docs = " · ".join(doc_link(catalogue, d, repo) for d in feature.docs)
    return (
        f'<tr id="{row_id(feature)}">'
        f'<th scope="row"><strong>{inline(feature.name)}.</strong> {inline(feature.summary)}</th>'
        f'<td><span class="badge badge-{feature.status}">{label}</span></td>'
        f"<td>{when}</td>"
        f"<td>{restrictions}</td>"
        f"<td>{issues}</td>"
        f"<td>{docs}</td>"
        "</tr>"
    )


def render_group(catalogue, group, repo):
    title = html.escape(group.title)
    rows = "\n".join(render_row(catalogue, feature, repo) for feature in group.features)
    return (
        f'      <section id="{group.id}"><h2>{title} '
        f'<a class="permalink" href="#{group.id}" aria-label="Link to {title}">#</a></h2>\n'
        '        <div class="table-wrap"><table class="feature-table"><thead><tr>'
        '<th scope="col">Feature</th><th scope="col">Status</th><th scope="col">Since</th>'
        '<th scope="col">Restrictions</th><th scope="col">Issues</th><th scope="col">Docs</th>'
        "</tr></thead><tbody>\n"
        f"{rows}\n"
        "</tbody></table></div>\n"
        "      </section>\n"
    )


def render(catalogue, repo=REPO):
    """The whole page for `catalogue`, as HTML."""
    release = html.escape(catalogue.release)
    tally = counts(catalogue)
    built = tally["ready"] + tally["beta"] + tally["alpha"]
    meanings = "".join(
        f'<tr><th scope="row"><span class="badge badge-{key}">{label}</span></th><td>{html.escape(meaning)}</td></tr>'
        for key, (label, meaning) in STATUSES.items()
    )
    contents = "".join(f'<a href="#{group.id}">{html.escape(group.title)}</a>' for group in catalogue.groups)
    groups = "".join(render_group(catalogue, group, repo) for group in catalogue.groups)
    source = f"{GITHUB}/blob/main/{SOURCE}"
    return f"""<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <meta name="description" content="What's ready in Reliaburger {release}, feature by feature: Ready, Beta, Alpha or Planned, with restrictions, open issues and the docs for each.">
  <meta name="theme-color" content="#181b19">
  <title>Feature status · Reliaburger</title>
  <link rel="stylesheet" href="./style.css">
  <link rel="stylesheet" href="./comparisons.css">
  <link rel="alternate" type="text/markdown" href="features.md">
</head>
<body>
  <a class="skip" href="#main">Skip to content</a>
  <div class="page">
    <header class="masthead">
      <a class="wordmark" href="./" aria-label="Reliaburger home"><span class="mark" aria-hidden="true">r/</span>reliaburger</a>
      <nav aria-label="Main navigation"><a href="./#install">Install</a><a href="./features.html">What's ready</a><a href="./compare/index.html">Compare</a><a href="{GITHUB}">GitHub <span aria-hidden="true">↗</span></a></nav>
    </header>
    <!-- Generated from {SOURCE} by `make features-page`: edit the TOML, not this file. -->
    <main id="main" class="comparison-page features-page">
      <div class="comparison-intro"><div class="eyebrow">FEATURE STATUS · {release}</div><h1>What's ready, and what isn't.</h1><p class="lede">Every feature of Reliaburger {release}, released on {html.escape(catalogue.released)}: how far along it is, what it doesn't do yet, and the open issues against it. Search the page for the thing you need.</p></div>
      <div class="comparison-note"><p>Before 1.0.0, all of it is for home labs and dev clusters, not production. 1.0.0 is expected in January 2027. Until then, an upgrade can mean starting a fresh cluster.</p></div>
      <section id="statuses"><h2>What the statuses mean <a class="permalink" href="#statuses" aria-label="Link to what the statuses mean">#</a></h2>
        <div class="table-wrap"><table class="status-table"><thead><tr><th scope="col">Status</th><th scope="col">What it means</th></tr></thead><tbody>{meanings}</tbody></table></div>
        <p>The release gate is what every release passes before it ships: two installs from empty caches, hours of soak under faults with apps and jobs, and power cuts mid-write (<a href="{GITHUB}/blob/v{release}/docs/releasing.md#the-release-gate">how it works</a>). Unit tests alone don't make a feature Ready.</p>
        <p>In {release}, {tally["ready"]} features are Ready, {tally["beta"]} are Beta and {tally["alpha"]} are Alpha, {built} in all, and {tally["planned"]} more are planned.</p>
        <nav class="comparison-toc" aria-label="Feature groups">{contents}</nav>
      </section>
{groups}      <section id="source"><h2>Where this comes from <a class="permalink" href="#source" aria-label="Link to where this comes from">#</a></h2>
        <p>We generate this page from <a href="{source}">docs/features.toml</a>, where each row also records the evidence its status rests on: a qualification record, a manual section or an issue. A nightly check fails when an issue it links closes, and we revisit every row after each release. Reviewed on {html.escape(catalogue.reviewed)}.</p>
        <p>Something here wrong, or missing? <a href="{GITHUB}/issues/new/choose">Open an issue</a> with the feature and what you saw. The <a href="{GITHUB}/blob/main/docs/roadmap.md">roadmap</a> has what comes next.</p>
      </section>
    </main>
    <footer><span>Reliaburger · by Miko Pawlikowski</span><a href="#statuses">Feature status for {release} · reviewed {html.escape(catalogue.reviewed)}</a><span class="footer-note">Static HTML and CSS, generated from docs/features.toml. No tracking or third-party requests.</span></footer>
  </div>
</body>
</html>
"""


# --- GitHub -------------------------------------------------------------------


def issue_state(number):
    """`OPEN` or `CLOSED`, from `gh issue view`."""
    result = subprocess.run(
        ["gh", "issue", "view", str(number), "--repo", REPOSITORY, "--json", "state", "--jq", ".state"],
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        raise RuntimeError(f"gh issue view {number} failed: {result.stderr.strip()}")
    return result.stdout.strip()


def check_issues(catalogue, state=issue_state):
    """Every issue a row links is still open: a known issue that closed has
    been fixed, and a planned row whose issue closed is either built or gone."""
    problems = []
    states = {}
    for feature in catalogue.features():
        for number in feature.issues:
            if number not in states:
                states[number] = state(number)
            if states[number] != "OPEN":
                problems.append(
                    f"{feature.group}: {feature.name!r} links #{number}, which is {states[number].lower()}"
                )
    return problems


# --- Command line -------------------------------------------------------------


def check(repo=REPO):
    """Every problem with the TOML and the page, as messages."""
    try:
        catalogue = load((repo / SOURCE).read_text(encoding="utf-8"))
    except SchemaError as error:
        return [f"{SOURCE}: {error}"]
    problems = check_versions(catalogue) + check_docs(catalogue, repo)
    if problems:
        return problems
    page = repo / PAGE
    if not page.is_file() or page.read_text(encoding="utf-8") != render(catalogue, repo):
        problems.append(f"{PAGE} is out of date with {SOURCE}; run `make features-page`, then `make agent-docs`")
    return problems


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("command", choices=["write", "check", "check-issues"])
    args = parser.parse_args(argv)
    if args.command == "write":
        catalogue = load((REPO / SOURCE).read_text(encoding="utf-8"))
        problems = check_versions(catalogue) + check_docs(catalogue, REPO)
        if problems:
            print("\n".join(problems), file=sys.stderr)
            return 1
        (REPO / PAGE).write_text(render(catalogue), encoding="utf-8")
        print(f"wrote {PAGE}; run `make agent-docs` for its Markdown copy")
        return 0
    if args.command == "check":
        problems = check()
    else:
        problems = check_issues(load((REPO / SOURCE).read_text(encoding="utf-8")))
    if problems:
        print("\n".join(problems), file=sys.stderr)
        return 1
    print("ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
