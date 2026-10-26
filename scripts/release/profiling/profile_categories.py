#!/usr/bin/env python3
"""Put every on-CPU sample of a profile into one named cost category.

  profile_categories.py SYSTEM_FOLDED BUN_FOLDED [--rate TASKS_PER_SECOND]
      [--cpus 4] [--json OUT]

SYSTEM_FOLDED holds the whole VM's folded stacks; BUN_FOLDED the same
window's stacks for Bun's process only (`profile_flame.py fold-cpu --tgid`).
Bun's samples go into Bun's categories, everything else into the system
ones. The first matching rule wins, so specific rules come before broad ones.

Bun's samples that match no rule are "uncategorised". When they're more than
UNCATEGORISED_LIMIT of Bun's samples, the table says to extend the map: a hot
path nobody has named is exactly what the release profile is there to find.
With a task rate, each category is also shown as CPU microseconds per task.

From the 2026-10-20 host-job profile record's tools/categorise.py, which the
release profile step (scripts/release/qualify-profile.sh) now maintains.
"""
import argparse
import json
from collections import defaultdict

# A share of Bun's samples above this in "uncategorised" asks for new rules.
UNCATEGORISED_LIMIT = 0.05

BUN_UNCATEGORISED = "Bun: uncategorised (extend the map)"
IDLE = "idle (no runnable work)"

# Bun's own categories, matched against the joined stack ("comm;root;...;leaf").
BUN_RULES = [
    ("Bun: GitOps loop", lambda s: "reliaburger::lettuce::" in s),
    ("Bun: task ledger (group commit, fsync)", lambda s: "task_ledger" in s),
    ("Bun: task-array leader tick and node sync",
     lambda s: "task_array_leader" in s or "task_array_node" in s or "task_array_state" in s),
    ("Bun: pool path, namespace policy check", lambda s: "TaskNamespacePolicy>::check" in s),
    ("Bun: pool path, wait_empty_task (cgroup.events read)", lambda s: "wait_empty_task" in s),
    ("Bun: pool path, helper protocol (socket read/write)", lambda s: "reusable_executor::protocol" in s),
    ("Bun: pool path, OCI spec and environment rebuild",
     lambda s: "generate_job_oci_spec" in s or "host_environment" in s or "oci::job_process" in s),
    ("Bun: pool path, rest of ReusablePool::run (slots lock, timers, logs)",
     lambda s: "ReusablePool<" in s and ">::run" in s),
    ("Bun: pool path, OwnedRunner::run outside the pool (template, admission)", lambda s: "OwnedRunner<" in s),
    ("Bun: pool path, chunk streaming and attempt bookkeeping",
     lambda s: "resume_chunk_streaming" in s or "TaskAttempts" in s or "run_chunk" in s),
    ("Bun: Raft core and log", lambda s: "openraft::" in s or "reliaburger::council::" in s),
    ("Bun: agent loop and supervisor", lambda s: "reliaburger::bun::agent" in s or "reliaburger::bun::supervisor" in s),
    ("Bun: API server", lambda s: "reliaburger::bun::api" in s or "axum::" in s or "hyper::" in s),
    ("Bun: metrics (Mayo)", lambda s: "reliaburger::mayo::" in s),
    ("Bun: logs (Ketchup)", lambda s: "reliaburger::ketchup::" in s),
    ("Bun: gossip (Mustard)", lambda s: "reliaburger::mustard::" in s),
    ("Bun: service discovery (Onion)", lambda s: "reliaburger::onion::" in s),
    ("Bun: containers (Grill)", lambda s: "reliaburger::grill::" in s),
    ("Bun: other Reliaburger code", lambda s: "reliaburger::" in s),
    ("Bun: async runtime (scheduler, timers, parking)", lambda s: "tokio::" in s),
]

# The rest of the VM, matched on the command name and the joined stack.
SYSTEM_RULES = [
    (IDLE, lambda c, s: c == "swapper"),
    ("helper/child: clone3 (process creation)",
     lambda c, s: c == "helper" and ("copy_process" in s or "__do_sys_clone3" in s or "ret_from_fork" in s)),
    ("helper/child: execve", lambda c, s: c in ("helper", "busybox") and "execve" in s),
    ("helper/child: exit and address-space teardown",
     lambda c, s: c in ("helper", "busybox") and ("do_exit" in s or "mmput" in s or "exit_group" in s)),
    ("helper/child: page faults",
     lambda c, s: c in ("helper", "busybox") and ("do_page_fault" in s or "do_mem_abort" in s or "do_translation_fault" in s)),
    ("helper: wait, poll, pipes and socket syscalls",
     lambda c, s: c == "helper" and ("invoke_syscall" in s or "el0_svc" in s or "entry_SYSCALL" in s)),
    ("helper: user space and other kernel", lambda c, s: c == "helper"),
    ("busybox task: user space and other kernel", lambda c, s: c == "busybox"),
    ("runc and containerd-style shims", lambda c, s: c.startswith("runc")),
    ("relish (raw scenario spawner and bench client)", lambda c, s: c in ("rb-relish", "relish")),
    ("other processes and kernel threads", lambda c, s: True),
]


