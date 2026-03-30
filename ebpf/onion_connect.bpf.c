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

/* ---------- Connect hook ------------------------------------------------ */

SEC("cgroup/connect4")
int onion_connect(struct bpf_sock_addr *ctx)
{
    __u32 dst_ip = bpf_ntohl(ctx->user_ip4);

    /* Only intercept VIPs in the 127.128.0.0/16 range */
    if ((dst_ip & VIP_MASK) != VIP_PREFIX)
        return 1;  /* not a VIP, pass through */

    /* Look up the backend list for this (VIP, port) */
    struct backend_key key = {
        .vip  = ctx->user_ip4,   /* keep network byte order */
        .port = ctx->user_port,
        ._pad = 0,
    };

    struct backend_value *val = bpf_map_lookup_elem(&backend_map, &key);
    if (!val || val->count == 0)
        return 0;  /* -ECONNREFUSED: no backends registered */

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
            return 0;  /* -ECONNREFUSED: cross-namespace denied */
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
        return 0;  /* -ECONNREFUSED: no healthy backends */

    /* Rewrite destination to the selected backend */
    struct backend_endpoint *be = &val->backends[selected_idx];
    ctx->user_ip4  = be->host_ip;
    ctx->user_port = be->host_port;

    return 1;  /* proceed with connect() to the rewritten address */
}

char _license[] SEC("license") = "GPL";
