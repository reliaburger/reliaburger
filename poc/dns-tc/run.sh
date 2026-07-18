#!/usr/bin/env bash
set -Eeuo pipefail

# Build and exercise the TC DNS proof of concept in two isolated network
# namespaces. Generated objects and packet captures stay outside the repository.

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
EVIDENCE_DIR=${EVIDENCE_DIR:-$(mktemp -d /tmp/reliaburger-dns-tc-evidence.XXXXXX)}
BUILD_DIR=$(mktemp -d /tmp/reliaburger-dns-tc-build.XXXXXX)
OBJECT="$BUILD_DIR/dns_tc.bpf.o"

suffix=$$
client_namespace="rbdc${suffix}"
upstream_namespace="rbdu${suffix}"
bridge="rbbr${suffix}"
client_host="rbhc${suffix}"
client_peer="rbcp${suffix}"
upstream_host="rbhu${suffix}"
upstream_peer="rbup${suffix}"
subnet_octet=$((suffix % 180 + 20))
client_ip="10.203.${subnet_octet}.2"
upstream_ip="10.203.${subnet_octet}.53"

upstream_pid=
client_capture_pid=
upstream_capture_pid=

mkdir -p "$EVIDENCE_DIR"
umask 022
exec > >(tee "$EVIDENCE_DIR/run.log") 2>&1

stop_captures() {
    local pid
    for pid in "$client_capture_pid" "$upstream_capture_pid"; do
        if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
            kill -INT "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
        fi
    done
    client_capture_pid=
    upstream_capture_pid=
}

cleanup() {
    local status=$?
    trap - EXIT INT TERM
    stop_captures
    if [[ -n "$upstream_pid" ]] && kill -0 "$upstream_pid" 2>/dev/null; then
        kill "$upstream_pid" 2>/dev/null || true
        wait "$upstream_pid" 2>/dev/null || true
    fi
    ip link del "$client_host" 2>/dev/null || true
    ip link del "$upstream_host" 2>/dev/null || true
    ip link del "$bridge" 2>/dev/null || true
    ip netns del "$client_namespace" 2>/dev/null || true
    ip netns del "$upstream_namespace" 2>/dev/null || true
    rm -rf "$BUILD_DIR"
    if ((status == 0)); then
        printf 'PASS: TC DNS proof completed; evidence retained in %s\n' "$EVIDENCE_DIR"
    else
        printf 'FAIL: TC DNS proof stopped with status %d; evidence retained in %s\n' \
            "$status" "$EVIDENCE_DIR"
    fi
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

require_command() {
    if ! command -v "$1" >/dev/null 2>&1; then
        printf 'required command not found: %s\n' "$1" >&2
        exit 1
    fi
}

wait_for_text() {
    local file=$1
    local pattern=$2
    local attempt
    for attempt in {1..50}; do
        if grep -Fq "$pattern" "$file" 2>/dev/null; then
            return 0
        fi
        sleep 0.1
    done
    printf 'timed out waiting for %q in %s\n' "$pattern" "$file" >&2
    return 1
}

assert_result() {
    local kind=$1
    local payload=$2
    python3 - "$kind" "$payload" <<'PY'
import json
import sys

kind, payload = sys.argv[1:]
result = json.loads(payload)
if kind == "answer":
    assert result.get("qr") is True, result
    assert result.get("answers") == 1, result
    assert result.get("address") == "127.128.1.10", result
elif kind == "edns":
    assert result.get("answers") == 1, result
    assert result.get("address") == "127.128.1.10", result
    assert result.get("additional") == 0, result
elif kind == "empty":
    assert result.get("qr") is True, result
    assert result.get("answers") == 0, result
elif kind == "timeout":
    assert result == {"timeout": True}, result
elif kind == "concurrent":
    assert result.get("count") == 64, result
    assert result.get("answered") == 64, result
    assert result.get("timeouts") == 0, result
    assert result.get("addresses") == ["127.128.1.10"], result
else:
    raise AssertionError(f"unknown assertion kind: {kind}")
PY
}

probe() {
    ip netns exec "$client_namespace" \
        python3 "$SCRIPT_DIR/dns_probe.py" "$@" --server "$upstream_ip"
}

find_service_map() {
    bpftool -j prog show name dns_tc_ingress | python3 -c '
import json
import sys
payload = json.load(sys.stdin)
programmes = payload if isinstance(payload, list) else [payload]
if not programmes:
    raise SystemExit("dns_tc_ingress programme not found")
programme = max(programmes, key=lambda entry: entry["id"])
map_ids = programme.get("map_ids", [])
if len(map_ids) != 1:
    raise SystemExit(f"expected one map, found {map_ids!r}")
print(map_ids[0])
'
}

map_redis() {
    local map_id=$1
    local key
    local -a key_bytes
    key=$(python3 - <<'PY'
import struct

wire = b"".join(bytes([len(label)]) + label.encode("ascii")
                for label in "redis.internal".split("."))
value = 2166136261
for octet in wire:
    value = ((value ^ octet) * 16777619) & 0xffffffff
print(" ".join(f"{octet:02x}" for octet in struct.pack("=I", value)))
PY
)
    read -r -a key_bytes <<< "$key"
    bpftool map update id "$map_id" \
        key hex "${key_bytes[@]}" value hex 7f 80 01 0a
}

attach_program() {
    local map_id
    tc filter replace dev "$client_host" ingress protocol all pref 1 \
        bpf direct-action object-file "$OBJECT" section tc/ingress
    map_id=$(find_service_map)
    map_redis "$map_id"
    printf 'attached dns_tc_ingress with service_v4 map id %s\n' "$map_id"
}

if ((EUID != 0)); then
    printf 'run this proof as root (it creates temporary network namespaces)\n' >&2
    exit 1
fi

for command in bpftool clang gcc grep ip python3 tc tcpdump tee; do
    require_command "$command"
done

case "$(uname -m)" in
    x86_64) bpf_arch=x86 ;;
    aarch64 | arm64) bpf_arch=arm64 ;;
    *)
        printf 'unsupported build architecture: %s\n' "$(uname -m)" >&2
        exit 1
        ;;
