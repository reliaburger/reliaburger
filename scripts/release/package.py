#!/usr/bin/env python3
"""Sign release binaries and guest images, and emit the schema-1 metadata.

OpenSSL 3 performs Ed25519 operations. The private key is PKCS#8 DER, matching
`relish dev keygen`. This script never creates or rotates the release identity.

`--channel nightly` signs a release train's nightly build (#1363) with the
nightly key instead: `RELIABURGER_NIGHTLY_KEY`, checked against
`NIGHTLY_SIGNING_KEYS` in src/upgrade/keys.rs, which only a nightly build
trusts. Each channel refuses the other's keys, and a nightly refuses to run
at all where the release key is in the environment.
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

# macOS is Apple silicon only: 0.2.1 was the last release with an Intel macOS
# build (#1018).
PLATFORMS = {
    "bun": ("linux-aarch64", "linux-x86_64"),
    "relish": ("linux-aarch64", "linux-x86_64", "macos-aarch64"),
}
GUEST_METADATA = "guest-image-metadata.json"
# SubjectPublicKeyInfo header for a 32-byte Ed25519 public key (RFC 8410).
ED25519_SPKI = bytes.fromhex("302a300506032b6570032100")
# What each channel signs with: the Actions secret holding the private key,
# and the list in src/upgrade/keys.rs holding the public keys its builds trust.
CHANNELS = {
    "release": ("RELIABURGER_RELEASE_KEY", "EMBEDDED_RELEASE_KEYS"),
    "nightly": ("RELIABURGER_NIGHTLY_KEY", "NIGHTLY_SIGNING_KEYS"),
}


def trusted_keys(source, channel):
    """The `ed25519:` keys in one channel's list in the text of keys.rs."""
    name = CHANNELS[channel][1]
    match = re.search(rf"pub const {name}: &\[&str\] =\s*&\[(.*?)\];", source, re.S)
    if not match:
        raise ValueError(f"cannot find {name} in src/upgrade/keys.rs")
    return re.findall(r'"(ed25519:[A-Za-z0-9+/=]+)"', match.group(1))


def verify_signature(public_key, data, signature):
    """Whether `signature` (base64) is `public_key`'s (`ed25519:BASE64`) Ed25519 signature of `data`."""
    try:
        raw = base64.b64decode(public_key.removeprefix("ed25519:"), validate=True)
        decoded = base64.b64decode(signature, validate=True)
    except ValueError:
        return False
    if not public_key.startswith("ed25519:") or len(raw) != 32 or len(decoded) != 64:
        return False
    with tempfile.TemporaryDirectory(prefix="reliaburger-verify-") as temporary:
        directory = Path(temporary)
        (directory / "key.der").write_bytes(ED25519_SPKI + raw)
        (directory / "data").write_bytes(data)
        (directory / "signature").write_bytes(decoded)
        result = subprocess.run(["openssl", "pkeyutl", "-verify", "-pubin", "-keyform", "DER", "-rawin",
                                 "-inkey", str(directory / "key.der"), "-in", str(directory / "data"),
                                 "-sigfile", str(directory / "signature")], capture_output=True)
    return result.returncode == 0


def openssl(*args):
    result = subprocess.run(["openssl", *map(str, args)], capture_output=True)
    if result.returncode:
        # OpenSSL's messages name the failing operation, not key material.
        detail = result.stderr.decode(errors="replace").strip()[:300]
        raise ValueError(f"OpenSSL release signing operation failed ({args[0]}): {detail}")
    return result.stdout


# PKCS#8 prefix for a bare 32-byte Ed25519 seed (RFC 8410).
ED25519_PKCS8_SEED = bytes.fromhex("302e020100300506032b657004220420")


