//! Gated runtime knobs: added capabilities, namespaced sysctls and devices
//! (#1122).
//!
//! An app asks for them in `[app.x.runtime]`. Each use needs two keys, the
//! same double gate host paths use: the node must list the value in its
//! `[runtime]` allowlist in `node.toml`, and the deploying token must carry
//! the `host-exec` permission. This module holds the types both sides parse,
//! the node's check, and the portable half of applying them to an OCI spec.
//!
//! The config fields parse straight into these types, so a value no node
//! could ever allow (an unknown capability, a sysctl that would change the
//! whole host, a device outside `/dev`) is a parse error with the field's
//! line, not a surprise at launch.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use super::oci::{OciCapabilities, OciDevice, OciDeviceRule, OciResources, OciSpec};

/// Every Linux capability, by its kernel name, as of Linux 5.9
/// (`CAP_CHECKPOINT_RESTORE` was the last added).
pub const KNOWN_CAPABILITIES: [&str; 41] = [
    "CAP_AUDIT_CONTROL",
    "CAP_AUDIT_READ",
    "CAP_AUDIT_WRITE",
    "CAP_BLOCK_SUSPEND",
    "CAP_BPF",
    "CAP_CHECKPOINT_RESTORE",
    "CAP_CHOWN",
    "CAP_DAC_OVERRIDE",
    "CAP_DAC_READ_SEARCH",
    "CAP_FOWNER",
    "CAP_FSETID",
    "CAP_IPC_LOCK",
    "CAP_IPC_OWNER",
    "CAP_KILL",
    "CAP_LEASE",
    "CAP_LINUX_IMMUTABLE",
    "CAP_MAC_ADMIN",
    "CAP_MAC_OVERRIDE",
    "CAP_MKNOD",
    "CAP_NET_ADMIN",
    "CAP_NET_BIND_SERVICE",
    "CAP_NET_BROADCAST",
    "CAP_NET_RAW",
    "CAP_PERFMON",
    "CAP_SETFCAP",
    "CAP_SETGID",
    "CAP_SETPCAP",
    "CAP_SETUID",
    "CAP_SYSLOG",
    "CAP_SYS_ADMIN",
    "CAP_SYS_BOOT",
    "CAP_SYS_CHROOT",
    "CAP_SYS_MODULE",
    "CAP_SYS_NICE",
    "CAP_SYS_PACCT",
    "CAP_SYS_PTRACE",
    "CAP_SYS_RAWIO",
    "CAP_SYS_RESOURCE",
    "CAP_SYS_TIME",
    "CAP_SYS_TTY_CONFIG",
    "CAP_WAKE_ALARM",
];

/// The network capabilities, which no container can use (#1427).
///
/// Bun makes each container's network namespace before runc starts it, so
/// the namespace belongs to the node's user namespace, while the container
/// runs in one of its own. The kernel checks these capabilities against the
/// user namespace that owns the network namespace, where the container has
/// none, so granting one changes nothing: `ip link`, a TUN device and raw
/// sockets still fail with `EPERM`. Refusing them at parse time says so,
/// instead of letting a deploy succeed and the workload fail.
pub const NETWORK_CAPABILITIES: [&str; 4] = [
    "CAP_NET_ADMIN",
    "CAP_NET_BIND_SERVICE",
    "CAP_NET_BROADCAST",
    "CAP_NET_RAW",
];

