/* Onion connect rewrite — intercepts connect() to VIPs.
 *
 * Attached to: BPF_CGROUP_INET4_CONNECT (root cgroup v2)
 *
 * When a process calls connect() with a destination in the VIP range
 * (127.128.0.0/16), this program:
 * 1. Looks up the backend list in backend_map
 * 2. Checks firewall rules (namespace isolation + per-app allow_from)
 * 3. Selects a healthy backend via round-robin
 * 4. Rewrites the destination address and port
 *
 * Non-VIP connections pass through untouched.
 */
#include "onion_common.h"
#include "smoker_common.h"

/* ---------- Map definitions --------------------------------------------- */

struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 65534);
    __type(key, struct backend_key);
    __type(value, struct backend_value);
    __uint(map_flags, BPF_F_NO_PREALLOC);
} backend_map SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 262144);
    __type(key, struct firewall_key);
    __type(value, struct firewall_value);
    __uint(map_flags, BPF_F_NO_PREALLOC);
} firewall_map SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 65536);
    __type(key, struct cgroup_ns_key);
    __type(value, struct cgroup_ns_value);
    __uint(map_flags, BPF_F_NO_PREALLOC);
} cgroup_namespace_map SEC(".maps");

/* ---------- Egress allowlist map ---------------------------------------- */

struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 65536);
    __type(key, struct egress_key);
    __type(value, struct egress_value);
    __uint(map_flags, BPF_F_NO_PREALLOC);
} egress_map SEC(".maps");

/* Exact IPv6 destinations. Same semantics as egress_map. */
struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 65536);
    __type(key, struct egress6_key);
    __type(value, struct egress_value);
    __uint(map_flags, BPF_F_NO_PREALLOC);
} egress6_map SEC(".maps");

/* CIDR allowlists per family. LPM tries: a lookup returns the entry with
 * the longest matching prefix, so userspace folds the ports of enclosing
 * prefixes into every more-specific entry (see merge_cidr_ports). */
struct {
    __uint(type, BPF_MAP_TYPE_LPM_TRIE);
    __uint(max_entries, 65536);
    __type(key, struct egress_cidr4_key);
    __type(value, struct egress_cidr_value);
    __uint(map_flags, BPF_F_NO_PREALLOC);
} egress_cidr4_map SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_LPM_TRIE);
    __uint(max_entries, 65536);
    __type(key, struct egress_cidr6_key);
    __type(value, struct egress_cidr_value);
    __uint(map_flags, BPF_F_NO_PREALLOC);
} egress_cidr6_map SEC(".maps");

/* Per-cgroup flag: 1 = egress enforcement active for this cgroup.
 * If a cgroup is not in this map, all egress is allowed (no config). */
struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 65536);
    __type(key, __u64);         /* cgroup ID */
    __type(value, __u32);       /* 1 = enforce egress */
    __uint(map_flags, BPF_F_NO_PREALLOC);
} egress_enabled_map SEC(".maps");

/* ---------- Egress policy helpers ---------------------------------------- */

/* Write the cgroup id into the first 8 bytes of an LPM key in big-endian
 * order, so both userspace and the kernel produce identical bytes. */
static __always_inline void fill_cgroup_be(__u8 *data, __u64 cg)
{
    data[0] = cg >> 56;
    data[1] = cg >> 48;
    data[2] = cg >> 40;
    data[3] = cg >> 32;
    data[4] = cg >> 24;
    data[5] = cg >> 16;
    data[6] = cg >> 8;
    data[7] = cg;
}

/* Does a CIDR value allow this (network byte order) port? */
static __always_inline int cidr_port_allowed(struct egress_cidr_value *cv,
                                             __u16 port_be)
{
    __u16 n = cv->count;
    if (n > MAX_CIDR_PORTS)
        n = MAX_CIDR_PORTS;

    #pragma unroll
    for (int i = 0; i < MAX_CIDR_PORTS; i++) {
        if (i >= n)
            break;
        if (cv->ports[i] == port_be)
            return 1;
    }
    return 0;
}

/* Is this IPv4 destination allowed for the cgroup? Exact match first,
 * then the CIDR trie. ip_be and port_be are in network byte order. */
static __always_inline int egress4_allowed(__u64 cg, __u32 ip_be, __u16 port_be)
{
    struct egress_key ek = {
        .src_cgroup_id = cg,
        .dst_ip        = ip_be,
        .dst_port      = port_be,
        ._pad          = 0,
    };
    struct egress_value *ev = bpf_map_lookup_elem(&egress_map, &ek);
    if (ev && ev->action == 1)
        return 1;

    struct egress_cidr4_key ck = {};
    ck.prefixlen = CIDR_CGROUP_PREFIX_BITS + 32;
    fill_cgroup_be(ck.data, cg);
    __builtin_memcpy(&ck.data[8], &ip_be, 4);
    struct egress_cidr_value *cv = bpf_map_lookup_elem(&egress_cidr4_map, &ck);
    if (cv && cidr_port_allowed(cv, port_be))
        return 1;

    return 0;
}

