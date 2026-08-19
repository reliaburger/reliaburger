//! BPF map operations for Smoker fault injection.
//!
//! Typed wrappers for writing and deleting fault entries in the BPF
//! maps. On non-Linux or without the `ebpf` feature, these are no-ops
//! that return an error explaining the requirement.

use super::bpf_types::*;

/// Errors from BPF map operations.
#[derive(Debug, thiserror::Error)]
pub enum BpfMapError {
    #[error("eBPF fault injection requires Linux with --features ebpf")]
    Unsupported,

    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    #[error("BPF map operation failed: {0}")]
    MapError(#[from] aya::maps::MapError),

    #[error("BPF map {map_name:?} not found in loaded program")]
    MapNotFound { map_name: String },
}

// ---------------------------------------------------------------------------
// Real implementations (Linux + ebpf feature)
// ---------------------------------------------------------------------------

#[cfg(all(feature = "ebpf", target_os = "linux"))]
mod inner {
    use super::*;
    use aya::maps::HashMap;

    /// Write a connect fault entry (drop, delay, or partition).
    pub fn write_connect_fault(
        bpf: &mut aya::Ebpf,
        key: BpfConnectFaultKey,
        value: BpfConnectFaultValue,
    ) -> Result<(), BpfMapError> {
        let mut map: HashMap<_, BpfConnectFaultKey, BpfConnectFaultValue> =
            HashMap::try_from(bpf.map_mut("fault_connect_map").ok_or_else(|| {
                BpfMapError::MapNotFound {
                    map_name: "fault_connect_map".into(),
                }
            })?)?;
        map.insert(key, value, 0)?;
        Ok(())
    }

    /// Delete a connect fault entry.
    pub fn delete_connect_fault(
        bpf: &mut aya::Ebpf,
        key: &BpfConnectFaultKey,
    ) -> Result<(), BpfMapError> {
        let mut map: HashMap<_, BpfConnectFaultKey, BpfConnectFaultValue> =
            HashMap::try_from(bpf.map_mut("fault_connect_map").ok_or_else(|| {
                BpfMapError::MapNotFound {
                    map_name: "fault_connect_map".into(),
                }
            })?)?;
        map.remove(key)?;
        Ok(())
    }

    /// Write a bandwidth fault entry.
    pub fn write_bw_fault(
        bpf: &mut aya::Ebpf,
        key: BpfBandwidthFaultKey,
        value: BpfBandwidthFaultValue,
    ) -> Result<(), BpfMapError> {
        let mut map: HashMap<_, BpfBandwidthFaultKey, BpfBandwidthFaultValue> =
            HashMap::try_from(bpf.map_mut("fault_bw_map").ok_or_else(|| {
                BpfMapError::MapNotFound {
                    map_name: "fault_bw_map".into(),
                }
            })?)?;
        map.insert(key, value, 0)?;
        Ok(())
    }

    /// Delete a bandwidth fault entry.
    pub fn delete_bw_fault(
        bpf: &mut aya::Ebpf,
        key: &BpfBandwidthFaultKey,
    ) -> Result<(), BpfMapError> {
        let mut map: HashMap<_, BpfBandwidthFaultKey, BpfBandwidthFaultValue> =
            HashMap::try_from(bpf.map_mut("fault_bw_map").ok_or_else(|| {
                BpfMapError::MapNotFound {
                    map_name: "fault_bw_map".into(),
                }
            })?)?;
        map.remove(key)?;
        Ok(())
    }

    /// Delete all entries from all fault BPF maps.
    ///
    /// Called on Bun startup so a hot restart (which reuses pinned maps rather
    /// than reloading the program) doesn't inherit a departed run's stale fault
    /// rules (M21). Best-effort: iterate each map's keys and remove them; a map
    /// that isn't present is simply skipped.
    pub fn cleanup_all_fault_maps(bpf: &mut aya::Ebpf) {
        if let Some(map_ref) = bpf.map_mut("fault_connect_map")
            && let Ok(mut map) =
                HashMap::<_, BpfConnectFaultKey, BpfConnectFaultValue>::try_from(map_ref)
        {
            let keys: Vec<_> = map.keys().filter_map(|k| k.ok()).collect();
            for key in keys {
                let _ = map.remove(&key);
            }
        }
        if let Some(map_ref) = bpf.map_mut("fault_bw_map")
            && let Ok(mut map) =
                HashMap::<_, BpfBandwidthFaultKey, BpfBandwidthFaultValue>::try_from(map_ref)
        {
            let keys: Vec<_> = map.keys().filter_map(|k| k.ok()).collect();
            for key in keys {
                let _ = map.remove(&key);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Stub implementations (non-Linux or no ebpf feature)
// ---------------------------------------------------------------------------

#[cfg(not(all(feature = "ebpf", target_os = "linux")))]
mod inner {
    use super::*;

    pub fn write_connect_fault(
        _key: BpfConnectFaultKey,
        _value: BpfConnectFaultValue,
    ) -> Result<(), BpfMapError> {
        Err(BpfMapError::Unsupported)
    }

    pub fn delete_connect_fault(_key: &BpfConnectFaultKey) -> Result<(), BpfMapError> {
        Err(BpfMapError::Unsupported)
    }

    pub fn write_bw_fault(
        _key: BpfBandwidthFaultKey,
        _value: BpfBandwidthFaultValue,
    ) -> Result<(), BpfMapError> {
        Err(BpfMapError::Unsupported)
    }

    pub fn delete_bw_fault(_key: &BpfBandwidthFaultKey) -> Result<(), BpfMapError> {
        Err(BpfMapError::Unsupported)
    }

    pub fn cleanup_all_fault_maps() {
        // No-op without eBPF
    }
}

// Re-export the active implementation
pub use inner::*;

#[cfg(test)]
mod tests {
    #[cfg(not(all(feature = "ebpf", target_os = "linux")))]
    use super::*;

    #[cfg(not(all(feature = "ebpf", target_os = "linux")))]
    #[test]
    fn stubs_return_unsupported() {
        let key = connect_fault_key(0x7F800003, 6379);
        let value = BpfConnectFaultValue {
            action: FAULT_ACTION_DROP,
            probability: 100,
            _pad: [0; 6],
            delay_ns: 0,
            jitter_ns: 0,
            expires_ns: 0,
        };
        let result = write_connect_fault(key, value);
        assert!(matches!(result, Err(BpfMapError::Unsupported)));
    }

    #[cfg(not(all(feature = "ebpf", target_os = "linux")))]
    #[test]
    fn bw_stub_returns_unsupported() {
        let key = bandwidth_fault_key(0x7F800003, 80);
        let value = BpfBandwidthFaultValue {
            rate_bytes_per_sec: 1024 * 1024,
            tokens: 0,
            last_refill_ns: 0,
            expires_ns: 0,
        };
        let result = write_bw_fault(key, value);
        assert!(matches!(result, Err(BpfMapError::Unsupported)));
    }
}
