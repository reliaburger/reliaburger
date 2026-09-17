#!/usr/bin/env python3
"""Mirror the exact guest images pinned into the CLI into a release directory."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import tempfile
import urllib.request


def fetch_image(image, directory):
    destination = directory / image["asset"]
    digest = hashlib.sha256()
    staged = None
    try:
        with tempfile.NamedTemporaryFile(dir=directory, delete=False) as output:
            staged = Path(output.name)
            with urllib.request.urlopen(image["url"], timeout=60) as response:
                if not response.url.startswith("https://"):
                    raise ValueError("guest image redirect must use HTTPS")
                total = 0
                while chunk := response.read(1024 * 1024):
                    total += len(chunk)
                    if total > 2 * 1024**3:
                        raise ValueError("guest image exceeds 2 GiB")
                    output.write(chunk)
                    digest.update(chunk)
            output.flush()
            os.fsync(output.fileno())
        if digest.hexdigest() != image["sha256"]:
            raise ValueError(f"guest image checksum mismatch: {image['asset']}")
        staged.replace(destination)
    finally:
        if staged is not None:
            staged.unlink(missing_ok=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--directory", type=Path, required=True)
    args = parser.parse_args()
    args.directory.mkdir(parents=True, exist_ok=True)
    images = json.loads(Path(__file__).with_name("guest-images.json").read_text())
    for image in images.values():
        fetch_image(image, args.directory)