esac

printf '=== Environment ===\n'
uname -a
clang --version
bpftool version
tc -V

multiarch=$(gcc -dumpmachine)
clang -O2 -g -target bpf -D"__TARGET_ARCH_${bpf_arch}" \
    -I/usr/include -I/usr/include/bpf -I"/usr/include/${multiarch}" \
    -c "$SCRIPT_DIR/dns_tc.bpf.c" -o "$OBJECT"
printf 'compiled %s\n' "$OBJECT"

ip netns add "$client_namespace"
ip netns add "$upstream_namespace"
ip link add "$bridge" type bridge
ip link set "$bridge" up

ip link add "$client_host" type veth peer name "$client_peer"
ip link set "$client_peer" netns "$client_namespace"
ip link set "$client_host" master "$bridge"
ip link set "$client_host" up
ip -n "$client_namespace" link set lo up
ip -n "$client_namespace" address add "$client_ip/24" dev "$client_peer"
ip -n "$client_namespace" link set "$client_peer" up

ip link add "$upstream_host" type veth peer name "$upstream_peer"
ip link set "$upstream_peer" netns "$upstream_namespace"
ip link set "$upstream_host" master "$bridge"
ip link set "$upstream_host" up
ip -n "$upstream_namespace" link set lo up
ip -n "$upstream_namespace" address add "$upstream_ip/24" dev "$upstream_peer"
ip -n "$upstream_namespace" link set "$upstream_peer" up

tc qdisc replace dev "$client_host" clsact
attach_program
tc filter show dev "$client_host" ingress
bpftool prog show name dns_tc_ingress
bpftool map show name service_v4

