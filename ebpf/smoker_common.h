/* Shared definitions for Smoker fault injection eBPF programs.
 *
 * These structs MUST match the #[repr(C)] Rust types in
 * src/smoker/bpf_types.rs exactly. Any change here requires a
 * corresponding change there.
 */
#ifndef SMOKER_COMMON_H
#define SMOKER_COMMON_H

/* ---------- Fault action constants --------------------------------------- */

#define FAULT_ACTION_NONE       0
#define FAULT_ACTION_DROP       1
#define FAULT_ACTION_DELAY      2
#define FAULT_ACTION_PARTITION  3

#define DNS_FAULT_NONE          0
#define DNS_FAULT_NXDOMAIN      1
#define DNS_FAULT_DELAY         2

/* ---------- fault_connect_map -------------------------------------------- */

/* Key: 16 bytes */
struct fault_connect_key {
    __u32 virtual_ip;         /* Onion-assigned VIP, network byte order */
    __u16 port;               /* network byte order */
    __u16 _pad;
    __u64 source_cgroup_id;   /* 0 = all callers, non-zero = partition */
};

/* Value: 32 bytes */
struct fault_connect_value {
    __u8  action;             /* FAULT_ACTION_* */
    __u8  probability;        /* 0-100 (FAULT_ACTION_DROP) */
    __u8  _pad[6];
    __u64 delay_ns;           /* nanoseconds (FAULT_ACTION_DELAY) */
    __u64 jitter_ns;          /* +/- random range (FAULT_ACTION_DELAY) */
    __u64 expires_ns;         /* CLOCK_MONOTONIC ns, 0 = no expiry */
};

/* ---------- fault_dns_map ------------------------------------------------ */

/* Key: 4 bytes */
struct fault_dns_key {
    __u32 service_name_hash;  /* FNV-1a hash of the service name */
};

/* Value: 24 bytes */
struct fault_dns_value {
    __u8  action;             /* DNS_FAULT_* */
    __u8  probability;        /* 0-100 */
    __u8  _pad[6];
    __u64 delay_ns;           /* nanoseconds (DNS_FAULT_DELAY) */
    __u64 expires_ns;         /* CLOCK_MONOTONIC ns, 0 = no expiry */
};

/* ---------- fault_bw_map ------------------------------------------------- */

/* Key: 8 bytes */
struct fault_bw_key {
    __u32 virtual_ip;         /* Onion-assigned VIP */
    __u16 port;               /* network byte order */
    __u16 _pad;
};

/* Value: 32 bytes */
struct fault_bw_value {
    __u64 rate_bytes_per_sec;
    __u64 tokens;             /* token bucket: current count */
    __u64 last_refill_ns;     /* token bucket: last refill timestamp */
    __u64 expires_ns;         /* CLOCK_MONOTONIC ns, 0 = no expiry */
};

/* ---------- fault_state_map ---------------------------------------------- */

/* Key: 4 bytes (ARRAY index, always 0) */
struct fault_state_key {
    __u32 index;
};

/* Value: 32 bytes (one per CPU) */
struct fault_state_value {
    __u64 prng_state;         /* xorshift64 PRNG seed */
    __u64 faults_injected;    /* counter */
    __u64 lookups;            /* counter */
    __u64 scratch_ts;         /* delay timer bookkeeping */
};

#endif /* SMOKER_COMMON_H */
