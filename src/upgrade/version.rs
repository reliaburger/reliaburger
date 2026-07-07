//! Binary version handling: parsing, ordering, and runtime resolution.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use super::error::UpgradeError;

/// A reliaburger binary version.
///
/// Wraps a semantic version; parsing accepts an optional leading `v`, and
/// display always includes one, matching binary file names (`bun-v0.2.0`)
/// and CLI usage (`relish upgrade start v0.2.0`).
///
/// Ordering follows semver precedence, including pre-release rules:
/// `v1.0.0-rc.1 < v1.0.0`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BinaryVersion(semver::Version);

impl BinaryVersion {
    /// File name for this version with the given binary stem,
    /// e.g. `file_name("bun")` for `v0.2.0` is `"bun-v0.2.0"`.
    pub fn file_name(&self, stem: &str) -> String {
        format!("{stem}-{self}")
    }
}

impl FromStr for BinaryVersion {
    type Err = UpgradeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();
        let stripped = trimmed.strip_prefix('v').unwrap_or(trimmed);
        if stripped.is_empty() {
            return Err(UpgradeError::InvalidVersion {
                input: s.to_string(),
                reason: "empty version".to_string(),
            });
        }
        let version =
            semver::Version::parse(stripped).map_err(|e| UpgradeError::InvalidVersion {
                input: s.to_string(),
                reason: e.to_string(),
            })?;
        Ok(Self(version))
    }
}

impl fmt::Display for BinaryVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "v{}", self.0)
    }
}

// Serialised as the display string ("v1.2.3") rather than a struct, so the
// same value reads naturally in JSON API responses, marker files, and TOML.
impl serde::Serialize for BinaryVersion {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for BinaryVersion {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Resolve the version this process should report.
///
/// In debug builds, a sidecar file `{exe}.version` next to the resolved
/// (symlink-free) executable overrides the compiled-in version. Integration
/// tests use this to make identical binaries report different versions. A
/// sidecar file is used instead of an environment variable because `exec()`
/// preserves the environment: an env override set for the old binary would
/// leak into the new one and misreport its version after an upgrade.
///
/// Release builds always report `CARGO_PKG_VERSION`.
pub fn resolve_running_version(exe_path: &Path) -> BinaryVersion {
    if cfg!(debug_assertions)
        && let Some(version) = sidecar_version(exe_path)
    {
        return version;
    }
    compiled_version()
}

/// The version compiled into this binary.
pub fn compiled_version() -> BinaryVersion {
    // Cargo guarantees CARGO_PKG_VERSION is valid semver, so this cannot fail.
    env!("CARGO_PKG_VERSION")
        .parse()
        .expect("CARGO_PKG_VERSION is valid semver")
}

/// Path of the version-override sidecar for a resolved executable path:
/// the file name with `.version` appended (`bun-v0.2.0` -> `bun-v0.2.0.version`).
///
/// Appends rather than using `Path::with_extension`, which would replace
/// everything after the last dot and turn `bun-v0.2.0` into `bun-v0.2.version`.
fn sidecar_path(resolved_exe: &Path) -> PathBuf {
    let mut name = resolved_exe
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".version");
    resolved_exe.with_file_name(name)
}

fn sidecar_version(exe_path: &Path) -> Option<BinaryVersion> {
    // Canonicalise explicitly: current_exe() resolves symlinks on Linux
    // (/proc/self/exe) but not necessarily on other platforms, and the
    // sidecar sits next to the real versioned file, not the symlink.
    let resolved = std::fs::canonicalize(exe_path).ok()?;
    let contents = std::fs::read_to_string(sidecar_path(&resolved)).ok()?;
    contents.trim().parse().ok()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> BinaryVersion {
        s.parse().unwrap()
    }

    #[test]
    fn parses_with_and_without_leading_v() {
        assert_eq!(v("v1.2.3"), v("1.2.3"));
        assert_eq!(v(" v1.2.3 "), v("1.2.3"));
    }

    #[test]
    fn rejects_garbage_and_empty_versions() {
        for input in ["", "v", "banana", "1.2", "v1.2.3.4", "1.2.x"] {
            assert!(
                input.parse::<BinaryVersion>().is_err(),
                "expected {input:?} to be rejected"
            );
        }
    }

    #[test]
    fn orders_semver_correctly() {
        assert!(v("0.1.0") < v("0.2.0"));
        assert!(v("0.2.0") < v("0.10.0"));
        assert!(v("0.10.0") < v("1.0.0"));
    }

    #[test]
    fn prerelease_sorts_before_release() {
        assert!(v("1.0.0-rc.1") < v("1.0.0"));
        assert!(v("1.0.0-rc.1") < v("1.0.0-rc.2"));
    }

    #[test]
    fn display_roundtrips_with_v_prefix() {
        assert_eq!(v("1.2.3").to_string(), "v1.2.3");
        assert_eq!(v("v1.2.3").to_string(), "v1.2.3");
        assert_eq!(
            v("v1.2.3").to_string().parse::<BinaryVersion>().unwrap(),
            v("1.2.3")
        );
    }

    #[test]
    fn file_name_matches_layout() {
        assert_eq!(v("0.2.0").file_name("bun"), "bun-v0.2.0");
    }

    #[test]
    fn serialises_as_display_string() {
        let json = serde_json::to_string(&v("1.0.0-rc.1")).unwrap();
        assert_eq!(json, "\"v1.0.0-rc.1\"");
        let back: BinaryVersion = serde_json::from_str(&json).unwrap();
        assert_eq!(back, v("1.0.0-rc.1"));
    }

    #[test]
    fn compiled_version_parses() {
        // Exercises the expect() in compiled_version.
        let _ = compiled_version();
    }

    #[test]
    fn sidecar_overrides_version_in_debug_builds() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("bun-v9.9.9");
        std::fs::write(&exe, b"not a real binary").unwrap();
        std::fs::write(dir.path().join("bun-v9.9.9.version"), "v9.9.9\n").unwrap();

        assert_eq!(resolve_running_version(&exe), v("9.9.9"));
    }

    #[test]
    fn sidecar_is_resolved_through_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("bun-v9.9.9");
        std::fs::write(&exe, b"not a real binary").unwrap();
        std::fs::write(dir.path().join("bun-v9.9.9.version"), "v9.9.9").unwrap();
        let link = dir.path().join("bun");
        std::os::unix::fs::symlink(&exe, &link).unwrap();

        assert_eq!(resolve_running_version(&link), v("9.9.9"));
    }

    #[test]
    fn missing_or_garbage_sidecar_falls_back_to_compiled_version() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("bun-v9.9.9");
        std::fs::write(&exe, b"not a real binary").unwrap();
        assert_eq!(resolve_running_version(&exe), compiled_version());

        std::fs::write(dir.path().join("bun-v9.9.9.version"), "not a version").unwrap();
        assert_eq!(resolve_running_version(&exe), compiled_version());
    }

    #[test]
    fn sidecar_path_appends_rather_than_replacing_extension() {
        assert_eq!(
            sidecar_path(Path::new("/opt/bun-v0.2.0")),
            Path::new("/opt/bun-v0.2.0.version")
        );
    }
}
