#!/usr/bin/env python3
"""Measure the resident memory of one idle Bun node and write a record.

Usage:
  idle_memory.py --bun PATH --relish PATH --record FILE
                 [--mode cluster|standalone] [--runtime RUNTIME]
                 [--settle SECONDS] [--samples N] [--interval SECONDS]
                 [--port-base N] [--keep]

The script creates a fresh cluster directory with `relish init`, starts Bun
on it with no workloads (`--cluster` in cluster mode, so the node is the
single-voter council leader; without it in standalone mode), waits
`--settle` seconds (default 600) for start-up work to finish, then takes
`--samples` readings (default 10) `--interval` seconds apart (default 30).
The node's storage lives in a fresh temporary directory and its ports move
to `--port-base` (default 29000) plus the usual last three digits (API
29117, gossip 29443 and so on), so it doesn't collide with a cluster
already running on the host.

Each reading is the Bun process's resident set size and the RSS of Bun plus
every descendant process. On Linux the Bun figure comes from VmRSS in
/proc/<pid>/status, with VmHWM as the peak; on macOS, from `ps -o rss`.
The descendant total always comes from `ps`, so it double-counts shared
pages and is an upper bound.

Run it as the user Bun runs as in production: root for rootful runc on
Linux, where `relish init` turns on eBPF and the DNS responder. The record
(Markdown) lists the binary, machine, method and every sample; nothing is
written if Bun exits early or no sample is taken. The cluster directory and
Bun's data are deleted afterwards unless --keep is given.
"""

from __future__ import annotations

import argparse
import dataclasses
import datetime
import hashlib
import os
import pathlib
import platform
import shutil
import signal
import statistics
import subprocess
import sys
import tempfile
import time


class IdleMemoryError(Exception):
    """The measurement can't produce an honest number."""


@dataclasses.dataclass(frozen=True)
class Sample:
    """One reading, taken `elapsed_secs` after Bun started."""

    elapsed_secs: int
    rss_kib: int
    tree_kib: int


def parse_proc_status(text: str) -> tuple[int, int | None]:
    """Return (VmRSS, VmHWM) in KiB from a /proc/<pid>/status body."""
    values: dict[str, int] = {}
    for line in text.splitlines():
        key, _, rest = line.partition(":")
        if key in ("VmRSS", "VmHWM"):
            values[key] = int(rest.split()[0])
    if "VmRSS" not in values:
        raise IdleMemoryError("no VmRSS in /proc status (process exited?)")
    return values["VmRSS"], values.get("VmHWM")


def parse_ps_table(text: str) -> dict[int, tuple[int, int]]:
    """Parse `ps -A -o pid=,ppid=,rss=` style output into {pid: (ppid, rss_kib)}.

    A header line, if present, is skipped.
    """
    rows: dict[int, tuple[int, int]] = {}
    for line in text.splitlines():
        fields = line.split()
        if len(fields) < 3 or not fields[0].isdigit():
            continue
        rows[int(fields[0])] = (int(fields[1]), int(fields[2]))
    return rows


def descendants(rows: dict[int, tuple[int, int]], root: int) -> list[int]:
    """Every process below `root` in the parent tree, excluding `root`."""
    children: dict[int, list[int]] = {}
    for pid, (ppid, _) in rows.items():
        children.setdefault(ppid, []).append(pid)
    found: list[int] = []
    pending = list(children.get(root, []))
    while pending:
        pid = pending.pop()
        found.append(pid)
        pending.extend(children.get(pid, []))
    return found


STORAGE_ROOT = "/var/lib/reliaburger"
PORT_KEYS = ("gossip_port", "raft_port", "reporting_port", "registry_port")


def isolate_config(text: str, state: pathlib.Path, port_base: int) -> str:
    """Point a `relish init` config's storage at `state` and its ports at
    `port_base` + the default's last three digits, so the measured node
    shares no data directory or port with anything else on the host."""
    if f'"{STORAGE_ROOT}/' not in text:
        raise IdleMemoryError(f"config has no storage under {STORAGE_ROOT}")
    text = text.replace(f'"{STORAGE_ROOT}/', f'"{state}/')
    lines = []
    for line in text.splitlines():
        key, sep, value = line.partition(" = ")
        if sep and key.strip() in PORT_KEYS and value.strip().isdigit():
            line = f"{key} = {port_base + int(value) % 1000}"
        lines.append(line)
    return "\n".join(lines) + "\n"


