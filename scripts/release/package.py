#!/usr/bin/env python3
"""Sign release binaries and emit the existing schema-1 metadata format.

OpenSSL 3 performs Ed25519 operations. The private key is PKCS#8 DER, matching
`relish dev keygen`. This script never creates or rotates the release identity.
"""
import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile

PLATFORMS = {
    "bun": ("linux-aarch64", "linux-x86_64"),
    "relish": ("linux-aarch64", "linux-x86_64", "macos-aarch64", "macos-x86_64"),
}
# SubjectPublicKeyInfo header for a 32-byte Ed25519 public key (RFC 8410).
ED25519_SPKI = bytes.fromhex("302a300506032b6570032100")


def openssl(*args):
    result = subprocess.run(["openssl", *map(str, args)], capture_output=True)
    if result.returncode:
        raise ValueError("OpenSSL release signing operation failed")
    return result.stdout


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def package_release(directory, version, repository, key, trusted_keys):
    if not re.fullmatch(r"v\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?", version):
        raise ValueError("release tag must be a version prefixed with v")
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository):
        raise ValueError("invalid GitHub repository")
    for binary, platforms in PLATFORMS.items():
        for platform in platforms:
            path = directory / f"{binary}-{platform}"
            if not path.is_file() or path.is_symlink() or path.stat().st_size == 0:
                raise ValueError(f"missing or invalid release binary: {path.name}")

    public = openssl("pkey", "-inform", "DER", "-in", key, "-pubout", "-outform", "DER")
    if len(public) != 44 or not public.startswith(ED25519_SPKI):
        raise ValueError("release key must be Ed25519")
    encoded = "ed25519:" + base64.b64encode(public[12:]).decode("ascii")
    if encoded not in trusted_keys:
        raise ValueError("signing key is not trusted by the release binaries")

    outputs = {}
    checksums = []
    for binary, platforms in PLATFORMS.items():
        artifacts = {}
        for platform in platforms:
            path = directory / f"{binary}-{platform}"
            signature = openssl("pkeyutl", "-sign", "-rawin", "-keyform", "DER", "-inkey", key, "-in", path)
            if len(signature) != 64:
                raise ValueError("unexpected Ed25519 signature length")
            digest = sha256(path)
            envelope = {
                "schema": 1, "sha256": digest,
                "embedded": base64.b64encode(signature).decode("ascii"),
                "external": None,
            }
            outputs[path.name + ".sig"] = envelope
            artifacts[platform] = {
                "url": f"https://github.com/{repository}/releases/download/{version}/{path.name}",
                "sha256": digest, "embedded_signature": envelope["embedded"],
                "external_signature": None,
            }
            checksums.append(f"{digest}  {path.name}\n")
        name = "metadata.json" if binary == "bun" else "cli-metadata.json"
        outputs[name] = {"schema": 1, "latest": version, "releases": [{"version": version, "platforms": artifacts}]}

    # No metadata is written until every expected artefact has been signed.
    for name, document in outputs.items():
        temporary = directory / (name + ".tmp")
        temporary.write_text(json.dumps(document, indent=2) + "\n")
        temporary.replace(directory / name)
    (directory / "SHA256SUMS").write_text("".join(checksums))

    template = Path(__file__).with_name("install.sh.in").read_text()
    template = template.replace("@VERSION@", version).replace("@REPOSITORY@", repository)
    for platform in PLATFORMS["relish"]:
        marker = "@" + platform.upper().replace("-", "_") + "@"
        template = template.replace(marker, sha256(directory / f"relish-{platform}"))
    (directory / "install.sh").write_text(template)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--directory", type=Path, required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--repository", required=True)
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[2]
    trusted = re.findall(r'"(ed25519:[A-Za-z0-9+/=]+)"', (root / "src/upgrade/keys.rs").read_text())
    encoded_key = os.environ.pop("RELIABURGER_RELEASE_KEY", None)
    if not encoded_key:
        raise ValueError("RELIABURGER_RELEASE_KEY must contain the base64 PKCS#8 release key")
    with tempfile.TemporaryDirectory(prefix="reliaburger-signing-") as temporary:
        key = Path(temporary) / "key.der"
        with key.open("xb") as stream:
            os.chmod(key, 0o600)
            stream.write(base64.b64decode(encoded_key, validate=True))
        package_release(args.directory, args.version, args.repository, key, trusted)


if __name__ == "__main__":
    main()
