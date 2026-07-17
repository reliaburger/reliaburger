//! BPF map key/value structs for Smoker fault injection.
//!
//! These `#[repr(C)]` structs match the C definitions in
//! `ebpf/smoker_common.h`. They must stay in sync. All padding
//! is explicit so there are no holes.

// ---------------------------------------------------------------------------
// Fault action constants (match smoker_common.h)
// ---------------------------------------------------------------------------

/// No fault — normal path.
pub const FAULT_ACTION_NONE: u8 = 0;
/// Return ECONNREFUSED on connect().
pub const FAULT_ACTION_DROP: u8 = 1;
/// Delay the connection by N nanoseconds.
pub const FAULT_ACTION_DELAY: u8 = 2;
/// Block connections from a specific source cgroup.
pub const FAULT_ACTION_PARTITION: u8 = 3;

// DNS faults act in the userspace `.internal` responder, not in the kernel,
// so there is no DNS fault map here (see `crate::onion::dns::DnsFaultState`).
// The in-kernel DNS eBPF object was never loaded, so its fault map was dead.

// ---------------------------------------------------------------------------
// fault_connect_map
// ---------------------------------------------------------------------------

/// Key for `fault_connect_map`.
///
/// BPF map type: `BPF_MAP_TYPE_HASH`, max_entries: 4096.
/// Identifies a target service by its virtual IP, port, and optionally
/// a source cgroup (for partition faults).
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct BpfConnectFaultKey {
    /// Virtual IP of the target service (Onion-assigned, e.g. 127.128.0.3).
    pub virtual_ip: u32,
    /// Target port (network byte order).
    pub port: u16,
    pub _pad: u16,
    /// Source cgroup ID. 0 = match all callers.
    /// Non-zero = only match connections from this cgroup (partition faults).
    pub source_cgroup_id: u64,
}

/// Value for `fault_connect_map`.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct BpfConnectFaultValue {
    /// Fault action: FAULT_ACTION_NONE/DROP/DELAY/PARTITION.
    pub action: u8,
    /// Drop probability 0-100 (action = DROP).
    pub probability: u8,
    pub _pad: [u8; 6],
    /// Delay in nanoseconds (action = DELAY).
    pub delay_ns: u64,
    /// Jitter in nanoseconds (action = DELAY). Actual delay = delay_ns +/- rand(jitter_ns).
    pub jitter_ns: u64,
    /// Expiry timestamp (CLOCK_MONOTONIC, nanoseconds). 0 = no expiry.
    pub expires_ns: u64,
}

// ---------------------------------------------------------------------------
// fault_bw_map
// ---------------------------------------------------------------------------

/// Key for `fault_bw_map`.
///
/// BPF map type: `BPF_MAP_TYPE_HASH`, max_entries: 1024.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct BpfBandwidthFaultKey {
    /// Virtual IP of the target service.
    pub virtual_ip: u32,
    /// Target port (network byte order).
    pub port: u16,
    pub _pad: u16,
}

/// Value for `fault_bw_map`.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct BpfBandwidthFaultValue {
    /// Rate limit in bytes per second.
    pub rate_bytes_per_sec: u64,
    /// Token bucket: current token count (bytes).
    pub tokens: u64,
    /// Token bucket: last refill timestamp (CLOCK_MONOTONIC, nanoseconds).
    pub last_refill_ns: u64,
    /// Expiry timestamp (CLOCK_MONOTONIC, nanoseconds). 0 = no expiry.
    pub expires_ns: u64,
}

// ---------------------------------------------------------------------------
// fault_state_map
// ---------------------------------------------------------------------------

/// Key for `fault_state_map`.
///
/// BPF map type: `BPF_MAP_TYPE_ARRAY`, max_entries: 1.
/// Single shared entry for PRNG state and counters.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct BpfFaultStateKey {
    /// Always 0 (single entry per CPU, PERCPU_ARRAY handles per-CPU storage).
    pub index: u32,
}

/// Value for `fault_state_map` (one per CPU).
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct BpfFaultStateValue {
    /// PRNG seed for this CPU (xorshift64).
    pub prng_state: u64,
    /// Counter: total faults injected on this CPU.
    pub faults_injected: u64,
    /// Counter: total fault map lookups on this CPU.
    pub lookups: u64,
    /// Scratch: timestamp for delay timer bookkeeping.
    pub scratch_ts: u64,
}

// ---------------------------------------------------------------------------
// aya Pod implementations (Linux + ebpf feature only)
// ---------------------------------------------------------------------------

