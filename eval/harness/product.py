"""Fetch and verify the product build under test."""

from __future__ import annotations

from pathlib import Path

from .core import Blocked, Config, RunLog
from .lab import sha256_file

ASSETS = ("SHA256SUMS", "bun-linux-x86_64", "relish-linux-x86_64")


def release_dir(cfg: Config) -> Path:
    return cfg.cache_dir / "releases" / cfg.product["release_tag"]


def fetch(cfg: Config, log: RunLog) -> dict[str, Path]:
    """Download the release assets once and check every one against config/product.yaml."""
    target = release_dir(cfg)
    target.mkdir(parents=True, exist_ok=True)
    pins = cfg.product["sha256"]
    paths: dict[str, Path] = {}
    for name in ASSETS:
        path = target / name
        if not path.exists():
            part = path.with_suffix(".part")
            log.run(["curl", "-fsSL", "--retry", "5", "-o", str(part),
                     f"{cfg.product['base_url']}/{name}"], check=True, timeout=1800)
            part.rename(path)
        digest = sha256_file(path)
        if pins.get(name) and digest != pins[name]:
            raise Blocked(f"checksum mismatch for {name}: got {digest}, pinned {pins[name]}")
        paths[name] = path
    # The release's own SHA256SUMS must agree with the pins too.
    sums = dict(line.split()[::-1] for line in paths["SHA256SUMS"].read_text().splitlines() if line.strip())
    for name in ("bun-linux-x86_64", "relish-linux-x86_64"):
        if sums.get(name) != pins[name]:
            raise Blocked(f"release SHA256SUMS disagrees with pin for {name}")
    paths["relish-linux-x86_64"].chmod(0o755)
    return paths


def host_relish(cfg: Config, log: RunLog) -> Path:
    return fetch(cfg, log)["relish-linux-x86_64"]