/// The capabilities the kernel checks against the node's own user
/// namespace, the initial one, whatever namespace the caller runs in
/// (#1474), each with what it guards.
///
/// A container runs in a user namespace of its own, so holding one of these
/// changes nothing: the operation still fails with "operation not
/// permitted". `make test-linux` runs each guarded operation inside a
/// container's user namespace holding every capability there, and checks
/// it's refused (`CAP_MAC_OVERRIDE`, which only Smack checks, excepted).
/// `CAP_MKNOD` and `CAP_AUDIT_WRITE` are in Docker's default set, which
/// every container still gets, but asking for them grants nothing either.
pub const HOST_CAPABILITIES: [(&str, &str); 22] = [
    ("CAP_AUDIT_CONTROL", "controlling the audit system"),
    ("CAP_AUDIT_READ", "reading the audit log"),
    ("CAP_AUDIT_WRITE", "writing audit records"),
    ("CAP_BLOCK_SUSPEND", "keeping the system awake"),
    ("CAP_BPF", "privileged BPF maps and programs"),
    (
        "CAP_DAC_READ_SEARCH",
        "opening files by handle (reading past file permissions is \
         CAP_DAC_OVERRIDE, which every container has)",
    ),
    ("CAP_IPC_LOCK", "locking memory beyond RLIMIT_MEMLOCK"),
    ("CAP_LEASE", "leases on other users' files"),
    (
        "CAP_LINUX_IMMUTABLE",
        "the immutable and append-only file flags",
    ),
    ("CAP_MAC_ADMIN", "loading AppArmor, SELinux or Smack policy"),
    ("CAP_MAC_OVERRIDE", "overriding Smack's access control"),
    ("CAP_MKNOD", "making device nodes"),
    ("CAP_PERFMON", "system-wide performance monitoring"),
    ("CAP_SYSLOG", "the kernel log"),
    ("CAP_SYS_MODULE", "loading kernel modules"),
    (
        "CAP_SYS_NICE",
        "raising priority: negative nice values and realtime scheduling",
    ),
    ("CAP_SYS_PACCT", "process accounting"),
    ("CAP_SYS_RAWIO", "raw I/O: FIBMAP, I/O ports and /dev/mem"),
    ("CAP_SYS_RESOURCE", "raising hard resource limits"),
    ("CAP_SYS_TIME", "setting the clock"),
    ("CAP_SYS_TTY_CONFIG", "configuring terminals"),
    ("CAP_WAKE_ALARM", "alarms that wake the system"),
];

/// The capabilities that amount to "nearly root" inside the container's
/// own namespaces: with one of these a process can mount filesystems,
/// trace any of its processes or end them all. No wildcard or pattern ever
/// admits them; a node must name each one, and Bun warns at startup when it
/// does.
pub const ESCAPE_CAPABILITIES: [&str; 3] = ["CAP_SYS_ADMIN", "CAP_SYS_BOOT", "CAP_SYS_PTRACE"];

/// The sysctls a container may set: the ones the kernel keeps per network
/// or IPC namespace. Anything else would change the whole node.
const NAMESPACED_SYSCTLS: &str = "net.*, kernel.shm*, kernel.msg*, kernel.sem and fs.mqueue.*";

/// The longest sysctl name or value accepted.
const MAX_SYSCTL_LENGTH: usize = 256;

/// The longest device path accepted, `PATH_MAX`-safe and far beyond any
/// real `/dev` entry.
const MAX_DEVICE_PATH_LENGTH: usize = 255;

/// A runtime knob that can't be used anywhere.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KnobError {
    #[error("unknown capability {0:?}: use a Linux capability name such as \"CAP_SYS_PTRACE\"")]
    UnknownCapability(String),
    #[error(
        "{0} can't be granted: a container's network namespace belongs to the node, and the \
         kernel checks network capabilities against the namespace's owner, so the container \
         would still get \"operation not permitted\". Set network sysctls with `sysctls`, which \
         the node writes into the namespace for it"
    )]
    NetworkCapability(&'static str),
    #[error(
        "{capability} can't be granted: the kernel checks it against the node's own user \
         namespace, not the container's, for {guards}, so the container would still get \
         \"operation not permitted\""
    )]
    HostCapability {
        capability: &'static str,
        guards: &'static str,
    },
    #[error(
        "{0:?} is not a capability: name each capability, because a wildcard would grant \
         CAP_SYS_ADMIN and the other escape hatches with it"
    )]
    CapabilityWildcard(String),
    #[error("sysctl {0:?} is malformed: use dotted lowercase names such as \"net.core.somaxconn\"")]
    MalformedSysctl(String),
    #[error(
        "sysctl {0:?} isn't namespaced: setting it would change the whole node, not just this \
         container, so no node may allow it. A container may set only {NAMESPACED_SYSCTLS}"
    )]
    HostSysctl(String),
    #[error(
        "sysctl pattern {0:?} would cover sysctls that aren't namespaced: a pattern ends in \".*\" \
         and starts with \"net.\" or \"fs.mqueue.\""
    )]
    HostSysctlPattern(String),
    #[error("sysctl value {0:?} must be 1 to 256 characters with no control characters")]
    MalformedSysctlValue(String),
    #[error("device {0:?} must be an absolute path under /dev with no '.' or '..' parts")]
    MalformedDevice(String),
    #[error("device permissions {0:?} must be r, w or rw")]
    MalformedPermissions(String),
    #[error(
        "device permission \"m\" (mknod) can't be granted: a container runs in its own user \
         namespace, where the kernel refuses to make device nodes whatever the device cgroup \
         allows. Pass the device itself with `path` instead"
    )]
    DeviceMknod,
}

