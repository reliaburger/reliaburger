//! A test double for a container runtime: [`HostContainers`] runs each
//! container's command as a host process on `ProcessGrill`.
//!
//! Since #914 the process runtime refuses an image app, and a node on it
//! tells the scheduler it runs no containers. The in-process cluster tests
//! still need image-only features, managed volumes above all, on nodes that
//! run on any developer's machine. This double says it runs containers and
//! hands `ProcessGrill` each one as a host workload. Nothing is pulled,
//! mounted or isolated, so it proves placement and lifecycle, not
//! containment. Bun never uses it.

use std::path::PathBuf;

use reliaburger::grill::oci::{HOST_ROOT, OciSpec};
use reliaburger::grill::process::ProcessGrill;
use reliaburger::grill::records::InstanceRecord;
use reliaburger::grill::state::ContainerState;
use reliaburger::grill::{Grill, GrillError, InstanceId, RuntimeGeneration, RuntimeLaunch};
use reliaburger::ketchup::types::{CaptureOffsets, CapturedLine};

/// `ProcessGrill`, posing as a container runtime for cluster tests.
#[derive(Clone)]
pub struct HostContainers {
    runtime: ProcessGrill,
    /// `false` makes this plain `ProcessGrill`: a node on
    /// `--runtime process`, which refuses image apps.
    containers: bool,
}

impl HostContainers {
    /// A double over a fresh in-memory `ProcessGrill` that runs containers.
    pub fn new() -> Self {
        Self {
            runtime: ProcessGrill::new(),
            containers: true,
        }
    }

    /// Plain `ProcessGrill`, as a node on `--runtime process` runs it.
    pub fn process_only() -> Self {
        Self {
            runtime: ProcessGrill::new(),
            containers: false,
        }
    }
}

/// `spec` as the host workload `ProcessGrill` runs: the container's command,
/// on the node's own filesystem.
fn on_the_host(spec: &OciSpec) -> OciSpec {
    let mut host = spec.clone();
    if !host.host_process {
        host.host_process = true;
        host.root.path = HOST_ROOT.to_string();
    }
    host
}

impl Grill for HostContainers {
    fn runs_containers(&self) -> bool {
        self.containers
    }

    async fn create(&self, instance: &InstanceId, spec: &OciSpec) -> Result<(), GrillError> {
        if !self.containers {
            return self.runtime.create(instance, spec).await;
        }
        self.runtime.create(instance, &on_the_host(spec)).await
    }

    async fn start(&self, instance: &InstanceId) -> Result<(), GrillError> {
        self.runtime.start(instance).await
    }

    async fn stop(&self, instance: &InstanceId) -> Result<(), GrillError> {
        self.runtime.stop(instance).await
    }

    async fn kill(&self, instance: &InstanceId) -> Result<(), GrillError> {
        self.runtime.kill(instance).await
    }

    async fn state(&self, instance: &InstanceId) -> Result<ContainerState, GrillError> {
        self.runtime.state(instance).await
    }

    async fn forget_retired_generation(
        &self,
        instance: &InstanceId,
        generation: &RuntimeGeneration,
    ) -> Result<(), GrillError> {
        self.runtime
            .forget_retired_generation(instance, generation)
            .await
    }

    async fn launch_generation(
        &self,
        instance: &InstanceId,
    ) -> Result<Option<RuntimeGeneration>, GrillError> {
        self.runtime.launch_generation(instance).await
    }

    async fn forget_retired(&self, instance: &InstanceId) -> Result<(), GrillError> {
        self.runtime.forget_retired(instance).await
    }

    async fn launch_inventory(&self) -> Result<Option<Vec<RuntimeLaunch>>, GrillError> {
        self.runtime.launch_inventory().await
    }

    async fn adopt(
        &self,
        instance: &InstanceId,
        record: &InstanceRecord,
    ) -> Result<bool, GrillError> {
        self.runtime.adopt(instance, record).await
    }

    async fn pid(&self, instance: &InstanceId) -> Result<Option<u32>, GrillError> {
        self.runtime.pid(instance).await
    }

    async fn log_stem(&self, instance: &InstanceId) -> Option<PathBuf> {
        self.runtime.log_stem(instance).await
    }

    async fn exit_code(&self, instance: &InstanceId) -> Result<Option<i32>, GrillError> {
        self.runtime.exit_code(instance).await
    }

    async fn logs(&self, instance: &InstanceId) -> Result<String, GrillError> {
        self.runtime.logs(instance).await
    }

    async fn exec(
        &self,
        instance: &InstanceId,
        command: &[String],
    ) -> Result<reliaburger::grill::ExecOutput, GrillError> {
        self.runtime.exec(instance, command).await
    }

    async fn follow_logs(
        &self,
        instance: &InstanceId,
        lines_tx: tokio::sync::mpsc::Sender<CapturedLine>,
        resume: &CaptureOffsets,
    ) {
        self.runtime.follow_logs(instance, lines_tx, resume).await
    }
}
