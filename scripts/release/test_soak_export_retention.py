"""The soak's metrics export keeps a bounded tail on the guest (#1418)."""
import json
import os
from pathlib import Path
import tempfile
import unittest

import soak_export_retention as retention

NOW = 1_800_000_000
KEEP = retention.KEEP_SECONDS


class Retention(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        root = Path(self.temp.name)
        self.source = root / "metrics"
        self.destination = root / "soak-export" / "metrics"
        self.source.mkdir(parents=True)
        (self.destination / "node-1").mkdir(parents=True)

    def exported(self, name, verified=True, in_source=True):
        if in_source:
            (self.source / name).write_bytes(b"x")
        relative = f"node-1/{'ab' * 32}-{name}"
        (self.destination / relative).write_bytes(b"x")
        if verified:
            with open(self.destination / ".soak-verified", "a") as listing:
                listing.write(relative + "\n")
        return relative

    def prune(self, now):
        return retention.prune(self.source, self.destination, now, KEEP)

    def present(self, relative):
        return (self.destination / relative).exists()

    def test_an_export_goes_once_its_source_has_been_gone_for_the_keep_window(self):
        old = self.exported("metrics_000001.parquet")
        self.assertEqual(self.prune(NOW), [])
        os.unlink(self.source / "metrics_000001.parquet")
        # The checker needs to see it at the destination after it left the source.
        self.assertEqual(self.prune(NOW + 60), [])
        self.assertEqual(self.prune(NOW + 60 + KEEP - 1), [])
        self.assertTrue(self.present(old))
        self.assertEqual(self.prune(NOW + 60 + KEEP), [old])
        self.assertFalse(self.present(old))
        state = json.loads((self.destination / retention.STATE_FILE).read_text())
        self.assertEqual(state, {})

    def test_an_export_whose_source_is_still_there_stays(self):
        kept = self.exported("metrics_000002.parquet")
        self.assertEqual(self.prune(NOW), [])
        self.assertEqual(self.prune(NOW + 10 * KEEP), [])
        self.assertTrue(self.present(kept))

    def test_an_unverified_export_stays(self):
        unverified = self.exported("metrics_000003.parquet", verified=False, in_source=False)
        self.assertEqual(self.prune(NOW), [])
        self.assertEqual(self.prune(NOW + 10 * KEEP), [])
        self.assertTrue(self.present(unverified))

    def test_a_day_of_exports_stays_bounded(self):
        # One file a minute for a day, the source keeping its newest ten, and
        # a heavy report every five minutes: the 0.2.3 day run kept all 1,440.
        for minute in range(24 * 60):
            self.exported(f"metrics_{minute:06}.parquet")
            stale = self.source / f"metrics_{minute - 10:06}.parquet"
            if stale.exists():
                stale.unlink()
            if minute % 5 == 0:
                self.prune(NOW + minute * 60)
        left = list(self.destination.glob("node-1/*.parquet"))
        self.assertLessEqual(len(left), 10 + KEEP // 60 + 10)

    def test_a_missing_destination_is_nothing_to_do(self):
        missing = Path(self.temp.name) / "nowhere"
        self.assertEqual(retention.prune(self.source, missing, NOW, KEEP), [])


if __name__ == "__main__":
    unittest.main()