/* IPv6 equivalent, including v4-mapped destinations. VIPs remain outside
 * egress policy because internal service discovery owns that range. */
static __always_inline int egress6_allowed(__u64 cg, struct bpf_sock_addr *ctx)
{
    if (ctx->user_ip6[0] == 0 && ctx->user_ip6[1] == 0 &&
        ctx->user_ip6[2] == bpf_htonl(0x0000FFFF)) {
        __u32 v4 = ctx->user_ip6[3];
        if ((bpf_ntohl(v4) & VIP_MASK) == VIP_PREFIX)
            return 1;
        return egress4_allowed(cg, v4, ctx->user_port);
    }

    struct egress6_key ek = {};
    ek.src_cgroup_id = cg;
    ek.dst_ip[0]     = ctx->user_ip6[0];
    ek.dst_ip[1]     = ctx->user_ip6[1];
    ek.dst_ip[2]     = ctx->user_ip6[2];
    ek.dst_ip[3]     = ctx->user_ip6[3];
    ek.dst_port      = ctx->user_port;
    struct egress_value *ev = bpf_map_lookup_elem(&egress6_map, &ek);
    if (ev && ev->action == 1)
        return 1;

    struct egress_cidr6_key ck = {};
    ck.prefixlen = CIDR_CGROUP_PREFIX_BITS + 128;
    fill_cgroup_be(ck.data, cg);
    __builtin_memcpy(&ck.data[8],  &ek.dst_ip[0], 4);
    __builtin_memcpy(&ck.data[12], &ek.dst_ip[1], 4);
    __builtin_memcpy(&ck.data[16], &ek.dst_ip[2], 4);
    __builtin_memcpy(&ck.data[20], &ek.dst_ip[3], 4);
    struct egress_cidr_value *cv = bpf_map_lookup_elem(&egress_cidr6_map, &ck);
    return cv && cidr_port_allowed(cv, ctx->user_port);
}

/* ---------- Smoker fault maps ------------------------------------------- */

struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 4096);
    __type(key, struct fault_connect_key);
    __type(value, struct fault_connect_value);
    __uint(map_flags, BPF_F_NO_PREALLOC);
} fault_connect_map SEC(".maps");

/* fault_state_map is not defined here — we use bpf_get_prandom_u32()
 * for probabilistic drops instead. The fault_state_key/value types in
 * smoker_common.h exist for userspace counters if needed later. */

/* ---------- Connect hook ------------------------------------------------ */