def _mib(kib: int) -> float:
    return round(kib / 1024, 1)


def summarise(samples: list[Sample]) -> dict[str, tuple[float, float, float]]:
    """Min, median and max in MiB for Bun's RSS and the process tree's."""
    if not samples:
        raise IdleMemoryError("no samples were taken")
    result = {}
    for name, values in (
        ("rss_mib", [s.rss_kib for s in samples]),
        ("tree_mib", [s.tree_kib for s in samples]),
    ):
        result[name] = (
            _mib(min(values)),
            _mib(int(statistics.median(values))),
            _mib(max(values)),
        )
    return result


def render_record(metadata: dict, samples: list[Sample], peak_kib: int | None = None) -> str:
    """The Markdown record for docs/qualification/."""
    summary = summarise(samples)
    rss = summary["rss_mib"]
    tree = summary["tree_mib"]
    lines = [
        f"# Idle node memory ({metadata['mode']}), {metadata['date']}",
        "",
        "Written by `scripts/release/idle_memory.py`.",
        "",
        "## Binary and machine",
        "",
        f"- Bun: `{metadata['bun_version']}`, commit `{metadata['commit']}`, "
        f"SHA-256 `{metadata['binary_sha256']}`",
        f"- Machine: {metadata['machine']}",
        f"- OS: {metadata['os']}",
        f"- Runtime flag: `--runtime {metadata['runtime']}`",
        "",
        "## Method",
        "",
        f"A fresh `relish init` directory, Bun started in {metadata['mode']} mode with "
        "no workloads deployed. The first sample was taken after "
        f"{metadata['settle_secs']} s, then one every {metadata['interval_secs']} s. "
        f"Bun's RSS comes from {metadata['source']}; the tree column adds every "
        "descendant process from `ps` and double-counts shared pages.",
        "",
        "## Result",
        "",
        "| | min | median | max |",
        "|---|---|---|---|",
        f"| Bun RSS (MiB) | {rss[0]} | {rss[1]} | {rss[2]} |",
        f"| Bun and descendants (MiB) | {tree[0]} | {tree[1]} | {tree[2]} |",
        "",
    ]
    if peak_kib is not None:
        lines += [f"Peak RSS since start (VmHWM): {_mib(peak_kib)} MiB.", ""]
    lines += [
        "## Samples",
        "",
        "| seconds since start | Bun RSS (MiB) | Bun and descendants (MiB) |",
        "|---|---|---|",
    ]
    for sample in samples:
        lines.append(
            f"| {sample.elapsed_secs} | {_mib(sample.rss_kib)} | {_mib(sample.tree_kib)} |"
        )
    lines.append("")
    return "\n".join(lines)


