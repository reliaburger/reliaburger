"""Tests for image/tests/installer_modules.py: the netboot installer has the
drivers a machine needs to reach the network (#1969). The Wyse 3040's
installer had r8169 without the Realtek PHY driver it asks for through a
softdep, so it never got an address.

    python3 -m unittest discover -s image/tests -p 'test_installer_modules.py'
"""

import struct
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
CHECK = HERE / "installer_modules.py"
KVER = "7.0.0-43-generic"


def run(*args):
    return subprocess.run([sys.executable, str(CHECK), *map(str, args)],
                          capture_output=True, text=True)


def cpio(files):
    """A newc cpio archive of {path: bytes}, as the kernel reads an initrd."""
    out = bytearray()

    def entry(name, data, mode):
        name_bytes = name.encode() + b"\0"
        header = "070701" + "".join(f"{v:08x}" for v in (
            0, mode, 0, 0, 1, 0, len(data), 0, 0, 0, 0, len(name_bytes), 0))
        out.extend(header.encode() + name_bytes)
        out.extend(b"\0" * (-len(out) % 4))
        out.extend(data)
        out.extend(b"\0" * (-len(out) % 4))

    for name, data in files.items():
        entry(name, data, 0o100644)
    entry("TRAILER!!!", b"", 0)
    return bytes(out)


def uki(initrd):
    """A minimal PE file with a .initrd section, as systemd-stub's UKIs
    carry one."""
    sections = [(b".cmdline", b"console=tty0\0"), (b".initrd", initrd)]
    pe = 0x40
    table = pe + 4 + 20
    data_start = table + 40 * len(sections)
    out = bytearray(data_start)
    out[0:2] = b"MZ"
    struct.pack_into("<I", out, 0x3C, pe)
    out[pe:pe + 4] = b"PE\0\0"
    struct.pack_into("<HHIIIHH", out, pe + 4, 0x8664, len(sections), 0, 0, 0, 0, 0)
    for i, (name, data) in enumerate(sections):
        offset = len(out)
        struct.pack_into("<8sIIII", out, table + 40 * i, name, len(data), 0, len(data), offset)
        out.extend(data)
    return bytes(out)


def module(firmware=(), softdep=None):
    """A stand-in .ko: just the .modinfo strings the check reads."""
    info = b"license=GPL\0"
    for name in firmware:
        info += b"firmware=" + name.encode() + b"\0"
    if softdep:
        info += b"softdep=" + softdep.encode() + b"\0"
    return b"\x7fELF" + info


KERNEL = f"usr/lib/modules/{KVER}/kernel/drivers"


def installer(drop=(), extra=None, softdep="softdep r8169 pre: realtek\n", builtin=""):
    """A complete x86_64 installer's files, minus `drop`."""
    files = {
        f"{KERNEL}/net/ethernet/realtek/r8169.ko": module(["rtl_nic/rtl8168h-2.fw"], "pre: realtek"),
        f"{KERNEL}/net/phy/realtek/realtek.ko": module(),
        f"{KERNEL}/net/ethernet/intel/e1000/e1000.ko": module(),
        f"{KERNEL}/net/ethernet/intel/e1000e/e1000e.ko": module(),
        f"{KERNEL}/net/ethernet/intel/igb/igb.ko": module(),
        f"{KERNEL}/net/ethernet/intel/igc/igc.ko": module(),
        f"{KERNEL}/net/ethernet/broadcom/tg3.ko": module(["tigon/tg3.bin", "tigon/tg3_tso.bin"]),
        f"{KERNEL}/net/usb/r8152.ko": module(["rtl_nic/rtl8153a-2.fw"]),
        f"{KERNEL}/net/usb/ax88179_178a.ko": module(),
        f"{KERNEL}/net/usb/cdc_ether.ko": module(),
        f"{KERNEL}/net/virtio_net.ko": module(),
        f"{KERNEL}/nvme/host/nvme.ko": module(),
        f"{KERNEL}/ata/ahci.ko": module(),
        f"{KERNEL}/mmc/core/mmc_block.ko": module(),
        f"{KERNEL}/mmc/host/sdhci-pci.ko": module(),
        f"{KERNEL}/mmc/host/sdhci-acpi.ko": module(),
        f"{KERNEL}/usb/storage/usb-storage.ko": module(),
        f"{KERNEL}/usb/host/xhci-pci.ko": module(),
        f"{KERNEL}/net/netconsole.ko": module(),
        f"usr/lib/modules/{KVER}/kernel/fs/configfs/configfs.ko": module(),
        f"usr/lib/modules/{KVER}/kernel/fs/fat/vfat.ko": module(),
        f"usr/lib/modules/{KVER}/kernel/fs/nls/nls_cp437.ko": module(),
        f"usr/lib/modules/{KVER}/kernel/fs/nls/nls_iso8859-1.ko": module(),
        f"usr/lib/modules/{KVER}/modules.softdep": softdep.encode(),
        f"usr/lib/modules/{KVER}/modules.builtin": builtin.encode(),
        "usr/lib/firmware/rtl_nic/rtl8168h-2.fw.zst": b"fw",
        "usr/lib/firmware/rtl_nic/rtl8153a-2.fw": b"fw",
    }
    for path in list(files):
        if any(path.endswith(d) for d in drop):
            del files[path]
    files.update(extra or {})
    return files


