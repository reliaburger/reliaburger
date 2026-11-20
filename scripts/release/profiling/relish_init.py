#!/usr/bin/env python3
"""Run `relish init` for a throwaway bench or measurement cluster.

From 0.3.0, `relish init` writes the root CA's key only into a backup the
operator holds, and without a terminal it needs `--root-backup` and a
passphrase (#709). These clusters are thrown away, so the backup and its
passphrase go into the script's own work directory, the passphrase file
owner-only and deleted as soon as init has read it. A published `relish`
from before 0.3.0 doesn't know the flags, so they're passed only when
`relish init --help` lists `--root-backup`.

Usage: relish_init.py RELISH CLUSTER_DIR CLUSTER_NAME NODE_ID WORK_DIR
"""
import os
from pathlib import Path
import subprocess
import sys


def knows_root_backup(relish: str) -> bool:
    """Whether this `relish init` takes `--root-backup`."""
    shown = subprocess.run([relish, "init", "--help"], capture_output=True, text=True, check=True)
    return "--root-backup" in shown.stdout


def init_cluster(relish: str, cluster_dir: Path, cluster: str, node: str, work: Path) -> None:
    """Run `relish init` into `cluster_dir`, with a root backup in `work`
    when this `relish` takes one."""
    command = [relish, "init", str(cluster_dir), "--cluster-name", cluster, "--node-id", node]
    if not knows_root_backup(relish):
        subprocess.run(command, check=True)
        return
    passphrase = Path(work) / "root-passphrase"
    descriptor = os.open(passphrase, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    try:
        with os.fdopen(descriptor, "w") as file:
            file.write("a throwaway passphrase for a bench cluster\n")
        command += ["--root-backup", str(Path(work) / "root-backup.age"),
                    "--passphrase-file", str(passphrase)]
        subprocess.run(command, check=True)
    finally:
        passphrase.unlink(missing_ok=True)


def main(argv: list[str]) -> int:
    if len(argv) != 5:
        print(__doc__.strip().splitlines()[-1], file=sys.stderr)
        return 2
    relish, cluster_dir, cluster, node, work = argv
    init_cluster(relish, Path(cluster_dir), cluster, node, Path(work))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
