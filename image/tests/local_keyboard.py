#!/usr/bin/env python3
"""The appliance's keyboard reaches only its status screen (#2279).

    local_keyboard.py config <image dir>
    local_keyboard.py usr <usr tree>

tty1 is reliaburger-console.service, the node's status screen. Everything
else the keyboard could do is off, or deliberately limited:

- no gettys on tty2-tty6: logind's NAutoVTs=0 and ReserveVT=0, and Ubuntu's
  getty-static.service masked, and nothing enables a getty or an autovt on
  any VT but tty1;
- Ctrl-Alt-Del does nothing: ctrl-alt-del.target masked, and PID 1's
  CtrlAltDelBurstAction=none, so seven quick presses don't force a reboot;
- magic SysRq is 176: sync, remount read-only and reboot only.

`config` reads <image dir>/mkosi.extra, the files the image adds. `usr`
reads a built image's /usr partition, extracted (fsck.erofs --extract), with
the image's /etc from its factory copy in share/factory/etc
(mkosi.finalize). Each settles every value the way systemd would, across
every file the image has, so a package that ships a later sysctl.d file or
drop-in fails it.
"""

import re
import sys
from pathlib import Path

# 16 sync, 32 remount read-only, 128 reboot and power off.
SYSRQ = "176"
MASKED = ("ctrl-alt-del.target", "getty-static.service")
CONSOLE = "reliaburger-console.service"
VT_UNIT = re.compile(r"^(?:getty|autovt)@tty(\d+)\.service$")


class Tree:
    """An image's /usr and /etc, wherever they are on this machine."""

    def __init__(self, usr, etc):
        self.usr, self.etc = Path(usr), Path(etc)

    def path(self, absolute):
        """Where an absolute path in the image is here, or None outside
        /usr and /etc."""
        for prefix, base in (("/usr/", self.usr), ("/etc/", self.etc)):
            if absolute.startswith(prefix):
                return base / absolute[len(prefix):]
        return None

    def read(self, path):
        """A file's text, following a symlink inside the image the way the
        image would; None if it doesn't resolve."""
        for _ in range(8):
            if not path.is_symlink():
                return path.read_text() if path.is_file() else None
            target = str(path.readlink())
            if target.startswith("/"):
                path = self.path(target)
                if path is None:
                    return None
            else:
                path = path.parent / target
        return None


def conf_d(tree, main, dropins):
    """systemd's order for a configuration file and its drop-ins: the main
    file (/etc's over /usr's), then every drop-in by file name, /etc's
    winning a name /usr also has."""
    files = []
    for base in (tree.etc / main, tree.usr / "lib" / main):
        if base.exists() or base.is_symlink():
            files.append(base)
            break
    named = {}
    for base in (tree.usr / "lib" / dropins, tree.usr / "local" / "lib" / dropins, tree.etc / dropins):
        if base.is_dir():
            for f in base.glob("*.conf"):
                named[f.name] = f
    return files + [named[n] for n in sorted(named)]


def setting(tree, files, section, key):
    """The value systemd ends up with: the last assignment wins."""
    value, where = None, None
    for path in files:
        current = None
        for line in (tree.read(path) or "").splitlines():
            line = line.strip()
            if not line or line[0] in "#;":
                continue
            if line.startswith("[") and line.endswith("]"):
                current = line[1:-1]
            elif current == section and "=" in line:
                k, v = (p.strip() for p in line.split("=", 1))
                if k == key:
                    value, where = v, path
    return value, where


def sysctl(tree, key):
    """systemd-sysctl's value for key: every sysctl.d file by name, /etc's
    winning a name /usr also has, the last assignment winning."""
    named = {}
    for base in (tree.usr / "lib" / "sysctl.d", tree.usr / "local" / "lib" / "sysctl.d", tree.etc / "sysctl.d"):
        if base.is_dir():
            for f in base.iterdir():
                if f.name.endswith(".conf"):
                    named[f.name] = f
    value, where = None, None
    for name in sorted(named):
        for line in (tree.read(named[name]) or "").splitlines():
            line = line.strip().lstrip("-")
            if not line or line[0] in "#;" or "=" not in line:
                continue
            k, v = (p.strip() for p in line.split("=", 1))
            if k.replace("/", ".") == key:
                value, where = v, named[name]
    return value, where


def vt_units_wanted(tree):
    """Every getty or autovt instance something enables, as paths."""
    found = []
    for base in (tree.etc / "systemd" / "system", tree.usr / "lib" / "systemd" / "system"):
        for wants in base.glob("*.wants"):
            found += [p for p in wants.iterdir() if VT_UNIT.match(p.name)]
    return found


def check(tree, built):
    problems = []
    logind = conf_d(tree, "systemd/logind.conf", "systemd/logind.conf.d")
    for key in ("NAutoVTs", "ReserveVT"):
        value, _ = setting(tree, logind, "Login", key)
        if value != "0":
            problems.append(f"logind's {key} is {value or 'the default'}, not 0: tty2-tty6 get gettys")
    system = conf_d(tree, "systemd/system.conf", "systemd/system.conf.d")
    value, _ = setting(tree, system, "Manager", "CtrlAltDelBurstAction")
    if value != "none":
        problems.append(f"CtrlAltDelBurstAction is {value or 'the default'}, not none: "
                        "seven quick Ctrl-Alt-Dels force a reboot")
    for unit in MASKED:
        mask = tree.etc / "systemd" / "system" / unit
        if not (mask.is_symlink() and str(mask.readlink()) == "/dev/null"):
            problems.append(f"{mask}: not masked (a symlink to /dev/null)")
    value, where = sysctl(tree, "kernel.sysrq")
    if value != SYSRQ:
        problems.append(f"kernel.sysrq is {value or 'the kernel default'}"
                        f"{f' ({where})' if where else ''}, not {SYSRQ}")
    for path in vt_units_wanted(tree):
        if VT_UNIT.match(path.name).group(1) != "1":
            problems.append(f"{path}: enables a login on a VT other than tty1")
    if not (tree.usr / "lib" / "systemd" / "system" / CONSOLE).is_file():
        problems.append(f"no {CONSOLE}: tty1 would have no status screen")
    if built and not (tree.etc / "systemd" / "system" / "multi-user.target.wants" / CONSOLE).is_symlink():
        problems.append(f"{CONSOLE} isn't enabled")
    return problems


def main(args):
    if len(args) == 2 and args[0] == "config":
        extra = Path(args[1]) / "mkosi.extra"
        tree, built = Tree(extra / "usr", extra / "etc"), False
    elif len(args) == 2 and args[0] == "usr":
        tree, built = Tree(args[1], Path(args[1]) / "share" / "factory" / "etc"), True
    else:
        print(__doc__.split("\n\n")[1])
        return 2
    problems = check(tree, built)
    for problem in problems:
        print(problem)
    if not problems:
        print(f"{args[1]}: no gettys on tty2-tty6, Ctrl-Alt-Del masked, kernel.sysrq = {SYSRQ}, "
              f"{CONSOLE} on tty1")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
