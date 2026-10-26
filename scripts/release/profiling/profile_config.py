#!/usr/bin/env python3
"""Set the bench cluster's node config, whatever order `relish init cluster` wrote it in.

  profile_config.py CONFIG BUSYBOX

Allowlists BUSYBOX for host jobs (`[process_workloads] allowed_binaries`,
with `mount_isolation = false`) and turns eBPF on (`[ebpf] enabled`), then
reads the file back and fails unless those are the values. The profile runs
two releases, and each writes its own config, so this edits by section and
key rather than by exact text.
"""
import re
import sys
import tomllib

WANTED = {
    "process_workloads": {"allowed_binaries": None, "mount_isolation": "false"},
    "ebpf": {"enabled": "true"},
}


def edit(text, busybox):
    """Return the config text with the bench's keys set, adding any missing key or section."""
    wanted = {section: dict(keys) for section, keys in WANTED.items()}
    wanted["process_workloads"]["allowed_binaries"] = '["%s"]' % busybox
    pending = {section: dict(keys) for section, keys in wanted.items()}
    out, section = [], None

    def flush(name):
        for key, value in pending.get(name, {}).items():
            out.append(f"{key} = {value}")
        pending.pop(name, None)

    for line in text.splitlines():
        header = re.match(r"^\s*\[([^\[\]]+)\]\s*$", line)
        if header:
            flush(section)
            section = header.group(1).strip()
            out.append(line)
            continue
        key = re.match(r"^\s*([A-Za-z0-9_-]+)\s*=", line)
        if key and section in wanted and key.group(1) in wanted[section]:
            value = wanted[section][key.group(1)]
            pending.get(section, {}).pop(key.group(1), None)
            out.append(f"{key.group(1)} = {value}")
            continue
        out.append(line)
    flush(section)
    for name in list(pending):
        out += ["", f"[{name}]"]
        flush(name)
    return "\n".join(out) + "\n"


def check(text, busybox):
    document = tomllib.loads(text)
    process = document.get("process_workloads", {})
    if process.get("allowed_binaries") != [busybox] or process.get("mount_isolation") is not False:
        raise ValueError("[process_workloads] didn't take the BusyBox allowlist")
    if document.get("ebpf", {}).get("enabled") is not True:
        raise ValueError("[ebpf] enabled didn't take")


def main():
    path, busybox = sys.argv[1], sys.argv[2]
    with open(path) as source:
        text = edit(source.read(), busybox)
    try:
        check(text, busybox)
    except (ValueError, tomllib.TOMLDecodeError) as error:
        sys.exit(f"profile_config: {error}")
    with open(path, "w") as out:
        out.write(text)


if __name__ == "__main__":
    main()