def _ps_rows() -> dict[int, tuple[int, int]]:
    output = subprocess.run(
        ["ps", "-A", "-o", "pid=,ppid=,rss="],
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    return parse_ps_table(output)


def _read(pid: int) -> tuple[int, int, int | None]:
    """(Bun RSS, tree RSS, peak) in KiB for the live process `pid`."""
    rows = _ps_rows()
    if pid not in rows:
        raise IdleMemoryError(f"bun (pid {pid}) is no longer running")
    peak = None
    status = pathlib.Path(f"/proc/{pid}/status")
    if status.exists():
        rss, peak = parse_proc_status(status.read_text())
    else:
        rss = rows[pid][1]
    tree = rss + sum(rows[child][1] for child in descendants(rows, pid))
    return rss, tree, peak


def _machine() -> str:
    if sys.platform == "darwin":
        chip = subprocess.run(
            ["sysctl", "-n", "machdep.cpu.brand_string"], capture_output=True, text=True
        ).stdout.strip()
        memory = int(
            subprocess.run(["sysctl", "-n", "hw.memsize"], capture_output=True, text=True).stdout
        )
        return f"{chip}, {memory // 2**30} GiB, {os.cpu_count()} CPUs"
    memory_kib = 0
    for line in pathlib.Path("/proc/meminfo").read_text().splitlines():
        if line.startswith("MemTotal:"):
            memory_kib = int(line.split()[1])
    return f"{platform.machine()}, {memory_kib // 2**20} GiB, {os.cpu_count()} CPUs"


def _os() -> str:
    if sys.platform == "darwin":
        return "macOS " + platform.mac_ver()[0]
    return f"Linux {platform.release()}"


def _sha256(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--bun", required=True, type=pathlib.Path)
    parser.add_argument("--relish", required=True, type=pathlib.Path)
    parser.add_argument("--record", required=True, type=pathlib.Path)
    parser.add_argument("--mode", choices=["cluster", "standalone"], default="cluster")
    parser.add_argument("--runtime", default="auto")
    parser.add_argument("--settle", type=int, default=600)
    parser.add_argument("--samples", type=int, default=10)
    parser.add_argument("--interval", type=int, default=30)
    parser.add_argument("--port-base", type=int, default=29000)
    parser.add_argument("--keep", action="store_true")
    args = parser.parse_args(argv)
    if args.samples < 1:
        raise IdleMemoryError("--samples must be at least 1")

    bun = args.bun.resolve()
    relish = args.relish.resolve()
    work = pathlib.Path(tempfile.mkdtemp(prefix="rb-idle-"))
    cluster = work / "cluster"
    subprocess.run(
        [str(relish), "init", str(cluster), "--cluster-name", "idle", "--node-id", "idle-01"],
        check=True,
        cwd=work,
    )
    config = cluster / "reliaburger.toml"
    config.write_text(isolate_config(config.read_text(), work / "state", args.port_base))
    command = [
        str(bun),
        "--runtime",
        args.runtime,
        "--config",
        str(config),
        "--listen",
        f"127.0.0.1:{args.port_base + 117}",
    ]
    if args.mode == "cluster":
        command.insert(1, "--cluster")
    log = (work / "bun.log").open("wb")
    started = time.monotonic()
    process = subprocess.Popen(command, cwd=work, stdout=log, stderr=subprocess.STDOUT)
    samples: list[Sample] = []
    peak = None
    try:
        deadline = started + args.settle
        while time.monotonic() < deadline:
            if process.poll() is not None:
                raise IdleMemoryError(f"bun exited with {process.returncode}; see {work}/bun.log")
            time.sleep(min(5.0, max(0.0, deadline - time.monotonic())))
        for index in range(args.samples):
            if process.poll() is not None:
                raise IdleMemoryError(f"bun exited with {process.returncode}; see {work}/bun.log")
            rss, tree, peak = _read(process.pid)
            samples.append(Sample(int(time.monotonic() - started), rss, tree))
            print(f"sample {index + 1}/{args.samples}: bun {_mib(rss)} MiB, tree {_mib(tree)} MiB")
            if index + 1 < args.samples:
                time.sleep(args.interval)
    finally:
        process.send_signal(signal.SIGTERM)
        try:
            process.wait(timeout=30)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
        log.close()

    version = subprocess.run([str(bun), "--version"], capture_output=True, text=True).stdout
    commit = subprocess.run(
        ["git", "rev-parse", "--short", "HEAD"],
        capture_output=True,
        text=True,
        cwd=pathlib.Path(__file__).resolve().parent,
    ).stdout.strip()
    metadata = {
        "date": datetime.date.today().isoformat(),
        "mode": args.mode,
        "bun_version": version.strip() or "unknown",
        "commit": commit or "unknown",
        "machine": _machine(),
        "os": _os(),
        "runtime": args.runtime,
        "settle_secs": args.settle,
        "interval_secs": args.interval,
        "source": "VmRSS in /proc/<pid>/status"
        if pathlib.Path("/proc/self/status").exists()
        else "`ps -o rss`",
        "binary_sha256": _sha256(bun),
    }
    args.record.write_text(render_record(metadata, samples, peak))
    print(f"wrote {args.record}")
    if not args.keep:
        shutil.rmtree(work, ignore_errors=True)
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main(sys.argv[1:]))
    except IdleMemoryError as error:
        print(f"idle_memory: {error}", file=sys.stderr)
        sys.exit(1)
