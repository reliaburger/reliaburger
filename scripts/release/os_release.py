#!/usr/bin/env python3
"""Sign and publish appliance OS builds (docs/plans/2026-10-01-plan-appliance-product.md, W1).

Subcommands, each run by .github/workflows/appliance.yml:

  sign <dir>...
      Sign every reliaburger-os*.SHA256SUMS under each directory with the
      release key (RELIABURGER_RELEASE_KEY), writing <sums>.sig: the raw
      64-byte Ed25519 signature that the installer and os-stage check with
      `openssl pkeyutl -verify -rawin`.
  channel --version V --out DIR ARCH=SUMS...
      Write os-channel.json, naming the newest version and each
      architecture's SHA256SUMS digest, and its raw signature
      os-channel.json.sig. The SHA256SUMS lists every artefact's digest, so
      the channel vouches for the whole release; bun and relish verify it
      (src/os/channel.rs).
  lab-channel --key KEY.der --version V --out DIR ARCH=SUMS...
      The same channel for a CI lab build, signed with that run's throwaway
      key (DER PKCS#8) instead of the release key, which it never reads. It
      names the run's next version, the one the lab updates to.
  next-version --week YYYY.WW TAG...
      Print the next YYYY.WW.N given the existing os-<version>-<arch> tags.
  unchanged --previous MANIFEST --current MANIFEST [--previous-record F --current-record F]
      Exit 0 when the package lists (name and version) are equal and so are
      the build records (the bun release and the image recipe's git tree),
      so a quiet week publishes nothing.
  prune --keep N TAG...
      Print the os-<version>-<arch> tags of every version beyond the newest
      N, oldest last.

Signing uses package.py's key handling, so the same secret works for both.
"""
import argparse
import base64
import hashlib
import json
import os
import re
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import package  # noqa: E402

VERSION = re.compile(r"(\d{4})\.(\d{2})\.(\d+)")
TAG_PREFIX = "os-"


def sign_file(key, path):
    """Ed25519 over the file's bytes; raw 64-byte signature beside it."""
    signature = package.openssl("pkeyutl", "-sign", "-rawin", "-keyform", "DER",
                                "-inkey", key, "-in", path)
    if len(signature) != 64:
        raise ValueError("unexpected Ed25519 signature length")
    path.with_name(path.name + ".sig").write_bytes(signature)


def sign_sums(directories, key):
    signed = []
    for directory in directories:
        for sums in sorted(Path(directory).glob("reliaburger-os*.SHA256SUMS")):
            sign_file(key, sums)
            signed.append(sums)
    if not signed:
        raise ValueError("no reliaburger-os*.SHA256SUMS to sign")
    return signed


