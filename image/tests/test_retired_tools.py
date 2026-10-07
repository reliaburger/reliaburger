"""The appliance's preview tools stay retired (W7, #407).

`image/tools/` held the scripts that ran the bare-metal preview before relish
could: `netboot-server.sh` (now `relish netboot`), `seed-fleet.sh`,
`node-toml.py` and the `seed-admin` helper (now `relish cluster create
--bare-metal`, `relish image seed` and `relish machines claim`). These tests
fail if any of them comes back, or if something still points at them. The
only survivor, `fleet-measure.sh`, moved to the lab, since S5 needs it and no
relish command samples a node's memory and eMMC writes yet.

    python3 -m unittest discover -s image/tests -p 'test_retired_tools.py'
"""

import os
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
IMAGE = ROOT / "image"
LAB = IMAGE / "lab"

RETIRED = ("netboot-server.sh", "seed-fleet.sh", "node-toml.py", "seed-admin",
           "seed-node1.sh", "seed-joiner.sh")

# History keeps the old names (the book's story of the spike, plans and
# qualification records), and the script tests here name them to pin their
# absence.
HISTORY = (ROOT / "docs" / "book", ROOT / "docs" / "plans", ROOT / "docs" / "qualification",
           IMAGE / "tests")
SKIP_DIRS = {".git", "target", "node_modules", "work", "mkosi.output", ".claude"}
TEXT = {".md", ".sh", ".py", ".rs", ".toml", ".yml", ".yaml", ".json", ".conf", ".txt", ""}


def text_files():
    for directory, subdirs, files in os.walk(ROOT):
        subdirs[:] = [d for d in subdirs if d not in SKIP_DIRS]
        here = Path(directory)
        if any(here == h or h in here.parents for h in HISTORY):
            continue
        for name in files:
            path = here / name
            if path.suffix in TEXT:
                yield path


class RetiredTools(unittest.TestCase):
    def test_image_tools_is_gone(self):
        self.assertFalse((IMAGE / "tools").exists())

    def test_the_lab_has_no_retired_scripts(self):
        for name in RETIRED:
            self.assertFalse((LAB / name).exists(), name)

    def test_nothing_points_at_the_retired_tools(self):
        for path in text_files():
            try:
                text = path.read_text()
            except (UnicodeDecodeError, OSError):
                continue
            # Rust tests pin the absence too: `assert!(!markdown.contains(…))`.
            lines = [line for line in text.splitlines() if "assert!(!" not in line]
            for needle in ("image/tools", "tools/seed-admin") + RETIRED:
                for line in lines:
                    self.assertFalse(needle in line, f"{path.relative_to(ROOT)} mentions {needle}")

    def test_fleet_measure_lives_in_the_lab(self):
        self.assertTrue((LAB / "fleet-measure.sh").is_file())
        self.assertTrue((LAB / "fleet-nodes.py").is_file())

    def test_the_lab_seeds_with_relish(self):
        seed = (LAB / "seed-lab.sh").read_text()
        self.assertTrue("cluster create --bare-metal" in seed)
        self.assertFalse("relish init" in seed)

    def test_lab_machines_boot_the_seeds_relish_writes(self):
        # `relish cluster create --bare-metal` writes <dir>/stick/seeds/<mac>.seed
        # (bare_metal::stick_file), the layout a real RBSEED stick carries.
        for script in ("rbnode.sh", "wyse.sh"):
            text = (LAB / script).read_text()
            self.assertTrue("cluster/stick/seeds/" in text, script)
            self.assertFalse(".tgz" in text, script)

    def test_the_seed_stick_comes_from_a_relish_stick_directory(self):
        text = (LAB / "make-seed-stick.sh").read_text()
        self.assertTrue("<stick dir>" in text)
        self.assertFalse("<seed.tgz>" in text)


if __name__ == "__main__":
    unittest.main()
