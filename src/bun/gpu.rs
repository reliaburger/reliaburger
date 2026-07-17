//! GPU detection.
//!
//! Discovers NVIDIA GPUs on a node so the scheduler can place GPU
//! workloads and the supervisor can refuse a GPU request the node can't
//! back. Detection probes the `/dev/nvidia*` device nodes and, when a
//! device is present, enumerates cards with `nvidia-smi`. It falls back
//! cleanly to "no GPUs" on any node without the hardware or the tool, so
//! it is safe to run everywhere.

use std::path::Path;

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
/// Injected in tests and used on nodes where GPU support is disabled.
pub struct StubGpuDetector;

impl GpuDetector for StubGpuDetector {
    fn detect(&self) -> Vec<GpuInfo> {
        Vec::new()
    }
}

/// A detector that probes for real NVIDIA hardware.
///
/// Checks for a `/dev/nvidia0` device node first (cheap, no process
/// spawn), then parses `nvidia-smi --query-gpu` output. Any missing
/// device, missing tool or unparseable line yields no GPUs — a node
/// without an NVIDIA card behaves exactly like [`StubGpuDetector`].
pub struct NvidiaGpuDetector;

impl GpuDetector for NvidiaGpuDetector {
    fn detect(&self) -> Vec<GpuInfo> {
        // Only Linux exposes /dev/nvidia* and ships nvidia-smi in the way we
        // parse; on other targets there is nothing to detect.
        if !cfg!(target_os = "linux") {
            return Vec::new();
        }
        if !Path::new("/dev/nvidia0").exists() {
            return Vec::new();
        }
        let output = std::process::Command::new("nvidia-smi")
            .args([
                "--query-gpu=index,name,memory.total",
                "--format=csv,noheader,nounits",
            ])
            .output();
        match output {
            Ok(out) if out.status.success() => {
                parse_nvidia_smi(&String::from_utf8_lossy(&out.stdout))
            }
            _ => Vec::new(),
        }
    }
}

/// Parse `nvidia-smi --query-gpu=index,name,memory.total
/// --format=csv,noheader,nounits` output into [`GpuInfo`] records.
///
/// Each line is `index, name, memory_total_mib`. Memory is reported in
/// MiB by `nvidia-smi`; we convert to bytes. Unparseable lines are
/// skipped rather than failing the whole probe.
fn parse_nvidia_smi(text: &str) -> Vec<GpuInfo> {
    let mut gpus = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split(',').map(str::trim);
        let (Some(index), Some(name), Some(vram_mib)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let (Ok(index), Ok(vram_mib)) = (index.parse::<u32>(), vram_mib.parse::<u64>()) else {
            continue;
        };
        gpus.push(GpuInfo {
            index,
            name: name.to_string(),
            vram_bytes: vram_mib * 1024 * 1024,
        });
    }
    gpus
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
    fn parses_two_gpus_from_nvidia_smi() {
        let text = "0, NVIDIA A100-SXM4-40GB, 40960\n1, NVIDIA A100-SXM4-40GB, 40960\n";
        let gpus = parse_nvidia_smi(text);
        assert_eq!(gpus.len(), 2);
        assert_eq!(gpus[0].index, 0);
        assert_eq!(gpus[0].name, "NVIDIA A100-SXM4-40GB");
        assert_eq!(gpus[0].vram_bytes, 40960 * 1024 * 1024);
        assert_eq!(gpus[1].index, 1);
    }

    #[test]
    fn skips_unparseable_lines() {
        let text = "garbage line\n0, NVIDIA L4, 24576\n\n";
        let gpus = parse_nvidia_smi(text);
        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].name, "NVIDIA L4");
    }

    #[test]
    fn empty_output_yields_no_gpus() {
        assert!(parse_nvidia_smi("").is_empty());
        assert!(parse_nvidia_smi("\n\n").is_empty());
    }

    /// On a node without an NVIDIA card the real detector must behave like
    /// the stub. This is portable: CI runners have no `/dev/nvidia0`. The
    /// GPU-present path is covered by the parser tests above and an
    /// `#[ignore]` hardware test.
    #[test]
    fn nvidia_detector_empty_without_hardware() {
        if !Path::new("/dev/nvidia0").exists() {
            assert!(NvidiaGpuDetector.detect().is_empty());
        }
    }

    /// Real-hardware acceptance: on a node with an NVIDIA GPU, the detector
    /// enumerates at least one card. Gated so it only runs where the
    /// hardware and `nvidia-smi` exist (`make test-linux`).
    #[test]
    #[ignore = "requires an NVIDIA GPU and nvidia-smi (RELIABURGER_GPU_TESTS)"]
    fn nvidia_detector_finds_hardware() {
        let gpus = NvidiaGpuDetector.detect();
        assert!(
            !gpus.is_empty(),
            "expected at least one GPU on a GPU-provisioned node"
        );
    }
}
