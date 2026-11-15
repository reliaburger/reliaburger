#!/usr/bin/env python3
"""a1029: put every on-CPU sample of a system-wide folded profile into one cost category.

  categorise.py FOLDED [TASKS_PER_SECOND]

The first matching rule wins. With a task rate, each category is also shown as
CPU microseconds per task (4 vCPUs x the profile's 20 s window).
"""
import sys
from collections import defaultdict

RULES = [
    ("idle (no runnable work)", lambda c, s: c == "swapper"),
    ("Bun: GitOps loop (DesiredState clone/drop per Raft metrics change)", lambda c, s: "lettuce::runner::spawn_" in s),
    ("Bun: Raft core and log", lambda c, s: "openraft::" in s or "council::durable" in s or "council::log" in s),
    ("Bun: task ledger (group commit, fsync)", lambda c, s: "task_ledger" in s),
    ("Bun: per task, namespace policy check (kernel lock, BPF map read)", lambda c, s: "TaskNamespacePolicy>::check" in s),
    ("Bun: per task, wait_empty_task (cgroup.events read)", lambda c, s: "wait_empty_task" in s),
    ("Bun: per task, helper protocol (socket read/write)", lambda c, s: "reusable_executor::protocol" in s),
    ("Bun: per task, OCI spec and environment rebuild", lambda c, s: "generate_job_oci_spec" in s or "host_environment" in s or "oci::job_process" in s),
    ("Bun: per task, rest of ReusablePool::run (slots lock, timers, logs)", lambda c, s: "ReusablePool<" in s and ">::run" in s),
    ("Bun: per task, OwnedRunner::run outside the pool (template, admission)", lambda c, s: "OwnedRunner<" in s),
    ("Bun: per task, chunk streaming and attempt bookkeeping", lambda c, s: "resume_chunk_streaming" in s or "TaskAttempts" in s or "run_chunk" in s),
    ("Bun: task-array leader tick and node sync", lambda c, s: "task_array_leader" in s or "task_array_node" in s or "task_array_state" in s),
    ("Bun: other (runtime, timers, wakeups, other subsystems)", lambda c, s: c in ("tokio-rt-worker", "rb-bun") or c.startswith("tokio")),
    ("helper/child: clone3 (process creation)", lambda c, s: c == "helper" and ("copy_process" in s or "__do_sys_clone3" in s or "ret_from_fork" in s)),
    ("helper/child: execve", lambda c, s: c in ("helper", "busybox") and ("execve" in s)),
    ("helper/child: exit and address-space teardown", lambda c, s: c in ("helper", "busybox") and ("do_exit" in s or "mmput" in s or "exit_group" in s)),
    ("helper/child: page faults", lambda c, s: c in ("helper", "busybox") and ("do_page_fault" in s or "do_mem_abort" in s or "do_translation_fault" in s)),
    ("helper: wait, poll, pipes and socket syscalls", lambda c, s: c == "helper" and ("invoke_syscall" in s or "el0_svc" in s)),
    ("helper: user space and other kernel", lambda c, s: c == "helper"),
    ("busybox `true`: user space and other kernel", lambda c, s: c == "busybox"),
    ("relish (raw scenario spawner and bench client)", lambda c, s: c.startswith("rb-relish") or c == "relish"),
    ("other processes and kernel threads", lambda c, s: True),
]


def load(path):
    out = defaultdict(float)
    for line in open(path):
        s, _, n = line.rstrip("\n").rpartition(" ")
        if s:
            out[s] += float(n)
    return out


def main():
    # categorise.py SYSTEM_FOLDED BUN_FOLDED [TASKS_PER_SECOND]
    system, bun = load(sys.argv[1]), load(sys.argv[2])
    rate = float(sys.argv[3]) if len(sys.argv) > 3 else 0.0
    cats = defaultdict(float)
    total = sum(system.values())
    comms = defaultdict(float)
    for stack, n in system.items():
        rest = n - bun.get(stack, 0.0)
        if rest <= 0:
            continue
        comm = stack.split(";", 1)[0]
        if comm == ":":
            comm = "busybox"  # perf printed ":PID" for short-lived task processes
        if comm in ("tokio-rt-worker", "rb-relish") or comm.startswith("tokio"):
            comm = "rb-relish"  # not Bun's pid, so the bench client's runtime
        for name, rule in RULES:
            if not name.startswith("Bun") and rule(comm, stack):
                cats[name] += rest
                if name.startswith("other"):
                    comms[comm] += rest
                break
    for stack, n in bun.items():
        for name, rule in RULES:
            if name.startswith("Bun") and rule("tokio-rt-worker", stack):
                cats[name] += n
                break
        else:
            cats["Bun: other (runtime, timers, wakeups, other subsystems)"] += n
    window_cpu_us = 4 * 20 * 1e6
    print(f"samples {total:.0f}")
    for name, _ in RULES:
        n = cats.get(name, 0.0)
        if not n:
            continue
        share = n / total
        per_task = f"{share * window_cpu_us / (rate * 20):8.1f} us/task" if rate else ""
        print(f"{100 * share:6.2f}%  {per_task}  {name}")
    print("other by command:", ", ".join(f"{k} {100 * v / total:.2f}%" for k, v in sorted(comms.items(), key=lambda x: -x[1])[:12]))


if __name__ == "__main__":
    main()
