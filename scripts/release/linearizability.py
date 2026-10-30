#!/usr/bin/env python3
"""Check a council history for linearizability (stdlib only, #1399).

A history is the JSON Lines file `council_history.py run` writes: one `call`
event when a client sends a council operation and one `return` event when it
gets an answer. An operation that never got an answer, or got a timeout or a
server error, may or may not have taken effect, and may take effect after
the client gave up, so it has no return time.

A history is linearizable when every operation can be given a single instant
between its call and its return, and the operations, run one at a time in
that order against a simple model of the council, give the answers the
clients saw. The search is the one Porcupine uses (Wing and Gong, with
Lowe's memoisation): try each operation that could come next, and back off
when the model disagrees with what a client saw. Each namespace is checked
apart (P-compositionality), since an operation on one never changes another.

The model is the council's test leases, which are what the API offers a
client: a namespace has at most one active lease (grant), a lease's expiry
moves only when its owner renews it (renew), reads return the latest
committed record (read), and a released lease is gone (release). The
council's lease reaper may clean up a lease once its expiry has passed, so
the model allows that, with a margin for clock skew between the host and
the nodes.

The search is bounded. A namespace whose search runs over its budget or its
deadline is reported as inconclusive rather than hanging the soak. Each
namespace gets its own deadline, so one slow namespace can't leave the rest
unchecked (#1437); a soak's fast and final tiers fail on an inconclusive
result (sustained_check.py).

Subcommands:
  check HISTORY [--out RESULT] [--budget N] [--deadline S]
      exit 0 when linearizable, 1 on a violation, 3 when inconclusive
"""
import argparse
import dataclasses
import json
from pathlib import Path
import sys
import time

INF = float("inf")
# How far the host's clock and a node's may disagree when the model decides
# whether the lease reaper could have removed an expired lease.
SKEW_MS = 30_000
# A grant with no answer made its lease, if at all, when it was handled. Once
# that lease's expiry is this far behind, the reaper has removed it, and the
# grant stops mattering to the search.
GRANT_RETIREMENT_MS = 900_000
# States the search may visit per namespace: a base plus a share per operation.
DEFAULT_BUDGET = 400_000
BUDGET_PER_OPERATION = 20
# Seconds each namespace's search may take.
DEFAULT_DEADLINE_SECONDS = 600

FREE = ("free",)
# The start of a slice cut from the middle of a history: any state at all.
ANY = ("any",)
# A held namespace: ("held", lease, expiry, lower_bound, cleaning).
# `lease` is None when the slice started before the grant (any lease may
# match), or "?<n>" for a grant that timed out (no lease the history names).
# `expiry` is None when a timed-out renew may have moved it to any value at
# or above `lower_bound`. `cleaning` is None when not yet seen.
UNKNOWN_HOLDER = ("held", None, None, -INF, None)

WRITES = ("grant", "renew", "release")
# What clients ask; the checker adds "begin_release" for a release's first step.
KINDS = ("grant", "renew", "read", "release")
READ_OUTCOMES = ("ok", "not_found")


@dataclasses.dataclass(frozen=True)
class Operation:
    """One council operation: what a client asked and what it saw."""

    id: int
    client: object
    kind: str
    namespace: str
    lease: object
    ttl_ms: int
    call: float
    ret: float
    call_wall: int
    ret_wall: object
    outcome: str
    expires: object
    cleaning: object

    def summary(self):
        return {"id": self.id, "client": self.client, "op": self.kind, "lease": self.lease,
                "call": self.call, "return": None if self.ret == INF else self.ret,
                "outcome": self.outcome, "expires_at_unix_ms": self.expires, "cleaning": self.cleaning}


def load_history(path):
    """Read a history file. A torn last line (a killed client) is skipped."""
    events = []
    for line in Path(path).read_text().splitlines():
        try:
            events.append(json.loads(line))
        except json.JSONDecodeError:
            continue
    return operations_from_events(events)


