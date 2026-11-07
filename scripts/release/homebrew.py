#!/usr/bin/env python3
"""Write the Homebrew bottles and formula for a published release, and push it to the tap (#1583).

    homebrew.py bottles --record candidate.json --qualified-digest SHA256 \\
        --binaries published/ --output bottles/
    homebrew.py formula --record candidate.json --qualified-digest SHA256 \\
        --bottles bottles/bottles.json --output relish.rb
    homebrew.py publish --formula relish.rb --version v0.2.7 --tap reliaburger/homebrew-tap

`bottles` packs each published relish binary, after checking its SHA-256
against the record, into a Homebrew bottle: a tarball in the keg layout
Homebrew pours into its Cellar. Nothing is compiled. Without a bottle,
Homebrew treats `bin.install` as a build from source and refuses to run
until the Command Line Tools are current, though there is nothing to build.
It writes `bottles.json`, each tag's bottle SHA-256, for `formula --bottles`.

`formula` fills `relish.rb.in` from a release's `candidate.json`: the same
record promotion verified, so the formula pins exactly the bytes that were
qualified and published. It never downloads a binary or hashes one itself.
Run it locally against any candidate's record as a dry run.

`publish` commits the formula to the tap as `Formula/relish.rb` and pushes.
It authenticates with `HOMEBREW_TAP_TOKEN`; without one it says so and
succeeds, so promotion works before the maintainer has made the tap.
"""
import argparse
import base64
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tarfile
import tempfile

from package import PLATFORMS

TEMPLATE = Path(__file__).with_name("relish.rb.in")
FORMULA_PATH = "Formula/relish.rb"
TOKEN = "HOMEBREW_TAP_TOKEN"
TAP = "reliaburger/homebrew-tap"

# The Homebrew bottle tags each published binary serves. One macOS binary
# runs on every listed release (it targets macOS 11), so each tag gets the
# same bytes; Homebrew on a newer macOS than the newest tag falls back to it.
BOTTLE_TAGS = {
    "macos-aarch64": ("arm64_golden_gate", "arm64_tahoe", "arm64_sequoia", "arm64_sonoma"),
    "linux-aarch64": ("arm64_linux",),
    "linux-x86_64": ("x86_64_linux",),
}


def placeholder(platform):
    """`macos-aarch64` becomes `@MACOS_AARCH64@`."""
    return "@" + platform.upper().replace("-", "_") + "@"


def release(record, qualified_digest=None):
    """The checked candidate.json document, and its tag, for a release that may reach the tap."""
    if qualified_digest is not None and hashlib.sha256(record).hexdigest() != qualified_digest:
        raise ValueError("candidate record differs from the qualified digest")
    document = json.loads(record)
    if document.get("channel", "release") != "release":
        raise ValueError("a nightly build never reaches the tap")
    tag = document["version"]
    if not re.fullmatch(r"v\d+\.\d+\.\d+", tag):
        raise ValueError(f"only a release tag reaches the tap, not {tag!r}")
    return document, tag


def bottle_name(version, tag):
    """The file name Homebrew asks a release download `root_url` for: one dash, as
    `Bottle::Filename#url_encode` writes it. Only GitHub Packages uses two."""
    return f"relish-{version}.{tag}.bottle.tar.gz"


def bottle(binary, version, platform):
    """A reproducible bottle: `relish/<version>/bin/relish` plus an install receipt."""
    receipt = {
        "homebrew_version": "reliaburger-homebrew.py",
        "used_options": [],
        "unused_options": [],
        "built_as_bottle": True,
        "poured_from_bottle": False,
        "installed_on_request": False,
        # Nothing in the binary names the Cellar, so nothing needs relocating.
        "changed_files": [],
        "time": None,
        "aliases": [],
        "runtime_dependencies": [],
        "source": {"spec": "stable", "versions": {"stable": version, "head": None, "version_scheme": 0}},
        "arch": "arm64" if platform.endswith("aarch64") else "x86_64",
        "built_on": {"os": "Macintosh" if platform.startswith("macos") else "Linux"},
    }
    files = [
        (f"relish/{version}/INSTALL_RECEIPT.json", 0o644,
         (json.dumps(receipt, indent=2, sort_keys=True) + "\n").encode()),
        (f"relish/{version}/bin/relish", 0o755, binary),
    ]
    archive = io.BytesIO()
    # Fixed owners, times and gzip header make the bytes depend only on the
    # binary, so a bottle can be rebuilt and compared.
    with gzip.GzipFile(fileobj=archive, mode="wb", mtime=0, filename="") as compressed:
        with tarfile.open(fileobj=compressed, mode="w", format=tarfile.PAX_FORMAT) as tar:
            for directory in ("relish", f"relish/{version}", f"relish/{version}/bin"):
                info = tarfile.TarInfo(directory)
                info.type, info.mode = tarfile.DIRTYPE, 0o755
                tar.addfile(info)
            for name, mode, data in files:
                info = tarfile.TarInfo(name)
                info.size, info.mode = len(data), mode
                tar.addfile(info, io.BytesIO(data))
    return archive.getvalue()