class InstallerModules(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self.tmp.name)

    def tearDown(self):
        self.tmp.cleanup()

    def check(self, files, arch="x86_64"):
        path = self.dir / "installer.efi"
        path.write_bytes(uki(cpio(files)))
        return run(path, arch)

    def test_a_complete_installer_passes(self):
        done = self.check(installer())
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertIn("r8169", done.stdout)

    def test_the_wyse_installer_without_the_realtek_phy_driver_fails(self):
        done = self.check(installer(drop=["realtek/realtek.ko"]))
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("r8169", done.stdout)
        self.assertIn("realtek", done.stdout)
        self.assertIn("softdep", done.stdout)

    def test_a_missing_softdep_of_any_module_fails(self):
        extra = {f"{KERNEL}/net/ethernet/other/nic.ko": module()}
        done = self.check(installer(
            extra=extra, softdep="softdep r8169 pre: realtek\nsoftdep nic pre: nic-phy post: nic-extra\n"))
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("nic_phy", done.stdout)
        self.assertIn("nic_extra", done.stdout)

    def test_a_softdep_of_a_module_the_installer_leaves_out_is_ignored(self):
        done = self.check(installer(softdep="softdep r8169 pre: realtek\nsoftdep absent pre: also-absent\n"))
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)

    def test_a_required_driver_missing_fails(self):
        done = self.check(installer(drop=["e1000e/e1000e.ko"]))
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("e1000e", done.stdout)

    def test_an_installer_that_cannot_stream_boot_logs_fails(self):
        # #2149: netconsole sends the installer's lines, and FAT mounts the
        # installed ESP to write netlog.conf.
        for missing in ("netconsole.ko", "vfat.ko", "nls_iso8859-1.ko"):
            done = self.check(installer(drop=[missing]))
            self.assertEqual(done.returncode, 1, done.stdout)
            self.assertIn(missing.removesuffix(".ko").replace("-", "_"), done.stdout)

    def test_a_built_in_driver_counts_as_present(self):
        done = self.check(installer(drop=["virtio_net.ko"],
                                    builtin="kernel/drivers/net/virtio_net.ko\n"))
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)

    def test_missing_firmware_for_a_required_driver_fails(self):
        done = self.check(installer(drop=["rtl8168h-2.fw.zst"]))
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("rtl_nic/rtl8168h-2.fw", done.stdout)

    def test_tg3s_firmware_is_optional(self):
        done = self.check(installer())
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertIn("tigon/tg3.bin", done.stdout)

    def test_a_file_without_an_initrd_is_refused(self):
        path = self.dir / "not-a-uki.efi"
        path.write_bytes(b"MZ" + b"\0" * 200)
        done = run(path, "x86_64")
        self.assertEqual(done.returncode, 1, done.stdout)

    def test_an_unknown_architecture_is_a_usage_error(self):
        path = self.dir / "installer.efi"
        path.write_bytes(uki(cpio(installer())))
        self.assertEqual(run(path, "riscv64").returncode, 2)


if __name__ == "__main__":
    unittest.main()
