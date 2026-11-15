#!/usr/bin/env python3
"""Record a history of council operations for the linearizability check (#1399).

A few client threads take turns holding test leases on a small set of
namespaces, through the lease API every node serves (`/v1/test/leases`):
grant a namespace, renew and read the lease, release it. Each operation
goes to a node picked at random, so a write through one node and a read
through another meet in the history. Clients whose grant is refused read
the holder's lease, so reads and writes from different clients overlap.

Every operation is two JSON Lines events in the history: `call` before the
request goes out and `return` when an answer (or a timeout) comes back,
each with a monotonic time (`mono`, seconds; it orders the history) and
the wall clock (`wall_ms`; the model compares it with lease expiries). A
timeout, a transport error or a 5xx is recorded as `unknown`: the council
may have applied it, so the checker allows both.

Test leases are the council state a client can write and read back: the
lease record is the key-value pair (lease id to expiry and state) and the
namespace is the lock. The council has no general key-value API to record.

`linearizability.py check` reads the history this writes.

Subcommands:
  run --history FILE --node NAME=PORT... --token-file F [--ca CA] [--clients N]
      [--namespaces N] [--ttl S] [--pause S] [--timeout S] [--duration S] [--seed N]
      until SIGTERM or --duration passes; with --ca, each node's forwarded port
      is checked against a certificate for NAME, otherwise plain HTTP
"""
import argparse
import dataclasses
import http.client
import json
from pathlib import Path
import random
import secrets
import signal
import socket
import ssl
import sys
import threading
import time

# Release answers that leave the lease in place, retried this many times.
RELEASE_ATTEMPTS = 5


def classify(kind, status, body):
    """What an HTTP answer means for the model: the outcome and anything the client learnt.

    `status` is None when no answer came back. Only an answer that proves the
    council did, or definitely did not, apply an operation is definite; the
    rest are `unknown`.
    """
    text = body or ""
    if status is None or status >= 500:
        return {"outcome": "unknown"}
    if kind in ("grant", "renew", "read") and status in (200, 201):
        try:
            lease = json.loads(text)
            return {"outcome": "ok", "lease": lease["lease_id"], "expires_at_unix_ms": lease["expires_at_unix_ms"],
                    "cleaning": (lease.get("state") or {}).get("status") == "cleaning"}
        except (ValueError, KeyError, TypeError, AttributeError):
            return {"outcome": "unknown"}
    if kind == "grant":
        if status == 409 and "already owned" in text:
            return {"outcome": "owned"}
        return {"outcome": "refused"}
    if kind == "renew":
        if status == 404 or (status == 409 and "not found" in text):
            return {"outcome": "not_found"}
        if status == 409 and "expired or cleanup has started" in text:
            return {"outcome": "not_active"}
        return {"outcome": "refused"}
    if kind == "read":
        return {"outcome": "not_found"} if status == 404 else {"outcome": "unknown"}
    if kind == "release":
        if status == 204:
            return {"outcome": "ok"}
        if status == 202:
            return {"outcome": "pending"}
        # A 409 may come after cleanup started, so it proves nothing.
        return {"outcome": "refused"} if status in (400, 401, 403) else {"outcome": "unknown"}
    raise ValueError(f"unknown operation {kind}")


class Recorder:
    """Append call and return events to a history file, one JSON object a line.

    A resumed run appends to the same file: ids carry on from the highest one
    there, and monotonic time never goes back, even across a host restart.
    """

    def __init__(self, path):
        self.path = Path(path)
        self.lock = threading.Lock()
        last_id, last_mono = 0, 0.0
        if self.path.exists():
            for line in self.path.read_text().splitlines():
                try:
                    event = json.loads(line)
                except json.JSONDecodeError:
                    continue
                last_id = max(last_id, int(event.get("id", 0)))
                last_mono = max(last_mono, float(event.get("mono", 0)))
        self.next_id = last_id + 1
        self.offset = max(0.0, last_mono - time.monotonic() + 1.0)
        self.file = self.path.open("a")

    def now(self):
        return time.monotonic() + self.offset, int(time.time() * 1000)

    def write(self, event):
        self.file.write(json.dumps(event, sort_keys=True) + "\n")
        self.file.flush()

    def call(self, client, op, namespace, lease, ttl_ms, node):
        with self.lock:
            op_id = self.next_id
            self.next_id += 1
            mono, wall = self.now()
            self.write({"event": "call", "id": op_id, "client": client, "op": op, "namespace": namespace,
                        "lease": lease, "ttl_ms": ttl_ms, "node": node, "mono": mono, "wall_ms": wall})
        return op_id, mono, wall

    def ret(self, op_id, answer, status, error=None):
        with self.lock:
            mono, wall = self.now()
            event = dict(answer, event="return", id=op_id, status=status, mono=mono, wall_ms=wall)
            if error:
                event["error"] = error
            self.write(event)

    def close(self):
        with self.lock:
            self.file.close()


