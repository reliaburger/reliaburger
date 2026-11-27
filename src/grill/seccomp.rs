//! The system-call filter every rootful runc container runs under (#2402).
//!
//! A user namespace stops container root from being host root, but it
//! leaves the kernel's whole system-call surface open: `keyctl`, `bpf`,
//! `io_uring_setup`, `userfaultfd`, `unshare` and the rest. A bug in any of
//! them is a node compromise. Docker and containerd close that surface with
//! an allow-list of about 370 calls, maintained against real images for a
//! decade, and we ship the same list rather than writing our own: a
//! deny-list misses every call added after it was written, and a list of
//! our own would break images that Docker runs.
//!
//! `seccomp_default.json` is Moby's default profile, copied unchanged from
//! <https://github.com/moby/profiles/blob/seccomp/v0.3.0/seccomp/default.json>
//! (tag `seccomp/v0.3.0`, commit `19b749451e67`, Apache-2.0 like this
//! project). Its format isn't the OCI one: each rule may name the
//! architectures and capabilities it applies to. [`default_profile`] turns
//! it into the OCI `linux.seccomp` block for this machine's architecture and
//! a container's capabilities, as Docker does when it starts a container. A
//! capability a runtime knob adds (#1122) widens the filter by the calls that
//! capability guards, so `CAP_SYS_ADMIN` brings `mount` and `unshare` with it.

use std::sync::OnceLock;

use serde::Deserialize;

use super::oci::{OciSeccomp, OciSeccompArg, OciSyscall};

/// Moby's default seccomp profile, `seccomp/v0.3.0`.
const MOBY_DEFAULT_PROFILE: &str = include_str!("seccomp_default.json");

