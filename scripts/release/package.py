#!/usr/bin/env python3
"""Sign release binaries and guest images, and emit the schema-1 metadata.

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
GUEST_METADATA = "guest-image-metadata.json"
# SubjectPublicKeyInfo header for a 32-byte Ed25519 public key (RFC 8410).
ED25519_SPKI = bytes.fromhex("302a300506032b6570032100")


def openssl(*args):
    result = subprocess.run(["openssl", *map(str, args)], capture_output=True)
    if result.returncode:
        # OpenSSL's messages name the failing operation, not key material.
        detail = result.stderr.decode(errors="replace").strip()[:300]
        raise ValueError(f"OpenSSL release signing operation failed ({args[0]}): {detail}")
    return result.stdout


# PKCS#8 prefix for a bare 32-byte Ed25519 seed (RFC 8410).
ED25519_PKCS8_SEED = bytes.fromhex("302e020100300506032b657004220420")


def release_key_der(encoded, scratch):
    """Return the release key as PKCS#8 DER.

    The secret should be base64 DER, but a base64 PEM file or a base64 raw
    32-byte seed are easy mistakes when setting it, so both are accepted and
    converted. A key that is none of these fails with a description of what
    was found, never with any of its bytes.
    """
    try:
        raw = base64.b64decode("".join(encoded.split()), validate=True)
    except ValueError:
        raise ValueError("RELIABURGER_RELEASE_KEY is not valid base64") from None
    if raw.lstrip().startswith(b"-----BEGIN"):
        pem = scratch / "key.pem"
        with pem.open("xb") as stream:
            os.chmod(pem, 0o600)
            stream.write(raw)
        result = subprocess.run(
            ["openssl", "pkey", "-in", str(pem), "-outform", "DER"], capture_output=True
        )
        pem.unlink()
        if result.returncode:
            raise ValueError(
                "RELIABURGER_RELEASE_KEY decodes to a PEM block OpenSSL cannot read as a "
                "private key: " + result.stderr.decode(errors="replace").strip()[:300]
            )
        return result.stdout
    if len(raw) == 32:
        return ED25519_PKCS8_SEED + raw
    if raw[:1] == b"\x30":
        return raw
    raise ValueError(
        f"RELIABURGER_RELEASE_KEY decodes to {len(raw)} bytes that are neither PKCS#8 DER, "
        "a PEM private key nor a 32-byte Ed25519 seed"
    )


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def guest_image_statement(version, arch, asset, digest, source_digest):
    """The text the release key signs for a guest image; see artifacts.rs."""
    return (f"reliaburger guest image v1\nversion {version}\narch {arch}\nasset {asset}\n"
            f"sha256 {digest}\nsource-sha256 {source_digest}\n").encode()


def sign_bytes(key, data):
    """Ed25519 over bytes that aren't a file yet."""
    with tempfile.TemporaryDirectory(prefix="reliaburger-statement-") as temporary:
        path = Path(temporary) / "statement"
        path.write_bytes(data)
        signature = openssl("pkeyutl", "-sign", "-rawin", "-keyform", "DER", "-inkey", key, "-in", path)
    if len(signature) != 64:
        raise ValueError("unexpected Ed25519 signature length")
    return base64.b64encode(signature).decode("ascii")


def guest_image_metadata(directory, version, key, pins):
    """Sign each built guest image's digest, binding it to its pinned source."""
    images = {}
    for arch, pin in sorted(pins["images"].items()):
        path = directory / pin["asset"]
        digest = sha256(path)
        source = pin["source"]
        statement = guest_image_statement(version, arch, pin["asset"], digest, source["sha256"])
        images[arch] = {
            "asset": pin["asset"], "sha256": digest, "size": path.stat().st_size,
            "signature": sign_bytes(key, statement),
            "source": {"url": source["url"], "sha256": source["sha256"]},
        }
    return {"schema": 1, "version": version, "images": images}


def package_release(directory, version, repository, key, trusted_keys, pins):
    if not re.fullmatch(r"v\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?", version):
        raise ValueError("release tag must be a version prefixed with v")
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository):
        raise ValueError("invalid GitHub repository")
    for binary, platforms in PLATFORMS.items():
        for platform in platforms:
            path = directory / f"{binary}-{platform}"
            if not path.is_file() or path.is_symlink() or path.stat().st_size == 0:
                raise ValueError(f"missing or invalid release binary: {path.name}")
    for image in pins["images"].values():
        path = directory / image["asset"]
        if not path.is_file() or path.is_symlink() or path.stat().st_size == 0:
            raise ValueError(f"missing or invalid guest image: {path.name}")

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
    outputs[GUEST_METADATA] = guest_image_metadata(directory, version, key, pins)
    for image in outputs[GUEST_METADATA]["images"].values():
        checksums.append(f"{image['sha256']}  {image['asset']}\n")

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
            stream.write(release_key_der(encoded_key, Path(temporary)))
        pins = json.loads(Path(__file__).with_name("guest-images.json").read_text())
        package_release(args.directory, args.version, args.repository, key, trusted, pins)


if __name__ == "__main__":
    main()
