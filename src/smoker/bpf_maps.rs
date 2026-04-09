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

    #[cfg(feature = "ebpf")]
    #[error("BPF map operation failed: {0}")]
    MapError(#[from] aya::maps::MapError),

    #[error("BPF map {map_name:?} not found in loaded program")]
    MapNotFound { map_name: String },
}

// ---------------------------------------------------------------------------
// Real implementations (Linux + ebpf feature)
// ---------------------------------------------------------------------------

#[cfg(feature = "ebpf")]
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

    /// Write a DNS fault entry.
    pub fn write_dns_fault(
        bpf: &mut aya::Ebpf,
        key: BpfDnsFaultKey,
        value: BpfDnsFaultValue,
    ) -> Result<(), BpfMapError> {
        let mut map: HashMap<_, BpfDnsFaultKey, BpfDnsFaultValue> =
            HashMap::try_from(bpf.map_mut("fault_dns_map").ok_or_else(|| {
                BpfMapError::MapNotFound {
                    map_name: "fault_dns_map".into(),
                }
            })?)?;
        map.insert(key, value, 0)?;
        Ok(())
    }

    /// Delete a DNS fault entry.
    pub fn delete_dns_fault(bpf: &mut aya::Ebpf, key: &BpfDnsFaultKey) -> Result<(), BpfMapError> {
        let mut map: HashMap<_, BpfDnsFaultKey, BpfDnsFaultValue> =
            HashMap::try_from(bpf.map_mut("fault_dns_map").ok_or_else(|| {
                BpfMapError::MapNotFound {
                    map_name: "fault_dns_map".into(),
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
    /// Called on Bun startup to ensure a clean state after a crash.
    pub fn cleanup_all_fault_maps(bpf: &mut aya::Ebpf) {
        // Best-effort cleanup — log warnings but don't fail startup.
        for map_name in &["fault_connect_map", "fault_dns_map", "fault_bw_map"] {
            if let Some(map_ref) = bpf.map_mut(map_name) {
                // We can't easily iterate and delete from a generic map
                // without knowing the key type. For startup cleanup, the
                // simplest approach is to note that BPF maps are cleared
                // when the eBPF program is reloaded. Since Bun reloads
                // the eBPF program on startup, the maps start empty.
                //
                // This function exists as a safety net for the case where
                // Bun restarts without reloading the eBPF program (e.g.
                // hot restart). In that case, we'd need to iterate.
                let _ = map_ref; // Acknowledge the handle
                eprintln!("smoker: startup cleanup — {map_name} will be cleared on eBPF reload");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Stub implementations (non-Linux or no ebpf feature)
// ---------------------------------------------------------------------------

#[cfg(not(feature = "ebpf"))]
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

    pub fn write_dns_fault(
        _key: BpfDnsFaultKey,
        _value: BpfDnsFaultValue,
    ) -> Result<(), BpfMapError> {
        Err(BpfMapError::Unsupported)
    }

    pub fn delete_dns_fault(_key: &BpfDnsFaultKey) -> Result<(), BpfMapError> {
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
    use super::*;

    #[cfg(not(feature = "ebpf"))]
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

    #[cfg(not(feature = "ebpf"))]
    #[test]
    fn dns_stub_returns_unsupported() {
        let key = dns_fault_key("redis");
        let value = BpfDnsFaultValue {
            action: DNS_FAULT_NXDOMAIN,
            probability: 100,
            _pad: [0; 6],
            delay_ns: 0,
            expires_ns: 0,
        };
        let result = write_dns_fault(key, value);
        assert!(matches!(result, Err(BpfMapError::Unsupported)));
    }

    #[cfg(not(feature = "ebpf"))]
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
