//! GPU detection types.
//!
//! Defines the interface for discovering GPUs on a node. The real
//! implementation (NVML) comes later; for now we provide a stub that
//! reports no GPUs, which is enough for scheduling logic and tests.

/// Information about a single GPU device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuInfo {
    /// Device index (0-based).
    pub index: u32,
    /// Human-readable device name, e.g. "NVIDIA A100".
    pub name: String,
    /// Total video memory in bytes.
    pub vram_bytes: u64,
}

/// Discovers GPUs available on the current node.
///
/// Implemented as a trait so tests can inject fake GPUs without
/// requiring actual hardware. The compiler monomorphises generic code
/// over concrete detector types, so there's no virtual dispatch cost.
pub trait GpuDetector {
    /// Return all GPUs visible to this node.
    fn detect(&self) -> Vec<GpuInfo>;
}

/// A detector that always reports no GPUs.
///
/// Used on nodes without GPU hardware, and as a placeholder until
/// NVML integration is implemented.
// TODO(Phase 1): replace with NvmlGpuDetector when GPU support is added
pub struct StubGpuDetector;

impl GpuDetector for StubGpuDetector {
    fn detect(&self) -> Vec<GpuInfo> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_detector_returns_empty() {
        let detector = StubGpuDetector;
        assert!(detector.detect().is_empty());
    }

    #[test]
    fn gpu_info_fields_accessible() {
        let gpu = GpuInfo {
            index: 0,
            name: "NVIDIA A100".to_string(),
            vram_bytes: 80 * 1024 * 1024 * 1024, // 80 GiB
        };
        assert_eq!(gpu.index, 0);
        assert_eq!(gpu.name, "NVIDIA A100");
        assert_eq!(gpu.vram_bytes, 85_899_345_920);
    }

    #[test]
    fn gpu_info_equality() {
        let a = GpuInfo {
            index: 0,
            name: "A100".to_string(),
            vram_bytes: 1024,
        };
        let b = a.clone();
        assert_eq!(a, b);
    }

    #[test]
    fn gpu_info_debug_format() {
        let gpu = GpuInfo {
            index: 1,
            name: "T4".to_string(),
            vram_bytes: 16_000_000_000,
        };
        let debug = format!("{gpu:?}");
        assert!(debug.contains("T4"));
        assert!(debug.contains("16000000000"));
    }
}