class PinnedHTTPSConnection(http.client.HTTPSConnection):
    """Connect to a forwarded port but check the certificate for the node's name."""

    def __init__(self, port, server_name, context, timeout):
        super().__init__("127.0.0.1", port, context=context, timeout=timeout)
        self.server_name = server_name
        self.tls = context

    def connect(self):
        raw = socket.create_connection(("127.0.0.1", self.port), self.timeout)
        self.sock = self.tls.wrap_socket(raw, server_hostname=self.server_name)


class HttpTransport:
    """The lease API on each node's forwarded port."""

    def __init__(self, ports, token, ca=None):
        self.ports = ports
        self.nodes = list(ports)
        self.token = token
        self.context = ssl.create_default_context(cafile=ca) if ca else None

    def request(self, node, method, path, body=None, timeout=10):
        port = self.ports[node]
        if self.context:
            connection = PinnedHTTPSConnection(port, node, self.context, timeout)
        else:
            connection = http.client.HTTPConnection("127.0.0.1", port, timeout=timeout)
        headers = {"Authorization": "Bearer " + self.token}
        data = None
        if body is not None:
            data = json.dumps(body).encode()
            headers["Content-Type"] = "application/json"
        try:
            connection.request(method, path, body=data, headers=headers)
            response = connection.getresponse()
            return response.status, response.read().decode(errors="replace")
        finally:
            connection.close()


@dataclasses.dataclass
class Options:
    namespaces: list
    ttl_seconds: int = 300
    pause_seconds: float = 1.0
    timeout_seconds: float = 10.0
    # Stop once the clients have started this many grants, renews and reads.
    max_operations: object = None


class Shared:
    """What the client threads share: the operation count and who holds what."""

    def __init__(self, max_operations):
        self.lock = threading.Lock()
        self.count = 0
        self.max_operations = max_operations
        self.holders = {}

    def take(self):
        with self.lock:
            if self.max_operations is not None and self.count >= self.max_operations:
                return False
            self.count += 1
            return True

    def hold(self, namespace, lease):
        with self.lock:
            self.holders[namespace] = lease

    def drop(self, namespace, lease):
        with self.lock:
            if self.holders.get(namespace) == lease:
                del self.holders[namespace]

    def holder(self, namespace):
        with self.lock:
            return self.holders.get(namespace)


