//! What kinds of job a node can run, as it advertises them over gossip.
//!
//! A node refuses a job run it can never take: a host command under mount
//! isolation, a container on a node without the rootful runtime, an
//! allowlist it can't enforce (#777). The leader learns each node's answer
//! from this summary, which rides every gossip datagram, so it can refuse a
//! run no live node could take at submission instead of a tick later (#788).

use serde::{Deserialize, Serialize};

/// The job kinds a node runs, one flag per question its task executor asks
/// before it takes a run.
///
/// Small on purpose: it rides every gossip datagram beside the node's labels
/// and endpoints, where each byte is room a membership update can't use.
/// Nine flags encode as nine bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobCapabilities {
    /// Runs host-process tasks (`runtime = "process"`) at all.
    pub host: bool,
    /// Enforces CPU and memory limits on host-process tasks.
    pub host_limits: bool,
    /// Runs container tasks of any size, and `shared-runc` executors: the
    /// rootful owned Linux runtime.
    pub containers: bool,
    /// Runs a single container task (`count = 1`) without CPU or memory
    /// limits, which more runtimes can than run whole arrays.
    pub singleton_images: bool,
    /// Runs a single container task with CPU or memory limits.
    pub singleton_image_limits: bool,
    /// Decrypts encrypted template environment values: the owned runtime
    /// with the namespace keys.
    pub encrypted_env: bool,
    /// Enforces a container task's egress allowlist and `allow_from` before
    /// it starts. A host-process task's never are.
    pub network_policy: bool,
    /// `[process_workloads] mount_isolation` is on, which host tasks can't
    /// run under yet (#714).
    pub mount_isolation: bool,
    /// `[process_workloads] allowed_binaries` names at least one binary. An
    /// empty allowlist refuses every host task.
    pub binaries: bool,
}
