//! Slirp lifecycle bound to the same original OCI generation as its launcher.

use std::collections::BTreeMap;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;
use crate::grill::command::{ClaimedCommandExecutor, CommandState};
use crate::grill::runc_intent::{RuntimeIntent, RuntimeRole};

fn directory(intent: &RuntimeIntent) -> PathBuf {
    PathBuf::from(format!(
        "/tmp/rbr-{}-{}",
        nix::unistd::geteuid(),
        intent.generation.as_str()
    ))
}

async fn api(socket: &Path, request: serde_json::Value) -> io::Result<serde_json::Value> {
    tokio::time::timeout(Duration::from_secs(2), async {
        let mut stream = tokio::net::UnixStream::connect(socket).await?;
        stream.write_all(&serde_json::to_vec(&request)?).await?;
        stream.shutdown().await?;
        let mut bytes = Vec::new();
        stream.take(65537).read_to_end(&mut bytes).await?;
        if bytes.len() > 65536 {
            return Err(io::Error::other("oversized rootless API response"));
        }
        let response: serde_json::Value = serde_json::from_slice(&bytes)?;
        if response.get("error").is_some() {
            return Err(io::Error::other(format!(
                "rootless API refused: {}",
                response["error"]
            )));
        }
        Ok(response)
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "rootless API request timed out"))?
}

async fn role_failure(
    context: &ClaimedCommandExecutor,
    role: RuntimeRole,
    message: &str,
) -> io::Error {
    let output = async {
        let stem = context
            .role_log_stem(role)
            .await?
            .ok_or_else(|| io::Error::other("missing role logs"))?;
        let file = tokio::fs::File::open(stem.with_extension("stderr")).await?;
        let mut bytes = Vec::new();
        file.take(4096).read_to_end(&mut bytes).await?;
        Ok::<_, io::Error>(String::from_utf8_lossy(&bytes).into_owned())
    }
    .await
    .unwrap_or_else(|error| format!("diagnostic unavailable: {error}"));
    io::Error::other(format!("{message}: {output}"))
}

async fn forwards(
    socket: &Path,
    mapping: Option<crate::grill::oci::PortMapping>,
) -> io::Result<bool> {
    let result = api(socket, serde_json::json!({"execute": "list_hostfwd"})).await?;
    // Released slirp returns bare entries; its manual also documents a return envelope.
    let result = result.get("return").unwrap_or(&result);
    let entries = result["entries"]
        .as_array()
        .ok_or_else(|| io::Error::other("invalid rootless forwarding inventory"))?;
    let Some(mapping) = mapping else {
        if !entries.is_empty() {
            return Err(io::Error::other("unexpected rootless forwarding entries"));
        }
        return Ok(true);
    };
    if entries.is_empty() {
        return Ok(false);
    }
    if entries.len() != 1
        || entries[0]["proto"] != "tcp"
        || entries[0]["host_addr"] != "0.0.0.0"
        || entries[0]["host_port"].as_u64() != Some(u64::from(mapping.host_port))
        || entries[0]["guest_port"].as_u64() != Some(u64::from(mapping.container_port))
        || entries[0]["guest_addr"] != "10.0.2.100"
    {
        return Err(io::Error::other(
            "rootless forwarding conflicts with original intent",
        ));
    }
    Ok(true)
}

/// How many helpers one supervision pass may start before it reports failure.
const HELPER_START_ATTEMPTS: u32 = 3;
/// Linear backoff between helper starts within one supervision pass.
const HELPER_RESTART_BACKOFF: Duration = Duration::from_millis(100);

/// Connection-level failures a starting or dying helper produces. Refusals and
/// conflicting forwarding inventories are semantic errors and never retried.
fn transient_api_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::UnexpectedEof
    )
}

async fn helper_running(context: &ClaimedCommandExecutor) -> io::Result<bool> {
    Ok(matches!(
        context.role_state(RuntimeRole::RootlessNetwork).await?,
        Some(CommandState::Running { .. })
    ))
}