// ---------------------------------------------------------------------------
// Capabilities
// ---------------------------------------------------------------------------

/// A Linux capability, by its canonical kernel name (`CAP_NET_ADMIN`).
///
/// It holds a `&'static str` from [`KNOWN_CAPABILITIES`], so it's `Copy`
/// and can't hold a name the kernel doesn't know.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(into = "String")]
pub struct Capability(&'static str);

// Written by hand rather than derived: serde's derive sees the
// `&'static str` and asks the input to live for `'static`, which no
// config file does. Reading an owned `String` and looking it up in
// `KNOWN_CAPABILITIES` needs no borrow at all.
impl<'de> Deserialize<'de> for Capability {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

impl Capability {
    /// The canonical name, such as `"CAP_SYS_PTRACE"`.
    pub fn name(self) -> &'static str {
        self.0
    }

    /// Whether this is one of the [`ESCAPE_CAPABILITIES`].
    pub fn is_escape_hatch(self) -> bool {
        ESCAPE_CAPABILITIES.contains(&self.0)
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl FromStr for Capability {
    type Err = KnobError;

    /// Accept `CAP_SYS_PTRACE`, `SYS_PTRACE` or `sys_ptrace`, the forms
    /// Docker and Kubernetes take. Refuse `ALL`, anything the kernel doesn't
    /// know, and the [`NETWORK_CAPABILITIES`] and [`HOST_CAPABILITIES`],
    /// which a container can't use.
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let upper = value.trim().to_ascii_uppercase();
        if upper == "ALL" || upper == "CAP_ALL" || upper.contains('*') {
            return Err(KnobError::CapabilityWildcard(value.to_string()));
        }
        let canonical = if upper.starts_with("CAP_") {
            upper
        } else {
            format!("CAP_{upper}")
        };
        if let Some(network) = NETWORK_CAPABILITIES
            .iter()
            .find(|network| **network == canonical)
        {
            return Err(KnobError::NetworkCapability(network));
        }
        if let Some((capability, guards)) = HOST_CAPABILITIES
            .iter()
            .find(|(host, _)| *host == canonical)
        {
            return Err(KnobError::HostCapability { capability, guards });
        }
        KNOWN_CAPABILITIES
            .iter()
            .find(|known| **known == canonical)
            .map(|known| Capability(known))
            .ok_or_else(|| KnobError::UnknownCapability(value.to_string()))
    }
}

impl TryFrom<String> for Capability {
    type Error = KnobError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<Capability> for String {
    fn from(capability: Capability) -> Self {
        capability.0.to_string()
    }
}

// ---------------------------------------------------------------------------
// Sysctls
// ---------------------------------------------------------------------------

/// The name of a namespaced sysctl, such as `net.core.somaxconn`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SysctlName(String);

impl SysctlName {
    /// The dotted name.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether the kernel keeps this sysctl per network namespace. Those are
    /// written into the namespace Bun creates for the instance; the IPC ones
    /// go in the OCI spec for the runtime to write.
    pub fn is_network(&self) -> bool {
        self.0.starts_with("net.")
    }
}

impl fmt::Display for SysctlName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Whether `name` is well formed: dot-separated parts of letters, digits,
/// `_` and `-`. Interface names can hold capitals, so those are allowed.
fn well_formed_sysctl(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_SYSCTL_LENGTH
        && name.split('.').all(|part| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        })
}

/// Whether the kernel keeps `name` per network or IPC namespace: the list
/// Docker, runc and Kubernetes all use.
fn namespaced_sysctl(name: &str) -> bool {
    name.starts_with("net.")
        || name.starts_with("fs.mqueue.")
        || name.starts_with("kernel.shm")
        || name.starts_with("kernel.msg")
        || name == "kernel.sem"
}

impl FromStr for SysctlName {
    type Err = KnobError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if !well_formed_sysctl(value) {
            return Err(KnobError::MalformedSysctl(value.to_string()));
        }
        if !namespaced_sysctl(value) {
            return Err(KnobError::HostSysctl(value.to_string()));
        }
        Ok(SysctlName(value.to_string()))
    }
}

impl TryFrom<String> for SysctlName {
    type Error = KnobError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<SysctlName> for String {
    fn from(name: SysctlName) -> Self {
        name.0
    }
}

/// A sysctl's value, as `sysctl -w` takes it: `"1024"`, or
/// `"0 2147483647"` for a pair.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SysctlValue(String);

impl SysctlValue {
    /// The value as written.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for SysctlValue {
    type Err = KnobError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.trim().is_empty()
            || value.len() > MAX_SYSCTL_LENGTH
            || value.chars().any(char::is_control)
        {
            return Err(KnobError::MalformedSysctlValue(value.to_string()));
        }
        Ok(SysctlValue(value.to_string()))
    }
}

impl TryFrom<String> for SysctlValue {
    type Error = KnobError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<SysctlValue> for String {
    fn from(value: SysctlValue) -> Self {
        value.0
    }
}

/// An entry in a node's `allowed_sysctls`: one exact sysctl, or every
/// sysctl under a prefix (`"net.ipv4.*"`).
///
/// A prefix pattern may only cover namespaced sysctls, so it must start
/// with `net.` or `fs.mqueue.`: `kernel.*` would cover `kernel.panic` too.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SysctlPattern(String);

impl SysctlPattern {
    /// Whether `name` matches this pattern.
    pub fn matches(&self, name: &SysctlName) -> bool {
        match self.0.strip_suffix('*') {
            Some(prefix) => name.as_str().starts_with(prefix),
            None => name.as_str() == self.0,
        }
    }
}

impl FromStr for SysctlPattern {
    type Err = KnobError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let Some(prefix) = value.strip_suffix(".*") else {
            return value
                .parse::<SysctlName>()
                .map(|name| SysctlPattern(name.0));
        };
        if !well_formed_sysctl(prefix) {
            return Err(KnobError::MalformedSysctl(value.to_string()));
        }
        let covered = format!("{prefix}.");
        if !(covered.starts_with("net.") || covered.starts_with("fs.mqueue.")) {
            return Err(KnobError::HostSysctlPattern(value.to_string()));
        }
        Ok(SysctlPattern(value.to_string()))
    }
}

impl TryFrom<String> for SysctlPattern {
    type Error = KnobError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<SysctlPattern> for String {
    fn from(pattern: SysctlPattern) -> Self {
        pattern.0
    }
}

impl fmt::Display for SysctlPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// ---------------------------------------------------------------------------
// Devices
// ---------------------------------------------------------------------------

/// A host device node, by its absolute path under `/dev`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct DevicePath(PathBuf);

impl DevicePath {
    /// The path.
    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

impl fmt::Display for DevicePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.display())
    }
}

