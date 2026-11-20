"""Checks that keep the changelog current.

`CHANGELOG.md` is the one file that says what changed in each release and
whether a cluster can roll onto it. It only helps while it has every release,
so these checks fail when a release is promoted, or the version is bumped,
without it.

Two heading formats are accepted, and nothing else at the `## ` level:

- `## [X.Y.Z] - YYYY-MM-DD` for a released version;
- `## [Unreleased] - X.Y.Z` for the version the train carries. There is at
  most one, and it comes first.

Every release in `docs/release-log.md` (a `## X.Y.Z:` heading) needs a
released section. The version in `Cargo.toml`, less any `-dev` suffix, needs
a section of either kind. Sections run newest first, every released one
starts its story with `### Upgrading`, and, as in the plans, nobody quotes
test counts: they go stale with every commit.
"""
from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[2]
CHANGELOG = ROOT / "CHANGELOG.md"
RELEASE_LOG = ROOT / "docs" / "release-log.md"
CARGO = ROOT / "Cargo.toml"

VERSION = r"\d+\.\d+\.\d+"
RELEASED = re.compile(rf"^## \[({VERSION})\] - (\d{{4}}-\d{{2}}-\d{{2}})$")
UNRELEASED = re.compile(rf"^## \[Unreleased\] - ({VERSION})$")
LOG_ENTRY = re.compile(rf"^## ({VERSION}):", re.MULTILINE)
CARGO_VERSION = re.compile(rf'^version\s*=\s*"({VERSION})(?:-[\w.]+)?"', re.MULTILINE)
SUBSECTIONS = ["Upgrading", "Added", "Changed", "Fixed", "Removed", "Known issues"]
TEST_COUNT = re.compile(r"(?<![\w.])\d[\d,]*\s+(?:[a-z-]+\s+)?tests\b|bringing the total to", re.IGNORECASE)


def key(version):
    return tuple(int(part) for part in version.split("."))


def sections(text):
    """Each `## ` section as (kind, version, body), in file order.

    `kind` is "released", "unreleased" or "unknown" for a heading in neither
    format, whose `version` is the heading itself.
    """
    found = []
    for line in text.splitlines():
        if line.startswith("## "):
            released, unreleased = RELEASED.match(line), UNRELEASED.match(line)
            if released:
                found.append(["released", released.group(1), []])
            elif unreleased:
                found.append(["unreleased", unreleased.group(1), []])
            else:
                found.append(["unknown", line, []])
        elif found:
            found[-1][2].append(line)
    return [(kind, version, "\n".join(body)) for kind, version, body in found]


def subsections(body):
    return re.findall(r"^### (.+?)\s*$", body, re.MULTILINE)


def cargo_version(text):
    match = CARGO_VERSION.search(text)
    return match.group(1) if match else None


class ChangelogTests(unittest.TestCase):
    def setUp(self):
        self.text = CHANGELOG.read_text(encoding="utf-8")
        self.sections = sections(self.text)

    def versions(self, kind):
        return [version for found, version, _ in self.sections if found == kind]

    def test_every_heading_is_a_release_or_the_unreleased_one(self):
        self.assertEqual(self.versions("unknown"), [],
                         "use `## [X.Y.Z] - YYYY-MM-DD` or `## [Unreleased] - X.Y.Z`")
        unreleased = [index for index, (kind, _, _) in enumerate(self.sections) if kind == "unreleased"]
        self.assertIn(unreleased, ([], [0]), "at most one Unreleased section, and it comes first")

    def test_every_release_in_the_release_log_has_a_section(self):
        logged = LOG_ENTRY.findall(RELEASE_LOG.read_text(encoding="utf-8"))
        self.assertTrue(logged, "docs/release-log.md has no `## X.Y.Z:` entries")
        missing = sorted(set(logged) - set(self.versions("released")), key=key)
        self.assertEqual(missing, [], "releases in docs/release-log.md with no dated section in CHANGELOG.md")

    def test_the_cargo_version_has_a_section(self):
        version = cargo_version(CARGO.read_text(encoding="utf-8"))
        self.assertIsNotNone(version, "no version line in Cargo.toml")
        self.assertIn(version, self.versions("released") + self.versions("unreleased"),
                      f"CHANGELOG.md has neither `## [{version}] - DATE` nor `## [Unreleased] - {version}`")

    def test_sections_run_newest_first(self):
        versions = [version for kind, version, _ in self.sections if kind != "unknown"]
        self.assertEqual(versions, sorted(set(versions), key=key, reverse=True))

    def test_every_release_says_how_to_upgrade_first(self):
        wrong = []
        for kind, version, body in self.sections:
            found = subsections(body)
            ordered = [name for name in SUBSECTIONS if name in found]
            if found != ordered:
                wrong.append(f"{version}: {found}, want a subset of {SUBSECTIONS} in that order")
            elif kind == "released" and found[:1] != ["Upgrading"]:
                wrong.append(f"{version}: no `### Upgrading`")
        self.assertEqual(wrong, [])

    def test_changelog_quotes_no_test_counts(self):
        counts = [f"CHANGELOG.md:{number}: {match.group(0)}"
                  for number, line in enumerate(self.text.splitlines(), 1)
                  for match in TEST_COUNT.finditer(line)]
        self.assertEqual(counts, [])

    def test_sections_reads_both_heading_formats(self):
        text = "# Changelog\n\n## [Unreleased] - 0.3.0\n\n### Added\n\n## [0.2.8] - 2026-11-12\n\n### Upgrading\n\n## 0.2.7\n"
        found = sections(text)
        self.assertEqual([(kind, version) for kind, version, _ in found],
                         [("unreleased", "0.3.0"), ("released", "0.2.8"), ("unknown", "## 0.2.7")])
        self.assertEqual(subsections(found[1][2]), ["Upgrading"])

    def test_versions_sort_by_number_and_cargo_drops_the_dev_suffix(self):
        self.assertGreater(key("0.10.0"), key("0.9.3"))
        self.assertEqual(cargo_version('[package]\nname = "x"\nversion = "0.3.1-dev"\n'), "0.3.1")


if __name__ == "__main__":
    unittest.main()
