#!/usr/bin/env python3
"""Record and verify an exact release candidate; never compile or sign at promotion."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess

from package import GUEST_METADATA, PLATFORMS, sha256

RECORD = "candidate.json"
PDFS = {"building-reliaburger.pdf", "reliaburger-design-docs.pdf",
        "reliaburger-roadmap.pdf", "reliaburger-whitepaper.pdf"}


def candidate_names(pins):
    names = {f"{binary}-{platform}" for binary, platforms in PLATFORMS.items() for platform in platforms}
    return names | {name + ".sig" for name in names} | PDFS | {
        "metadata.json", "cli-metadata.json", "SHA256SUMS", "install.sh", GUEST_METADATA,
    } | {image["asset"] for image in pins["images"].values()}


def identity(version, repository, commit, run_id, run_attempt):
    if not re.fullmatch(r"v\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?", version):
        raise ValueError("candidate version must be a release tag")
    if not re.fullmatch(r"[A-Za-z0-9_-][A-Za-z0-9_.-]*/[A-Za-z0-9_-][A-Za-z0-9_.-]*", repository):
        raise ValueError("invalid candidate repository")
    if not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise ValueError("candidate commit must be a full Git SHA")
    if any(type(number) is not int or number < 1 for number in (run_id, run_attempt)):
        raise ValueError("candidate run and attempt must be positive integers")
    return dict(version=version, repository=repository, commit=commit,
                run_id=run_id, run_attempt=run_attempt)


def check_guest_images(directory, pins, assets):
    """Every built image must be the one its signed metadata names, built from the pinned source."""
    images = json.loads((directory / GUEST_METADATA).read_text()).get("images", {})
    if set(images) != set(pins["images"]):
        raise ValueError("guest image metadata must cover exactly the pinned architectures")
    for arch, pin in pins["images"].items():
        image = images[arch]
        if (image.get("asset") != pin["asset"]
                or image.get("sha256") != assets[pin["asset"]]["sha256"]
                or image.get("source", {}).get("sha256") != pin["source"]["sha256"]):
            raise ValueError("candidate guest image does not match its metadata or its pinned source")


def inventory(directory, pins, recorded=False):
    expected = candidate_names(pins) | ({RECORD} if recorded else set())
    paths = list(directory.iterdir())
    if {path.name for path in paths} != expected:
        raise ValueError("candidate asset inventory does not match the complete release matrix")
    if any(path.is_symlink() or not path.is_file() or path.stat().st_size == 0 for path in paths):
        raise ValueError("candidate assets must be non-empty regular files")
    assets = {path.name: {"sha256": sha256(path), "size": path.stat().st_size}
              for path in sorted(paths) if path.name != RECORD}
    check_guest_images(directory, pins, assets)
    return assets


def record_candidate(directory, pins, **expected):
    expected = identity(**expected)
    path = directory / RECORD
    if path.exists() or path.is_symlink():
        raise FileExistsError("candidate record already exists")
    document = dict(schema=1, **expected, assets=inventory(directory, pins))
    encoded = (json.dumps(document, indent=2, sort_keys=True) + "\n").encode()
    with path.open("xb") as output:
        output.write(encoded)
        output.flush()
        os.fsync(output.fileno())
    return hashlib.sha256(encoded).hexdigest()


def verify_candidate(directory, pins, qualified_digest, **expected):
    expected = identity(**expected)
    if not re.fullmatch(r"[0-9a-f]{64}", qualified_digest):
        raise ValueError("qualification must supply the candidate record's SHA-256")
    path = directory / RECORD
    if path.is_symlink() or not path.is_file() or sha256(path) != qualified_digest:
        raise ValueError("candidate record differs from the qualified digest")
    document = json.loads(path.read_text())
    if document != dict(schema=1, **expected, assets=inventory(directory, pins, recorded=True)):
        raise ValueError("candidate identity or bytes differ from qualification")
    return document


def verify_run(run, repository, commit, run_id):
    """Only a successful main-branch manual build can supply a candidate."""
    expected = dict(id=run_id, head_sha=commit, head_branch="main", event="workflow_dispatch",
                    path=".github/workflows/build.yml", status="completed", conclusion="success")
    if any(run.get(key) != value for key, value in expected.items()):
        raise ValueError("candidate run is not a successful manual main build of this tag's commit")
    if any(run.get(key, {}).get("full_name") != repository for key in ("repository", "head_repository")):
        raise ValueError("candidate run belongs to another repository")
    if type(run.get("run_attempt")) is not int or run["run_attempt"] < 1:
        raise ValueError("candidate run attempt is invalid")


def verify_uploaded_assets(release, document, qualified_digest, record_size):
    """Require GitHub's stored asset hashes before making the draft public."""
    if release.get("tag_name") != document["version"] or release.get("draft") is not True:
        raise ValueError("publication requires the exact version's unpublished draft")
    expected = dict(document["assets"], **{RECORD: {"sha256": qualified_digest, "size": record_size}})
    assets = release.get("assets", [])
    if len(assets) != len(expected) or {asset["name"] for asset in assets} != set(expected):
        raise ValueError("uploaded release asset inventory differs from qualification")
    for asset in assets:
        details = expected[asset["name"]]
        if (asset.get("state") != "uploaded" or asset.get("size") != details["size"]
                or asset.get("digest") != "sha256:" + details["sha256"]):
            raise ValueError("uploaded release asset bytes differ from qualification")