def sha256_file(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def channel_document(version, sums_by_arch):
    """The channel's JSON bytes: canonical (sorted keys, no spaces), so the
    signature covers exactly what bun parses."""
    if not VERSION.fullmatch(version):
        raise ValueError(f"OS version must be YYYY.WW.N, not {version!r}")
    if not sums_by_arch:
        raise ValueError("the channel needs at least one architecture")
    architectures = {}
    for arch, sums in sorted(sums_by_arch.items()):
        if arch not in ("x86_64", "aarch64"):
            raise ValueError(f"unknown architecture {arch!r}")
        name = Path(sums).name
        if name != f"reliaburger-os_{version}.SHA256SUMS":
            raise ValueError(f"{name} is not version {version}'s SHA256SUMS")
        architectures[arch] = {"tag": release_tag(version, arch), "sums": name,
                               "sums_sha256": sha256_file(sums)}
    document = {"schema": 1, "version": version, "architectures": architectures}
    return json.dumps(document, sort_keys=True, separators=(",", ":")).encode() + b"\n"


def release_tag(version, arch):
    """One GitHub release per architecture: both builds name their files the
    same (reliaburger-os_<version>.raw.zst, ...), and a release's assets
    share one namespace."""
    return f"{TAG_PREFIX}{version}-{arch}"


def tag_version(tag):
    """The YYYY.WW.N in os-YYYY.WW.N-<arch>, or None."""
    match = re.fullmatch(r"os-(\d{4}\.\d{2}\.\d+)-(x86_64|aarch64)", tag)
    return match[1] if match else None


def write_channel(version, sums_by_arch, out, key):
    out = Path(out)
    out.mkdir(parents=True, exist_ok=True)
    path = out / "os-channel.json"
    path.write_bytes(channel_document(version, sums_by_arch))
    sign_file(key, path)
    return path


def next_version(week, tags):
    """YYYY.WW.N: one more than the highest N released this week, else 0."""
    if not re.fullmatch(r"\d{4}\.\d{2}", week):
        raise ValueError(f"week must be YYYY.WW, not {week!r}")
    numbers = []
    for tag in tags:
        version = tag_version(tag)
        match = VERSION.fullmatch(version) if version else None
        if match and f"{match[1]}.{match[2]}" == week:
            numbers.append(int(match[3]))
    return f"{week}.{max(numbers) + 1 if numbers else 0}"


def packages(manifest):
    """{name: version} from an mkosi JSON manifest."""
    return {p["name"]: p["version"] for p in manifest.get("packages", [])}


def unchanged(previous, current, previous_record=None, current_record=None):
    """Same packages, and the same build record: a new bun release or a
    change to image/ is a change even when no package moved."""
    return packages(previous) == packages(current) and previous_record == current_record


def version_key(version):
    return tuple(int(part) for part in VERSION.fullmatch(version).groups())


def prune(tags, keep):
    """The os-* release tags to delete: every architecture's release of each
    version older than the newest `keep` versions."""
    versions = sorted({v for v in map(tag_version, tags) if v}, key=version_key, reverse=True)
    old = set(versions[keep:])
    return sorted((t for t in tags if tag_version(t) in old),
                  key=lambda t: (version_key(tag_version(t)), t), reverse=True)


def with_key(action):
    encoded = os.environ.pop("RELIABURGER_RELEASE_KEY", None)
    if not encoded:
        raise ValueError("RELIABURGER_RELEASE_KEY must contain the base64 PKCS#8 release key")
    with tempfile.TemporaryDirectory(prefix="reliaburger-os-signing-") as temporary:
        key = Path(temporary) / "key.der"
        with key.open("xb") as stream:
            os.chmod(key, 0o600)
            stream.write(package.release_key_der(encoded, Path(temporary)))
        root = Path(__file__).resolve().parents[2]
        trusted = re.findall(r'"(ed25519:[A-Za-z0-9+/=]+)"', (root / "src/upgrade/keys.rs").read_text())
        public = package.openssl("pkey", "-inform", "DER", "-in", key, "-pubout", "-outform", "DER")

        if "ed25519:" + base64.b64encode(public[12:]).decode() not in trusted:
            raise ValueError("signing key is not trusted by the release binaries")
        return action(key)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    s = sub.add_parser("sign")
    s.add_argument("directories", nargs="+")
    c = sub.add_parser("channel")
    c.add_argument("--version", required=True)
    c.add_argument("--out", required=True)
    c.add_argument("sums", nargs="+", help="ARCH=path/to/reliaburger-os_V.SHA256SUMS")
    lab = sub.add_parser("lab-channel")
    lab.add_argument("--key", required=True, help="the run's throwaway Ed25519 key, DER PKCS#8")
    lab.add_argument("--version", required=True)
    lab.add_argument("--out", required=True)
    lab.add_argument("sums", nargs="+", help="ARCH=path/to/reliaburger-os_V.SHA256SUMS")
    n = sub.add_parser("next-version")
    n.add_argument("--week", required=True)
    n.add_argument("tags", nargs="*")
    u = sub.add_parser("unchanged")
    u.add_argument("--previous", required=True)
    u.add_argument("--current", required=True)
    u.add_argument("--previous-record")
    u.add_argument("--current-record")
    p = sub.add_parser("prune")
    p.add_argument("--keep", type=int, required=True)
    p.add_argument("tags", nargs="*")
    args = parser.parse_args(argv)

    if args.command == "sign":
        for path in with_key(lambda key: sign_sums(args.directories, key)):
            print(f"signed {path}")
    elif args.command == "channel":
        sums = dict(item.split("=", 1) for item in args.sums)
        print(with_key(lambda key: write_channel(args.version, sums, args.out, key)))
    elif args.command == "lab-channel":
        key = Path(args.key)
        if not key.is_file():
            raise FileNotFoundError(f"no key at {key}")
        sums = dict(item.split("=", 1) for item in args.sums)
        print(write_channel(args.version, sums, args.out, key))
    elif args.command == "next-version":
        print(next_version(args.week, args.tags))
    elif args.command == "unchanged":
        def record(path):
            return Path(path).read_text().strip() if path and Path(path).is_file() else None
        same = unchanged(json.loads(Path(args.previous).read_text()), json.loads(Path(args.current).read_text()),
                         record(args.previous_record), record(args.current_record))
        print("unchanged" if same else "changed")
        return 0 if same else 1
    elif args.command == "prune":
        for tag in prune(args.tags, args.keep):
            print(tag)
    return 0


if __name__ == "__main__":
    sys.exit(main())
