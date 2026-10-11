#!/usr/bin/env python3
"""Write what a published relish accepts, read from its own help.

The site, the README and the five-minute tour must only advertise commands the
installer's default release has, or label them with the release that adds
them. tests/suite/website.rs checks every advertised `relish` line against
scripts/release/released-cli.tsv, which this script writes from the published
binary: every command, its positionals, and every option with its aliases, its
value and the values clap lists for it.

Usage:
  scripts/release/released_cli.py VERSION [RELISH] > scripts/release/released-cli.tsv

With no RELISH it downloads the release's relish for this machine and checks
it against the release's SHA256SUMS. Run it when the installer's default
version changes (docs/releasing.md, "Update the docs").
"""
import hashlib
import platform
import re
import subprocess
import sys
import tempfile
import urllib.request
from pathlib import Path

RELEASES = "https://github.com/reliaburger/reliaburger/releases/download"
SECTIONS = ("Commands:", "Arguments:", "Options:")
POSSIBLE = re.compile(r"\[possible values: ([^\]]+)\]")


def parse_help(text):
    """The subcommands, positionals and options in one `relish … -h` page."""
    commands, arguments, options = [], [], []
    section = None
    for line in text.splitlines():
        if line in SECTIONS:
            section = line
            continue
        if not line.startswith("  ") or not line.strip():
            if not line.startswith(" "):
                section = None
            continue
        entry = line.strip()
        if section == "Commands:":
            name = entry.split()[0]
            if name != "help":
                commands.append(name)
        elif section == "Arguments:":
            arguments.append(entry.split()[0])
        elif section == "Options:":
            options.append(parse_option(entry))
    return commands, arguments, options


def parse_option(entry):
    """`-f, --file <PATH>  text [possible values: a, b]` as (aliases, value, choices)."""
    usage = re.split(r"\s{2,}", entry, maxsplit=1)[0]
    value = ""
    if " " in usage.split(", ")[-1]:
        usage, value = usage.rsplit(" ", 1)
    aliases = [alias.strip() for alias in usage.split(",")]
    match = POSSIBLE.search(entry)
    choices = [choice.strip() for choice in match.group(1).split(",")] if match else []
    return aliases, value, choices


def snapshot(version, help_for):
    """Every row for the release, sorted after the version line.

    `help_for(path)` returns the short help for the subcommand path, a list of
    words after `relish`. The root's options are global, so a subcommand
    only lists its own.
    """
    rows = set()
    global_options = []
    pending = [[]]
    while pending:
        path = pending.pop()
        name = " ".join(["relish", *path])
        commands, arguments, options = parse_help(help_for(path))
        rows.add(f"command\t{name}")
        for argument in arguments:
            rows.add(f"argument\t{name}\t{argument}")
        if not path:
            global_options = options
        for aliases, value, choices in options:
            if aliases in (["-h", "--help"], ["-V", "--version"]):
                continue
            # clap repeats global options on every subcommand; keep them once.
            if path and (aliases, value, choices) in global_options:
                continue
            rows.add(f"option\t{name}\t{','.join(aliases)}\t{value}\t{','.join(choices)}")
        pending.extend([*path, command] for command in commands)
    # clap's own `help` takes the command path to explain.
    rows.add("command\trelish help")
    rows.add("argument\trelish help\t[COMMAND]...")
    return [f"version\t{version}", *sorted(rows)]


def version_of(output):
    """`relish 0.1.6 (82fa78e)` as `v0.1.6`; a development build is refused."""
    match = re.match(r"relish (\d+\.\d+\.\d+) ", output)
    if not match:
        raise ValueError(f"not a released relish: {output.strip()!r}")
    return f"v{match.group(1)}"


def download(version, directory):
    system = {"Darwin": "macos", "Linux": "linux"}[platform.system()]
    machine = {"arm64": "aarch64", "aarch64": "aarch64", "x86_64": "x86_64"}[platform.machine()]
    name = f"relish-{system}-{machine}"
    sums = urllib.request.urlopen(f"{RELEASES}/{version}/SHA256SUMS").read().decode()
    expected = next(line.split()[0] for line in sums.splitlines() if line.endswith(f"  {name}"))
    binary = Path(directory) / name
    binary.write_bytes(urllib.request.urlopen(f"{RELEASES}/{version}/{name}").read())
    if hashlib.sha256(binary.read_bytes()).hexdigest() != expected:
        raise SystemExit(f"{name} doesn't match the release's SHA256SUMS")
    binary.chmod(0o755)
    return binary


def main():
    if len(sys.argv) not in (2, 3):
        raise SystemExit(__doc__)
    version = sys.argv[1]
    with tempfile.TemporaryDirectory() as directory:
        relish = Path(sys.argv[2]) if len(sys.argv) == 3 else download(version, directory)

        def run(*arguments):
            return subprocess.run(
                [str(relish), *arguments], check=True, capture_output=True, text=True
            ).stdout

        found = version_of(run("--version"))
        if found != version:
            raise SystemExit(f"{relish} is {found}, not {version}")
        rows = snapshot(version, lambda path: run(*path, "-h"))
    print(f"# What relish {version} accepts, read from its -h pages by")
    print("# scripts/release/released_cli.py. Regenerate it when docs/website/install.sh's")
    print("# default version changes; tests/suite/website.rs checks the two match.")
    print("\n".join(rows))


if __name__ == "__main__":
    main()