impl FromStr for DevicePath {
    type Err = KnobError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        // Split the string itself: `Path::components` quietly drops a `.`
        // part, and the path a node lists must be the path an app asks for.
        let well_formed = value.len() <= MAX_DEVICE_PATH_LENGTH
            && !value.chars().any(char::is_control)
            && value.strip_prefix("/dev/").is_some_and(|rest| {
                rest.split('/')
                    .all(|part| !part.is_empty() && part != "." && part != "..")
            });
        if !well_formed {
            return Err(KnobError::MalformedDevice(value.to_string()));
        }
        Ok(DevicePath(PathBuf::from(value)))
    }
}

impl TryFrom<String> for DevicePath {
    type Error = KnobError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<DevicePath> for String {
    fn from(path: DevicePath) -> Self {
        path.0.to_string_lossy().into_owned()
    }
}

/// What a container may do with a device: read it, write it, or both, in
/// the cgroup device controller's `rwm` notation.
///
/// There's no `m` (mknod): a container runs in its own user namespace, and
/// the kernel lets only the initial user namespace make device nodes, so a
/// device cgroup rule allowing it would grant nothing (#1442).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct DevicePermissions {
    pub read: bool,
    pub write: bool,
}

impl Default for DevicePermissions {
    /// `rw`: Docker's `--device` default without the mknod no container
    /// here can use.
    fn default() -> Self {
        Self {
            read: true,
            write: true,
        }
    }
}

