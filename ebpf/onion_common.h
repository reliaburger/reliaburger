/* Shared definitions for Onion eBPF programs.
 *
 * These structs MUST match the #[repr(C)] Rust types in
 * src/onion/types.rs exactly. Any change here requires a
 * corresponding change there.
 */
#ifndef ONION_COMMON_H
#define ONION_COMMON_H

#include <linux/bpf.h>
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_endian.h>

/* ---------- Constants --------------------------------------------------- */

#define MAX_BACKENDS        32
#define MAX_DNS_NAME_LEN    256
#define VIP_PREFIX          0x7F800000  /* 127.128.0.0 */
#define VIP_MASK            0xFFFF0000  /* /16 */
#define DNS_PORT            53
#define FIREWALL_DENY       0
#define FIREWALL_ALLOW      1

/* ---------- dns_map ----------------------------------------------------- */

struct dns_key {
    char name[MAX_DNS_NAME_LEN];  /* null-terminated, lowercase */
};

struct dns_value {
    __u32 vip;  /* network byte order */
};

/* ---------- backend_map ------------------------------------------------- */

struct backend_key {
    __u32 vip;    /* network byte order */
    __u16 port;   /* network byte order */
    __u16 _pad;
};

struct backend_endpoint {
    __u32 host_ip;    /* network byte order */
    __u16 host_port;  /* network byte order */
    __u8  healthy;    /* 1 = healthy, 0 = unhealthy */
    __u8  _pad;
};

struct backend_value {
    __u32 count;
    __u32 rr_index;
    __u32 app_id;
    __u32 namespace_id;
    struct backend_endpoint backends[MAX_BACKENDS];
};

/* ---------- firewall_map ------------------------------------------------ */

struct firewall_key {
    __u64 src_cgroup_id;
    __u32 dst_app_id;
    __u32 _pad;
};

struct firewall_value {
    __u32 action;  /* FIREWALL_DENY or FIREWALL_ALLOW */
};

/* ---------- cgroup_namespace_map ---------------------------------------- */

struct cgroup_ns_key {
    __u64 cgroup_id;
};

struct cgroup_ns_value {
    __u32 namespace_id;
};

/* ---------- egress_map -------------------------------------------------- */

struct egress_key {
    __u64 src_cgroup_id;   /* cgroup of the connecting app */
    __u32 dst_ip;          /* destination IP, network byte order */
    __u16 dst_port;        /* destination port, network byte order */
    __u16 _pad;
};

struct egress_value {
    __u32 action;          /* 1 = allow */
};

/* ---------- dns_pending_map (internal, sendmsg -> recvmsg) -------------- */

struct pending_dns_response {
    __u32 vip;                        /* VIP to return, network byte order */
    __u16 query_id;                   /* DNS transaction ID */
    __u16 qname_len;                  /* length of the query name */
    char  qname[MAX_DNS_NAME_LEN];    /* original query name */
};

/* ---------- DNS wire format helpers ------------------------------------- */

struct dns_header {
    __u16 id;
    __u16 flags;
    __u16 qdcount;
    __u16 ancount;
    __u16 nscount;
    __u16 arcount;
} __attribute__((packed));

#define DNS_QR_RESPONSE   0x8000
#define DNS_AA_FLAG       0x0400
#define DNS_TYPE_A        1
#define DNS_CLASS_IN      1

#endif /* ONION_COMMON_H */
