"""mkosi's tools tree holds what an Ubuntu image needs, and nothing that
Debian can drop from under it.

`ToolsTree=yes` builds mkosi's tools from Debian testing (the default when
the runner is Ubuntu). Its default `package-manager` profile adds dnf5,
zypper and pacman for building other distributions, and Debian testing
removed dnf5 on 18 November 2026, which failed every appliance build with
`Unable to locate package dnf5`. The appliance is Ubuntu, so the profile
goes, and the one package in it an Ubuntu build does need, the Ubuntu
archive keyring that apt checks resolute's Release file against, is named
on its own.

    python3 -m unittest discover -s image/tests -p 'test_tools_tree.py'
"""

import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
CONF = ROOT / "image" / "mkosi.conf"


def build_section():
    """The `[Build]` options as lists of values. Not configparser: mkosi
    repeats options and indents continuation lines."""
    options = {}
    section = None
    for line in CONF.read_text().splitlines():
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        if stripped.startswith("[") and stripped.endswith("]"):
            section = stripped[1:-1]
            continue
        if section != "Build" or "=" not in stripped:
            continue
        key, _, value = stripped.partition("=")
        options.setdefault(key, []).extend(v.strip() for v in value.split(",") if v.strip())
    return options


class ToolsTreeTests(unittest.TestCase):
    def test_the_tools_tree_is_built(self):
        self.assertEqual(build_section().get("ToolsTree"), ["yes"])

    def test_no_foreign_package_managers(self):
        profiles = build_section().get("ToolsTreeProfiles")
        self.assertIsNotNone(profiles, "without ToolsTreeProfiles mkosi adds package-manager")
        self.assertNotIn("package-manager", profiles)

    def test_the_tools_keep_what_they_had_otherwise(self):
        # misc and runtime are the other two default profiles.
        self.assertEqual(sorted(build_section()["ToolsTreeProfiles"]), ["misc", "runtime"])

    def test_the_ubuntu_archive_keyring_is_still_there(self):
        self.assertIn("ubuntu-keyring", build_section().get("ToolsTreePackages", []))


if __name__ == "__main__":
    unittest.main()