/// Why the default profile can't be built.
#[derive(Debug, thiserror::Error)]
pub enum SeccompError {
    #[error("the embedded seccomp profile doesn't parse: {0}")]
    Profile(String),
    #[error("no seccomp profile for the {0} architecture")]
    UnsupportedArchitecture(&'static str),
}

/// Moby's profile format: the OCI fields plus per-rule conditions.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MobyProfile {
    default_action: String,
    default_errno_ret: Option<u32>,
    arch_map: Vec<MobyArchitecture>,
    syscalls: Vec<MobyRule>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MobyArchitecture {
    architecture: String,
    sub_architectures: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MobyRule {
    names: Vec<String>,
    action: String,
    errno_ret: Option<u32>,
    #[serde(default)]
    args: Option<Vec<OciSeccompArg>>,
    #[serde(default)]
    includes: MobyCondition,
    #[serde(default)]
    excludes: MobyCondition,
}

/// Which containers a rule applies to. Architectures use Go's names
/// (`amd64`, `arm64`), as Moby does.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MobyCondition {
    #[serde(default)]
    caps: Vec<String>,
    #[serde(default)]
    arches: Vec<String>,
    // A rule may also name the oldest kernel it needs (`minKernel`). Only
    // `ptrace` carries one (4.8), below every kernel Reliaburger supports,
    // so it's always met and isn't read; a test fails if a profile update
    // raises it.
}

impl MobyRule {
    /// Whether this rule applies to a container on `arch` holding
    /// `capabilities`: Moby's rules, where an include needs every
    /// capability it names and an exclude needs any one.
    fn applies(&self, arch: &str, capabilities: &[String]) -> bool {
        let holds = |cap: &String| capabilities.contains(cap);
        let excluded =
            self.excludes.arches.iter().any(|a| a == arch) || self.excludes.caps.iter().any(holds);
        let included = (self.includes.arches.is_empty()
            || self.includes.arches.iter().any(|a| a == arch))
            && self.includes.caps.iter().all(holds);
        included && !excluded
    }
}

/// The embedded profile, parsed on first use.
fn moby_profile() -> Result<&'static MobyProfile, SeccompError> {
    // `OnceLock` runs the closure once, for the first caller, and hands every
    // caller after it a reference to the stored result: the parse costs
    // nothing on later container starts.
    static PROFILE: OnceLock<Result<MobyProfile, String>> = OnceLock::new();
    PROFILE
        .get_or_init(|| {
            serde_json::from_str(MOBY_DEFAULT_PROFILE).map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(|error| SeccompError::Profile(error.clone()))
}

/// This machine's architecture: Go's name for it, which the profile's rules
/// use, and libseccomp's, which its architecture map uses.
fn native_architecture() -> Result<(&'static str, &'static str), SeccompError> {
    match std::env::consts::ARCH {
        "x86_64" => Ok(("amd64", "SCMP_ARCH_X86_64")),
        "aarch64" => Ok(("arm64", "SCMP_ARCH_AARCH64")),
        other => Err(SeccompError::UnsupportedArchitecture(other)),
    }
}

/// The OCI seccomp filter for a container on this machine whose bounding
/// set is `capabilities`.
///
/// Every call the profile doesn't allow fails with `EPERM`; `clone3` fails
/// with `ENOSYS` instead, so the C library falls back to `clone`, whose
/// namespace flags the filter can inspect.
pub fn default_profile(capabilities: &[String]) -> Result<OciSeccomp, SeccompError> {
    let profile = moby_profile()?;
    let (arch, seccomp_arch) = native_architecture()?;
    let architectures = profile
        .arch_map
        .iter()
        .filter(|entry| entry.architecture == seccomp_arch)
        .flat_map(|entry| {
            std::iter::once(entry.architecture.clone())
                .chain(entry.sub_architectures.iter().flatten().cloned())
        })
        .collect();
    let syscalls = profile
        .syscalls
        .iter()
        .filter(|rule| rule.applies(arch, capabilities))
        .map(|rule| OciSyscall {
            names: rule.names.clone(),
            action: rule.action.clone(),
            errno_ret: rule.errno_ret,
            args: rule.args.clone().unwrap_or_default(),
        })
        .collect();
    Ok(OciSeccomp {
        default_action: profile.default_action.clone(),
        default_errno_ret: profile.default_errno_ret,
        architectures,
        syscalls,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capabilities(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    fn allowed(profile: &OciSeccomp, name: &str) -> bool {
        profile.syscalls.iter().any(|rule| {
            rule.action == "SCMP_ACT_ALLOW"
                && rule.args.is_empty()
                && rule.names.iter().any(|n| n == name)
        })
    }

    #[test]
    fn the_embedded_profile_denies_by_default_and_allows_ordinary_calls() {
        let profile = default_profile(&capabilities(&["CAP_CHOWN"])).unwrap();
        assert_eq!(profile.default_action, "SCMP_ACT_ERRNO");
        assert_eq!(profile.default_errno_ret, Some(1));
        for call in ["read", "write", "openat", "execve", "futex", "epoll_wait"] {
            assert!(allowed(&profile, call), "{call} is refused");
        }
        for call in [
            "keyctl",
            "add_key",
            "userfaultfd",
            "kexec_load",
            "open_by_handle_at",
        ] {
            assert!(!allowed(&profile, call), "{call} is allowed");
        }
    }

    #[test]
    fn the_filter_names_only_this_machines_architecture() {
        let profile = default_profile(&[]).unwrap();
        let expected = match std::env::consts::ARCH {
            "x86_64" => vec!["SCMP_ARCH_X86_64", "SCMP_ARCH_X86", "SCMP_ARCH_X32"],
            "aarch64" => vec!["SCMP_ARCH_AARCH64", "SCMP_ARCH_ARM"],
            other => panic!("untested architecture {other}"),
        };
        assert_eq!(profile.architectures, expected);
        let arch_only = if std::env::consts::ARCH == "x86_64" {
            "arch_prctl"
        } else {
            "arm_fadvise64_64"
        };
        assert!(allowed(&profile, arch_only));
        assert!(!allowed(&profile, "s390_runtime_instr"));
    }

    #[test]
    fn mount_and_unshare_come_only_with_cap_sys_admin() {
        let ordinary = default_profile(&capabilities(&["CAP_CHOWN", "CAP_SETUID"])).unwrap();
        for call in ["mount", "unshare", "setns", "bpf"] {
            assert!(!allowed(&ordinary, call), "{call} without CAP_SYS_ADMIN");
        }
        // clone stays allowed, but only without namespace flags.
        let clone = ordinary
            .syscalls
            .iter()
            .find(|rule| rule.names == ["clone"])
            .unwrap();
        assert_eq!(clone.args[0].op, "SCMP_CMP_MASKED_EQ");
        let clone3 = ordinary
            .syscalls
            .iter()
            .find(|rule| rule.names == ["clone3"])
            .unwrap();
        assert_eq!(
            (clone3.action.as_str(), clone3.errno_ret),
            ("SCMP_ACT_ERRNO", Some(38))
        );

        let admin = default_profile(&capabilities(&["CAP_SYS_ADMIN"])).unwrap();
        for call in ["mount", "unshare", "setns", "clone", "clone3"] {
            assert!(allowed(&admin, call), "{call} with CAP_SYS_ADMIN");
        }
        assert!(
            !admin
                .syscalls
                .iter()
                .any(|rule| rule.names == ["clone3"] && rule.action == "SCMP_ACT_ERRNO")
        );
    }

    #[test]
    fn every_kernel_floor_in_the_profile_is_below_the_supported_kernels() {
        // Rules with a `minKernel` are always applied; Reliaburger supports
        // kernels from 5.8. Raise this check before taking a profile whose
        // floor is above that.
        let profile: serde_json::Value = serde_json::from_str(MOBY_DEFAULT_PROFILE).unwrap();
        for rule in profile["syscalls"].as_array().unwrap() {
            for side in ["includes", "excludes"] {
                let Some(floor) = rule[side]["minKernel"].as_str() else {
                    continue;
                };
                let mut parts = floor.split('.').map(|part| part.parse::<u32>().unwrap());
                let version = (parts.next().unwrap(), parts.next().unwrap_or(0));
                assert!(version <= (5, 8), "{} needs kernel {floor}", rule["names"]);
            }
        }
    }
}