// SAFETY: All BPF map structs are #[repr(C)], Copy, 'static, and have
// no padding holes (all padding is explicit via _pad fields). This makes
// them safe to interpret as raw bytes for BPF map operations.
#[cfg(feature = "ebpf")]
unsafe impl aya::Pod for BpfConnectFaultKey {}
#[cfg(feature = "ebpf")]
unsafe impl aya::Pod for BpfConnectFaultValue {}
#[cfg(feature = "ebpf")]
unsafe impl aya::Pod for BpfBandwidthFaultKey {}
#[cfg(feature = "ebpf")]
unsafe impl aya::Pod for BpfBandwidthFaultValue {}
#[cfg(feature = "ebpf")]
unsafe impl aya::Pod for BpfFaultStateKey {}
#[cfg(feature = "ebpf")]
unsafe impl aya::Pod for BpfFaultStateValue {}

/// Build a connect fault key for a service VIP + port (all callers).
pub fn connect_fault_key(virtual_ip: u32, port: u16) -> BpfConnectFaultKey {
    BpfConnectFaultKey {
        virtual_ip,
        port,
        _pad: 0,
        source_cgroup_id: 0,
    }
}

/// Build a connect fault key for a partition (specific source cgroup).
pub fn partition_fault_key(
    virtual_ip: u32,
    port: u16,
    source_cgroup_id: u64,
) -> BpfConnectFaultKey {
    BpfConnectFaultKey {
        virtual_ip,
        port,
        _pad: 0,
        source_cgroup_id,
    }
}

/// Build a bandwidth fault key.
pub fn bandwidth_fault_key(virtual_ip: u32, port: u16) -> BpfBandwidthFaultKey {
    BpfBandwidthFaultKey {
        virtual_ip,
        port,
        _pad: 0,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Size assertions — must match the C struct sizes in smoker_common.h.
    // If any of these fail, the Rust and C layouts have diverged.

    #[test]
    fn connect_fault_key_size() {
        assert_eq!(std::mem::size_of::<BpfConnectFaultKey>(), 16);
    }

    #[test]
    fn connect_fault_value_size() {
        assert_eq!(std::mem::size_of::<BpfConnectFaultValue>(), 32); // 8 header + 3x u64
    }

    #[test]
    fn bandwidth_fault_key_size() {
        assert_eq!(std::mem::size_of::<BpfBandwidthFaultKey>(), 8);
    }

    #[test]
    fn bandwidth_fault_value_size() {
        assert_eq!(std::mem::size_of::<BpfBandwidthFaultValue>(), 32);
    }

    #[test]
    fn fault_state_key_size() {
        assert_eq!(std::mem::size_of::<BpfFaultStateKey>(), 4);
    }

    #[test]
    fn fault_state_value_size() {
        assert_eq!(std::mem::size_of::<BpfFaultStateValue>(), 32);
    }

    // Field offset assertions (verify alignment matches C expectations)

    #[test]
    fn connect_fault_key_field_offsets() {
        let key = BpfConnectFaultKey {
            virtual_ip: 0,
            port: 0,
            _pad: 0,
            source_cgroup_id: 0,
        };
        let base = &key as *const _ as usize;
        assert_eq!(&key.virtual_ip as *const _ as usize - base, 0);
        assert_eq!(&key.port as *const _ as usize - base, 4);
        assert_eq!(&key.source_cgroup_id as *const _ as usize - base, 8);
    }

    #[test]
    fn connect_fault_value_field_offsets() {
        let val = BpfConnectFaultValue {
            action: 0,
            probability: 0,
            _pad: [0; 6],
            delay_ns: 0,
            jitter_ns: 0,
            expires_ns: 0,
        };
        let base = &val as *const _ as usize;
        assert_eq!(&val.action as *const _ as usize - base, 0);
        assert_eq!(&val.probability as *const _ as usize - base, 1);
        assert_eq!(&val.delay_ns as *const _ as usize - base, 8);
        assert_eq!(&val.jitter_ns as *const _ as usize - base, 16);
        assert_eq!(&val.expires_ns as *const _ as usize - base, 24);
    }

    // Key builder tests

    #[test]
    fn connect_fault_key_all_callers() {
        let key = connect_fault_key(0x7F800003, 6379);
        assert_eq!(key.virtual_ip, 0x7F800003);
        assert_eq!(key.port, 6379);
        assert_eq!(key.source_cgroup_id, 0);
    }

    #[test]
    fn partition_fault_key_sets_cgroup() {
        let key = partition_fault_key(0x7F800003, 6379, 12345);
        assert_eq!(key.source_cgroup_id, 12345);
    }

    #[test]
    fn bandwidth_fault_key_builder() {
        let key = bandwidth_fault_key(0x7F800003, 80);
        assert_eq!(key.virtual_ip, 0x7F800003);
        assert_eq!(key.port, 80);
    }
}