def bottles(record, binaries, output, qualified_digest=None):
    """Write a bottle per tag from the verified binaries in `binaries`; return each tag's SHA-256."""
    document, tag = release(record, qualified_digest)
    version = tag[1:]
    output = Path(output)
    output.mkdir(parents=True, exist_ok=True)
    digests = {}
    for platform in PLATFORMS["relish"]:
        name = f"relish-{platform}"
        asset = document["assets"].get(name)
        path = Path(binaries) / name
        if asset is None or not path.is_file():
            raise ValueError(f"{name} is missing from the record or from {binaries}")
        binary = path.read_bytes()
        if hashlib.sha256(binary).hexdigest() != asset["sha256"]:
            raise ValueError(f"{name} differs from the SHA-256 the record publishes")
        data = bottle(binary, version, platform)
        for bottle_tag in BOTTLE_TAGS[platform]:
            (output / bottle_name(version, bottle_tag)).write_bytes(data)
            digests[bottle_tag] = hashlib.sha256(data).hexdigest()
    return digests


def bottle_block(root_url, digests):
    lines = ["", "  bottle do", f'    root_url "{root_url}"']
    lines += [f'    sha256 cellar: :any_skip_relocation, {tag}: "{digest}"' for tag, digest in digests.items()]
    return "\n".join(lines + ["  end", ""])


def formula(record, qualified_digest=None, bottles=None, root_url=None):
    """The formula text for the release `record` (candidate.json's bytes) describes.

    With `bottles` (tag to SHA-256), it names a bottle per tag, fetched from
    `root_url`: by default the tap's release for the tag, which `publish
    --bottles` makes, so the published reliaburger release never changes.
    """
    document, tag = release(record, qualified_digest)
    text = TEMPLATE.read_text()
    if root_url is None:
        root_url = f"https://github.com/{TAP}/releases/download/{tag}"
    text = text.replace("@BOTTLE@\n", bottle_block(root_url, bottles) if bottles else "")
    text = (text.replace("@TAG@", tag).replace("@VERSION@", tag[1:])
            .replace("@REPOSITORY@", document["repository"]))
    for platform in PLATFORMS["relish"]:
        asset = document["assets"].get(f"relish-{platform}")
        if asset is None:
            raise ValueError(f"the record has no relish-{platform}")
        if placeholder(platform) not in text:
            raise ValueError(f"relish.rb.in has no {placeholder(platform)}; add relish-{platform} to it")
        text = text.replace(placeholder(platform), asset["sha256"])
    leftover = re.findall(r"@[A-Z0-9_]+@", text)
    if leftover:
        raise ValueError(f"relish.rb.in names platforms no release builds: {sorted(set(leftover))}")
    return text


def git(*args, cwd=None, token=None):
    """Run git; with a token, send it as a header so it never lands in .git/config."""
    command = ["git"]
    if token:
        basic = base64.b64encode(f"x-access-token:{token}".encode()).decode()
        command += ["-c", f"http.https://github.com/.extraheader=AUTHORIZATION: basic {basic}"]
    subprocess.run(command + list(args), cwd=cwd, check=True)


