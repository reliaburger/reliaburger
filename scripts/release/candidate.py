#!/usr/bin/env python3
"""Record, verify, stage and promote an exact release candidate; never compile or sign."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys

from package import GUEST_METADATA, PLATFORMS, sha256

RECORD = "candidate.json"
# Staging tags start with this, so nothing that expects a `v1.2.3` release tag
# (promotion, `v*` tag rules, version parsers) can mistake one for a release.
STAGING_PREFIX = "staging-"
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


def staging_tag(version, run_id, run_attempt):
    """The pre-release tag a candidate is staged under: never a release tag."""
    identity(version, "reliaburger/reliaburger", "0" * 40, run_id, run_attempt)
    return f"{STAGING_PREFIX}{version}-{run_id}-{run_attempt}"


def staging_step(release, tag):
    """What a staging run does next, given the release already under `tag` (or None).

    Re-running stage.yml for the same candidate attempt resumes an unfinished
    draft or re-verifies the published pre-release; it never replaces one.
    """
    if release is None:
        return "create"
    if release.get("tag_name") != tag or release.get("prerelease") is not True:
        raise ValueError(f"{tag} exists but is not this candidate's staging pre-release")
    return "resume" if release.get("draft") is True else "verify"


def verify_uploaded_assets(release, document, qualified_digest, record_size,
                           tag=None, draft=True, prerelease=False):
    """Require GitHub's stored asset hashes before making the draft public.

    Promotion checks the version's own unpublished, final draft; staging passes
    its staging tag and checks a pre-release, before and after publishing it.
    """
    if (release.get("tag_name") != (tag or document["version"])
            or release.get("draft") is not draft
            or release.get("prerelease") is not prerelease):
        raise ValueError("release tag, draft or pre-release state differs from what publication expects")
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


def gh(*arguments):
    # stdout stays clean for the one line callers capture (the staged URL).
    subprocess.run(["gh", *arguments], check=True, stdout=sys.stderr)


def candidate_run(repository, run_id, commit=None):
    """Fetch a build run and require a successful manual main build of `commit`.

    Staging has no tag yet, so it takes the commit from the run itself; the
    run still has to be a manual main build, and the recorded digest still
    has to match every byte.
    """
    run = gh_json(f"repos/{repository}/actions/runs/{run_id}")
    verify_run(run, repository, run.get("head_sha") if commit is None else commit, run_id)
    return run


def download_candidate(directory, repository, run):
    if directory.exists():
        raise FileExistsError("candidate download directory must be new")
    gh("run", "download", str(run["id"]), "--repo", repository,
       "--name", f"candidate-{run['head_sha']}-{run['run_attempt']}", "--dir", str(directory))


def verify_downloaded(directory, pins, repository, run, qualified_digest, version=None):
    """Verify a downloaded candidate against its run and the qualified digest.

    Without a tag, the version comes from the record, which is safe only
    because the record itself must hash to the operator's qualified digest.
    """
    if version is None:
        path = directory / RECORD
        if path.is_symlink() or not path.is_file() or sha256(path) != qualified_digest:
            raise ValueError("candidate record differs from the qualified digest")
        version = json.loads(path.read_text()).get("version", "")
    return verify_candidate(directory, pins, qualified_digest, version=version,
                            repository=repository, commit=run["head_sha"],
                            run_id=run["id"], run_attempt=run["run_attempt"])


def find_release(repository, tag):
    """The release under `tag`, drafts included, or None when there isn't one."""
    result = subprocess.run(["gh", "release", "view", tag, "--repo", repository,
                             "--json", "databaseId"], capture_output=True, text=True)
    if result.returncode:
        if "not found" in result.stderr.lower():
            return None
        raise RuntimeError(f"could not look up release {tag}: {result.stderr.strip()}")
    return gh_json(f"repos/{repository}/releases/{json.loads(result.stdout)['databaseId']}")


def staging_notes(document, tag, qualified_digest):
    base = f"https://github.com/{document['repository']}/releases/download/{tag}"
    return (
        f"**Staging candidate, not a release.** These are the unchanged signed "
        f"{document['version']} candidate files from build run {document['run_id']} "
        f"(attempt {document['run_attempt']}) at commit {document['commit']}, staged "
        f"so the real installer can be qualified before promotion. Every version "
        f"string inside them still says {document['version']}.\n\n"
        f"`candidate.json` SHA-256: `{qualified_digest}`\n\n"
        f"Qualification only:\n\n"
        f"```sh\ncurl -fsSL https://reliaburger.com/install.sh | "
        f"RELIABURGER_RELEASE_BASE_URL={base} sh\n```\n"
    )


