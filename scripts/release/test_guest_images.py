"""The guest image pins and the script that builds the release image from them."""
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import unittest

HERE = Path(__file__).resolve().parent
PINS = json.loads((HERE / "guest-images.json").read_text())
BUILD = HERE / "build_guest_image.sh"


class GuestImagePinTests(unittest.TestCase):
    def test_every_supported_architecture_is_pinned_to_a_dated_upstream_image(self):
        self.assertEqual(set(PINS["images"]), {"aarch64", "x86_64"})
        for arch, image in PINS["images"].items():
            with self.subTest(arch=arch):
                source = image["source"]
                self.assertRegex(source["url"], r"^https://cloud-images\.ubuntu\.com/releases/noble/release-\d{8}/")
                self.assertNotIn("current", source["url"])
                self.assertRegex(source["sha256"], r"^[0-9a-f]{64}$")
                self.assertTrue(source["file"].endswith(f"-{arch}.img"))
                self.assertRegex(image["asset"], rf"^reliaburger-guest-[a-z0-9.-]+-{arch}\.qcow2$")

    def test_package_list_is_plain_debian_names(self):
        # The list reaches a shell in the build script and in the VM's
        # provisioning; the CLI refuses anything else too.
        self.assertIn("runc", PINS["packages"])
        for package in PINS["packages"]:
            self.assertRegex(package, r"^[a-z0-9][a-z0-9+.-]*$")


class BuildScriptTests(unittest.TestCase):
    def test_script_parses_and_is_executable(self):
        self.assertTrue(os.access(BUILD, os.X_OK))
        result = subprocess.run(["bash", "-n", str(BUILD)], capture_output=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        if shutil.which("shellcheck"):
            result = subprocess.run(["shellcheck", str(BUILD)], capture_output=True)
            self.assertEqual(result.returncode, 0, result.stdout)

    def test_script_seals_per_vm_identity_before_compressing(self):
        text = BUILD.read_text()
        seal = text.index('>"$root/etc/machine-id"')
        for step in ("cloud-init clean", "ssh_host_", "policy-rc.d"):
            self.assertIn(step, text)
        self.assertLess(text.index("cloud-init clean"), text.index("qemu-img convert -c"))
        self.assertLess(seal, text.index("qemu-img convert -c"))
        # zstd qcow2 clusters are unreadable to Lima 2.1.0.
        self.assertRegex(text, r"qemu-img convert -c -O qcow2 -o compression_type=zlib")

    def test_script_refuses_to_run_without_root(self):
        if os.geteuid() == 0:
            self.skipTest("running as root")
        result = subprocess.run(["bash", str(BUILD), "--output", "unused"], capture_output=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(b"run as root", result.stderr)
        self.assertFalse(Path("unused").exists())

    def test_script_verifies_the_upstream_image_before_using_it(self):
        text = BUILD.read_text()
        self.assertLess(text.index("sha256sum --check"), text.index("qemu-img convert -O raw"))
        self.assertTrue(re.search(r"--proto '=https'", text))


if __name__ == "__main__":
    unittest.main()