impl fmt::Display for DevicePermissions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (granted, letter) in [(self.read, "r"), (self.write, "w")] {
            if granted {
                f.write_str(letter)?;
            }
        }
        Ok(())
    }
}

impl FromStr for DevicePermissions {
    type Err = KnobError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let malformed = || KnobError::MalformedPermissions(value.to_string());
        let mut permissions = DevicePermissions {
            read: false,
            write: false,
        };
        for letter in value.chars() {
            let slot = match letter {
                'r' => &mut permissions.read,
                'w' => &mut permissions.write,
                'm' => return Err(KnobError::DeviceMknod),
                _ => return Err(malformed()),
            };
            if *slot {
                return Err(malformed());
            }
            *slot = true;
        }
        if value.is_empty() {
            return Err(malformed());
        }
        Ok(permissions)
    }
}

impl TryFrom<String> for DevicePermissions {
    type Error = KnobError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<DevicePermissions> for String {
    fn from(permissions: DevicePermissions) -> Self {
        permissions.to_string()
    }
}

/// One device an app asks for: `{ path = "/dev/fuse", permissions = "rw" }`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceRequest {
    /// The host device, which appears at the same path in the container.
    pub path: DevicePath,
    /// What the container may do with it. `rw` when unset.
    #[serde(default)]
    pub permissions: DevicePermissions,
}

// ---------------------------------------------------------------------------
// The app's request and the node's allowlist
// ---------------------------------------------------------------------------

/// `[app.x.runtime]`: the runtime knobs an app asks for.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeKnobs {
    /// Capabilities added to the container's default set.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub capabilities: BTreeSet<Capability>,
    /// Namespaced sysctls set in the container's own namespaces.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sysctls: BTreeMap<SysctlName, SysctlValue>,
    /// Host devices passed into the container.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub devices: Vec<DeviceRequest>,
}

/// The three kinds of runtime knob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnobKind {
    Capability,
    Sysctl,
    Device,
}

impl KnobKind {
    /// The field under `[app.x.runtime]`.
    pub fn field(self) -> &'static str {
        match self {
            KnobKind::Capability => "capabilities",
            KnobKind::Sysctl => "sysctls",
            KnobKind::Device => "devices",
        }
    }

    /// The key under a node's `[runtime]` that allows this kind.
    pub fn allowlist_key(self) -> &'static str {
        match self {
            KnobKind::Capability => "allowed_capabilities",
            KnobKind::Sysctl => "allowed_sysctls",
            KnobKind::Device => "allowed_devices",
        }
    }
}

/// One use of a runtime knob, for messages: its kind and the value it
/// names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnobUse {
    pub kind: KnobKind,
    /// The capability, sysctl name or device path.
    pub value: String,
}

impl fmt::Display for KnobUse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "runtime.{} {}", self.kind.field(), self.value)
    }
}

impl RuntimeKnobs {
    /// Whether the app asks for nothing.
    pub fn is_empty(&self) -> bool {
        self.capabilities.is_empty() && self.sysctls.is_empty() && self.devices.is_empty()
    }

    /// Every knob the app uses, capabilities first, then sysctls, then
    /// devices.
    pub fn uses(&self) -> impl Iterator<Item = KnobUse> + '_ {
        let capabilities = self.capabilities.iter().map(|capability| KnobUse {
            kind: KnobKind::Capability,
            value: capability.to_string(),
        });
        let sysctls = self.sysctls.keys().map(|name| KnobUse {
            kind: KnobKind::Sysctl,
            value: name.to_string(),
        });
        let devices = self.devices.iter().map(|device| KnobUse {
            kind: KnobKind::Device,
            value: device.path.to_string(),
        });
        capabilities.chain(sysctls).chain(devices)
    }
}

/// Why a node refused an app's runtime knobs.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KnobRefusal {
    /// The node's `[runtime]` allowlist doesn't list the value.
    #[error(
        "{knob} refused by the node gate: this node's [runtime] {allowlist} in node.toml \
         doesn't list it",
        allowlist = knob.kind.allowlist_key()
    )]
    NotAllowed { knob: KnobUse },
    /// The node's runtime can't apply runtime knobs at all.
    #[error(
        "{knob} refused: this node's runtime ({runtime}) can't apply runtime knobs; \
         only rootful runc can"
    )]
    Unsupported {
        knob: KnobUse,
        runtime: &'static str,
    },
}