def operations_from_events(events):
    """Pair call and return events into operations the checker can use.

    A read without an answer says nothing, and a write the council refused
    for a reason the model doesn't cover (400, 403, 429) changed nothing,
    so both are dropped. A write without a definite answer may yet happen:
    it never returns.
    """
    calls, returns = {}, {}
    for event in events:
        if event.get("event") == "call":
            calls[event["id"]] = event
        elif event.get("event") == "return":
            returns[event["id"]] = event
    operations = []
    for op_id, call in calls.items():
        answer = returns.get(op_id)
        outcome = answer.get("outcome", "unknown") if answer else "unknown"
        kind = call["op"]
        if kind == "read" and outcome not in READ_OUTCOMES:
            continue
        if kind in WRITES and outcome == "refused":
            continue
        definite = answer is not None and outcome != "unknown"
        if kind == "release":
            # A release is two council writes: cleanup starts, then the lease
            # goes. A read in between sees the lease cleaning, so the start is
            # an operation of its own, which may or may not have happened.
            operations.append(Operation(
                id=op_id, client=call.get("client"), kind="begin_release", namespace=call["namespace"],
                lease=call.get("lease"), ttl_ms=0, call=call["mono"], ret=INF, call_wall=call.get("wall_ms", 0),
                ret_wall=None, outcome="unknown", expires=None, cleaning=None))
        operations.append(Operation(
            id=op_id, client=call.get("client"), kind=kind, namespace=call["namespace"],
            lease=(answer or {}).get("lease") if kind == "grant" else call.get("lease"),
            ttl_ms=call.get("ttl_ms", 0), call=call["mono"], ret=answer["mono"] if definite else INF,
            call_wall=call.get("wall_ms", 0), ret_wall=answer.get("wall_ms") if definite else None,
            outcome=outcome, expires=(answer or {}).get("expires_at_unix_ms"),
            cleaning=(answer or {}).get("cleaning")))
    operations.sort(key=lambda operation: (operation.call, operation.id))
    return operations


# --- the model ------------------------------------------------------------------


def could_expire(state, operation):
    """Whether the reaper could have removed this lease before the operation took effect."""
    if state[0] != "held":
        return False
    if operation.ret_wall is None:
        return True
    _, _, expiry, lower_bound, _ = state
    floor = lower_bound if expiry is None else expiry
    return operation.ret_wall >= floor - SKEW_MS


def matches(holder, lease):
    return holder is None or holder == lease


def expiry_matches(state, seen):
    _, _, expiry, lower_bound, _ = state
    if expiry is None:
        return seen is not None and seen >= lower_bound - SKEW_MS
    return expiry == seen


def step(state, operation):
    """Every state the model may be in after `operation`; empty when it cannot happen."""
    if state == ANY:
        before = [FREE, UNKNOWN_HOLDER]
    else:
        before = [state]
        if could_expire(state, operation):
            before += [FREE, state[:4] + (True,)]
    after = []
    for current in before:
        for successor in apply(current, operation):
            if successor not in after:
                after.append(successor)
    return after


def apply(state, op):
    held = state[0] == "held"
    holder = state[1] if held else None
    lease = op.lease
    kind, outcome = op.kind, op.outcome
    if kind == "grant":
        if outcome == "ok":
            return [("held", lease, op.expires, -INF, False)] if not held else []
        if outcome == "owned":
            return [state] if held else []
        if outcome == "unknown":
            return [("held", f"?{op.id}", None, op.call_wall + op.ttl_ms, False)] if not held else []
    elif kind == "renew":
        mine = held and matches(holder, lease)
        if outcome == "ok":
            return [("held", lease, op.expires, -INF, False)] if mine and state[4] is not True else []
        if outcome == "not_found":
            return [state] if not held or holder != lease else []
        if outcome == "not_active":
            if not mine:
                return []
            if state[4] is False and not could_expire(state, op):
                return []
            return [("held", lease) + state[2:]]
        if outcome == "unknown":
            return [("held", lease, None, op.call_wall + op.ttl_ms, False)] if mine and state[4] is not True else []
    elif kind == "read":
        if outcome == "ok":
            if not held or not matches(holder, lease) or not expiry_matches(state, op.expires):
                return []
            if state[4] is not None and state[4] != bool(op.cleaning):
                return []
            return [("held", lease, op.expires, -INF, bool(op.cleaning))]
        if outcome == "not_found":
            return [state] if not held or holder != lease else []
    elif kind == "begin_release":
        return [("held", lease) + state[2:4] + (True,)] if held and matches(holder, lease) else []
    elif kind == "release":
        if outcome == "ok":
            if not held:
                return [FREE]
            if holder == lease:
                return [FREE]
            return [FREE, state] if holder is None else [state]
        if outcome == "pending":
            return [("held", lease) + state[2:4] + (True,)] if held and matches(holder, lease) else []
        if outcome == "unknown":
            if held and matches(holder, lease):
                return [("held", lease) + state[2:4] + (True,), FREE]
            return []
    raise ValueError(f"no model for {kind} {outcome}")


