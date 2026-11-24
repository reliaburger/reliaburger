#!/usr/bin/env python3
"""The appliance can build images (#2144): Buildah and what it needs.

    builder_packages.py config <image dir>
    builder_packages.py manifest <manifest>...

`config` reads <image dir>/mkosi.conf and its .conf.d drop-ins, skipping any
that only apply to a profile, and fails unless their Packages= name Buildah
and each package the image has to name for it (mkosi installs no
Recommends), and RemoveFiles= drops Buildah's test helpers.

`manifest` reads the JSON manifests mkosi writes beside a built image, which
list what was actually installed, dependencies included. It fails unless
each has Buildah, its configuration (containers-common and
containers-image), a network stack, a runtime and gpg, and fails if a
recommend the node never uses came in: GnuPG's agents (libgpgme takes
"gnupg | gpg"), fuse-overlayfs (the node builds with vfs), crun (it runs
RUN steps with runc) or cpp.
"""

import json
import re
import sys
from pathlib import Path

# What mkosi.conf has to name: Buildah, and the recommends it can't build
# without. netavark is containers-common's network stack; apt would
# otherwise pick any provider of container-network-stack.
NAMED = ("buildah", "netavark", "gpg", "runc", "uidmap")
# What a built image has to have: the above, and Buildah's configuration.
INSTALLED = (*NAMED, "golang-github-containers-common", "golang-github-containers-image")
UNWANTED = ("gnupg", "dirmngr", "gpg-agent", "gpgsm", "fuse-overlayfs", "crun", "cpp")
# Ubuntu's package puts Buildah's test helpers here; Buildah never runs them.
TEST_HELPERS = "/usr/libexec/buildah"


def config_files(image):
    if (image / "mkosi.conf").is_file():
        yield image / "mkosi.conf"
    yield from sorted((image / "mkosi.conf.d").glob("*.conf"))


def parse(path):
    """The file's Match profiles and the values of its Content settings.
    Values continue on indented lines, separated by commas or whitespace."""
    profiles, content = [], {}
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
        elif section == "Content":
            content.setdefault(key, []).extend(words)
    return profiles, content


def check_config(image):
    packages, removed = set(), set()
    for path in config_files(Path(image)):
        profiles, content = parse(path)
        if profiles:
            continue
        packages.update(content.get("Packages", []))
        removed.update(r.rstrip("/") for r in content.get("RemoveFiles", []))
    problems = [f"{image}: Packages= doesn't name {p}" for p in NAMED if p not in packages]
    if TEST_HELPERS not in removed:
        problems.append(f"{image}: RemoveFiles= keeps {TEST_HELPERS}, Buildah's test helpers")
    for problem in problems:
        print(problem)
    if not problems:
        print(f"{image}: Buildah and {', '.join(NAMED[1:])}; no test helpers")
    return 1 if problems else 0


def check_manifests(paths):
    failed = False
    for path in paths:
        packages = json.loads(Path(path).read_text()).get("packages")
        if not packages:
            print(f"{path}: no package list")
            failed = True
            continue
        names = {p["name"] for p in packages}
        missing = [p for p in INSTALLED if p not in names]
        unwanted = [p for p in UNWANTED if p in names]
        if missing:
            print(f"{path}: can't build images without {', '.join(missing)}")
        if unwanted:
            print(f"{path}: has {', '.join(unwanted)}, which the node never uses")
        if missing or unwanted:
            failed = True
        else:
            print(f"{path}: Buildah and what it needs, nothing more")
    return 1 if failed else 0


def main(args):
    if args[:1] == ["config"] and len(args) == 2:
        return check_config(args[1])
    if args[:1] == ["manifest"] and len(args) > 1:
        return check_manifests(args[1:])
    print(__doc__.split("\n\n")[1])
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