/// Install (or confirm) the original forward on the bound helper. `Ok(false)`
/// means the helper exited first: the owner can still report a SIGKILLed helper
/// as running until it reaps it, so an unanswered socket is only an error while
/// the owner keeps confirming the helper alive past the deadline.
async fn helper_ready(
    context: &ClaimedCommandExecutor,
    socket: &Path,
    mapping: Option<crate::grill::oci::PortMapping>,
) -> io::Result<bool> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if !helper_running(context).await? {
            return Ok(false);
        }
        let error = match install_forward(socket, mapping).await {
            Ok(()) => return Ok(true),
            Err(error) => error,
        };
        if !helper_running(context).await? {
            return Ok(false);
        }
        if !transient_api_error(&error) || tokio::time::Instant::now() >= deadline {
            return Err(error);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn install_forward(
    socket: &Path,
    mapping: Option<crate::grill::oci::PortMapping>,
) -> io::Result<()> {
    if forwards(socket, mapping).await? {
        return Ok(());
    }
    let mapping = mapping.ok_or_else(|| io::Error::other("missing original port mapping"))?;
    let added = api(socket, serde_json::json!({"execute": "add_hostfwd", "arguments": {"proto": "tcp", "host_addr": "0.0.0.0", "host_port": mapping.host_port, "guest_addr": "10.0.2.100", "guest_port": mapping.container_port}})).await?;
    if added["return"]["id"].as_u64().is_none_or(|id| id == 0) {
        return Err(io::Error::other(
            "invalid rootless forwarding acknowledgement",
        ));
    }
    if !forwards(socket, Some(mapping)).await? {
        return Err(io::Error::other("rootless forwarding was not installed"));
    }
    Ok(())
}

impl RuncGrill {
    pub(super) async fn owned_prepare_rootless(
        &self,
        id: &InstanceId,
        context: &ClaimedCommandExecutor,
    ) -> io::Result<()> {
        let intent = context.intent().await?;
        let directory = directory(&intent);
        let executable = self.ownership()?.executable.clone();
        let config_path = self.bundle_base.join(&id.0).join("config.json");
        let instance = id.0.clone();
        tokio::task::spawn_blocking(move || {
            std::fs::DirBuilder::new().mode(0o700).create(&directory)?;
            let mut config: serde_json::Value = serde_json::from_slice(&std::fs::read(&config_path)?)?;
            config["hooks"] = serde_json::json!({"createRuntime": [{
                "path": executable,
                "args": [executable, "__rootless-network-gate", "--directory", directory, "--instance", instance],
                "timeout": 20
            }]});
            crate::sesame::identity::atomic_write_mode(&config_path, &serde_json::to_vec_pretty(&config)?, Some(0o600))
        }).await.map_err(io::Error::other)?
    }

    /// Ensure the owned helper is running and serving the original forward.
    /// A helper that dies before or during this check is replaced; only a
    /// helper this call started that keeps dying before readiness is an error.
    pub(super) async fn owned_rootless_network(
        &self,
        context: &ClaimedCommandExecutor,
        pid: u32,
    ) -> io::Result<()> {
        let intent = context.intent().await?;
        let directory = directory(&intent);
        crate::grill::process_owner::validate_socket_directory(&directory)?;
        let socket = directory.join("api.sock");
        let mut started = 0;
        loop {
            match context.role_state(RuntimeRole::RootlessNetwork).await? {
                Some(CommandState::Running { .. }) => {}
                // Only terminal roles (or fenced, never-started preparations) may
                // replace this socket. start_role performs the final admission check.
                Some(CommandState::Prepared) => {
                    return Err(io::Error::other(
                        "prepared rootless helper requires retirement",
                    ));
                }
                None | Some(CommandState::Cancelled | CommandState::Retired { .. }) => {
                    if started == HELPER_START_ATTEMPTS {
                        return Err(role_failure(
                            context,
                            RuntimeRole::RootlessNetwork,
                            "rootless network helper exited before readiness",
                        )
                        .await);
                    }
                    if started > 0 {
                        tokio::time::sleep(HELPER_RESTART_BACKOFF * started).await;
                    }
                    self.start_rootless_helper(context, &socket, pid).await?;
                    started += 1;
                }
            }
            if helper_ready(context, &socket, intent.spec.port_mapping).await? {
                return Ok(());
            }
        }
    }

    async fn start_rootless_helper(
        &self,
        context: &ClaimedCommandExecutor,
        socket: &Path,
        pid: u32,
    ) -> io::Result<()> {
        crate::grill::process_owner::remove_socket(socket)?;
        let stem = context
            .role_log_stem(RuntimeRole::Launcher)
            .await?
            .ok_or_else(|| io::Error::other("rootless launcher has no owner"))?;
        let launcher = stem
            .parent()
            .ok_or_else(|| io::Error::other("invalid launcher owner path"))?;
        let args = vec![
            "__rootless-network".into(),
            "--launcher".into(),
            launcher
                .to_str()
                .ok_or_else(|| io::Error::other("non-UTF-8 launcher path"))?
                .into(),
            "--container-pid".into(),
            pid.to_string(),
            "--api-socket".into(),
            socket
                .to_str()
                .ok_or_else(|| io::Error::other("non-UTF-8 rootless socket path"))?
                .into(),
        ];
        context
            .start_role(
                RuntimeRole::RootlessNetwork,
                &self.ownership()?.executable,
                &args,
                &BTreeMap::new(),
            )
            .await
    }

    pub(super) async fn owned_open_rootless_gate(
        &self,
        context: &ClaimedCommandExecutor,
    ) -> io::Result<()> {
        let directory = directory(&context.intent().await?);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let pid = loop {
            match tokio::fs::read(directory.join("init.json")).await {
                Ok(bytes) => break serde_json::from_slice::<u32>(&bytes)?,
                Err(error)
                    if error.kind() == io::ErrorKind::NotFound
                        && tokio::time::Instant::now() < deadline =>
                {
                    if matches!(
                        context.role_state(RuntimeRole::Launcher).await?,
                        Some(CommandState::Retired { .. })
                    ) {
                        return Err(role_failure(
                            context,
                            RuntimeRole::Launcher,
                            "rootless launcher exited before namespace creation",
                        )
                        .await);
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(error) => return Err(error),
            }
        };
        self.owned_rootless_network(context, pid).await?;
        tokio::task::spawn_blocking(move || {
            crate::sesame::identity::atomic_write_mode(
                &directory.join("ready"),
                b"ready",
                Some(0o600),
            )
        })
        .await
        .map_err(io::Error::other)?
    }

    pub(super) async fn owned_remove_rootless(&self, intent: &RuntimeIntent) -> io::Result<()> {
        let directory = directory(intent);
        tokio::task::spawn_blocking(move || {
            match crate::grill::process_owner::validate_socket_directory(&directory) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error),
            }
            crate::grill::process_owner::remove_socket(&directory.join("api.sock"))?;
            for name in ["init.json", "ready"] {
                match std::fs::remove_file(directory.join(name)) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
            std::fs::remove_dir(directory)
        })
        .await
        .map_err(io::Error::other)?
    }

    pub(in crate::grill::runc) async fn owned_network_record(
        &self,
        instance: &InstanceId,
    ) -> Option<crate::grill::records::RootlessNetworkRecord> {
        if !self.rootless {
            return None;
        }
        self.owned_operation(instance, |runtime, id, context| async move {
            let Some(CommandState::Running { pid }) =
                context.role_state(RuntimeRole::RootlessNetwork).await?
            else {
                return Ok(None);
            };
            let Some(container_pid) = runtime.owned_running_pid(&id, &context).await? else {
                return Ok(None);
            };
            let intent = context.intent().await?;
            Ok(Some(crate::grill::records::RootlessNetworkRecord {
                api_socket: directory(&intent).join("api.sock"),
                owner_pid: pid,
                owner_pid_started_at: crate::grill::records::process_start_time(pid)
                    .ok_or_else(|| io::Error::other("rootless helper identity is unavailable"))?,
                container_pid,
                port_mapping: intent.spec.port_mapping,
            }))
        })
        .await
        .ok()
        .flatten()
    }
}