/// A node's `[runtime]` allowlist, as it's advertised to the scheduler.
///
/// The default allows nothing. A node whose runtime can't apply knobs
/// advertises the default, whatever `node.toml` lists, so the scheduler
/// keeps knob-using apps off it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeAllowlist {
    /// `allowed_capabilities`.
    #[serde(default)]
    pub capabilities: BTreeSet<Capability>,
    /// `allowed_sysctls`.
    #[serde(default)]
    pub sysctls: Vec<SysctlPattern>,
    /// `allowed_devices`.
    #[serde(default)]
    pub devices: BTreeSet<DevicePath>,
}

impl RuntimeAllowlist {
    /// Whether this allows nothing at all.
    pub fn is_empty(&self) -> bool {
        self.capabilities.is_empty() && self.sysctls.is_empty() && self.devices.is_empty()
    }

    /// Admit `knobs`, or name the first one this allowlist doesn't list.
    pub fn check(&self, knobs: &RuntimeKnobs) -> Result<(), KnobRefusal> {
        let refuse = |kind, value| {
            Err(KnobRefusal::NotAllowed {
                knob: KnobUse { kind, value },
            })
        };
        if let Some(capability) = knobs
            .capabilities
            .iter()
            .find(|capability| !self.capabilities.contains(capability))
        {
            return refuse(KnobKind::Capability, capability.to_string());
        }
        if let Some(name) = knobs
            .sysctls
            .keys()
            .find(|name| !self.sysctls.iter().any(|pattern| pattern.matches(name)))
        {
            return refuse(KnobKind::Sysctl, name.to_string());
        }
        if let Some(device) = knobs
            .devices
            .iter()
            .find(|device| !self.devices.contains(&device.path))
        {
            return refuse(KnobKind::Device, device.path.to_string());
        }
        Ok(())
    }

    /// Whether this allowlist admits every one of `knobs`.
    pub fn admits(&self, knobs: &RuntimeKnobs) -> bool {
        self.check(knobs).is_ok()
    }

    /// The escape-hatch capabilities this allowlist names, for a startup
    /// warning.
    pub fn escape_capabilities(&self) -> Vec<Capability> {
        self.capabilities
            .iter()
            .copied()
            .filter(|capability| capability.is_escape_hatch())
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Applying the knobs to an OCI spec
// ---------------------------------------------------------------------------

/// A character or block device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    Char,
    Block,
}

impl DeviceKind {
    /// The OCI `type` letter.
    pub fn letter(self) -> &'static str {
        match self {
            DeviceKind::Char => "c",
            DeviceKind::Block => "b",
        }
    }
}

/// What the node's own device node says: its kind, numbers, mode and owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostDevice {
    pub kind: DeviceKind,
    pub major: i64,
    pub minor: i64,
    /// The permission bits, such as `0o666`.
    pub file_mode: u32,
    pub uid: u32,
    pub gid: u32,
}

/// Why a device couldn't be passed in.
#[derive(Debug, thiserror::Error)]
pub enum DeviceLookupError {
    #[error("device {path} can't be read on this node: {source}")]
    Unreadable {
        path: String,
        source: std::io::Error,
    },
    #[error("{path} is not a character or block device on this node")]
    NotADevice { path: String },
}

/// Split a Linux `dev_t` into its major and minor numbers, the way glibc's
/// `major()` and `minor()` macros do.
pub fn split_linux_device_number(device: u64) -> (i64, i64) {
    let major = ((device >> 8) & 0xfff) | ((device >> 32) & 0xffff_f000);
    let minor = (device & 0xff) | ((device >> 12) & 0xffff_ff00);
    // Both fit in 32 bits by construction, so the casts can't wrap.
    (major as i64, minor as i64)
}

