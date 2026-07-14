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
// TODO(Phase 2): replace with NvmlGpuDetector when GPU scheduling is added
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
}
