"""Promotion writes the MCP Registry entry with each published bundle's digest (#1695)."""
import hashlib
import json
from pathlib import Path
import tempfile
import unittest

from mcp_registry import PACKAGE_TRANSPORT, entry, main
from package import PLATFORMS, mcpb_name

SERVER = Path(__file__).resolve().parents[2] / "server.json"


def server(version="0.2.7"):
    document = json.loads(SERVER.read_text())
    document["version"] = version
    return json.dumps(document, indent=2) + "\n"


def record(version="v0.2.7", channel=None, platforms=PLATFORMS["relish"]):
    assets = {mcpb_name(platform): {"sha256": hashlib.sha256(platform.encode()).hexdigest(), "size": 1}
              for platform in platforms}
    assets["relish-linux-x86_64"] = {"sha256": "f" * 64, "size": 1}
    document = dict(schema=1, version=version, repository="reliaburger/reliaburger", commit="a" * 40,
                    run_id=1, run_attempt=1, assets=assets)
    if channel:
        document["channel"] = channel
    return (json.dumps(document, indent=2, sort_keys=True) + "\n").encode()


class RegistryEntryTests(unittest.TestCase):
    def test_each_platform_bundle_is_a_package_with_its_published_digest(self):
        document = json.loads(entry(record(), server()))
        packages = document["packages"]
        self.assertEqual(len(packages), len(PLATFORMS["relish"]))
        for package, platform in zip(packages, PLATFORMS["relish"]):
            with self.subTest(platform=platform):
                self.assertEqual(package, {
                    "registryType": "mcpb",
                    "identifier": "https://github.com/reliaburger/reliaburger/releases/download/"
                                  f"v0.2.7/relish-{platform}.mcpb",
                    "version": "0.2.7",
                    "fileSha256": hashlib.sha256(platform.encode()).hexdigest(),
                    "transport": PACKAGE_TRANSPORT,
                })
                # The registry refuses a bundle URL without "mcp" in it.
                self.assertIn("mcp", package["identifier"].lower())

    def test_each_package_has_what_the_vendored_registry_schema_asks_of_one(self):
        # The registry's schema, vendored for the Rust validation of
        # server.json; Python checks the parts a package entry adds.
        schema = json.loads((SERVER.parent / "tests/fixtures/mcp-registry/server.schema.json").read_text())
        definition = schema["definitions"]["Package"]
        stdio = schema["definitions"]["StdioTransport"]["properties"]["type"]["enum"]
        for package in json.loads(entry(record(), server()))["packages"]:
            self.assertLessEqual(set(definition["required"]), set(package))
            self.assertLessEqual(set(package), set(definition["properties"]))
            self.assertRegex(package["fileSha256"], definition["properties"]["fileSha256"]["pattern"])
            self.assertIn(package["transport"]["type"], stdio)
            self.assertIn("mcpb", definition["properties"]["registryType"]["examples"])

    def test_everything_else_in_the_entry_is_kept(self):
        before = json.loads(server())
        after = json.loads(entry(record(), server()))
        del after["packages"]
        self.assertEqual(after, before)

    def test_a_record_that_differs_from_the_qualified_digest_is_refused(self):
        data = record()
        entry(data, server(), hashlib.sha256(data).hexdigest())
        with self.assertRaisesRegex(ValueError, "qualified digest"):
            entry(data, server(), "0" * 64)

    def test_nightly_builds_and_pre_releases_never_reach_the_registry(self):
        with self.assertRaisesRegex(ValueError, "nightly"):
            entry(record(channel="nightly"), server())
        with self.assertRaisesRegex(ValueError, "release tag"):
            entry(record(version="v0.2.7-rc.1"), server())

    def test_an_entry_for_another_version_is_refused(self):
        # `make agent-docs` sets server.json's version; a tag whose entry
        # still names the last release would publish the wrong one.
        with self.assertRaisesRegex(ValueError, "make agent-docs"):
            entry(record(), server("0.2.6"))

    def test_a_missing_bundle_is_refused(self):
        with self.assertRaisesRegex(ValueError, "relish-macos-aarch64.mcpb"):
            entry(record(platforms=("linux-aarch64", "linux-x86_64")), server())

    def test_an_entry_that_already_lists_packages_is_refused(self):
        document = json.loads(server())
        document["packages"] = []
        with self.assertRaisesRegex(ValueError, "packages"):
            entry(record(), json.dumps(document))

    def test_the_command_line_writes_the_entry_from_files(self):
        with tempfile.TemporaryDirectory() as scratch:
            scratch = Path(scratch)
            (scratch / "candidate.json").write_bytes(record())
            (scratch / "tagged.json").write_text(server())
            main(["--record", str(scratch / "candidate.json"), "--server", str(scratch / "tagged.json"),
                  "--qualified-digest", hashlib.sha256(record()).hexdigest(),
                  "--output", str(scratch / "server.json")])
            self.assertEqual((scratch / "server.json").read_text(), entry(record(), server()))


if __name__ == "__main__":
    unittest.main()
