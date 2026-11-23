#!/usr/bin/env python3
"""A published image has no SSH server (#401): bun is the only way in.

    no_ssh_server.py config <image dir>
    no_ssh_server.py manifest [--lab] <manifest>...

`config` reads the mkosi configs under <image dir> (mkosi.conf, its .conf.d
drop-ins, and each subimage's, such as the netboot installer) and fails if an
SSH server is in a Packages= anywhere but a file that only applies to the
`lab` profile ([Match] Profiles=lab).

`manifest` reads the JSON manifests mkosi writes beside a built image, which
list what was actually installed, dependencies included, and fails if one
has an SSH server. With --lab it checks the other way: the lab image must
have openssh-server, so a lab build proves the check sees one.
"""

import json
import re
import sys
from pathlib import Path

SSH_SERVERS = re.compile(r"^(openssh-server|dropbear(-bin|-run)?|tinysshd|lsh-server)$")


def config_files(image):
    """Every mkosi config file of the image and its subimages."""
    roots = [image, *sorted(p for p in (image / "mkosi.images").glob("*") if p.is_dir())]
    for root in roots:
        if (root / "mkosi.conf").is_file():
            yield root / "mkosi.conf"
        yield from sorted((root / "mkosi.conf.d").glob("*.conf"))


def parse(path):
    """The file's Match profiles and the packages its Packages= settings
    name. Values continue on indented lines, separated by commas or
    whitespace."""
    profiles, packages = [], []
    section = key = None
    for raw in path.read_text().splitlines():
        line = raw.split("#", 1)[0].rstrip()
        if not line.strip():
            continue
        if line.startswith("[") and line.endswith("]"):
            section, key = line[1:-1], None
            continue
        if raw[:1].isspace() and key is not None:
            value = line
        elif "=" in line:
            key, value = (part.strip() for part in line.split("=", 1))
        else:
            continue
        words = [w for w in re.split(r"[,\s]+", value) if w]
        if section == "Match" and key == "Profiles":
            profiles += words
        elif section == "Content" and key == "Packages":
            packages += words
    return profiles, packages


def check_config(image):
    found = []
    for path in config_files(Path(image)):
        profiles, packages = parse(path)
        if profiles and set(profiles) == {"lab"}:
            continue
        found += [(path, p) for p in packages if SSH_SERVERS.match(p)]
    for path, package in found:
        print(f"{path}: {package} would be in a published image; only the lab profile may have it")
    if not found:
        print(f"{image}: SSH servers only in the lab profile")
    return 1 if found else 0


def check_manifests(paths, lab):
    failed = False
    for path in paths:
        packages = json.loads(Path(path).read_text()).get("packages")
        if not packages:
            print(f"{path}: no package list")
            failed = True
            continue
        names = {p["name"] for p in packages}
        servers = sorted(n for n in names if SSH_SERVERS.match(n))
        if lab and "openssh-server" not in names:
            print(f"{path}: the lab image has no openssh-server, so this check can't see one")
            failed = True
        elif not lab and servers:
            print(f"{path}: a published image with an SSH server: {', '.join(servers)}")
            failed = True
        else:
            print(f"{path}: {'openssh-server, as the lab profile adds' if lab else 'no SSH server'}")
    return 1 if failed else 0


def main(args):
    if args[:1] == ["config"] and len(args) == 2:
        return check_config(args[1])
    if args[:1] == ["manifest"]:
        lab = "--lab" in args[1:]
        paths = [a for a in args[1:] if a != "--lab"]
        if paths:
            return check_manifests(paths, lab)
    print(__doc__.split("\n\n")[1])
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