/// Read a host device node. Runs on the node at launch, where the device
/// lives, and reads it with `tokio::fs`, so a slow `stat` never holds an
/// async worker thread.
#[cfg(unix)]
pub async fn host_device(path: &Path) -> Result<HostDevice, DeviceLookupError> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    let display = path.display().to_string();
    let metadata =
        tokio::fs::metadata(path)
            .await
            .map_err(|source| DeviceLookupError::Unreadable {
                path: display.clone(),
                source,
            })?;
    let file_type = metadata.file_type();
    let kind = if file_type.is_char_device() {
        DeviceKind::Char
    } else if file_type.is_block_device() {
        DeviceKind::Block
    } else {
        return Err(DeviceLookupError::NotADevice { path: display });
    };
    let (major, minor) = split_linux_device_number(metadata.rdev());
    Ok(HostDevice {
        kind,
        major,
        minor,
        file_mode: metadata.mode() & 0o7777,
        uid: metadata.uid(),
        gid: metadata.gid(),
    })
}

/// Read every host device `knobs` asks for, by path, for
/// [`apply_to_spec`]'s lookup.
#[cfg(unix)]
pub async fn host_devices(
    knobs: &RuntimeKnobs,
) -> Result<BTreeMap<PathBuf, HostDevice>, DeviceLookupError> {
    let mut devices = BTreeMap::new();
    for request in &knobs.devices {
        let path = request.path.as_path();
        devices.insert(path.to_path_buf(), host_device(path).await?);
    }
    Ok(devices)
}

/// Write an app's runtime knobs into the OCI spec a runtime is about to
/// hand to runc: the added capabilities joined to whatever set the spec
/// already grants, the sysctls into `linux.sysctl`, and each device into
/// `linux.devices` with a matching allow rule in the cgroup device
/// controller (`linux.resources.devices`).
///
/// `lookup` finds a host device; runc's caller reads them first with
/// [`host_devices`] and looks them up there, and tests pass a table. Call it after the user namespace has set the default
/// capabilities, so they're joined rather than replaced.
pub fn apply_to_spec<F>(
    spec: &mut OciSpec,
    knobs: &RuntimeKnobs,
    lookup: F,
) -> Result<(), DeviceLookupError>
where
    F: Fn(&Path) -> Result<HostDevice, DeviceLookupError>,
{
    if !knobs.capabilities.is_empty() {
        let capabilities = spec
            .process
            .capabilities
            .get_or_insert_with(|| OciCapabilities {
                bounding: Vec::new(),
                effective: Vec::new(),
                permitted: Vec::new(),
            });
        for set in [
            &mut capabilities.bounding,
            &mut capabilities.effective,
            &mut capabilities.permitted,
        ] {
            let mut joined: BTreeSet<String> = set.drain(..).collect();
            joined.extend(knobs.capabilities.iter().map(|c| c.name().to_string()));
            *set = joined.into_iter().collect();
        }
    }
    for (name, value) in &knobs.sysctls {
        spec.linux
            .sysctl
            .insert(name.as_str().to_string(), value.as_str().to_string());
    }
    for request in &knobs.devices {
        let device = lookup(request.path.as_path())?;
        spec.linux.devices.push(OciDevice {
            path: request.path.as_path().to_path_buf(),
            kind: device.kind.letter().to_string(),
            major: device.major,
            minor: device.minor,
            file_mode: Some(device.file_mode),
            uid: Some(device.uid),
            gid: Some(device.gid),
        });
        spec.linux
            .resources
            .get_or_insert_with(OciResources::default)
            .devices
            .push(OciDeviceRule {
                allow: true,
                kind: Some(device.kind.letter().to_string()),
                major: Some(device.major),
                minor: Some(device.minor),
                access: Some(request.permissions.to_string()),
            });
    }
    Ok(())
}

/// Take the network sysctls out of the spec's `linux.sysctl`, leaving the
/// IPC ones for the runtime.
///
/// A rootful container runs in its own user namespace, but its network
/// namespace is created by the node beforehand, so it belongs to the node's
/// user namespace. Container root can't write that namespace's sysctls, so
/// runc writing them would fail; Bun writes them into the namespace itself
/// instead. The IPC namespace is the container's own, so runc can.
pub fn take_network_sysctls(spec: &mut OciSpec) -> BTreeMap<String, String> {
    let (network, ipc): (BTreeMap<_, _>, BTreeMap<_, _>) = std::mem::take(&mut spec.linux.sysctl)
        .into_iter()
        .partition(|(name, _)| name.starts_with("net."));
    spec.linux.sysctl = ipc;
    network
}

#[cfg(test)]
mod tests;