SEC("cgroup/connect4")
int onion_connect(struct bpf_sock_addr *ctx)
{
    __u32 dst_ip = bpf_ntohl(ctx->user_ip4);

    /* Only intercept VIPs in the 127.128.0.0/16 range */
    if ((dst_ip & VIP_MASK) != VIP_PREFIX) {
        /* Not a VIP — check egress allowlist.
         * If the calling cgroup has egress enforcement enabled and
         * the destination is neither an exact allow entry nor inside
         * an allowed CIDR, deny the connection. */
        __u64 eg_cgroup = bpf_get_current_cgroup_id();
        __u32 *enforced = bpf_map_lookup_elem(&egress_enabled_map, &eg_cgroup);
        if (enforced && *enforced == 1 &&
            !egress4_allowed(eg_cgroup, ctx->user_ip4, ctx->user_port))
            return 0;  /* EPERM: egress not allowed */
        return 1;  /* pass through (no enforcement or allowed) */
    }

    /* --- Smoker fault check (before normal path) --- */
    {
        __u64 src_cgroup = bpf_get_current_cgroup_id();

        /* Check partition fault: specific source -> destination */
        struct fault_connect_key fkey = {
            .virtual_ip       = ctx->user_ip4,
            .port             = ctx->user_port,
            .source_cgroup_id = src_cgroup,
        };
        struct fault_connect_value *fval =
            bpf_map_lookup_elem(&fault_connect_map, &fkey);

        if (!fval) {
            /* Also check wildcard (source_cgroup_id = 0) for
             * non-partition faults that apply to all callers */
            fkey.source_cgroup_id = 0;
            fval = bpf_map_lookup_elem(&fault_connect_map, &fkey);
        }

        if (fval) {
            /* Check expiry */
            __u64 now = bpf_ktime_get_ns();
            if (fval->expires_ns == 0 || now <= fval->expires_ns) {
                /* Validate action field */
                if (fval->action > FAULT_ACTION_PARTITION)
                    goto no_fault;

                if (fval->action == FAULT_ACTION_PARTITION) {
                    /* Block this connection entirely */
                    return 0;  /* deny -> EPERM */
                }

                if (fval->action == FAULT_ACTION_DROP) {
                    /* Probabilistic drop using kernel PRNG */
                    __u32 rand = bpf_get_prandom_u32();
                    __u8 roll = rand % 100;
                    if (roll < fval->probability)
                        return 0;  /* deny -> EPERM */
                }

                /* FAULT_ACTION_DELAY is handled in sock_ops or tc netem,
                 * not in the connect hook. Fall through to normal path. */
            }
        }
    }
no_fault:

    /* Look up the backend list for this (VIP, port) */
    struct backend_key key = {
        .vip  = ctx->user_ip4,   /* keep network byte order */
        .port = ctx->user_port,
        ._pad = 0,
    };

    struct backend_value *val = bpf_map_lookup_elem(&backend_map, &key);
    if (!val || val->count == 0)
        return 0;  /* deny -> EPERM: no backends registered */

    /* --- Firewall: namespace isolation --- */
    __u64 src_cgroup = bpf_get_current_cgroup_id();

    struct cgroup_ns_key ns_key = { .cgroup_id = src_cgroup };
    struct cgroup_ns_value *src_ns = bpf_map_lookup_elem(
        &cgroup_namespace_map, &ns_key);

    if (src_ns && src_ns->namespace_id != val->namespace_id) {
        /* Cross-namespace connection. Check firewall_map for allow. */
        struct firewall_key fw_key = {
            .src_cgroup_id = src_cgroup,
            .dst_app_id    = val->app_id,
            ._pad          = 0,
        };
        struct firewall_value *fw = bpf_map_lookup_elem(
            &firewall_map, &fw_key);
        if (!fw || fw->action == FIREWALL_DENY)
            return 0;  /* deny -> EPERM: cross-namespace denied */
    }

    /* --- Backend selection: round-robin among healthy --- */
    __u32 selected_idx = 0;
    int found = 0;

    /* Try up to count times to find a healthy backend.
     * We increment rr_index non-atomically. BPF map lookups return
     * a pointer to a copy, so true atomicity isn't possible anyway.
     * The slight skew from concurrent access is acceptable for
     * round-robin — it's still roughly even distribution. */
    __u32 rr = val->rr_index;

    #pragma unroll
    for (int i = 0; i < MAX_BACKENDS; i++) {
        if (i >= val->count)
            break;

        __u32 idx = (rr + i) % val->count;

        if (idx < MAX_BACKENDS && val->backends[idx].healthy == 1) {
            selected_idx = idx;
            found = 1;
            val->rr_index = rr + i + 1;
            break;
        }
    }

    if (!found)
        return 0;  /* deny -> EPERM: no healthy backends */

    /* Rewrite destination to the selected backend */
    struct backend_endpoint *be = &val->backends[selected_idx];
    ctx->user_ip4  = be->host_ip;
    ctx->user_port = be->host_port;

    return 1;  /* proceed with connect() to the rewritten address */
}

/* ---------- Connect6 hook ------------------------------------------------ */

/* IPv6 egress policy. Without this hook a dual-stack workload bypasses the
 * whole allowlist by connecting over IPv6 (NET7). VIP rewrite stays
 * v4-only: VIPs live in 127.128.0.0/16 and the service map never hands out
 * IPv6 backends, so this program is pure policy — no rewriting.
 */
SEC("cgroup/connect6")
int onion_connect6(struct bpf_sock_addr *ctx)
{
    __u64 cg = bpf_get_current_cgroup_id();
    __u32 *enforced = bpf_map_lookup_elem(&egress_enabled_map, &cg);
    if (!enforced || *enforced != 1)
        return 1;  /* no enforcement for this cgroup */

    return egress6_allowed(cg, ctx);
}

/* Unconnected UDP uses sendmsg()/sendto() without invoking connect hooks.
 * Mirror the same policy at both sendmsg hooks so protocol choice cannot
 * bypass a declared allowlist. */
SEC("cgroup/sendmsg4")
int onion_sendmsg4(struct bpf_sock_addr *ctx)
{
    __u64 cg = bpf_get_current_cgroup_id();
    __u32 *enforced = bpf_map_lookup_elem(&egress_enabled_map, &cg);
    if (!enforced || *enforced != 1)
        return 1;
    if ((bpf_ntohl(ctx->user_ip4) & VIP_MASK) == VIP_PREFIX)
        return 1;
    return egress4_allowed(cg, ctx->user_ip4, ctx->user_port);
}

SEC("cgroup/sendmsg6")
int onion_sendmsg6(struct bpf_sock_addr *ctx)
{
    __u64 cg = bpf_get_current_cgroup_id();
    __u32 *enforced = bpf_map_lookup_elem(&egress_enabled_map, &cg);
    if (!enforced || *enforced != 1)
        return 1;
    return egress6_allowed(cg, ctx);
}

char _license[] SEC("license") = "GPL";