def parse_folded(lines):
    """Folded stack lines ("a;b;c 12") into {stack: weight}."""
    out = defaultdict(float)
    for line in lines:
        stack, _, count = line.rstrip("\n").rpartition(" ")
        if not stack:
            continue
        try:
            weight = float(count)
        except ValueError:
            continue
        out[stack] += weight
    return dict(out)


def system_comm(stack):
    comm = stack.split(";", 1)[0]
    if comm == ":" or comm.startswith(":"):
        return "busybox"  # perf prints ":PID" for short-lived task processes
    if comm in ("tokio-rt-worker", "rb-relish") or comm.startswith("tokio"):
        return "rb-relish"  # not Bun's pid, so the bench client's runtime
    return comm


def bun_category(stack):
    for name, rule in BUN_RULES:
        if rule(stack):
            return name
    return BUN_UNCATEGORISED


def categorise(system, bun):
    """Split a window's samples into categories.

    Returns {"total", "bun_total", "categories": {name: samples},
    "bun_categories": {name: samples}, "uncategorised_share", "other_by_command"}.
    Bun's samples are counted once, in Bun's categories, even though the
    system profile holds them too.
    """
    categories = defaultdict(float)
    bun_categories = defaultdict(float)
    other_by_command = defaultdict(float)
    for stack, n in system.items():
        rest = n - bun.get(stack, 0.0)
        if rest <= 0:
            continue
        comm = system_comm(stack)
        for name, rule in SYSTEM_RULES:
            if rule(comm, stack):
                categories[name] += rest
                if name.startswith("other"):
                    other_by_command[comm] += rest
                break
    for stack, n in bun.items():
        name = bun_category(stack)
        categories[name] += n
        bun_categories[name] += n
    bun_total = sum(bun.values())
    total = sum(system.values()) or sum(categories.values())
    return {
        "total": total,
        "bun_total": bun_total,
        "categories": dict(categories),
        "bun_categories": dict(bun_categories),
        "uncategorised_share": bun_categories.get(BUN_UNCATEGORISED, 0.0) / bun_total if bun_total else 0.0,
        "other_by_command": dict(other_by_command),
    }


def ordered_names():
    return [IDLE] + [name for name, _ in BUN_RULES] + [BUN_UNCATEGORISED] + [
        name for name, _ in SYSTEM_RULES if name != IDLE]


def table(result, rate=0.0, cpus=4):
    """A plain-text table: share of the VM, share of Bun, and µs per task with a rate."""
    total, bun_total = result["total"], result["bun_total"]
    lines = [f"samples {total:.0f}, Bun {bun_total:.0f}"]
    for name in ordered_names():
        n = result["categories"].get(name, 0.0)
        if not n or not total:
            continue
        share = n / total
        per_task = f"{share * cpus * 1e6 / rate:8.1f} us/task" if rate else ""
        of_bun = f"{100 * n / bun_total:6.2f}% of Bun" if name.startswith("Bun") and bun_total else " " * 13
        lines.append(f"{100 * share:6.2f}%  {of_bun}  {per_task}  {name}")
    others = sorted(result["other_by_command"].items(), key=lambda item: -item[1])[:12]
    if others and total:
        lines.append("other by command: " + ", ".join(f"{k} {100 * v / total:.2f}%" for k, v in others))
    if result["uncategorised_share"] > UNCATEGORISED_LIMIT:
        lines.append(uncategorised_warning(result["uncategorised_share"]))
    return "\n".join(lines)


def uncategorised_warning(share):
    return (f"{100 * share:.1f}% of Bun's samples are uncategorised, over the "
            f"{100 * UNCATEGORISED_LIMIT:.0f}% limit: extend BUN_RULES in "
            "scripts/release/profiling/profile_categories.py")


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("system")
    parser.add_argument("bun")
    parser.add_argument("--rate", type=float, default=0.0)
    parser.add_argument("--cpus", type=int, default=4)
    parser.add_argument("--json")
    args = parser.parse_args()
    with open(args.system) as system, open(args.bun) as bun:
        result = categorise(parse_folded(system), parse_folded(bun))
    result["rate"] = args.rate
    if args.json:
        with open(args.json, "w") as out:
            json.dump(result, out, indent=2, sort_keys=True)
    print(table(result, args.rate, args.cpus))


if __name__ == "__main__":
    main()
