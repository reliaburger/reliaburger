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
APPLIANCE = HERE.parent.parent / "image" / "mkosi.conf"
# Guest-only packages. The appliance builds no images, so it has no Buildah.
GUEST_ONLY = {"buildah"}


def appliance_packages():
    """The Packages= list of the appliance's mkosi.conf."""
    packages, listing = [], False
    for line in APPLIANCE.read_text().splitlines():
        if line.startswith("Packages="):
            listing = True
        elif listing and line[:1].isspace() and line.strip():
            packages.append(line.strip())
        elif listing:
            break
    return packages


class GuestImagePinTests(unittest.TestCase):
    def test_every_supported_architecture_is_pinned_to_a_dated_upstream_image(self):
        self.assertEqual(set(PINS["images"]), {"aarch64", "x86_64"})
        for arch, image in PINS["images"].items():
            with self.subTest(arch=arch):
                source = image["source"]
                self.assertRegex(source["url"], r"^https://cloud-images\.ubuntu\.com/releases/resolute/release-\d{8}/ubuntu-26\.04-")
                self.assertNotIn("current", source["url"])
                self.assertRegex(source["sha256"], r"^[0-9a-f]{64}$")
                self.assertTrue(source["file"].endswith(f"-{arch}.img"))
                self.assertRegex(image["asset"], rf"^reliaburger-guest-ubuntu-26\.04-\d{{8}}-{arch}\.qcow2$")

    def test_package_list_is_plain_debian_names(self):
        # The list reaches a shell in the build script and in the VM's
        # provisioning; the CLI refuses anything else too.
        self.assertIn("runc", PINS["packages"])
        for package in PINS["packages"]:
            self.assertRegex(package, r"^[a-z0-9][a-z0-9+.-]*$")

    def test_nodes_can_build_images(self):
        # `relish build` needs Buildah on a node, and the five-minute tour
        # builds examples/demo/burger on the quickstart cluster. Its
        # dependencies (containers-common, the CNI plugins, netavark) come
        # with it; the node runs it with the vfs storage driver, so it needs
        # no fuse-overlayfs.
        self.assertIn("buildah", PINS["packages"])

    def test_guest_and_appliance_share_the_node_packages(self):
        # Both run Ubuntu 26.04 and the same bun, so a node package added to
        # one and not the other is a node that works in one place only.
        appliance = appliance_packages()
        self.assertIn("linux-image-generic", appliance)
        node = [package for package in PINS["packages"] if package not in GUEST_ONLY]
        self.assertEqual(node, appliance[-len(node):])
        for package in GUEST_ONLY:
            self.assertNotIn(package, appliance)


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

    def test_image_leaves_the_clock_to_the_lima_guest_agent(self):
        # Lima's guest agent sets the clock to the host's every 10 s and can't
        # be turned off; timesyncd running beside it fought it, and the agent
        # stepped the clock back every 10 s (#608).
        text = BUILD.read_text()
        # Ubuntu 26.04 syncs time with chrony, not timesyncd as 24.04 did.
        disable = text.index("in_guest systemctl disable chrony.service")
        self.assertLess(text.index("in_guest apt-get"), disable)
        self.assertLess(disable, text.index("qemu-img convert -c"))

    def test_script_grows_the_root_filesystem_before_installing(self):
        # Ubuntu 26.04's cloud image leaves its 2.5 GiB root about 160 MiB
        # free, and the node packages need about 270 MiB.
        text = BUILD.read_text()
        install = text.index("in_guest apt-get -o Acquire::Retries=3 install")
        for step in ("truncate --size=+", "growpart ", "resize2fs "):
            with self.subTest(step=step):
                self.assertLess(text.index(step), install)
        self.assertLess(text.index("truncate --size=+"), text.index("losetup --find"))

    def test_image_names_its_network_interface_before_first_boot(self):
        # Ubuntu 26.04's initrd brings the NIC up as enp0s1. Lima's network
        # config renames it to eth0, which cloud-init can't do to an
        # interface that is up, so a fresh VM waited two minutes for an eth0
        # that never came. A .link file in place before first boot has udev
        # rename it at switch-root instead.
        text = BUILD.read_text()
        link = text.index("/etc/systemd/network/10-reliaburger-eth0.link")
        self.assertIn("Driver=virtio_net", text)
        self.assertIn("Name=eth0", text)
        self.assertLess(link, text.index("qemu-img convert -c"))

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