def upload_bottles(directory, version, tap, token):
    """Attach every bottle in `directory` to the tap's release `version`, making it if needed."""
    files = sorted(str(path) for path in Path(directory).glob("*.bottle.tar.gz"))
    if not files:
        raise ValueError(f"{directory} holds no bottles")
    environment = {**os.environ, "GH_TOKEN": token}
    exists = subprocess.run(["gh", "release", "view", version, "--repo", tap],
                            env=environment, capture_output=True).returncode == 0
    if exists:
        command = ["gh", "release", "upload", version, "--repo", tap, "--clobber", *files]
    else:
        command = ["gh", "release", "create", version, "--repo", tap, "--title", f"relish {version}",
                   "--notes", f"Homebrew bottles of the published relish {version}.", *files]
    subprocess.run(command, env=environment, check=True)
    print(f"uploaded {len(files)} bottles to {tap} {version}")


def publish(text, version, remote, token=None, bottles=None, tap=TAP):
    """Commit `text` as the tap's formula and push it. Returns False when it was already there.

    With `bottles`, a directory `bottles` wrote, they go to the tap's release
    first, so the formula never names a bottle Homebrew can't download.
    """
    if bottles is not None:
        upload_bottles(bottles, version, tap, token)
    with tempfile.TemporaryDirectory(prefix="reliaburger-tap-") as scratch:
        checkout = Path(scratch) / "tap"
        git("clone", "--quiet", "--depth", "1", remote, str(checkout), token=token)
        path = checkout / FORMULA_PATH
        if path.exists() and path.read_text() == text:
            print(f"{FORMULA_PATH} already describes relish {version}")
            return False
        path.parent.mkdir(exist_ok=True)
        path.write_text(text)
        git("add", FORMULA_PATH, cwd=checkout)
        git("-c", "user.name=github-actions[bot]",
            "-c", "user.email=41898282+github-actions[bot]@users.noreply.github.com",
            "commit", "--quiet", "-m", f"relish {version.removeprefix('v')}", cwd=checkout)
        git("push", "--quiet", "origin", "HEAD", cwd=checkout, token=token)
        print(f"pushed relish {version} to {remote}")
        return True


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    operations = parser.add_subparsers(dest="operation", required=True)
    pack = operations.add_parser("bottles", help="pack each verified published binary as a bottle")
    pack.add_argument("--record", type=Path, required=True)
    pack.add_argument("--qualified-digest", help="refuse a record whose SHA-256 differs")
    pack.add_argument("--binaries", type=Path, required=True, help="the downloaded relish-<platform> files")
    pack.add_argument("--output", type=Path, required=True)
    write = operations.add_parser("formula", help="write the formula from a release's candidate.json")
    write.add_argument("--record", type=Path, required=True)
    write.add_argument("--qualified-digest", help="refuse a record whose SHA-256 differs")
    write.add_argument("--bottles", type=Path, help="the bottles.json that `bottles` wrote")
    write.add_argument("--root-url", help="where Homebrew fetches bottles; default: the release")
    write.add_argument("--output", type=Path, help="default: standard output")
    push = operations.add_parser("publish", help="commit the formula to the tap and push it")
    push.add_argument("--formula", type=Path, required=True)
    push.add_argument("--version", required=True)
    push.add_argument("--tap", default=TAP, help="the tap's GitHub repository")
    push.add_argument("--remote", help="clone and push here instead of the tap on GitHub")
    push.add_argument("--bottles", type=Path, help="first attach these bottles to the tap's release")
    args = parser.parse_args(argv)

    if args.operation == "bottles":
        digests = bottles(args.record.read_bytes(), args.binaries, args.output, args.qualified_digest)
        (args.output / "bottles.json").write_text(json.dumps(digests, indent=2) + "\n")
        return

    if args.operation == "formula":
        digests = json.loads(args.bottles.read_text()) if args.bottles else None
        text = formula(args.record.read_bytes(), args.qualified_digest, digests, args.root_url)
        if args.output:
            args.output.write_text(text)
        else:
            sys.stdout.write(text)
        return

    token = os.environ.get(TOKEN)
    remote = args.remote or f"https://github.com/{args.tap}.git"
    if not token and not args.remote:
        print(f"::notice title=Homebrew tap not updated::{TOKEN} is not set, so {args.tap} keeps "
              f"its formula. See docs/releasing.md#the-homebrew-tap.")
        return
    publish(args.formula.read_text(), args.version, remote, token, args.bottles, args.tap)


if __name__ == "__main__":
    main()
