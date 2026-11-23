#!/usr/bin/env python3
"""The netboot installer has the drivers a machine needs to reach the
network and its disk (#1969).

    installer_modules.py <installer.efi> <x86_64|aarch64>

The installer is a UKI whose initrd is the whole system it runs, and
mkosi's KernelModules= picks which kernel modules go in. mkosi adds each
module's hard dependencies but not its soft ones, so the Wyse 3040's
installer had r8169 without the Realtek PHY driver r8169 loads first
(`softdep r8169 pre: realtek`), and never got an address.

This unpacks the installer's initrd and fails if:
- a module it carries has a soft dependency it doesn't carry;
- a driver in REQUIRED is missing (built into the kernel counts);
- a driver in REQUIRED asks for firmware the initrd doesn't have, unless
  OPTIONAL_FIRMWARE says why that's fine.
"""

import lzma
import re
import struct
import subprocess
import sys
from pathlib import Path

# The wired NICs, disks and buses of the machines the appliance is for:
# VMs, mini PCs and thin clients. Names as the kernel spells them, with
# dashes as underscores.
COMMON = {
    "r8169", "realtek",  # Realtek RTL8111/8168 (the Wyse 3040) and RTL8125, and its PHY
    "e1000e", "igc", "igb", "e1000",  # Intel I219, I225/I226, I210/I350, QEMU's e1000
    "tg3",  # Broadcom NetXtreme (HP mini PCs)
    "r8152", "ax88179_178a", "cdc_ether",  # USB Ethernet adapters
    "virtio_net",
    "nvme", "ahci", "mmc_block", "usb_storage", "xhci_pci",
    # Boot logs over the network (#2149): netconsole through configfs, and
    # the installed ESP mounted to write netlog.conf.
    "netconsole", "configfs", "vfat", "nls_cp437", "nls_iso8859_1",
}
REQUIRED = {
    "x86_64": COMMON | {"sdhci_pci", "sdhci_acpi"},
    "aarch64": COMMON,
}

# Firmware a driver asks for but works without on the machines we target.
OPTIONAL_FIRMWARE = {
    "tg3": "only the 5701 A0, TSO offload on the 5703-5705 and the 57766 in old "
           "Macs load it; the BCM5720 and BCM5762 in mini PCs run without",
}

CPIO_MAGIC = b"070701"
ZSTD_MAGIC = b"\x28\xb5\x2f\xfd"


def initrd(uki):
    """The .initrd section of a PE file, or None."""
    data = Path(uki).read_bytes()
    if data[:2] != b"MZ" or len(data) < 0x40:
        return None
    pe = struct.unpack_from("<I", data, 0x3C)[0]
    if data[pe:pe + 4] != b"PE\0\0":
        return None
    count, = struct.unpack_from("<H", data, pe + 6)
    optional, = struct.unpack_from("<H", data, pe + 20)
    table = pe + 24 + optional
    for i in range(count):
        entry = table + 40 * i
        name = data[entry:entry + 8].rstrip(b"\0")
        virtual_size, _, raw_size, raw = struct.unpack_from("<IIII", data, entry + 8)
        if name == b".initrd":
            return data[raw:raw + min(virtual_size, raw_size)]
    return None


def decompress(data):
    """zstd or xz data, or data as it is."""
    if data.startswith(ZSTD_MAGIC):
        return subprocess.run(["zstd", "-dcq"], input=data, capture_output=True, check=True).stdout
    if data.startswith(b"\xfd7zXZ"):
        return lzma.decompress(data)
    return data


def cpio_files(data):
    """{path: bytes} from one or more newc cpio archives, each possibly
    compressed, as the kernel unpacks an initrd."""
    files = {}
    while data:
        data = data.lstrip(b"\0")
        if not data:
            break
        if not data.startswith(CPIO_MAGIC):
            data = decompress(data)
            if not data.startswith(CPIO_MAGIC):
                raise ValueError("the initrd isn't a newc cpio archive")
        offset = 0
        while True:
            header = data[offset:offset + 110]
            if header[:6] != CPIO_MAGIC:
                raise ValueError(f"bad cpio header at {offset}")
            fields = [int(header[6 + 8 * i:14 + 8 * i], 16) for i in range(13)]
            mode, size, name_size = fields[1], fields[6], fields[11]
            name_start = offset + 110
            name = data[name_start:name_start + name_size - 1].decode()
            body = name_start + name_size
            body += -body % 4
            if name == "TRAILER!!!":
                offset = body
                break
            if mode & 0o170000 == 0o100000:
                files[name.lstrip("./").lstrip("/")] = data[body:body + size]
            offset = body + size
            offset += -offset % 4
        data = data[offset:]
    return files


def normalise(name):
    return name.replace("-", "_")


def module_name(path):
    return normalise(re.sub(r"\.ko(\.(zst|xz))?$", "", path.rsplit("/", 1)[-1]))


def modinfo(data, key):
    text = decompress(data)
    return sorted({m.decode() for m in re.findall(rb"(?<![a-z_])" + key + rb"=([^\x00]*)", text)})


def check(files, arch):
    """Problems, and notes on what passed, for the installer's files."""
    trees = sorted({p.split("/kernel/")[0] for p in files if "/modules/" in p and "/kernel/" in p})
    if len(trees) != 1:
        return [f"expected one kernel's modules, found {len(trees) or 'none'}"], []
    tree = trees[0]
    modules = {module_name(p): p for p in files
               if p.startswith(tree + "/kernel/") and re.search(r"\.ko(\.(zst|xz))?$", p)}
    builtin = {module_name(line) for line in files.get(tree + "/modules.builtin", b"").decode().split()}
    present = set(modules) | builtin
    problems, notes = [], []

    for line in files.get(tree + "/modules.softdep", b"").decode().splitlines():
        words = line.split()
        if len(words) < 3 or words[0] != "softdep" or normalise(words[1]) not in modules:
            continue
        missing = [normalise(w) for w in words[2:] if not w.endswith(":") and normalise(w) not in present]
        if missing:
            problems.append(f"{words[1]} has a softdep on {', '.join(missing)}, which the installer lacks")

    firmware = {p.split("/firmware/", 1)[1] for p in files if "/lib/firmware/" in p}
    for name in sorted(REQUIRED[arch]):
        if name not in present:
            problems.append(f"{name}: required, and neither a module in the installer nor built in")
            continue
        if name not in modules:
            notes.append(f"{name}: built in")
            continue
        wanted = modinfo(files[modules[name]], b"firmware")
        lacking = [f for f in wanted if not {f, f + ".zst", f + ".xz"} & firmware]
        if lacking and name in OPTIONAL_FIRMWARE:
            notes.append(f"{name}: without {', '.join(lacking)} ({OPTIONAL_FIRMWARE[name]})")
        elif lacking:
            problems.append(f"{name}: firmware {', '.join(lacking)} isn't in the installer")
        else:
            notes.append(f"{name}: present" + (f", with its {len(wanted)} firmware files" if wanted else ""))
    return problems, notes


def main(args):
    if len(args) != 2 or args[1] not in REQUIRED:
        print(__doc__.split("\n\n")[1])
        return 2
    data = initrd(args[0])
    if data is None:
        print(f"{args[0]}: no .initrd section, so not an installer UKI")
        return 1
    problems, notes = check(cpio_files(data), args[1])
    for line in notes:
        print(f"ok: {line}")
    for line in problems:
        print(f"MISSING: {line}")
    if not problems:
        print(f"{args[0]}: every required {args[1]} driver, its soft dependencies and its firmware are in")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
