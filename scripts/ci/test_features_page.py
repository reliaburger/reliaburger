"""The feature status page only helps if it's honest: these tests hold
docs/features.toml and docs/website/features.html to that (#1729).

They stay offline. The issue-state check runs against a stub here, and
against GitHub nightly in .github/workflows/features.yml.
"""
from pathlib import Path
import tempfile
import unittest

import features_page

REPO = Path(__file__).resolve().parents[2]

MINIMAL = """\
release = "0.2.7"
released = "7 November 2026"
reviewed = "7 November 2026"

[milestones]
"0.3.0" = 6
"Later" = 9

[[group]]
id = "cluster"
title = "Getting a cluster"

[[group.feature]]
name = "Laptop cluster"
status = "ready"
since = "0.1.0"
summary = "One command builds a cluster."
restrictions = "Needs `lima`."
issues = [10]
docs = ["guide.md#install-it"]
evidence = "The staged install."

[[group.feature]]
name = "Bare metal"
status = "planned"
milestone = "0.3.0"
summary = "Netboot."
restrictions = ""
issues = [11]
docs = ["guide.md"]
evidence = "Roadmap."
"""

GUIDE = """\
# The guide

## Install it

```sh
# Not a heading
```

## Install it
"""


def catalogue(text=MINIMAL):
    return features_page.load(text)


def broken(old, new):
    """MINIMAL with one change, which must make it fail to load."""
    assert old in MINIMAL, old
    return MINIMAL.replace(old, new, 1)


class SchemaTests(unittest.TestCase):
    def test_a_minimal_catalogue_loads(self):
        loaded = catalogue()
        self.assertEqual([g.id for g in loaded.groups], ["cluster"])
        self.assertEqual([f.status for f in loaded.features()], ["ready", "planned"])

    def test_a_status_must_be_one_of_the_four(self):
        with self.assertRaisesRegex(features_page.SchemaError, "isn't one of"):
            catalogue(broken('status = "ready"', 'status = "stable"'))

    def test_a_planned_feature_needs_an_issue(self):
        with self.assertRaisesRegex(features_page.SchemaError, "needs an issue"):
            catalogue(broken("issues = [11]", "issues = []"))

    def test_a_planned_feature_needs_a_known_milestone(self):
        with self.assertRaisesRegex(features_page.SchemaError, r"isn't in \[milestones\]"):
            catalogue(broken('milestone = "0.3.0"', 'milestone = "0.9.0"'))

    def test_a_built_feature_needs_since_and_no_milestone(self):
        with self.assertRaisesRegex(features_page.SchemaError, "needs `since`"):
            catalogue(broken('since = "0.1.0"\n', ""))
        with self.assertRaisesRegex(features_page.SchemaError, "only a planned feature"):
            catalogue(broken('since = "0.1.0"\n', 'since = "0.1.0"\nmilestone = "0.3.0"\n'))

    def test_every_feature_carries_its_evidence_and_docs(self):
        with self.assertRaisesRegex(features_page.SchemaError, "missing"):
            catalogue(broken('evidence = "The staged install."\n', ""))
        with self.assertRaisesRegex(features_page.SchemaError, "at least one path"):
            catalogue(broken('docs = ["guide.md#install-it"]', "docs = []"))

    def test_unknown_keys_are_refused(self):
        with self.assertRaisesRegex(features_page.SchemaError, "unknown keys"):
            catalogue(broken('since = "0.1.0"\n', 'since = "0.1.0"\nowner = "me"\n'))

    def test_a_feature_cannot_ship_after_the_release_the_page_describes(self):
        loaded = catalogue(broken('since = "0.1.0"', 'since = "0.3.0"'))
        self.assertEqual(len(features_page.check_versions(loaded)), 1)


class DocsLinkTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.repo = Path(self.directory.name)
        (self.repo / "guide.md").write_text(GUIDE, encoding="utf-8")

    def tearDown(self):
        self.directory.cleanup()

    def test_anchors_follow_github_including_repeats(self):
        anchors = [anchor for _, _, anchor in features_page.headings(GUIDE)]
        self.assertEqual(anchors, ["the-guide", "install-it", "install-it-1"])

    def test_headings_drop_code_and_punctuation(self):
        self.assertEqual(
            features_page.slug("`relish wtf`: what's broken and why"),
            "relish-wtf-whats-broken-and-why",
        )
        self.assertEqual(features_page.slug("runc (Linux)"), "runc-linux")

    def test_a_link_to_a_file_lands_on_its_title(self):
        self.assertEqual(features_page.resolve_doc("guide.md", self.repo), "The guide")
        self.assertEqual(features_page.resolve_doc("guide.md#install-it-1", self.repo), "Install it")

    def test_a_missing_file_or_anchor_is_a_problem(self):
        loaded = catalogue(broken('docs = ["guide.md#install-it"]', 'docs = ["guide.md#uninstall-it", "gone.md"]'))
        problems = features_page.check_docs(loaded, self.repo)
        self.assertEqual(len(problems), 2, problems)
        self.assertIn("#uninstall-it", problems[0])
        self.assertIn("gone.md doesn't exist", problems[1])


class PageTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.repo = Path(self.directory.name)
        (self.repo / "guide.md").write_text(GUIDE, encoding="utf-8")
        self.page = features_page.render(catalogue(), self.repo)

    def tearDown(self):
        self.directory.cleanup()

    def test_the_page_names_its_markdown_copy(self):
        self.assertIn('<link rel="alternate" type="text/markdown" href="features.md">', self.page)

    def test_the_page_loads_the_script_that_adds_the_markdown_buttons(self):
        # Every page offers "Copy as Markdown" and "Open in Claude" (#1695).
        self.assertIn('<script src="assets/copy.js" defer></script>\n</body>', self.page)

    def test_a_row_shows_status_restrictions_issues_and_docs(self):
        row = self.page[self.page.index('<tr id="cluster-laptop-cluster">') :]
        row = row[: row.index("</tr>")]
        self.assertIn('class="badge badge-ready">Ready<', row)
        self.assertIn("<td>0.1.0</td>", row)
        self.assertIn("Needs <code>lima</code>.", row)
        self.assertIn('href="https://github.com/reliaburger/reliaburger/issues/10">#10<', row)
        self.assertIn("/blob/v0.2.7/guide.md#install-it", row)

    def test_a_planned_row_links_its_milestone(self):
        self.assertIn(
            'Planned for <a href="https://github.com/reliaburger/reliaburger/milestone/6">0.3.0</a>',
            self.page,
        )

    def test_every_section_has_an_id_and_a_permalink(self):
        for section in ("statuses", "cluster", "source"):
            self.assertIn(f'<section id="{section}">', self.page)
            self.assertIn(f'<a class="permalink" href="#{section}"', self.page)

    def test_text_is_escaped(self):
        loaded = catalogue(broken("One command builds a cluster.", "Uses <script> & `a<b>`."))
        page = features_page.render(loaded, self.repo)
        self.assertIn("Uses &lt;script&gt; &amp; <code>a&lt;b&gt;</code>.", page)

    def test_the_counts_add_up(self):
        self.assertIn("1 features are Ready, 0 are Beta and 0 are Alpha, 1 in all, and 1 more are planned", self.page)


class IssueStateTests(unittest.TestCase):
    def test_a_closed_issue_is_reported_once_per_row(self):
        states = {10: "OPEN", 11: "CLOSED"}
        problems = features_page.check_issues(catalogue(), state=states.__getitem__)
        self.assertEqual(len(problems), 1)
        self.assertIn("#11, which is closed", problems[0])

    def test_each_issue_is_asked_about_once(self):
        asked = []

        def state(number):
            asked.append(number)
            return "OPEN"

        loaded = catalogue(broken("issues = [11]", "issues = [10]"))
        self.assertEqual(features_page.check_issues(loaded, state=state), [])
        self.assertEqual(asked, [10])


class RepositoryTests(unittest.TestCase):
    """The real docs/features.toml and the page generated from it."""

    def test_the_catalogue_and_its_page_are_consistent(self):
        self.assertEqual(features_page.check(REPO), [])

    def test_the_markdown_copy_exists_and_llms_txt_lists_it(self):
        self.assertTrue((REPO / "docs/website/features.md").is_file())
        llms = (REPO / "docs/website/llms.txt").read_text(encoding="utf-8")
        self.assertIn("](https://reliaburger.com/features.md)", llms)

    def test_the_page_describes_the_release_the_installer_installs(self):
        installer = (REPO / "docs/website/install.sh").read_text(encoding="utf-8")
        default = installer.split("${RELIABURGER_VERSION:-", 1)[1].split("}", 1)[0]
        loaded = features_page.load((REPO / features_page.SOURCE).read_text(encoding="utf-8"))
        self.assertEqual(f"v{loaded.release}", default, "update docs/features.toml after the release")

    def test_ci_checks_the_live_issue_states_nightly(self):
        # Its own workflow, like flakes.yml: ci.yml is also called from
        # workflows that grant only `contents: read`.
        workflow = (REPO / ".github" / "workflows" / "features.yml").read_text()
        self.assertRegex(workflow, r"(?m)^  schedule:\n    - cron: ")
        self.assertRegex(workflow, r"(?m)^  issues: read$")
        self.assertIn("GH_TOKEN: ${{ github.token }}", workflow)
        self.assertIn("python3 scripts/ci/features_page.py check-issues", workflow)
        self.assertIn("docs/features.toml", workflow)

    def test_the_homepage_links_the_page(self):
        homepage = (REPO / "docs/website/index.html").read_text(encoding="utf-8")
        self.assertIn('href="features"', homepage)


if __name__ == "__main__":
    unittest.main()
