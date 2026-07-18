# TC DNS synthesis proof of concept

This experiment answers a service DNS query in an eBPF program attached at TC
ingress. It exists to settle one narrow question from the codebase review: can
the kernel synthesise the DNS response directly, even though Reliaburger's
current cgroup socket-address hooks can't access DNS payloads?

Yes. A packet hook can do it. That doesn't automatically make it the right
production design.

## What it proves

`dns_tc.bpf.c` parses one bounded, uncompressed IPv4/UDP A question, looks up
the lower-case wire-format name in a BPF hash map, appends an A record, removes
an EDNS OPT record when present, swaps the packet endpoints, recalculates the
IPv4 and UDP checksums, and redirects the response into the workload's veth
peer. Queries without a map entry continue to the upstream DNS server.

`run.sh` builds a disposable topology with a Linux bridge and separate client
and upstream network namespaces. It checks:

- a mapped `redis.internal` query;
- a missing internal service and an external query passing upstream;
- EDNS removal and header correction;
- malformed input;
- 64 concurrent queries;
- successful IPv4 delivery and the synthesised UDP checksum reported by `tcpdump`;
- detach and reattach behaviour; and
- packet and server logs showing that mapped queries never reached upstream.

The runner deletes its namespaces, bridge, veths and loaded program on exit.
It preserves the generated log and packet captures in `EVIDENCE_DIR`.

## Run it

Use a throwaway Linux host or VM with kernel 5.10 or newer. The proof needs
root, Clang's BPF backend, libbpf headers, `bpftool`, `iproute2`, `tcpdump`,
GCC (to locate the multiarch headers), Bash and Python 3.

```console
sudo EVIDENCE_DIR=/tmp/reliaburger-dns-tc-evidence ./run.sh
```

The command exits non-zero on any failed assertion. Generated `.bpf.o` and
`.pcap` files aren't source and aren't checked in. See
`evidence/verified-2026-07-18.txt` for the dated run recorded during the review.

## Boundaries

This is deliberately a proof, not a production resolver. It supports Ethernet,
IPv4, UDP, one uncompressed A/IN question, a 256-byte DNS payload and a 30-second
TTL. It doesn't implement IPv6, TCP fallback, CNAMEs, DNSSEC, VLAN parsing,
fragmentation, negative synthesis, retries or a stable control-plane map
owner. The map is also local to each loaded TC program, and its 32-bit FNV key
doesn't provide production-grade collision protection.

A production version would need a loader and lifecycle owner, pinned or shared
maps, dual-stack parsing, TCP handling or an explicit fallback contract,
bounded observability, compatibility checks and tests across supported kernels.
The codebase review compares that cost with the current userspace resolver and
a hybrid TC fast path.