# --- the search -----------------------------------------------------------------


def holder_of(state):
    """The lease holding the namespace: a lease id, "?<n>", None for any lease, or FREE."""
    if state[0] == "held":
        return state[1]
    return FREE if state == FREE else None


class Unanswered:
    """The writes with no answer, indexed so the search only looks at the ones that could matter.

    A renew or release of lease L can only change anything while L holds the
    namespace, and L never comes back once it has gone, since lease ids are
    never reused. A grant with no answer can only take effect while the
    namespace is free, and stops mattering once the lease it might have made
    has certainly been reaped.
    """

    def __init__(self, operations):
        self.operations = operations
        self.grants = [index for index, op in enumerate(operations) if op.kind == "grant"]
        self.by_lease = {}
        for index, op in enumerate(operations):
            if op.kind != "grant":
                self.by_lease.setdefault(op.lease, []).append(index)

    def retired(self, index, now_wall):
        op = self.operations[index]
        return op.kind == "grant" and now_wall > op.call_wall + op.ttl_ms + GRANT_RETIREMENT_MS

    def relevant(self, state, now_wall):
        holder = holder_of(state)
        if holder is None:
            indices = range(len(self.operations))
        elif holder == FREE:
            indices = self.grants
        else:
            indices = self.by_lease.get(holder, ())
        return [index for index in indices if not self.retired(index, now_wall)]

    def canonical(self, applied, state, now_wall):
        """Forget applied writes that can never matter again, so equal futures meet in the memo."""
        holder = holder_of(state)
        if holder is None:
            return applied
        kept = frozenset(index for index in applied
                         if (self.operations[index].kind == "grant" and not self.retired(index, now_wall))
                         or self.operations[index].lease == holder)
        return kept if len(kept) != len(applied) else applied


def search(operations, initial, budget, deadline):
    """'ok', 'fail' or 'budget': can these operations be put in an order the model accepts?"""
    completed = [op for op in operations if op.ret != INF]
    optional = Unanswered([op for op in operations if op.ret == INF])
    total = len(completed)
    stack = [(initial, 0, frozenset(), frozenset())]
    seen = set()
    while stack:
        node = stack.pop()
        if node in seen:
            continue
        seen.add(node)
        if len(seen) > budget or (len(seen) % 1024 == 0 and time.monotonic() > deadline):
            return "budget"
        state, frontier, done, applied = node
        if frontier == total:
            return "ok"
        now_wall = completed[frontier].call_wall
        # The operations that may come next: none other has returned before they were called.
        earliest_return, candidates = INF, []
        for index in range(frontier, total):
            operation = completed[index]
            if operation.call > earliest_return:
                break
            if index not in done:
                candidates.append(index)
                earliest_return = min(earliest_return, operation.ret)
        # A write with no answer may take effect at any point after its call, or never.
        for index in optional.relevant(state, now_wall):
            operation = optional.operations[index]
            if operation.call > earliest_return or index in applied:
                continue
            for successor in step(state, operation):
                if successor != state:
                    stack.append((successor, frontier, done,
                                  optional.canonical(applied | {index}, successor, now_wall)))
        for index in reversed(candidates):
            for successor in step(state, completed[index]):
                now_done, now_frontier = done | {index}, frontier
                while now_frontier in now_done:
                    now_done = now_done - {now_frontier}
                    now_frontier += 1
                later_wall = completed[now_frontier].call_wall if now_frontier < total else now_wall
                stack.append((successor, now_frontier, now_done, optional.canonical(applied, successor, later_wall)))
    return "fail"


def unanswered(operation):
    """The operation as it looked before its answer arrived."""
    if operation.kind == "read":
        return None
    return dataclasses.replace(operation, ret=INF, ret_wall=None, outcome="unknown", expires=None,
                               lease=None if operation.kind == "grant" else operation.lease)


def prefix(operations, events, count):
    """The history as it stood after its first `count` call and return events."""
    called, returned = set(), set()
    for _, is_call, op_id in events[:count]:
        (called if is_call else returned).add(op_id)
    kept = []
    for operation in operations:
        if operation.id not in called:
            continue
        if operation.ret == INF or operation.id in returned:
            kept.append(operation)
        else:
            replaced = unanswered(operation)
            if replaced is not None:
                kept.append(replaced)
    return kept