def gh_json(*arguments):
    result = subprocess.run(["gh", "api", *arguments], check=True, capture_output=True, text=True)
    return json.loads(result.stdout)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", choices=["record", "verify", "fetch", "verify-upload"])
    parser.add_argument("--directory", type=Path, default=Path("dist"))
    parser.add_argument("--version", required=True)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--run-id", type=int, required=True)
    parser.add_argument("--run-attempt", type=int, default=1)
    parser.add_argument("--qualified-digest")
    args = parser.parse_args()
    expected = identity(args.version, args.repository, args.commit, args.run_id, args.run_attempt)
    pins = json.loads(Path(__file__).with_name("guest-images.json").read_text())
    if args.operation == "record":
        print(record_candidate(args.directory, pins, **expected))
        return

    if args.operation == "verify":
        verify_candidate(args.directory, pins, args.qualified_digest or "", **expected)
        print(f"verified local candidate {args.commit}")
        return

    # Resolve the remote tag again immediately before publication. No tag is created.
    commit = gh_json(f"repos/{args.repository}/commits/{args.version}")["sha"]
    if commit != args.commit:
        raise ValueError("release tag no longer points to the qualified candidate commit")
    run = gh_json(f"repos/{args.repository}/actions/runs/{args.run_id}")
    verify_run(run, args.repository, args.commit, args.run_id)
    expected["run_attempt"] = run["run_attempt"]
    if args.operation == "fetch":
        if args.directory.exists():
            raise FileExistsError("candidate download directory must be new")
        subprocess.run(["gh", "run", "download", str(args.run_id), "--repo", args.repository,
                        "--name", f"candidate-{args.commit}-{run['run_attempt']}",
                        "--dir", str(args.directory)], check=True)
    document = verify_candidate(args.directory, pins, args.qualified_digest or "", **expected)
    if args.operation == "verify-upload":
        result = subprocess.run(["gh", "release", "view", args.version, "--repo", args.repository,
                                 "--json", "databaseId"], check=True, capture_output=True, text=True)
        release_id = json.loads(result.stdout)["databaseId"]
        release = gh_json(f"repos/{args.repository}/releases/{release_id}")
        verify_uploaded_assets(release, document, args.qualified_digest,
                               (args.directory / RECORD).stat().st_size)
    print(f"verified candidate {args.commit} from run {args.run_id}, attempt {run['run_attempt']}")


if __name__ == "__main__":
    main()
