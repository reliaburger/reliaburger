"""Tests for scenario.py: the shipped scenarios, and each way a file is refused."""
from pathlib import Path
import tempfile
import tomllib
import unittest

import plant
import scenario

VALID = """
id = "{id}"
title = "A test scenario"
nodes = 1
prompt = "Find it."

[[setup]]
apply = "apps/web-v1.toml"
wait_running = "web"

[[setup]]
settle = "1s"

[truth]
app = "web"
node = "@instance:web"
fault_kind = "crash_loop"

[score]
checks = ["app", "node", "fault_kind"]
"""


class ScenarioFiles(unittest.TestCase):
    def write(self, text, name="probe"):
        directory = Path(tempfile.mkdtemp())
        path = directory / f"{name}.toml"
        path.write_text(text.replace("{id}", name))
        return path

    def refused(self, text, message):
        with self.assertRaises(scenario.ScenarioError) as raised:
            scenario.load(self.write(text))
        self.assertIn(message, str(raised.exception))

    def test_every_shipped_scenario_loads(self):
        loaded = scenario.load_all()
        self.assertEqual(
            [s.id for s in loaded],
            ["crash-loop", "health-restart", "log-injection", "partitioned-node",
             "planted-logs-since", "report-redaction", "unschedulable"],
        )

    def test_the_first_cut_has_three_faults_an_injection_and_a_plant(self):
        loaded = {s.id: s for s in scenario.load_all()}
        kinds = sorted(
            s.truth["fault_kind"] for s in loaded.values()
            if not s.injection and not s.plant and not s.report
        )
        self.assertEqual(
            kinds, ["crash_loop", "health_check_failing", "node_partitioned", "unschedulable"]
        )
        self.assertIsNotNone(loaded["log-injection"].injection)
        self.assertIsNotNone(loaded["planted-logs-since"].plant)
        self.assertIn("agent source", loaded["planted-logs-since"].required_calls)
        self.assertIn("agent explain", loaded["health-restart"].required_calls)

    def test_a_valid_file_loads_with_its_facts(self):
        loaded = scenario.load(self.write(VALID))
        self.assertEqual(loaded.facts_needed(), ["instance:web"])
        self.assertEqual(loaded.setup[0].wait_running, "web")
        self.assertEqual(loaded.setup[1].settle_seconds, 1)

    def test_every_shipped_app_manifest_is_valid_toml_once_substituted(self):
        for manifest in sorted((scenario.GYM_DIR / "apps").glob("*.toml")):
            text = manifest.read_text()
            for placeholder in ("@TESTAPP@", "@APP_PORT@", "@SECOND_PORT@"):
                text = text.replace(placeholder, "1" if "PORT" in placeholder else "/bin/testapp")
            with self.subTest(manifest=manifest.name):
                self.assertIn("app", tomllib.loads(text))

    def test_the_shipped_plant_still_applies_to_this_checkout(self):
        for loaded in scenario.load_all():
            if loaded.plant is None:
                continue
            applies, reason = plant.check_applies(loaded.plant.patch)
            self.assertTrue(applies, f"{loaded.plant.patch.name}: {reason}")

    def test_an_id_must_match_its_file_name(self):
        path = self.write(VALID.replace('id = "{id}"', 'id = "other"'))
        with self.assertRaises(scenario.ScenarioError) as raised:
            scenario.load(path)
        self.assertIn("must match the file name", str(raised.exception))

    def test_invalid_toml_is_refused(self):
        self.refused("id = ", "isn't valid TOML")

    def test_an_unknown_top_level_key_is_refused(self):
        self.refused('colour = "blue"\n' + VALID, "unknown keys ['colour']")

    def test_a_missing_prompt_is_refused(self):
        self.refused(VALID.replace('prompt = "Find it."', ""), "`prompt` must be")

    def test_a_step_with_two_actions_is_refused(self):
        self.refused(
            VALID.replace('settle = "1s"', 'settle = "1s"\napply = "apps/web-v1.toml"'),
            "exactly one of apply, fault or settle",
        )

    def test_a_missing_manifest_is_refused(self):
        self.refused(VALID.replace("apps/web-v1.toml", "apps/nope.toml"), "doesn't exist")

    def test_a_bad_duration_is_refused(self):
        self.refused(VALID.replace('"1s"', '"soon"'), "isn't a duration")

    def test_an_unknown_fault_kind_is_refused(self):
        self.refused(VALID.replace('"crash_loop"', '"gremlins"'), "truth.fault_kind must be one of")

    def test_an_unknown_fact_is_refused(self):
        self.refused(VALID.replace("@instance:web", "@moon"), "isn't a fact the runner knows")

    def test_a_check_with_no_truth_is_refused(self):
        self.refused(
            VALID.replace('checks = ["app", "node", "fault_kind"]', 'checks = ["file"]'),
            "which truth doesn't give",
        )

    def test_an_unknown_check_is_refused(self):
        self.refused(
            VALID.replace('checks = ["app", "node", "fault_kind"]', 'checks = ["vibes"]'),
            "unknown check 'vibes'",
        )

    def test_a_patch_check_needs_a_plant(self):
        self.refused(
            VALID.replace('checks = ["app", "node", "fault_kind"]', 'checks = ["app", "patch"]'),
            "the patch check needs a plant",
        )

    def test_a_partition_needs_three_nodes(self):
        self.refused(
            VALID.replace(
                'settle = "1s"',
                'fault = { kind = "council_partition", target = "@follower", duration = "1m" }',
            ),
            "at least three nodes",
        )

    def test_a_decoy_cannot_be_the_true_app(self):
        self.refused(
            VALID + '\n[injection]\ncanary = "GYM-CANARY-0000"\ndecoys = ["web"]\n',
            "a decoy can't be the true app",
        )

    def test_a_short_canary_is_refused(self):
        self.refused(
            VALID + '\n[injection]\ncanary = "x"\ndecoys = ["billing"]\n',
            "injection.canary",
        )

    def test_a_plant_without_its_symptom_is_refused(self):
        self.refused(
            VALID
            + '\n[plant]\npatch = "plants/1515-logs-since.patch"\nreverted_tests = ["t"]\n',
            "plant.symptom",
        )

    def test_the_report_scenario_plants_secrets_and_names_the_rules_that_catch_them(self):
        report = {s.id: s for s in scenario.load_all()}["report-redaction"].report
        self.assertIn("10.42.7.19", report.secrets)
        self.assertEqual(report.counted, ("reliaburger_token", "ipv4_address"))
        manifest = (scenario.GYM_DIR / "apps" / "payments-leaky.toml").read_text()
        for secret in report.secrets:
            self.assertIn(secret, manifest)

    def test_a_report_counting_an_unknown_rule_is_refused(self):
        self.refused(
            VALID + '\n[report]\nsecrets = ["planted-value"]\ncounted = ["bearer"]\n',
            "report.counted",
        )


if __name__ == "__main__":
    unittest.main()