def release_key_der(encoded, scratch, secret="RELIABURGER_RELEASE_KEY"):
    """Return the release key as PKCS#8 DER.

    The secret should be base64 DER, but a base64 PEM file or a base64 raw
    32-byte seed are easy mistakes when setting it, so both are accepted and
    converted. A key that is none of these fails with a description of what
    was found, never with any of its bytes.
    """
    try:
        raw = base64.b64decode("".join(encoded.split()), validate=True)
    except ValueError:
        raise ValueError(f"{secret} is not valid base64") from None
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
                f"{secret} decodes to a PEM block OpenSSL cannot read as a "
                "private key: " + result.stderr.decode(errors="replace").strip()[:300]
            )
        return result.stdout
    if len(raw) == 32:
        return ED25519_PKCS8_SEED + raw
    if raw[:1] == b"\x30":
        return raw
    raise ValueError(
        f"{secret} decodes to {len(raw)} bytes that are neither PKCS#8 DER, "
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


def compatibility_pair(source):
    """The cluster formats (protocol, state) in `CURRENT` of src/compatibility.rs."""
    match = re.search(r"pub const CURRENT: Compatibility = Compatibility \{\s*protocol: (\d+),\s*state: (\d+),", source)
    if not match:
        raise ValueError("cannot find CURRENT in src/compatibility.rs")
    return {"protocol": int(match.group(1)), "state": int(match.group(2))}


def package_release(directory, version, repository, key, trusted, pins, forbidden_keys=()):
    """Sign every artefact with `key`, which must be in `trusted` and not in `forbidden_keys`.

    `trusted` is the signing channel's list in keys.rs and `forbidden_keys`
    the other channel's, so a key that somehow landed in both lists still
    can't sign: the release key never signs a nightly, nor the nightly key a
    release.
    """
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
    if encoded in forbidden_keys:
        raise ValueError("signing key belongs to the other channel; the release and nightly keys never mix")
    if encoded not in trusted:
        raise ValueError("signing key is not trusted by the binaries it signs")

    # Published per release so `relish upgrade check` can tell an operator
    # that a release with other formats needs a fresh cluster.
    root = Path(__file__).resolve().parents[2]
    compatibility = compatibility_pair((root / "src/compatibility.rs").read_text())

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
        release = {"version": version, "compatibility": compatibility, "platforms": artifacts}
        outputs[name] = {"schema": 1, "latest": version, "releases": [release]}
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
        binary = directory / f"relish-{platform}"
        # The installer names the size before it downloads.
        template = template.replace(marker[:-1] + "_SIZE@", str(binary.stat().st_size))
        template = template.replace(marker, sha256(binary))
    (directory / "install.sh").write_text(template)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--directory", type=Path, required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--channel", choices=sorted(CHANNELS), default="release",
                        help="release (main) or nightly (a release-* train's nightly build)")
    args = parser.parse_args(argv)
    secret = CHANNELS[args.channel][0]
    if args.channel == "nightly" and "RELIABURGER_RELEASE_KEY" in os.environ:
        raise ValueError("RELIABURGER_RELEASE_KEY is set; a nightly build never runs beside the release key")
    root = Path(__file__).resolve().parents[2]
    source = (root / "src/upgrade/keys.rs").read_text()
    trusted = trusted_keys(source, args.channel)
    forbidden = [key for channel in CHANNELS if channel != args.channel for key in trusted_keys(source, channel)]
    encoded_key = os.environ.pop(secret, None)
    if not encoded_key:
        raise ValueError(f"{secret} must contain the base64 PKCS#8 {args.channel} signing key")
    with tempfile.TemporaryDirectory(prefix="reliaburger-signing-") as temporary:
        key = Path(temporary) / "key.der"
        with key.open("xb") as stream:
            os.chmod(key, 0o600)
            stream.write(release_key_der(encoded_key, Path(temporary), secret))
        pins = json.loads(Path(__file__).with_name("guest-images.json").read_text())
        package_release(args.directory, args.version, args.repository, key, trusted, pins,
                        forbidden_keys=forbidden)


if __name__ == "__main__":
    main()