class Client:
    """One client thread's loop: grant, renew and read, release."""

    def __init__(self, number, recorder, transport, nodes, options, rng, stop, shared):
        self.number = number
        self.recorder = recorder
        self.transport = transport
        self.nodes = nodes
        self.options = options
        self.random = rng
        self.stop = stop
        self.shared = shared

    def operate(self, kind, namespace, lease=None, avoid=None):
        """Send one operation to a random node (not `avoid`, when another is there) and record it."""
        choices = [node for node in self.nodes if node != avoid] or self.nodes
        node = self.random.choice(choices)
        ttl = self.options.ttl_seconds
        method, path, body = {
            "grant": ("POST", "/v1/test/leases", {"ttl_seconds": ttl, "namespace": namespace}),
            "renew": ("POST", f"/v1/test/leases/{lease}/renew", {"ttl_seconds": ttl}),
            "read": ("GET", f"/v1/test/leases/{lease}", None),
            "release": ("DELETE", f"/v1/test/leases/{lease}", None),
        }[kind]
        ttl_ms = ttl * 1000 if kind in ("grant", "renew") else 0
        op_id, _, _ = self.recorder.call(self.number, kind, namespace, lease, ttl_ms, node)
        status, text, error = None, "", None
        try:
            status, text = self.transport.request(node, method, path, body, timeout=self.options.timeout_seconds)
        except Exception as failure:  # any transport failure leaves the outcome unknown
            error = f"{type(failure).__name__}: {failure}"
        answer = classify(kind, status, text)
        if answer["outcome"] in ("unknown", "refused") and status is not None:
            error = f"HTTP {status}: {text[:200]}"
        self.recorder.ret(op_id, answer, status, error)
        answer["node"] = node
        return answer

    def pause(self):
        self.stop.wait(self.options.pause_seconds * self.random.uniform(0.5, 1.5))

    def hold_lease(self, namespace, lease):
        self.shared.hold(namespace, lease)
        for _ in range(self.random.randint(1, 4)):
            if self.stop.is_set() or not self.shared.take():
                break
            answer = self.operate(self.random.choice(("renew", "read")), namespace, lease)
            # Straight after a write, read it back through another node: the
            # window where a lagging node could answer with the old value.
            if answer["outcome"] == "ok" and self.shared.take():
                self.operate("read", namespace, lease, avoid=answer["node"])
            self.pause()
        # Release whatever happens, even when stopping, so no lease is left behind.
        for _ in range(RELEASE_ATTEMPTS):
            if self.operate("release", namespace, lease)["outcome"] in ("ok", "refused"):
                break
            time.sleep(min(self.options.pause_seconds, 1.0))
        self.shared.drop(namespace, lease)

    def run(self):
        while not self.stop.is_set() and self.shared.take():
            namespace = self.random.choice(self.options.namespaces)
            answer = self.operate("grant", namespace)
            if answer["outcome"] == "ok":
                self.pause()
                self.hold_lease(namespace, answer["lease"])
            elif answer["outcome"] == "owned":
                other = self.shared.holder(namespace)
                if other and self.shared.take():
                    self.operate("read", namespace, other)
            self.pause()


def run_clients(recorder, transport, nodes, options, clients, seed, stop):
    """Run the client threads until `stop` is set or the operation budget is spent."""
    shared = Shared(options.max_operations)
    threads = []
    for number in range(1, clients + 1):
        loop = Client(number, recorder, transport, list(nodes), options, random.Random(f"{seed}-{number}"), stop,
                      shared)
        thread = threading.Thread(target=loop.run, name=f"council-client-{number}", daemon=True)
        thread.start()
        threads.append(thread)
    for thread in threads:
        thread.join()
    return shared.count


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest="command", required=True)
    run = commands.add_parser("run")
    run.add_argument("--history", required=True)
    run.add_argument("--node", action="append", required=True, help="NAME=PORT, once per node")
    run.add_argument("--token-file", required=True)
    run.add_argument("--ca")
    run.add_argument("--clients", type=int, default=4)
    run.add_argument("--namespaces", type=int, default=2)
    run.add_argument("--ttl", type=int, default=300)
    run.add_argument("--pause", type=float, default=1.0)
    run.add_argument("--timeout", type=float, default=10.0)
    run.add_argument("--duration", type=float)
    run.add_argument("--seed", default=None)
    args = parser.parse_args(argv)

    ports = {}
    for entry in args.node:
        name, _, port = entry.partition("=")
        ports[name] = int(port)
    transport = HttpTransport(ports, Path(args.token_file).read_text().strip(), args.ca)
    # A fresh suffix each run: a lease an earlier run left behind holds none of these.
    suffix = secrets.token_hex(3)
    options = Options(namespaces=[f"rbtest-lin-{suffix}-{index}" for index in range(args.namespaces)],
                      ttl_seconds=args.ttl, pause_seconds=args.pause, timeout_seconds=args.timeout)
    stop = threading.Event()
    signal.signal(signal.SIGTERM, lambda *_: stop.set())
    signal.signal(signal.SIGINT, lambda *_: stop.set())
    if args.duration is not None:
        threading.Timer(args.duration, stop.set).start()
    recorder = Recorder(args.history)
    seed = args.seed if args.seed is not None else suffix
    print(f"council history: {args.clients} clients on {', '.join(options.namespaces)}, seed {seed}",
          file=sys.stderr, flush=True)
    count = run_clients(recorder, transport, list(ports), options, args.clients, seed, stop)
    recorder.close()
    print(f"council history: {count} operations started", file=sys.stderr, flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