ip netns exec "$upstream_namespace" \
    python3 "$SCRIPT_DIR/upstream_server.py" --listen "$upstream_ip" \
    >"$EVIDENCE_DIR/upstream.log" 2>&1 &
upstream_pid=$!
wait_for_text "$EVIDENCE_DIR/upstream.log" ready

ip netns exec "$client_namespace" \
    tcpdump --immediate-mode -i "$client_peer" -n -U \
    -w "$EVIDENCE_DIR/client.pcap" udp port 53 \
    >"$EVIDENCE_DIR/client-tcpdump.log" 2>&1 &
client_capture_pid=$!
ip netns exec "$upstream_namespace" \
    tcpdump --immediate-mode -i "$upstream_peer" -n -U \
    -w "$EVIDENCE_DIR/upstream.pcap" udp port 53 \
    >"$EVIDENCE_DIR/upstream-tcpdump.log" 2>&1 &
upstream_capture_pid=$!
wait_for_text "$EVIDENCE_DIR/client-tcpdump.log" 'listening on'
wait_for_text "$EVIDENCE_DIR/upstream-tcpdump.log" 'listening on'

printf '=== Functional cases ===\n'
normal=$(probe redis.internal)
printf 'mapped: %s\n' "$normal"
assert_result answer "$normal"

edns=$(probe redis.internal --edns)
printf 'EDNS: %s\n' "$edns"
assert_result edns "$edns"

missing=$(probe missing.internal)
printf 'missing: %s\n' "$missing"
assert_result empty "$missing"

external=$(probe example.com)
printf 'external: %s\n' "$external"
assert_result empty "$external"

malformed=$(probe ignored.internal --malformed)
printf 'malformed: %s\n' "$malformed"
assert_result timeout "$malformed"

concurrent=$(probe redis.internal --count 64 --summary)
printf 'concurrent: %s\n' "$concurrent"
assert_result concurrent "$concurrent"

stop_captures
ip netns exec "$client_namespace" \
    tcpdump -nn -vv -r "$EVIDENCE_DIR/client.pcap" \
    >"$EVIDENCE_DIR/client.txt" 2>&1
ip netns exec "$upstream_namespace" \
    tcpdump -nn -vv -r "$EVIDENCE_DIR/upstream.pcap" \
    >"$EVIDENCE_DIR/upstream.txt" 2>&1

if grep -Fq 'redis.internal' "$EVIDENCE_DIR/upstream.log"; then
    printf 'mapped query reached the upstream DNS server\n' >&2
    exit 1
fi
grep -Fq 'missing.internal' "$EVIDENCE_DIR/upstream.log"
grep -Fq 'example.com' "$EVIDENCE_DIR/upstream.log"
if grep -Fq 'redis.internal' "$EVIDENCE_DIR/upstream.txt"; then
    printf 'mapped query appeared in the upstream packet capture\n' >&2
    exit 1
fi
grep -Fq 'missing.internal' "$EVIDENCE_DIR/upstream.txt"
grep -Fq 'example.com' "$EVIDENCE_DIR/upstream.txt"
grep -Fq '[udp sum ok]' "$EVIDENCE_DIR/client.txt"
grep -Fq 'A 127.128.1.10' "$EVIDENCE_DIR/client.txt"
printf 'capture: matched queries absent upstream; synthesised UDP checksum valid\n'

printf '=== Attach/detach lifecycle ===\n'
tc filter del dev "$client_host" ingress pref 1
detached=$(probe redis.internal)
printf 'detached: %s\n' "$detached"
assert_result empty "$detached"
grep -Fq 'redis.internal' "$EVIDENCE_DIR/upstream.log"

attach_program
reattached=$(probe redis.internal)
printf 'reattached: %s\n' "$reattached"
assert_result answer "$reattached"

printf '=== Upstream packet evidence ===\n'
cat "$EVIDENCE_DIR/upstream.txt"
printf '=== Synthesised response evidence ===\n'
grep -F -m 2 'A 127.128.1.10' "$EVIDENCE_DIR/client.txt"