def stage(directory, document, qualified_digest):
    """Publish a verified candidate as a pre-release under its staging tag, or
    re-verify the one an earlier run published. Never marks anything latest."""
    tag = staging_tag(document["version"], document["run_id"], document["run_attempt"])
    repository = document["repository"]
    record_size = (directory / RECORD).stat().st_size
    step = staging_step(find_release(repository, tag), tag)
    if step == "resume":
        # An unpublished draft from an interrupted run: nobody can have
        # installed from it, so start it again rather than patch it up.
        gh("release", "delete", tag, "--repo", repository, "--yes")
    if step in ("create", "resume"):
        gh("release", "create", tag, "--repo", repository, "--target", document["commit"],
           "--draft", "--prerelease", "--latest=false",
           "--title", f"Staging candidate {document['version']} (run {document['run_id']}, "
                      f"attempt {document['run_attempt']})",
           "--notes", staging_notes(document, tag, qualified_digest),
           *sorted(str(path) for path in directory.iterdir()))
        verify_uploaded_assets(find_release(repository, tag), document, qualified_digest,
                               record_size, tag=tag, draft=True, prerelease=True)
        gh("release", "edit", tag, "--repo", repository,
           "--draft=false", "--prerelease", "--latest=false")
    verify_uploaded_assets(find_release(repository, tag), document, qualified_digest,
                           record_size, tag=tag, draft=False, prerelease=True)
    if gh_json(f"repos/{repository}/commits/{tag}")["sha"] != document["commit"]:
        raise ValueError(f"{tag} does not point at the candidate commit")
    return tag


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", choices=["record", "verify", "fetch", "verify-upload",
                                              "stage-fetch", "stage"])
    parser.add_argument("--directory", type=Path, default=Path("dist"))
    parser.add_argument("--version", help="release tag; staging reads it from the record")
    parser.add_argument("--repository", required=True)
    parser.add_argument("--commit", help="candidate commit; staging reads it from the run")
    parser.add_argument("--run-id", type=int, required=True)
    parser.add_argument("--run-attempt", type=int, default=1)
    parser.add_argument("--qualified-digest")
    args = parser.parse_args()
    pins = json.loads(Path(__file__).with_name("guest-images.json").read_text())
    digest = args.qualified_digest or ""

    if args.operation in ("stage-fetch", "stage"):
        # No tag exists yet and none is read: the run names the commit and the
        # digest-pinned record names the version.
        run = candidate_run(args.repository, args.run_id)
        if args.operation == "stage-fetch":
            download_candidate(args.directory, args.repository, run)
        document = verify_downloaded(args.directory, pins, args.repository, run, digest)
        if args.operation == "stage-fetch":
            print(f"verified candidate {run['head_sha']} from run {args.run_id}, "
                  f"attempt {run['run_attempt']}")
            return
        tag = stage(args.directory, document, digest)
        print(f"https://github.com/{args.repository}/releases/download/{tag}")
        return

    if not args.version or not args.commit:
        parser.error(f"{args.operation} requires --version and --commit")
    expected = identity(args.version, args.repository, args.commit, args.run_id, args.run_attempt)
    if args.operation == "record":
        print(record_candidate(args.directory, pins, **expected))
        return

    if args.operation == "verify":
        verify_candidate(args.directory, pins, digest, **expected)
        print(f"verified local candidate {args.commit}")
        return

    # Resolve the remote tag again immediately before publication. No tag is created.
    commit = gh_json(f"repos/{args.repository}/commits/{args.version}")["sha"]
    if commit != args.commit:
        raise ValueError("release tag no longer points to the qualified candidate commit")
    run = candidate_run(args.repository, args.run_id, args.commit)
    if args.operation == "fetch":
        download_candidate(args.directory, args.repository, run)
    document = verify_downloaded(args.directory, pins, args.repository, run, digest, args.version)
    if args.operation == "verify-upload":
        release = find_release(args.repository, args.version)
        if release is None:
            raise ValueError(f"no draft release for {args.version}")
        verify_uploaded_assets(release, document, digest, (args.directory / RECORD).stat().st_size)
    print(f"verified candidate {args.commit} from run {args.run_id}, attempt {run['run_attempt']}")


if __name__ == "__main__":
    main()