def smallest_failing_slice(operations, budget, deadline):
    """Cut a failing history down: the shortest failing prefix, then its shortest failing tail.

    The tail starts at a quiet moment, when no answered operation was in
    flight, and is checked from any state at all. Writes still unanswered
    at that moment stay in, since they may take effect inside the tail.
    """
    events = sorted([(op.call, True, op.id) for op in operations]
                    + [(op.ret, False, op.id) for op in operations if op.ret != INF],
                    key=lambda event: (event[0], not event[1]))
    low, high = 1, len(events)
    while low < high:
        middle = (low + high) // 2
        if search(prefix(operations, events, middle), FREE, budget, deadline) == "fail":
            high = middle
        else:
            low = middle + 1
    failing = prefix(operations, events, low)
    answered = [op for op in failing if op.ret != INF]
    cuts = sorted({op.ret for op in answered
                   if not any(other.call < op.ret < other.ret for other in answered)})

    # A lease released before the cut never holds the namespace after it, so
    # unanswered writes to it can't matter there.
    released = {}
    for op in answered:
        if op.kind == "release" and op.outcome == "ok":
            released[op.lease] = min(released.get(op.lease, INF), op.ret)

    def tail(cut):
        return [op for op in failing
                if op.call > cut or (op.ret == INF and released.get(op.lease, INF) > cut)]

    best = failing
    low, high = 0, len(cuts) - 1
    while low <= high:
        middle = (low + high) // 2
        candidate = tail(cuts[middle])
        if candidate and search(candidate, ANY, budget, deadline) == "fail":
            best = candidate
            low = middle + 1
        else:
            high = middle - 1
    return best


def check(operations, budget=None, deadline_seconds=DEFAULT_DEADLINE_SECONDS):
    """Check every namespace; the result says how many operations were checked and the verdict.

    Each namespace's search, and the failing slice it may need, gets
    `deadline_seconds` of its own.
    """
    by_namespace = {}
    for operation in operations:
        by_namespace.setdefault(operation.namespace, []).append(operation)
    def counted(ops):
        return sum(1 for op in ops if op.kind in KINDS)

    result = {"verdict": "linearizable", "operations": counted(operations), "checked": 0,
              "partitions": len(by_namespace), "inconclusive": [], "unanswered_writes":
              sum(1 for op in operations if op.ret == INF and op.kind in KINDS)}
    for namespace, ops in sorted(by_namespace.items()):
        allowed = budget if budget is not None else DEFAULT_BUDGET + BUDGET_PER_OPERATION * len(ops)
        deadline = time.monotonic() + deadline_seconds
        verdict = search(ops, FREE, allowed, deadline)
        if verdict == "ok":
            result["checked"] += counted(ops)
        elif verdict == "budget":
            result["inconclusive"].append(namespace)
        else:
            result["checked"] += counted(ops)
            result.setdefault("violations", []).append(namespace)
            if "violation" in result:
                continue
            failing = smallest_failing_slice(ops, allowed, deadline)
            result["violation"] = {"namespace": namespace,
                                   "slice": [op.summary() for op in failing if op.kind in KINDS]}
    if "violation" in result:
        result["verdict"] = "violation"
    elif result["inconclusive"]:
        result["verdict"] = "inconclusive"
    return result


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest="command", required=True)
    checking = commands.add_parser("check", help="check a history; exit 1 on a violation, 3 if inconclusive")
    checking.add_argument("history")
    checking.add_argument("--out")
    checking.add_argument("--budget", type=int, help="states to visit per namespace (default: scales with its size)")
    checking.add_argument("--deadline", type=float, default=DEFAULT_DEADLINE_SECONDS,
                          help="seconds each namespace's search may take")
    args = parser.parse_args(argv)
    result = check(load_history(args.history), budget=args.budget, deadline_seconds=args.deadline)
    text = json.dumps(result, indent=1, sort_keys=True) + "\n"
    if args.out:
        Path(args.out).write_text(text)
    else:
        sys.stdout.write(text)
    return {"linearizable": 0, "violation": 1, "inconclusive": 3}[result["verdict"]]


if __name__ == "__main__":
    sys.exit(main())
